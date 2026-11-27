//! Confined SDK input ABI. Descriptors are owned; all output buffers are borrowed.
use crate::rt;
use libloading::Library;
use std::ffi::{CStr, c_char, c_int};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[repr(C)]
#[derive(Default)]
pub(super) struct Event {
    pub(super) seconds: i64,
    pub(super) microseconds: i32,
    pub(super) kind: u16,
    pub(super) code: u16,
    pub(super) value: i32,
}
#[repr(C)]
#[derive(Default)]
pub(super) struct State {
    pub(super) position: i32,
    pub(super) touch: i32,
    pub(super) keys: u32,
}
#[repr(C)]
#[derive(Default)]
pub(super) struct TouchState {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) pressure: i32,
}
pub(super) struct Api {
    _library: Option<Library>,
    open: unsafe extern "C" fn(c_int, c_int) -> c_int,
    read: unsafe extern "C" fn(c_int, *mut Event) -> c_int,
    state: unsafe extern "C" fn(c_int, c_int, *mut State) -> c_int,
    touch_open: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    touch_state: Option<unsafe extern "C" fn(c_int, *mut TouchState) -> c_int>,
}
impl Api {
    pub(super) fn load(_blocking: &rt::Blocking) -> Result<Self, String> {
        let library = unsafe { Library::new("libq2platform.so") }.map_err(|e| e.to_string())?;
        unsafe {
            let size = library
                .get::<unsafe extern "C" fn() -> u32>(b"q2_input_bridge_size\0")
                .map_err(|e| e.to_string())?;
            if size() as usize != std::mem::size_of::<Event>() {
                return Err("evdev bridge ABI mismatch".into());
            }
            Ok(Self {
                open: *library.get(b"q2_input_open\0").map_err(|e| e.to_string())?,
                read: *library.get(b"q2_input_read\0").map_err(|e| e.to_string())?,
                state: *library
                    .get(b"q2_input_state\0")
                    .map_err(|e| e.to_string())?,
                touch_open: library.get(b"q2_touchscreen_open\0").ok().map(|open| *open),
                touch_state: library.get(b"q2_touchscreen_state\0").ok().map(|state| *state),
                _library: Some(library),
            })
        }
    }
    pub(super) fn device(&self, kind: usize, _blocking: &rt::Blocking) -> Result<(OwnedFd, State), String> {
        let fd = unsafe { (self.open)(kind as c_int, 1) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut state = State::default();
        if unsafe { (self.state)(fd.as_raw_fd(), kind as c_int, &mut state) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        log::info!(
            "mainline input connected: {}",
            if kind == 1 {
                "hyn_sliderbar"
            } else {
                "Q2 buttons"
            }
        );
        Ok((fd, state))
    }
    pub(super) fn open_touch(&self, path: &CStr, blocking: &rt::Blocking) -> Result<(OwnedFd, TouchState), String> {
        let open = self.touch_open.ok_or("Touchscreen bridge unavailable")?;
        let fd = unsafe { open(path.as_ptr()) };
        if fd < 0 { return Err(std::io::Error::last_os_error().to_string()); }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let state = self.touch_snapshot(&fd, blocking).ok_or("Touchscreen state unavailable")?;
        Ok((fd, state))
    }
    pub(super) fn touch_snapshot(&self, fd: &impl AsRawFd, _blocking: &rt::Blocking) -> Option<TouchState> {
        let read_state = self.touch_state?;
        let mut state = TouchState::default();
        (unsafe { read_state(fd.as_raw_fd(), &mut state) } == 0).then_some(state)
    }
}

impl Api {
    /// The C bridge borrows this initialized event buffer only during read.
    /// Preserve EINTR separately: it must not clear reactor edge readiness.
    pub(super) fn read_event(&self, fd: &impl AsRawFd) -> Read {
        let mut event = Event::default();
        let rc = unsafe { (self.read)(fd.as_raw_fd(), &mut event) };
        if rc > 0 { Read::Event(event) }
        else if rc < 0 { Read::Disconnected }
        else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { Read::Interrupted }
        else { Read::WouldBlock }
    }
    pub(super) fn snapshot(&self, fd: &impl AsRawFd, kind: usize, _blocking: &rt::Blocking) -> Option<State> {
        let mut state = State::default();
        if unsafe { (self.state)(fd.as_raw_fd(), kind as c_int, &mut state) } == 0 { Some(state) } else { None }
    }
}
pub(super) enum Read { Event(Event), Disconnected, Interrupted, WouldBlock }

#[cfg(test)]
#[path="input_ffi_fixture.rs"]
pub(super) mod fixture;
