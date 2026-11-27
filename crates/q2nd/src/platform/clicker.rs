#![forbid(unsafe_code)]
//! Mainline's PWM beeper; no vendor MCU firmware or per-click subprocesses.
//! Each complete pulse owns one finite local job. evdev writes and close can
//! sleep even with O_NONBLOCK, so neither runs on the I/O executor.
use super::beeper::Beeper;
use crate::rt::{Blocking, Jobs};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Click { Normal, Tone(u16) }

trait Device: Send + 'static {
    fn set(&mut self, blocking: &Blocking, click: Option<Click>) -> io::Result<()>;
    fn pause(&mut self, _blocking: &Blocking, duration: Duration) { std::thread::sleep(duration); }
}
impl Device for Beeper {
    fn set(&mut self, blocking: &Blocking, click: Option<Click>) -> io::Result<()> {
        match click {
            Some(Click::Tone(hz)) => self.tone(blocking, hz),
            _ => self.set(blocking, click.is_some()),
        }
    }
}

/// Even an abandoned job reply must close evdev on a finite worker. The
/// completion sender travels with the descriptor, never with its observer.
struct Owner<D: Device>(Option<(D, std::sync::mpsc::Sender<()>)>);
impl<D: Device> Owner<D> {
    fn close(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.0.take().map(|(device, done)| crate::rt::jobs().run_isolated(move |blocking| finish(blocking, device, done)))
    }
}
impl<D: Device> Drop for Owner<D> { fn drop(&mut self) { let _ = self.close(); } }

fn finish<D: Device>(blocking: &Blocking, mut device: D, done: std::sync::mpsc::Sender<()>) {
    if let Err(error) = device.set(blocking, None) { log::warn!("Clicker final stop failed: {error}"); }
    // Beeper drops the descriptor before its loaded Library. Do not
    // acknowledge shutdown until both have actually retired.
    drop(device);
    let _ = done.send(());
}

pub struct Clicker {
    sender: Option<Sender<Click>>,
    /// Covers dispatch as well as sounding: scrolling cannot build a backlog.
    busy: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    done: Option<std::sync::mpsc::Receiver<()>>,
    pitched: bool,
}

impl Clicker {
    pub fn open() -> Result<Self, String> {
        let beeper = Beeper::open()?;
        let pitched = beeper.pitched();
        Ok(Self::start(beeper, pitched))
    }
    fn start(device: impl Device, pitched: bool) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        let busy = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (finished, done) = std::sync::mpsc::channel();
        crate::rt::io().spawn(run(receiver, busy.clone(), stopped.clone(), Owner(Some((device, finished)))));
        Self { sender: Some(sender), busy, stopped, done: Some(done), pitched }
    }
    fn send(&self, click: Click) {
        if self.busy.load(Ordering::Acquire) { return; }
        if let Some(sender) = &self.sender { let _ = sender.try_send(click); }
    }
    pub fn click(&self) { self.send(Click::Normal); }
    pub fn tone(&self, hz: u16) {
        if !self.pitched { self.click(); return; }
        self.send(Click::Tone(hz.clamp(500, 4000)));
    }
}
impl Drop for Clicker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.sender.take();
        if let Some(done) = self.done.take() {
            // Ordinary platform teardown may wait boundedly; unwinding a
            // runtime must not park it. Either way the owner retains cleanup.
            if !crate::rt::may_wait_for_tasks() || done.recv_timeout(Duration::from_millis(200)).is_err() {
                log::warn!("Clicker shutdown completion not observed");
            }
        }
    }
}

fn pulse<D: Device>(jobs: &Jobs, mut owner: Owner<D>, stopped: Arc<AtomicBool>, click: Click)
    -> tokio::task::JoinHandle<(Owner<D>, io::Result<()>)>
{
    jobs.run_isolated(move |blocking| {
        // This check admits the pulse. Stop racing after it cannot cancel a
        // hardware start; the admitted pulse must still reach its off step.
        let result = if stopped.load(Ordering::Acquire) { Ok(()) } else {
            let device = &mut owner.0.as_mut().expect("live clicker owner").0;
            device.set(blocking, Some(click)).and_then(|()| {
                // Keep on/interval/off in the same job: off cannot queue
                // behind unrelated work. This is not exact acoustic timing.
                device.pause(blocking, Duration::from_millis(match click { Click::Normal => 2, Click::Tone(_) => 8 }));
                device.set(blocking, None)
            })
        };
        // A failed write may already have changed hardware state. Retry off
        // and close in this same turn, not behind another queued local job.
        if result.is_err() && let Some((device, done)) = owner.0.take() { finish(blocking, device, done); }
        (owner, result)
    })
}
async fn run<D: Device>(mut receiver: Receiver<Click>, busy: Arc<AtomicBool>, stopped: Arc<AtomicBool>, mut owner: Owner<D>) {
    while let Some(click) = receiver.recv().await {
        if stopped.load(Ordering::Acquire) { break; }
        busy.store(true, Ordering::Release);
        match pulse(crate::rt::jobs(), owner, stopped.clone(), click).await {
            Ok((returned, result)) => {
                owner = returned;
                if let Err(error) = result { log::warn!("Clicker pulse failed: {error}"); break; }
            }
            Err(_) => { log::warn!("Clicker pulse worker stopped"); return; }
        }
        busy.store(false, Ordering::Release);
    }
    receiver.close();
    if let Some(close) = owner.close() { let _ = close.await; }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc as sync};
    const BOUND: Duration = Duration::from_secs(5);
    #[derive(Debug, PartialEq, Eq)]
    enum Event { Set(Option<Click>), Pause(Duration), Closed }
    struct Fake {
        events: Arc<Mutex<Vec<Event>>>,
        hold: Option<(sync::Sender<()>, sync::Receiver<()>)>,
        fail: Option<bool>,
        close_hold: Option<(sync::Sender<()>, sync::Receiver<()>)>,
    }
    impl Device for Fake {
        fn set(&mut self, _: &Blocking, click: Option<Click>) -> io::Result<()> {
            assert!(crate::rt::on_pool());
            self.events.lock().unwrap().push(Event::Set(click));
            if click.is_some() && let Some((entered, release)) = self.hold.take() {
                entered.send(()).unwrap(); release.recv_timeout(BOUND).unwrap();
            }
            if self.fail == Some(click.is_some()) { Err(io::Error::other("generated device error")) } else { Ok(()) }
        }
        fn pause(&mut self, _: &Blocking, duration: Duration) { self.events.lock().unwrap().push(Event::Pause(duration)); }
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            assert!(crate::rt::on_pool());
            if let Some((entered, release)) = self.close_hold.take() {
                entered.send(()).unwrap(); release.recv_timeout(BOUND).unwrap();
            }
            self.events.lock().unwrap().push(Event::Closed);
        }
    }
    fn fixture(fail: Option<bool>) -> (Fake, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (Fake { events: events.clone(), hold: None, fail, close_hold: None }, events)
    }
    fn join(done: sync::Receiver<()>) { done.recv_timeout(BOUND).unwrap(); }
    fn owner(device: Fake) -> (Owner<Fake>, sync::Receiver<()>) {
        let (send, done) = sync::channel(); (Owner(Some((device, send))), done)
    }
    #[test]
    fn busy_clicks_are_dropped_and_shutdown_stops_the_burst() {
        let (mut device, events) = fixture(None);
        let (entered, waiting) = sync::channel(); let (release, resume) = sync::channel();
        device.hold = Some((entered, resume));
        let mut clicker = Clicker::start(device, true);
        clicker.click(); waiting.recv_timeout(BOUND).unwrap();
        for _ in 0..20 { clicker.click(); clicker.tone(1500); }
        let done = clicker.done.take().unwrap();
        clicker.stopped.store(true, Ordering::Release); clicker.sender.take();
        release.send(()).unwrap(); join(done);
        assert_eq!(*events.lock().unwrap(), [Event::Set(Some(Click::Normal)), Event::Pause(Duration::from_millis(2)), Event::Set(None), Event::Set(None), Event::Closed]);
    }
    #[test]
    fn failed_start_attempts_off_and_stops_accepting_clicks() {
        let (device, events) = fixture(Some(true)); let mut clicker = Clicker::start(device, true);
        let done = clicker.done.take().unwrap(); clicker.click(); join(done);
        let sender = clicker.sender.as_ref().unwrap().clone();
        let (closed, closure) = sync::channel();
        crate::rt::io().spawn(async move { sender.closed().await; closed.send(()).unwrap(); });
        closure.recv_timeout(BOUND).unwrap();
        assert!(clicker.sender.as_ref().unwrap().try_send(Click::Normal).is_err());
        assert_eq!(*events.lock().unwrap(), [Event::Set(Some(Click::Normal)), Event::Set(None), Event::Closed]);
    }
    #[test]
    fn pitched_click_preserves_pitch_and_has_a_bounded_duration() {
        let (device, events) = fixture(None); let (owner, done) = owner(device);
        let job = pulse(crate::rt::jobs(), owner, Arc::default(), Click::Tone(1500));
        crate::rt::io().spawn(async move { drop(job.await.unwrap()); }); join(done);
        assert_eq!(*events.lock().unwrap(), [Event::Set(Some(Click::Tone(1500))), Event::Pause(Duration::from_millis(8)), Event::Set(None), Event::Set(None), Event::Closed]);
    }
    #[test]
    fn stalled_pulse_and_cancelled_observer_keep_io_live_and_close_before_ack() {
        // Both a successful abandoned sounding reply and an off-error owner
        // must retain cleanup after the coordinator is cancelled.
        for failure in [None, Some(false)] {
            let (mut device, events) = fixture(failure);
            let (entered, waiting) = sync::channel(); let (release, resume) = sync::channel();
            device.hold = Some((entered, resume));
            let (closing, close_seen) = sync::channel(); let (close_release, close_resume) = sync::channel();
            device.close_hold = Some((closing, close_resume));
            let (owner, done) = owner(device);
            let (sender, receiver) = mpsc::channel(1); sender.try_send(Click::Normal).unwrap();
            let worker = crate::rt::io().spawn(run(receiver, Arc::default(), Arc::default(), owner));
            waiting.recv_timeout(BOUND).unwrap();
            let (beat, heartbeat) = sync::channel();
            crate::rt::io().spawn(async move { tokio::task::yield_now().await; beat.send(()).unwrap(); });
            heartbeat.recv_timeout(BOUND).unwrap();
            worker.abort();
            let (cancelled, cancellation) = sync::channel();
            crate::rt::io().spawn(async move { assert!(worker.await.unwrap_err().is_cancelled()); cancelled.send(()).unwrap(); });
            cancellation.recv_timeout(BOUND).unwrap();
            assert!(matches!(done.try_recv(), Err(sync::TryRecvError::Empty)));
            release.send(()).unwrap(); close_seen.recv_timeout(BOUND).unwrap();
            assert!(matches!(done.try_recv(), Err(sync::TryRecvError::Empty)), "ACK must follow actual device close");
            close_release.send(()).unwrap(); join(done);
            assert_eq!(*events.lock().unwrap(), [Event::Set(Some(Click::Normal)), Event::Pause(Duration::from_millis(2)), Event::Set(None), Event::Set(None), Event::Closed]);
        }
    }
    #[test]
    fn shutdown_discards_queued_pulse_and_abandoned_reply_still_closes() {
        // A private fixture lane proves queued-before-start behavior without
        // starving the shared pool or racing other tests' saturation controls.
        let jobs = Jobs::serial("clicker-test").unwrap();
        let (entered, waiting) = sync::channel(); let (release, resume) = sync::channel();
        let held = jobs.run_isolated(move |_| { entered.send(()).unwrap(); resume.recv_timeout(BOUND).unwrap(); });
        waiting.recv_timeout(BOUND).unwrap();
        let (device, events) = fixture(None); let (owner, done) = owner(device);
        let stopped = Arc::new(AtomicBool::new(false));
        let job = pulse(&jobs, owner, stopped.clone(), Click::Normal);
        stopped.store(true, Ordering::Release); drop(job);
        release.send(()).unwrap();
        crate::rt::io().spawn(async move { held.await.unwrap(); });
        join(done); drop(jobs);
        assert_eq!(*events.lock().unwrap(), [Event::Set(None), Event::Closed]);
    }
}
