//! Owning evdev PWM-beeper bridge. Async pulse scheduling lives in clicker;
//! only this module sees the ABI, raw descriptor and loaded symbol pointers.
use libloading::Library;
use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

type Set = unsafe extern "C" fn(c_int, c_int) -> c_int;

pub(super) struct Beeper {
    // Drop the descriptor before unloading the bridge.
    fd: OwnedFd,
    set: Set,
    tone: Option<Set>,
    _library: Library,
}

impl Beeper {
    pub(super) fn open() -> Result<Self, String> {
        assert!(crate::rt::may_wait_for_tasks(), "clicker discovery requires ordinary startup");
        // SAFETY: the installed bridge exports this fixed C ABI. Resolve all
        // mandatory symbols before acquiring a descriptor, retaining the
        // library for every call. Tone remains optional on older bridges.
        let library = unsafe { Library::new("libq2platform.so") }.map_err(|e| e.to_string())?;
        let open = *unsafe { library.get::<unsafe extern "C" fn() -> c_int>(b"q2_clicker_open\0") }.map_err(|e| e.to_string())?;
        let set = *unsafe { library.get::<Set>(b"q2_clicker_set\0") }.map_err(|e| e.to_string())?;
        let tone = unsafe { library.get::<Set>(b"q2_clicker_tone\0") }.ok().map(|s| *s);
        // SAFETY: open takes no arguments; a nonnegative return transfers one
        // descriptor which is immediately placed in its sole Rust owner.
        let raw = unsafe { open() };
        if raw < 0 { return Err(io::Error::last_os_error().to_string()); }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(Self { fd, set, tone, _library: library })
    }

    pub(super) fn pitched(&self) -> bool { self.tone.is_some() }

    pub(super) fn set(&mut self, _blocking: &crate::rt::Blocking, enabled: bool) -> io::Result<()> {
        // SAFETY: the owning descriptor/library remain live for this call.
        result(unsafe { (self.set)(self.fd.as_raw_fd(), c_int::from(enabled)) })
    }

    pub(super) fn tone(&mut self, blocking: &crate::rt::Blocking, hz: u16) -> io::Result<()> {
        match self.tone {
            // SAFETY: the loaded function and descriptor are live; only a
            // bounded integer frequency crosses this no-pointer ABI.
            Some(tone) => result(unsafe { tone(self.fd.as_raw_fd(), hz.clamp(500, 4000).into()) }),
            None => self.set(blocking, true),
        }
    }
}

fn result(value: c_int) -> io::Result<()> {
    if value == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}
