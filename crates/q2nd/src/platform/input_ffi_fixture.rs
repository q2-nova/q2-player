//! Generated pipe endpoints implementing the production SDK ABI.
use super::*;
use std::sync::{Mutex, OnceLock, atomic::Ordering};
pub(in crate::platform) struct MockDevices {
    pub(in crate::platform) readers: [Option<OwnedFd>; 3],
}
static DEVICES: OnceLock<Mutex<MockDevices>> = OnceLock::new();
pub(in crate::platform) static MOCK_TEST: Mutex<()> = Mutex::new(());
pub(in crate::platform) fn mock_devices() -> &'static Mutex<MockDevices> {
    DEVICES.get_or_init(|| {
        Mutex::new(MockDevices {
            readers: [None, None, None],
        })
    })
}
pub(in crate::platform) fn new_device(kind: usize) -> OwnedFd {
    let mut fds = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
        0
    );
    mock_devices().lock().unwrap().readers[kind] =
        Some(unsafe { OwnedFd::from_raw_fd(fds[0]) });
    unsafe { OwnedFd::from_raw_fd(fds[1]) }
}
#[derive(Default)]
pub(in crate::platform) struct Hooks { pub(in crate::platform) stall_open: bool, pub(in crate::platform) stall_state: bool, pub(in crate::platform) entered: usize, pub(in crate::platform) opens: [usize; 3], pub(in crate::platform) held: u32, pub(in crate::platform) fail_state: bool }
pub(in crate::platform) static HOOKS: Mutex<Hooks> = Mutex::new(Hooks { stall_open: false, stall_state: false, entered: 0, opens: [0; 3], held: 0, fail_state: false });
static RELEASE: std::sync::Condvar = std::sync::Condvar::new();
pub(in crate::platform) static INTERRUPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub(in crate::platform) fn mock_api() -> Api {
    Api {
        _library: None, open: mock_open, read: mock_read, state: mock_state,
        touch_open: Some(mock_touch_open), touch_state: Some(mock_touch_state),
    }
}
fn wait_hook(open: bool, kind: usize) -> (u32, bool) {
    let mut hooks = HOOKS.lock().unwrap();
    if open { hooks.opens[kind] += 1; }
    if if open { hooks.stall_open } else { hooks.stall_state } {
        hooks.entered += 1;
        RELEASE.notify_all();
        while if open { hooks.stall_open } else { hooks.stall_state } {
            hooks = RELEASE.wait(hooks).unwrap();
        }
    }
    (hooks.held, hooks.fail_state)
}
pub(in crate::platform) fn release_hooks() {
    let mut hooks = HOOKS.lock().unwrap();
    hooks.stall_open = false;
    hooks.stall_state = false;
    RELEASE.notify_all();
}
pub(in crate::platform) struct Fixture { pub(in crate::platform) buttons: OwnedFd, pub(in crate::platform) _wheel: OwnedFd }
impl Fixture {
    pub(in crate::platform) fn new() -> Self {
        *HOOKS.lock().unwrap() = Hooks::default();
        INTERRUPTS.store(0, Ordering::SeqCst);
        Self { buttons: new_device(0), _wheel: new_device(1) }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        release_hooks();
        *mock_devices().lock().unwrap() = MockDevices { readers: [None, None, None] };
        *HOOKS.lock().unwrap() = Hooks::default();
    }
}
unsafe extern "C" fn mock_open(kind: c_int, _: c_int) -> c_int {
    wait_hook(true, kind as usize);
    let devices = mock_devices().lock().unwrap();
    devices.readers[kind as usize]
        .as_ref()
        .map(|fd| unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })
        .unwrap_or(-1)
}
unsafe extern "C" fn mock_read(fd: c_int, event: *mut Event) -> c_int {
    if INTERRUPTS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
        unsafe { *libc::__errno_location() = libc::EINTR; }
        return 0;
    }
    let n = unsafe { libc::read(fd, event.cast(), std::mem::size_of::<Event>()) };
    if n == std::mem::size_of::<Event>() as isize {
        1
    } else if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
    {
        0
    } else {
        -1
    }
}
unsafe extern "C" fn mock_state(_: c_int, kind: c_int, state: *mut State) -> c_int {
    let (held, fail) = wait_hook(false, kind as usize);
    unsafe { state.write(State { keys: held, ..State::default() }); }
    if fail { -1 } else { 0 }
}
unsafe extern "C" fn mock_touch_open(path: *const c_char) -> c_int {
    if unsafe { CStr::from_ptr(path) } != c"/dev/input/event42" {
        unsafe { *libc::__errno_location() = libc::ENODEV; }
        return -1;
    }
    unsafe { mock_open(2, 1) }
}
unsafe extern "C" fn mock_touch_state(_: c_int, state: *mut TouchState) -> c_int {
    let (_, fail) = wait_hook(false, 2);
    unsafe { state.write(TouchState::default()); }
    if fail { -1 } else { 0 }
}
pub(in crate::platform) fn writer_has_reader(fd: &OwnedFd) -> bool {
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
    assert!(unsafe { libc::poll(&mut poll, 1, 0) } >= 0);
    poll.revents & libc::POLLERR == 0
}
pub(in crate::platform) fn send_key(fd: &OwnedFd, down: bool) {
    let event = Event {
        kind: 1,
        code: 116,
        value: i32::from(down),
        ..Default::default()
    };
    send_event(fd, &event);
}
pub(in crate::platform) fn send_event(fd: &OwnedFd, event: &Event) {
    assert_eq!(unsafe { libc::write(fd.as_raw_fd(), (event as *const Event).cast(), std::mem::size_of::<Event>()) }, std::mem::size_of::<Event>() as isize);
}
