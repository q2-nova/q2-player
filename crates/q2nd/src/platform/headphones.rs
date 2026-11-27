//! Standard EV_SW jack state; keep the SDK's ioctl ABI inside the C bridge.
use libloading::Library;
use std::ffi::c_int;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub struct Headphones {
    _library: Library,
    open: unsafe extern "C" fn(c_int, c_int) -> c_int,
    state: unsafe extern "C" fn(c_int) -> c_int,
    fd: Option<OwnedFd>,
    retry: Instant,
}

impl Headphones {
    pub fn open() -> Result<Self, String> {
        let library = unsafe { Library::new("libq2platform.so") }.map_err(|e| e.to_string())?;
        let open = *unsafe { library.get(b"q2_input_open\0") }.map_err(|e| e.to_string())?;
        let state = *unsafe { library.get(b"q2_headphones_state\0") }.map_err(|e| e.to_string())?;
        Ok(Self { _library: library, open, state, fd: None, retry: Instant::now() })
    }

    fn get(&mut self, _blocking: &crate::rt::Blocking) -> Option<bool> {
        if self.fd.is_none() {
            if Instant::now() < self.retry { return None; }
            self.retry = Instant::now() + Duration::from_secs(5);
            let fd = unsafe { (self.open)(0, 1) };
            if fd < 0 { return None; }
            self.fd = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        // EVIOCGSW returns current state even if the independent event queue
        // overflowed. An unavailable/rebinding device must not look unplugged.
        let state = unsafe { (self.state)(self.fd.as_ref()?.as_raw_fd()) };
        if !(0..=3).contains(&state) {
            self.fd = None;
            self.retry = Instant::now() + Duration::from_secs(5);
            return None;
        }
        Some(state != 0)
    }
}

/// The SDK handle/descriptor moves into one finite observation and returns
/// with its sample. A retained receiver owns completion across cancellation.
pub(super) struct Owner {
    state: Option<Headphones>,
    pending: Option<tokio::sync::oneshot::Receiver<(Headphones, Option<bool>)>>,
}
impl Owner {
    pub(super) fn new(headphones: Headphones) -> Self {
        Self { state: Some(headphones), pending: None }
    }
    pub(super) async fn get(&mut self) -> Option<bool> {
        // A cancelled caller may have left an actual read in flight. Join it
        // to restore exclusive state, but never publish that earlier sample
        // as a fresh unplug observation for this caller.
        if self.pending.is_some() { let _ = self.finish().await; }
        self.launch();
        self.finish().await
    }
    fn launch(&mut self) {
        let Some(mut state) = self.state.take() else { return; };
        let (reply, result) = tokio::sync::oneshot::channel();
        self.pending = Some(result);
        crate::rt::jobs().run_isolated(move |blocking| {
            let sample = state.get(blocking);
            // If the owner was dropped, failed delivery destroys the SDK/fd
            // here only after its last use, still on this finite worker.
            let _ = reply.send((state, sample));
        });
    }
    async fn finish(&mut self) -> Option<bool> {
        let result = self.pending.as_mut()?.await.ok();
        self.pending = None;
        self.restore_sample(result)
    }
    fn restore_sample(&mut self, result: Option<(Headphones, Option<bool>)>) -> Option<bool> {
        let (state, sample) = result?;
        self.state = Some(state);
        sample
    }
    fn finish_ordinary(&mut self) -> Option<bool> {
        let result = self.pending.take()?.blocking_recv().ok();
        self.restore_sample(result)
    }
    pub(super) fn get_ordinary(&mut self) -> Option<bool> {
        assert!(crate::rt::may_wait_for_tasks(), "await headphone observation on runtime/pool callers");
        if self.pending.is_some() { let _ = self.finish_ordinary(); }
        self.launch();
        self.finish_ordinary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, os::fd::IntoRawFd, sync::{Mutex, mpsc}};
    #[derive(Default)]
    struct Probe {
        opens: usize, reads: usize, pool: Vec<bool>, values: VecDeque<i32>,
        hold: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>, timed_out: bool,
    }
    static PROBE: Mutex<Option<Probe>> = Mutex::new(None);
    extern "C" fn open(_: c_int, _: c_int) -> c_int {
        let mut probe = PROBE.lock().unwrap();
        let probe = probe.as_mut().unwrap();
        probe.opens += 1;
        probe.pool.push(crate::rt::on_pool() && !crate::rt::on_executor());
        std::fs::File::open("/dev/null").map(IntoRawFd::into_raw_fd).unwrap_or(-1)
    }
    extern "C" fn state(_: c_int) -> c_int {
        let (value, hold) = {
            let mut probe = PROBE.lock().unwrap();
            let probe = probe.as_mut().unwrap();
            probe.reads += 1;
            probe.pool.push(crate::rt::on_pool() && !crate::rt::on_executor());
            (probe.values.pop_front().unwrap_or(-1), probe.hold.take())
        };
        if let Some((entered, release)) = hold {
            let _ = entered.send(());
            if release.recv_timeout(Duration::from_secs(5)).is_err() {
                PROBE.lock().unwrap().as_mut().unwrap().timed_out = true;
                return -1;
            }
        }
        value
    }
    struct Release(Option<mpsc::Sender<()>>);
    impl Release { fn now(&mut self) { if let Some(send) = self.0.take() { let _ = send.send(()); } } }
    impl Drop for Release { fn drop(&mut self) { self.now(); } }
    #[test]
    fn cancelled_headphone_read_is_joined_then_resampled_without_false_unplug() {
        let (entered, observed) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let mut release = Release(Some(release));
        *PROBE.lock().unwrap() = Some(Probe { values: [0, 1, -1, 0].into(), hold: Some((entered, wait)), ..Default::default() });
        let mut owner = Owner::new(Headphones {
            _library: libloading::os::unix::Library::this().into(), open, state, fd: None, retry: Instant::now(),
        });
        let ui = crate::rt::Ui::new().unwrap();
        ui.block_on(async {
            let sample = owner.get();
            tokio::pin!(sample);
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(sample.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            }).await;
            // Cancel only the observer; the finite read still owns its state.
        });
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        release.now();
        assert_eq!(ui.block_on(owner.get()), Some(true), "old cancelled false result is not a fresh unplug");
        assert_eq!(ui.block_on(owner.get()), None, "read/rebind errors remain unknown");
        assert!(owner.state.as_ref().unwrap().fd.is_none());
        assert_eq!(ui.block_on(owner.get()), None, "missing device retains the retry interval");
        {
            let probe = PROBE.lock().unwrap(); let probe = probe.as_ref().unwrap();
            assert_eq!((probe.opens, probe.reads), (1, 3));
            assert!(probe.pool.iter().all(|on_pool| *on_pool)); assert!(!probe.timed_out);
        }
        owner.state.as_mut().unwrap().retry = Instant::now();
        assert_eq!(ui.block_on(owner.get()), Some(false));
        assert_eq!(PROBE.lock().unwrap().as_ref().unwrap().opens, 2);
        drop(owner);
        PROBE.lock().unwrap().take();
    }
}
