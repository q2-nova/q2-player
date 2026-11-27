//! Mainline input through the SDK bridge, with readiness on the I/O reactor.
#![forbid(unsafe_code)]
use super::InputSink;
use crate::input::{Key, Raw, Wheel};
use crate::rt;
use tokio::sync::{oneshot, watch};
pub(super) use super::input_ffi::Api;
use super::input_ffi::{Event, State, Read};
use std::fs::File;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicI32, Ordering},
};
use std::time::{Duration, Instant};

const KEYS: [(u16, Key); 5] = [
    (116, Key::Center),
    (103, Key::Back),
    (108, Key::PlayPause),
    (165, Key::Prev),
    (163, Key::Next),
];

/// The bridge selects CLOCK_MONOTONIC on each evdev descriptor. Translate
/// the kernel's frame timestamps with one reference per reactor drain.
#[derive(Clone, Copy)]
pub(super) struct FrameClock {
    now: Instant,
    monotonic: Option<Duration>,
}

impl FrameClock {
    pub(super) fn read(timestamped: bool) -> Self {
        let monotonic = timestamped.then(super::system::monotonic_time).flatten();
        let now = Instant::now();
        Self { now, monotonic }
    }

    pub(super) fn timestamp(self, event: &Event) -> Instant {
        let Some(clock) = self.monotonic else { return self.now; };
        let valid = event.seconds >= 0 && (0..1_000_000).contains(&event.microseconds);
        if !valid { return self.now; }
        let at = Duration::new(event.seconds as u64, event.microseconds as u32 * 1000);
        let age = clock.saturating_sub(at);
        self.now.checked_sub(age).unwrap_or(self.now)
    }
}

struct Decoder {
    wheel: Wheel,
    state: State,
    suppressed: u32,
    ignore_touch: bool,
    dropped: bool,
}
impl Decoder {
    fn new(step: i32, state: State) -> Self {
        let suppressed = state.keys;
        let ignore_touch = state.touch != 0;
        Self {
            wheel: Wheel::new(step),
            state,
            suppressed,
            ignore_touch,
            dropped: false,
        }
    }
    fn lost(&mut self, wheel: bool, sink: &dyn Fn(Raw)) {
        if wheel {
            sink(Raw::WheelEnd);
            self.wheel.sample(None);
        } else {
            sink(Raw::KeysReset);
            sink(Raw::HeadphonesChanged);
        }
        self.dropped = true;
    }
    fn event(&mut self, e: &Event, is_wheel: bool, step: i32, clock: FrameClock, sink: &dyn Fn(Raw)) {
        if e.kind == 0 && e.code == 3 {
            self.lost(is_wheel, sink);
        }
        if self.dropped {
            return;
        }
        if is_wheel {
            match (e.kind, e.code) {
                (3, 0) => self.state.position = e.value,
                (3, 24) | (1, 330) => self.state.touch = i32::from(e.value != 0),
                (0, 0) => {
                    self.wheel.step = step.max(4);
                    if self.state.touch == 0 {
                        self.ignore_touch = false;
                        self.wheel.sample(None);
                        sink(Raw::WheelEnd);
                    } else if !self.ignore_touch && (1..=200).contains(&self.state.position) {
                        let ticks = self.wheel.sample(Some(self.state.position));
                        if ticks != 0 {
                            sink(Raw::WheelMotion {
                                ticks,
                                at: clock.timestamp(e),
                            });
                        }
                    }
                }
                _ => {}
            }
        } else if e.kind == 5 && matches!(e.code, 2 | 6) {
            // SW_HEADPHONE_INSERT / SW_LINEOUT_INSERT. Sample both switches
            // authoritatively in the existing headphone owner, not this edge.
            sink(Raw::HeadphonesChanged);
        } else if e.kind == 1 && (e.value == 0 || e.value == 1) {
            if let Some((i, (_, key))) = KEYS
                .iter()
                .enumerate()
                .find(|(_, (code, _))| *code == e.code)
            {
                let bit = 1 << i;
                let was_down = self.state.keys & bit != 0;
                if e.value == 1 {
                    self.state.keys |= bit;
                } else {
                    self.state.keys &= !bit;
                }
                if self.suppressed & bit != 0 {
                    if e.value == 0 {
                        self.suppressed &= !bit;
                    }
                } else if was_down != (e.value == 1) {
                    sink(Raw::Key {
                        key: *key,
                        down: e.value == 1,
                    });
                }
            }
        }
    }
}

// The final close of a Q2 evdev descriptor can sleep for 90 ms. Keep the
// descriptor and its shutdown acknowledgement together from discovery through
// registration, rebind and cancellation (including an abandoned job result).
type InputFd = rt::ReactorFile<Arc<Completed>>;
impl rt::CloseFile for Arc<Completed> {
    fn close(self, file: File) {
        if rt::on_executor() {
            scanner().run_isolated(move |_| {
                drop(file);
                drop(self);
            });
        } else {
            drop(file);
        }
    }
}

fn prepare_device(api: &Api, kind: usize, step: i32, completion: &Arc<Completed>, blocking: &rt::Blocking) -> Result<(InputFd, Decoder), String> {
    let (fd, state) = api.device(kind, blocking)?;
    Ok((InputFd::new(fd, completion.clone()), Decoder::new(step, state)))
}
/// Independent device discovery/state lane: no resident worker before use;
/// its worker retires after the usual ten seconds. General card/DNS work
/// cannot occupy this lane. The static runtime is never dropped on an executor.
pub(super) fn scanner() -> &'static rt::Jobs {
    static SCANNER: OnceLock<rt::Jobs> = OnceLock::new();
    SCANNER.get_or_init(|| rt::Jobs::serial("input-scan").expect("input scan lane"))
}

struct Prepared {
    api: Arc<Api>,
    devices: [(InputFd, Decoder); 2],
    _completion: Arc<Completed>,
}

async fn prepare(api: Option<Api>, step: i32, completion: Arc<Completed>) -> Result<Prepared, String> {
    scanner().run_isolated(move |blocking| {
        let _completion = completion;
        let api = Arc::new(match api { Some(api) => api, None => Api::load(blocking)? });
        let devices = [prepare_device(&api, 0, step, &_completion, blocking)?, prepare_device(&api, 1, step, &_completion, blocking)?];
        Ok(Prepared { api, devices, _completion })
    }).await.map_err(|_| "Input discovery failed".to_string())?
}

/// A token follows finite device work even when its awaiting task is aborted.
/// Completion means neither a callback nor an in-flight ioctl remains.
pub(super) struct Completed(pub(super) Option<oneshot::Sender<()>>);
impl Drop for Completed {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() { let _ = done.send(()); }
    }
}

pub(super) struct CompletedWork<T> {
    pub(super) value: T,
    pub(super) _completion: Arc<Completed>,
}

struct Device {
    fd: Arc<rt::Fd<Arc<Completed>>>,
    decoder: Decoder,
}
impl Device {
    fn register((fd, decoder): (InputFd, Decoder)) -> Result<Self, String> {
        Ok(Self { fd: Arc::new(rt::Fd::register(fd).map_err(|e| e.to_string())?), decoder })
    }
}

const BATCH_LIMIT: usize = 64;
const REBIND: Duration = Duration::from_millis(500);
pub(super) struct Batch {
    pub(super) events: Vec<Event>,
    pub(super) drained: bool,
    pub(super) disconnected: bool,
}

pub(super) async fn read_batch<C: rt::CloseFile>(api: &Api, fd: &rt::Fd<C>, dropped: bool) -> std::io::Result<Batch> {
    fd.read_with(|file| {
        let mut batch = Batch { events: Vec::new(), drained: false, disconnected: false };
        for _ in 0..BATCH_LIMIT {
            match api.read_event(file) {
                Read::Event(event) => { batch.events.push(event); continue; }
                Read::Disconnected => { batch.disconnected = true; break; }
                Read::Interrupted => continue,
                Read::WouldBlock => {}
            }
            if batch.events.is_empty() && !dropped {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            batch.drained = true;
            break;
        }
        Ok(batch)
    }).await
}

pub(super) fn cancelled(cancel: &watch::Receiver<()>) -> bool { cancel.has_changed().is_err() }

async fn watch_device(
    api: Arc<Api>, kind: usize, initial: Device, sink: InputSink,
    step: Arc<AtomicI32>, mut cancel: watch::Receiver<()>, completed: Arc<Completed>,
) {
    let mut device = Some(initial);
    let mut retry = tokio::time::Instant::now() + REBIND;
    loop {
        if cancelled(&cancel) { return; }
        if device.is_none() {
            tokio::select! {
                biased;
                _ = cancel.changed() => return,
                _ = tokio::time::sleep_until(retry) => {}
            }
            let api = api.clone();
            let completion = completed.clone();
            let current_step = step.load(Ordering::Relaxed);
            // Await the finite job even if cancelled: close before suspend
            // joins it. The other device and I/O reactor remain responsive.
            let found = scanner().run_isolated(move |blocking| {
                let _completion = completion;
                CompletedWork { value: prepare_device(&api, kind, current_step, &_completion, blocking), _completion }
            }).await;
            if cancelled(&cancel) { return; }
            device = found.ok().and_then(|v| v.value.ok()).and_then(|v| Device::register(v).ok());
            if kind == 0 && device.is_some() { sink(Raw::HeadphonesChanged); }
            retry = tokio::time::Instant::now() + REBIND;
            continue;
        }
        let current = device.as_mut().unwrap();
        let batch = tokio::select! {
            biased;
            _ = cancel.changed() => return,
            batch = read_batch(&api, current.fd.as_ref(), current.decoder.dropped) => batch,
        };
        let mut gone = batch.is_err();
        if let Ok(batch) = batch {
            let clock = FrameClock::read(kind == 1);
            for event in &batch.events {
                if cancelled(&cancel) { return; }
                current.decoder.event(event, kind == 1, step.load(Ordering::Relaxed), clock, &*sink);
            }
            gone = batch.disconnected;
            if current.decoder.dropped && batch.drained && !gone {
                let api = api.clone();
                let fd = current.fd.clone();
                let completion = completed.clone();
                let snapshot = scanner().run_isolated(move |_blocking| {
                    let _completion = completion;
                    let fd = fd; // close before completion even if the awaiting task is aborted
                    api.snapshot(&*fd, kind, _blocking)
                }).await;
                if cancelled(&cancel) { return; }
                match snapshot.ok().flatten() {
                    Some(state) => {
                        current.decoder = Decoder::new(step.load(Ordering::Relaxed), state);
                        if kind == 0 { sink(Raw::HeadphonesChanged); }
                    },
                    None => gone = true,
                }
            }
        }
        if gone {
            current.decoder.lost(kind == 1, &*sink);
            device = None;
            log::warn!("mainline input disconnected: kind={kind}; waiting for rebind");
        }
        // Bound hot devices and repeated EINTR: neither input, cancellation
        // nor another I/O task can be starved by an always-ready descriptor.
        tokio::task::yield_now().await;
    }
}

pub struct Input {
    cancel: Option<watch::Sender<()>>,
    ready: Option<oneshot::Receiver<Result<(), String>>>,
    completed: Option<oneshot::Receiver<()>>,
    registration: Option<Result<(), String>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Input {
    /// Startup caller is outside the UI runtime. Resume uses open_into.
    pub fn open(sink: InputSink, step: Arc<AtomicI32>) -> Result<Self, String> {
        Self::open_with(None, sink, step)
    }
    fn open_with(api: Option<Api>, sink: InputSink, step: Arc<AtomicI32>) -> Result<Self, String> {
        assert!(rt::may_wait_for_tasks(), "synchronous input open requires a blocking owner");
        let mut input = Self::start(api, sink, step);
        let result = input.ready.take().unwrap().blocking_recv()
            .map_err(|_| "Input registration stopped".to_string()).and_then(|r| r);
        input.registration = Some(result.clone());
        result?;
        Ok(input)
    }
    #[cfg(test)]
    pub(super) fn open_api(api: Api, sink: InputSink, step: Arc<AtomicI32>) -> Result<Self, String> {
        Self::open_with(Some(api), sink, step)
    }
    fn start(api: Option<Api>, sink: InputSink, step: Arc<AtomicI32>) -> Self {
        let (cancel, receive_cancel) = watch::channel(());
        let (send_ready, ready) = oneshot::channel();
        let (send_done, completed) = oneshot::channel();
        let completion = Arc::new(Completed(Some(send_done)));
        let task = rt::io().spawn(async move {
            let _completion = completion;
            let prepared = match prepare(api, step.load(Ordering::Relaxed), _completion.clone()).await {
                Ok(prepared) => prepared,
                Err(error) => { let _ = send_ready.send(Err(error)); return; }
            };
            if cancelled(&receive_cancel) { return; }
            let [buttons, wheel] = prepared.devices;
            let devices = Device::register(buttons).and_then(|buttons| {
                Device::register(wheel).map(|wheel| (buttons, wheel))
            });
            let (buttons, wheel) = match devices {
                Ok(devices) => devices,
                Err(error) => { let _ = send_ready.send(Err(error)); return; }
            };
            if send_ready.send(Ok(())).is_err() { return; }
            tokio::join!(
                watch_device(prepared.api.clone(), 0, buttons, sink.clone(), step.clone(), receive_cancel.clone(), _completion.clone()),
                watch_device(prepared.api, 1, wheel, sink, step, receive_cancel, _completion.clone()),
            );
        });
        Self { cancel: Some(cancel), ready: Some(ready), completed: Some(completed), registration: None, task: Some(task) }
    }
    /// The slot belongs to the platform outside the UI future. Cancellation
    /// retains discovery and registration for close or a subsequent await.
    pub(super) async fn open_api_into(slot: &mut Option<Self>, api: Option<Api>, sink: InputSink, step: Arc<AtomicI32>) -> Result<(), String> {
        // Install the owner before the first await (including discovery). A
        // cancelled caller can re-enter this same pending registration.
        let input = slot.get_or_insert_with(|| Self::start(api, sink, step));
        if input.cancel.is_none() {
            input.close().await;
            slot.take();
            return Err("Input is closing".into());
        }
        if input.registration.is_none() {
            let result = input.ready.as_mut().unwrap().await
                .map_err(|_| "Input registration stopped".to_string()).and_then(|r| r);
            input.registration = Some(result);
            input.ready.take();
        }
        let result = input.registration.as_ref().unwrap().clone();
        if result.is_err() {
            input.close().await;
            slot.take();
        }
        result
    }
    pub fn is_ready(&self) -> bool {
        matches!(self.registration, Some(Ok(()))) && self.cancel.is_some()
            && self.task.as_ref().is_some_and(|task| !task.is_finished())
    }
    /// Joined cancellation, including any discovery/state syscall. Cancelling
    /// the future leaves the receiver in this owner for a later close or Drop.
    pub async fn close(&mut self) {
        self.cancel.take();
        if let Some(done) = self.completed.as_mut() { let _ = done.await; }
        self.completed.take();
        if let Some(task) = self.task.as_mut() { let _ = task.await; }
        self.task.take();
        self.ready.take();
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        self.cancel.take();
        if let Some(done) = self.completed.take() {
            assert!(rt::may_wait_for_tasks(), "input must be asynchronously closed before executor teardown");
            let _ = done.blocking_recv();
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Mutex;

    pub(in crate::platform) use crate::platform::input_ffi::fixture::*;
    fn wait_for(mut condition: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(
                Instant::now() < end,
                "generated input reactor did not make progress"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn io_thread() -> std::path::PathBuf {
        let mut found = None;
        wait_for(|| {
            found = std::fs::read_dir("/proc/self/task")
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    std::fs::read_to_string(path.join("comm"))
                        .is_ok_and(|name| name.trim() == "io")
                });
            found.is_some()
        });
        found.unwrap()
    }
    fn switches(thread: &std::path::Path) -> u64 {
        std::fs::read_to_string(thread.join("status"))
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("voluntary_ctxt_switches:")
                    .and_then(|value| value.trim().parse().ok())
            })
            .unwrap()
    }
    #[test]
    fn cancelled_discovery_reenters_one_owned_registration_and_close() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let _fixture = Fixture::new();
        HOOKS.lock().unwrap().stall_open = true;
        let mut slot = None;
        let ui = rt::Ui::new().unwrap();
        ui.block_on(async {
            assert!(tokio::time::timeout(Duration::from_millis(20), Input::open_api_into(
                &mut slot, Some(mock_api()), Arc::new(|_| {}), Arc::new(AtomicI32::new(20)))).await.is_err());
        });
        wait_for(|| HOOKS.lock().unwrap().entered == 1);
        assert!(!slot.as_ref().unwrap().is_ready());
        release_hooks();
        ui.block_on(Input::open_api_into(&mut slot, Some(mock_api()), Arc::new(|_| {}), Arc::new(AtomicI32::new(20)))).unwrap();
        assert_eq!(HOOKS.lock().unwrap().opens, [1, 1, 0], "reentry must not discover twice");
        assert!(slot.as_ref().unwrap().is_ready());
        ui.block_on(slot.as_mut().unwrap().close());
        assert!(!slot.as_ref().unwrap().is_ready());
        drop(slot);
    }

    #[test]
    fn cancelled_close_joins_stalled_discovery_even_after_task_abort() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let _fixture = Fixture::new();
        HOOKS.lock().unwrap().stall_open = true;
        let mut input = Input::start(Some(mock_api()), Arc::new(|_| panic!("closed input callback")), Arc::new(AtomicI32::new(20)));
        wait_for(|| HOOKS.lock().unwrap().entered == 1);
        let ui = rt::Ui::new().unwrap();
        input.task.as_ref().unwrap().abort();
        ui.block_on(async {
            assert!(tokio::time::timeout(Duration::from_millis(20), input.close()).await.is_err(), "finite scanner remains owned after task abort");
        });
        release_hooks();
        ui.block_on(input.close());
        assert!(input.completed.is_none());
        drop(input);
    }

    #[test]
    fn cancelled_registered_input_waits_for_close_without_holding_the_reactor() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let _fixture = Fixture::new();
        let mut input = Input::open_api(mock_api(), Arc::new(|_| {}), Arc::new(AtomicI32::new(20))).unwrap();
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let blocker = scanner().run_isolated(move |_| {
            entered.send(()).unwrap();
            let _ = held.recv_timeout(Duration::from_secs(5));
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        input.cancel.take();
        let ui = rt::Ui::new().unwrap();
        ui.block_on(async {
            // The reactor has retired both registrations while their final
            // close is still queued behind the occupied scanner.
            tokio::time::timeout(Duration::from_secs(2), input.task.as_mut().unwrap()).await.unwrap().unwrap();
            input.task.take();
            assert!(matches!(input.completed.as_mut().unwrap().try_recv(), Err(oneshot::error::TryRecvError::Empty)));
            release.send(()).unwrap();
            blocker.await.unwrap();
            input.close().await;
        });
    }

    #[test]
    fn stalled_state_recovery_keeps_other_input_live_and_close_joined() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let fixture = Fixture::new();
        let (send, receive) = std::sync::mpsc::channel();
        let mut input = Input::open_api(mock_api(), Arc::new(move |e| { let _ = send.send(e); }), Arc::new(AtomicI32::new(20))).unwrap();
        HOOKS.lock().unwrap().stall_state = true;
        let event = Event { kind: 0, code: 3, ..Default::default() };
        send_event(&fixture._wheel, &event);
        wait_for(|| HOOKS.lock().unwrap().entered == 1);
        send_key(&fixture.buttons, true);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !matches!(receive.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(), Raw::Key { down: true, .. }) {}
        let ui = rt::Ui::new().unwrap();
        ui.block_on(async { assert!(tokio::time::timeout(Duration::from_millis(20), input.close()).await.is_err()); });
        release_hooks();
        ui.block_on(input.close());
        assert_eq!(HOOKS.lock().unwrap().opens, [1, 1, 0]);
    }

    #[test]
    fn eintr_batches_preserve_readiness_and_held_key_snapshot() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let fixture = Fixture::new();
        HOOKS.lock().unwrap().held = 1;
        let (send, receive) = std::sync::mpsc::channel();
        let input = Input::open_api(mock_api(), Arc::new(move |e| { let _ = send.send(e); }), Arc::new(AtomicI32::new(20))).unwrap();
        // More EINTRs than a batch must yield without clearing the sole edge.
        INTERRUPTS.store(BATCH_LIMIT * 3, Ordering::SeqCst);
        send_key(&fixture.buttons, true);
        send_key(&fixture.buttons, false);
        send_key(&fixture.buttons, true);
        send_key(&fixture.buttons, false);
        assert!(matches!(receive.recv_timeout(Duration::from_secs(1)).unwrap(), Raw::Key { down: true, .. }));
        assert!(matches!(receive.recv_timeout(Duration::from_secs(1)).unwrap(), Raw::Key { down: false, .. }));
        assert!(receive.recv_timeout(Duration::from_millis(20)).is_err(), "held wake press cannot escape suppression");
        drop(input);
    }

    #[test]
    fn discovery_and_registration_failure_leave_no_live_owner() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let _fixture = Fixture::new();
        HOOKS.lock().unwrap().fail_state = true;
        assert!(Input::open_api(mock_api(), Arc::new(|_| {}), Arc::new(AtomicI32::new(20))).is_err());
        HOOKS.lock().unwrap().fail_state = false;
        // Regular files cannot enter epoll: actual registration error path.
        mock_devices().lock().unwrap().readers[1] = Some(std::fs::File::open("/dev/null").unwrap().into());
        let mut slot = None;
        rt::Ui::new().unwrap().block_on(Input::open_api_into(&mut slot, Some(mock_api()), Arc::new(|_| {}), Arc::new(AtomicI32::new(20)))).unwrap_err();
        assert!(slot.is_none());
    }

    fn scanner_threads() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().filter_map(Result::ok)
            .filter(|entry| std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|name| name.trim() == "input-scan"))
            .count()
    }

    #[test]
    fn discovery_worker_retires_while_input_delivery_remains_live() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let fixture = Fixture::new();
        let (send, receive) = std::sync::mpsc::channel();
        let input = Input::open_api(mock_api(), Arc::new(move |e| { let _ = send.send(e); }), Arc::new(AtomicI32::new(20))).unwrap();
        assert_eq!(scanner_threads(), 1);
        let deadline = Instant::now() + Duration::from_secs(12);
        while scanner_threads() != 0 {
            assert!(Instant::now() < deadline, "idle discovery worker did not retire");
            std::thread::sleep(Duration::from_millis(10));
        }
        send_key(&fixture.buttons, true);
        assert!(matches!(receive.recv_timeout(Duration::from_secs(1)).unwrap(), Raw::Key { down: true, .. }));
        assert_eq!(scanner_threads(), 0, "ordinary input delivery must not recreate discovery worker");
        drop(input);
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn failed_input_task_is_unready_and_completion_still_joins() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let fixture = Fixture::new();
        let mut input = Input::open_api(mock_api(), Arc::new(|_| panic!("generated input task failure")), Arc::new(AtomicI32::new(20))).unwrap();
        send_key(&fixture.buttons, true);
        wait_for(|| input.task.as_ref().unwrap().is_finished());
        assert!(!input.is_ready(), "health must not advertise a dead event owner");
        rt::Ui::new().unwrap().block_on(input.close());
    }

    #[test]
    fn actual_input_reactor_idle_delivery_rebind_and_shutdown() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        *HOOKS.lock().unwrap() = Hooks::default();
        let mut buttons = Some(new_device(0));
        let wheel = new_device(1);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let input = Input::open_api(
            mock_api(),
            Arc::new(move |event| sink.lock().unwrap().push(event)),
            Arc::new(AtomicI32::new(20)),
        )
        .unwrap();
        let thread = io_thread();
        std::thread::sleep(Duration::from_millis(20));
        let before = switches(&thread);
        let idle = Instant::now();
        std::thread::sleep(Duration::from_secs(2));
        let idle_seconds = idle.elapsed().as_secs_f64();
        let voluntary = switches(&thread) - before;
        let delivered = Instant::now();
        send_key(buttons.as_ref().unwrap(), true);
        send_key(buttons.as_ref().unwrap(), false);
        wait_for(|| {
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, Raw::Key { .. }))
                .count()
                == 2
        });
        let delivery_us = delivered.elapsed().as_micros();
        // A real pipe HUP follows the same poll error path as evdev removal.
        mock_devices().lock().unwrap().readers[0] = None;
        drop(buttons.take());
        wait_for(|| {
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Raw::KeysReset))
        });
        buttons = Some(new_device(0));
        std::thread::sleep(Duration::from_millis(600));
        send_key(buttons.as_ref().unwrap(), true);
        send_key(buttons.as_ref().unwrap(), false);
        wait_for(|| {
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, Raw::Key { .. }))
                .count()
                == 4
        });
        let opens = HOOKS.lock().unwrap().opens;
        assert!(opens[0] >= 2);
        assert_eq!(opens[1], 1, "connected wheel must not be periodically rediscovered");
        assert!(events.lock().unwrap().iter().filter(|e| matches!(e, Raw::HeadphonesChanged)).count() >= 2,
            "loss and successful button-device rebind require authoritative jack samples");
        let shutdown = Instant::now();
        let (stopped, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(input);
            let _ = stopped.send(());
        });
        receive
            .recv_timeout(Duration::from_millis(250))
            .expect("shutdown must wake an idle input poll");
        let shutdown_ms = shutdown.elapsed().as_secs_f64() * 1000.0;
        assert!(shutdown_ms < 250.0, "input shutdown must remain bounded");
        drop(buttons);
        drop(wheel);
        *mock_devices().lock().unwrap() = MockDevices {
            readers: [None, None, None],
        };
        let output = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"))
            .join(format!("input-reactor-{}.json", fastrand::u64(..)));
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(output, serde_json::to_vec_pretty(&serde_json::json!({
            "scope": "generated SDK pipe endpoints driving the production reactor; execution domain belongs to the harness receipt",
            "idle_seconds": idle_seconds, "voluntary_switches": voluntary,
            "input_delivery_us": delivery_us, "shutdown_ms": shutdown_ms,
            "keys_delivered": 4, "disconnect_and_rebind": true,
            "power_measurement": false
        })).unwrap()).unwrap();
    }
    #[test]
    fn held_button_deadline_fires_without_input_poll_ticks() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let buttons = new_device(0);
        let wheel = new_device(1);
        let (send, receive) = std::sync::mpsc::channel();
        let input = Input::open_api(
            mock_api(),
            Arc::new(move |event| {
                send.send(event).unwrap();
            }),
            Arc::new(AtomicI32::new(20)),
        )
        .unwrap();
        send_key(&buttons, true);
        let Raw::Key { key, down } = receive.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("expected a real decoded key down");
        };
        let mut keys = crate::input::Keys::default();
        let mut events = Vec::new();
        keys.key(key, down, Instant::now(), &mut events);
        let deadline = keys.next_deadline().unwrap();
        // The UI waits on its own deadline. The input worker stays in poll,
        // delivering no periodic message to drive a held-key timer.
        assert!(matches!(
            receive.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        keys.tick(Instant::now(), &mut events);
        assert_eq!(events, vec![crate::input::Event::CenterLong]);
        send_key(&buttons, false);
        let Raw::Key { key, down } = receive.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("expected key release");
        };
        keys.key(key, down, Instant::now(), &mut events);
        assert_eq!(
            events,
            vec![crate::input::Event::CenterLong],
            "no short press after hold"
        );
        assert!(!keys.any_held());
        drop(input);
        drop((buttons, wheel));
        *mock_devices().lock().unwrap() = MockDevices {
            readers: [None, None, None],
        };
    }
    #[test]
    fn jack_edges_and_dropped_event_recovery_publish_non_gesture_notifications() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let fixture = Fixture::new();
        let (send, receive) = std::sync::mpsc::channel();
        let input = Input::open_api(mock_api(), Arc::new(move |e| { let _ = send.send(e); }), Arc::new(AtomicI32::new(20))).unwrap();
        for code in [2, 6] {
            send_event(&fixture.buttons, &Event {kind: 5, code, value: 1, ..Default::default()});
            assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), Raw::HeadphonesChanged);
        }
        send_event(&fixture.buttons, &Event {kind: 0, code: 3, ..Default::default()});
        assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), Raw::KeysReset);
        assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), Raw::HeadphonesChanged);
        assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), Raw::HeadphonesChanged,
            "successful resynchronization publishes even without a surviving switch edge");
        drop(input);
    }

    #[test]
    fn queued_wheel_frames_keep_their_speed_through_list_navigation() {
        use crate::ui::list::{ListView, Row, RowAction};
        let now = Instant::now();
        let clock = FrameClock { now, monotonic: Some(Duration::from_secs(20)) };
        for (spacing_ms, expected, letter) in [(45, 40, Some("B")), (60, 19, None)] {
            // All twenty frames can be drained in one reactor read. The first
            // frame establishes the ring position and the next nineteen emit one
            // detent each, crossing the 1/200 seam along the way.
            let mut decoder = Decoder::new(20, State::default());
            let mut list = ListView::new(true);
            list.alphabetical = true;
            list.set_rows((0..120).map(|i| {
                let letter = ['A', 'B', 'C'][i / 40];
                Row::new(format!("{letter} Album {i}"), RowAction::None)
            }).collect());
            let out = RefCell::new(Vec::new());
            decoder.event(&Event { kind: 1, code: 330, value: 1, ..Default::default() },
                true, 20, clock, &|raw| out.borrow_mut().push(raw));
            for frame in 0..20 {
                let position = (9i32 - 20 * frame).rem_euclid(200) + 1;
                decoder.event(&Event { kind: 3, code: 0, value: position, ..Default::default() },
                    true, 20, clock, &|raw| out.borrow_mut().push(raw));
                decoder.event(&Event { kind: 0, code: 0, seconds: 18 + i64::from(frame * spacing_ms / 1000),
                    microseconds: (frame * spacing_ms % 1000) * 1000, ..Default::default() },
                    true, 20, clock, &|raw| out.borrow_mut().push(raw));
            }
            let events = out.into_inner();
            assert_eq!(events.len(), 19);
            for (index, raw) in events.into_iter().enumerate() {
                let Raw::WheelMotion { ticks: 1, at } = raw else { panic!("{raw:?}"); };
                assert_eq!(at, now - Duration::from_millis(2000 - (index as u64 + 1) * spacing_ms as u64));
                list.wheel(1, at);
                if index < 15 {
                    assert_eq!(list.selected, index + 1);
                }
            }
            assert_eq!(list.selected, expected);
            assert_eq!(list.letter_label(), letter);
        }
    }

    #[test]
    fn wheel_frames_wrap_lift_and_recovery() {
        let out = RefCell::new(Vec::new());
        let sink = |r| out.borrow_mut().push(r);
        let mut d = Decoder::new(20, State::default());
        let clock = FrameClock { now: Instant::now(), monotonic: None };
        let event = |kind, code, value| Event {
            kind,
            code,
            value,
            ..Default::default()
        };
        for e in [
            event(3, 0, 10),
            event(3, 24, 1),
            event(1, 330, 1),
            event(0, 0, 0),
            event(3, 0, 190),
        ] {
            d.event(&e, true, 20, clock, &sink);
        }
        assert!(
            out.borrow().is_empty(),
            "motion waits for complete SYN frame"
        );
        d.event(&event(0, 0, 0), true, 20, clock, &sink);
        assert!(matches!(
            out.borrow().last(),
            Some(Raw::WheelMotion { ticks: 1, .. })
        ));
        d.event(&event(0, 3, 0), true, 20, clock, &sink);
        d.event(&event(3, 0, 90), true, 20, clock, &sink);
        assert!(matches!(out.borrow().last(), Some(Raw::WheelEnd)));
        let count = out.borrow().len();
        d = Decoder::new(
            20,
            State {
                position: 90,
                touch: 1,
                keys: 0,
            },
        );
        for e in [event(3, 0, 30), event(0, 0, 0)] {
            d.event(&e, true, 20, clock, &sink);
        }
        assert_eq!(
            out.borrow().len(),
            count,
            "resync ignores a held touch until lift"
        );
        for e in [
            event(3, 24, 0),
            event(0, 0, 0),
            event(3, 0, 100),
            event(3, 24, 1),
            event(0, 0, 0),
            event(3, 0, 80),
            event(0, 0, 0),
        ] {
            d.event(&e, true, 20, clock, &sink);
        }
        assert!(matches!(
            out.borrow().last(),
            Some(Raw::WheelMotion { ticks: 1, .. })
        ));
    }
    #[test]
    fn held_keys_on_reconnect_do_not_click_and_loss_cancels() {
        let clock = FrameClock { now: Instant::now(), monotonic: None };
        let out = RefCell::new(Vec::new());
        let sink = |r| out.borrow_mut().push(r);
        let mut d = Decoder::new(
            20,
            State {
                keys: 1,
                ..Default::default()
            },
        );
        for value in [1, 2, 0] {
            d.event(
                &Event {
                    kind: 1,
                    code: 116,
                    value,
                    ..Default::default()
                },
                false,
                20,
                clock,
                &sink,
            );
        }
        assert!(out.borrow().is_empty());
        d.event(
            &Event {
                kind: 1,
                code: 116,
                value: 1,
                ..Default::default()
            },
            false,
            20,
            clock,
            &sink,
        );
        d.lost(false, &sink);
        assert_eq!(
            *out.borrow(),
            vec![
                Raw::Key {
                    key: Key::Center,
                    down: true
                },
                Raw::KeysReset,
                Raw::HeadphonesChanged
            ]
        );
    }
}
