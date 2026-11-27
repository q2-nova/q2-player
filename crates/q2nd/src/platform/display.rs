//! Exclusive KMS ownership. Page-flip completion uses reactor readiness;
//! potentially sleeping modesets run as finite jobs with the owning handle.
use libloading::Library;
use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::io;
use std::marker::PhantomData;
use std::os::fd::BorrowedFd;
use std::ptr::NonNull;
use std::time::Duration;

type Close = unsafe extern "C" fn(*mut c_void);
type Enable = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type Begin = unsafe extern "C" fn(*mut c_void, *const u32, u32) -> c_int;
type Finish = unsafe extern "C" fn(*mut c_void) -> c_int;
type Descriptor = unsafe extern "C" fn(*mut c_void) -> c_int;

struct Handle {
    pointer: NonNull<c_void>,
    close: Close,
    enable: Enable,
    begin: Begin,
    finish: Finish,
    descriptor: Descriptor,
    _library: Option<Library>,
    _exclusive: PhantomData<Cell<()>>,
}
// SAFETY: the bridge uses only this handle's owned DRM fd/mappings and contains
// no thread-local state. The complete handle moves to one finite modeset job;
// no other operation can use it until returned. It is deliberately not Sync.
unsafe impl Send for Handle {}
impl Handle {
    fn enable(&mut self, enabled: bool) -> io::Result<()> {
        // SAFETY: exclusive live handle; no submitted flip remains outstanding.
        if unsafe { (self.enable)(self.pointer.as_ptr(), i32::from(enabled)) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: sole owner closes once, before unloading the retained library.
        unsafe { (self.close)(self.pointer.as_ptr()) };
    }
}

type ModeReply = tokio::sync::oneshot::Receiver<(Handle, io::Result<()>)>;
pub(super) struct Display {
    handle: Option<Handle>,
    mode: Option<(bool, ModeReply)>,
    enabled: bool,
    readiness: Option<crate::rt::Fd>,
    // The original deadline survives cancellation; retry cannot extend it.
    pending: Option<tokio::time::Instant>,
}
impl Display {
    pub(super) fn open() -> Result<Self, String> {
        crate::record_profile_stage("profile-platform-dlopen-begin");
        // SAFETY: fixed installed ABI; resolve all symbols before opening and
        // retain the library for the entire exclusive handle lifetime.
        let library = unsafe { Library::new("libq2platform.so") }.map_err(|e| e.to_string())?;
        crate::record_profile_stage("profile-platform-dlopen-returned");
        let (open, close, enable, begin, finish, descriptor) = unsafe {
            (*library.get::<unsafe extern "C" fn() -> *mut c_void>(b"q2_display_open\0").map_err(|e| e.to_string())?,
             *library.get::<Close>(b"q2_display_close\0").map_err(|e| e.to_string())?,
             *library.get::<Enable>(b"q2_display_enable\0").map_err(|e| e.to_string())?,
             *library.get::<Begin>(b"q2_display_begin\0").map_err(|e| e.to_string())?,
             *library.get::<Finish>(b"q2_display_finish\0").map_err(|e| e.to_string())?,
             *library.get::<Descriptor>(b"q2_display_descriptor\0").map_err(|e| e.to_string())?)
        };
        // SAFETY: non-null transfers sole ownership, no borrowed arguments.
        let pointer = unsafe { open() };
        crate::record_profile_stage("profile-platform-kms-returned");
        let pointer = NonNull::new(pointer).ok_or_else(|| format!("KMS open: {}", io::Error::last_os_error()))?;
        Ok(Self { handle: Some(Handle { pointer, close, enable, begin, finish, descriptor, _library: Some(library), _exclusive: PhantomData }), mode: None, enabled: true, readiness: None, pending: None })
    }

    pub(super) fn enabled(&self) -> bool { self.enabled }

    async fn finish_mode(&mut self) -> io::Result<()> {
        if let Some((enabled, mode)) = self.mode.as_mut() {
            let enabled = *enabled;
            let result = mode.await;
            self.mode.take();
            let (handle, result) = result.map_err(|_| io::Error::other("KMS modeset worker stopped"))?;
            self.handle = Some(handle);
            result?;
            self.enabled = enabled;
        }
        Ok(())
    }

    pub(super) async fn settle(&mut self) -> io::Result<()> {
        self.finish_mode().await?;
        let Some(deadline) = self.pending else { return Ok(()) };
        let handle = self.handle.as_mut().ok_or_else(|| io::Error::other("KMS owner unavailable"))?;
        let readiness = self.readiness.as_ref().ok_or_else(|| io::Error::other("KMS reactor unavailable"))?;
        tokio::time::timeout_at(deadline, readiness.read_with(|_| {
            // SAFETY: unique handle; finish performs one nonblocking read and
            // retains pending state on EAGAIN. No slice/pointer is retained.
            if unsafe { (handle.finish)(handle.pointer.as_ptr()) } != 0 {
                Err(io::Error::last_os_error())
            } else { Ok(()) }
        })).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "KMS flip timed out"))??;
        self.pending = None;
        Ok(())
    }

    pub(super) async fn present(&mut self, pixels: &[u32]) -> io::Result<()> {
        self.settle().await?;
        let count = u32::try_from(pixels.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "display frame too large"))?;
        let handle = self.handle.as_mut().ok_or_else(|| io::Error::other("KMS owner unavailable"))?;
        // SAFETY: exclusive handle and borrowed contiguous pixels. C validates
        // count and copies before returning; no pixels cross an await boundary.
        let result = unsafe { (handle.begin)(handle.pointer.as_ptr(), pixels.as_ptr(), count) };
        match result {
            0 => Ok(()),
            1 => {
                self.pending = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                if self.readiness.is_none() {
                    // SAFETY: descriptor is borrowed from this live exclusive handle.
                    // Duplicate before registering: the reactor owns its independent
                    // fd until deregistration and never outlives its file description.
                    let descriptor = unsafe { (handle.descriptor)(handle.pointer.as_ptr()) };
                    if descriptor < 0 { return Err(io::Error::last_os_error()); }
                    let owned = unsafe { BorrowedFd::borrow_raw(descriptor) }.try_clone_to_owned()?;
                    self.readiness = Some(crate::rt::Fd::new(owned)?);
                }
                self.settle().await
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Only ordinary startup/teardown adapters may use the synchronous modeset.
    pub(super) fn enable(&mut self, enabled: bool) -> io::Result<()> {
        assert!(crate::rt::may_wait_for_tasks(), "KMS modeset belongs off the executor");
        assert!(self.mode.is_none() && self.pending.is_none());
        self.handle.as_mut().ok_or_else(|| io::Error::other("KMS owner unavailable"))?.enable(enabled)?;
        self.enabled = enabled;
        Ok(())
    }

    pub(super) async fn enable_async(&mut self, enabled: bool) -> io::Result<()> {
        self.settle().await?;
        self.start_mode(enabled)?;
        self.finish_mode().await
    }

    /// Starts a modeset after the caller has joined an earlier blanking.
    pub(super) fn start_mode(&mut self, enabled: bool) -> io::Result<()> {
        if self.mode.is_some() || self.pending.is_some() {
            return Err(io::Error::other("KMS has an outstanding operation"));
        }
        if self.enabled == enabled { return Ok(()); }
        let mut handle = self.handle.take().ok_or_else(|| io::Error::other("KMS owner unavailable"))?;
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mode = Some((enabled, receive));
        crate::rt::jobs().run_isolated(move |_blocking| {
            let result = handle.enable(enabled);
            let _ = send.send((handle, result));
        });
        Ok(())
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        self.readiness.take();
        if let Some((_, mode)) = self.mode.take() {
            assert!(crate::rt::may_wait_for_tasks(), "pending KMS owner closes outside the executor");
            if let Ok((handle, _)) = mode.blocking_recv() { self.handle = Some(handle); }
        }
        // The retained Handle is destroyed after this method on the ordinary
        // platform owner, or in its finite job if delivery was impossible.
    }
}

#[cfg(test)]
#[path = "display_fixture.rs"]
pub(super) mod tests;
