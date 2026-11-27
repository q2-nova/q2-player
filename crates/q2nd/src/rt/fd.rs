//! A descriptor on the reactor: readiness for things tokio has no type for
//! (evdev, netlink, eventfd, a FIFO). The two `unsafe` sites of the wake
//! plumbing are here; reads and writes go through `File`, which is safe for
//! any descriptor.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

pub(crate) trait CloseFile {
    fn close(self, file: File);
}

impl CloseFile for () {
    fn close(self, file: File) { drop(file); }
}

/// Owns a single stable descriptor from discovery through deregistration.
/// The same cleanup also runs if registration fails or discovery is cancelled.
pub(crate) struct ReactorFile<C: CloseFile>(Option<(File, C)>);

impl<C: CloseFile> ReactorFile<C> {
    pub fn new(fd: OwnedFd, close: C) -> Self { Self(Some((fd.into(), close))) }
    fn file(&self) -> &File { &self.0.as_ref().unwrap().0 }
}
impl<C: CloseFile> AsRawFd for ReactorFile<C> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd { self.file().as_raw_fd() }
}
impl<C: CloseFile> Drop for ReactorFile<C> {
    fn drop(&mut self) {
        if let Some((file, owner)) = self.0.take() { owner.close(file); }
    }
}

pub struct Fd<C: CloseFile = ()> {
    inner: AsyncFd<ReactorFile<C>>,
}

impl<C: CloseFile> AsRawFd for Fd<C> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd { self.inner.get_ref().as_raw_fd() }
}

impl Fd {
    /// Takes `fd`, makes it non-blocking and registers it with the runtime
    /// this is called on.
    pub fn new(fd: OwnedFd) -> io::Result<Fd> {
        Self::register(ReactorFile::new(fd, ()))
    }
}

// Input devices supply their own final-close owner. Registration errors and
// cancelled readiness tasks must return ownership through the same Drop path.
impl<C: CloseFile> Fd<C> {
    pub fn register(fd: ReactorFile<C>) -> io::Result<Self> {
        let raw = fd.as_raw_fd();
        // SAFETY: `raw` is open for the whole call (`fd` is owned here), and
        // F_GETFL/F_SETFL take no pointers.
        let set = unsafe {
            let flags = libc::fcntl(raw, libc::F_GETFL);
            if flags < 0 { -1 } else { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) }
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is open, non-blocking, owned by the value
        // moved into the registration, and not registered anywhere else, so
        // it stays valid and is deregistered before it closes.
        let inner = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE | Interest::WRITABLE) }
            .map_err(io::Error::from)?;
        Ok(Fd { inner })
    }

    /// Runs a nonblocking receive operation once readable. The closure must
    /// return WouldBlock when readiness was stale; try_io then rearms epoll.
    /// Keeps protocol-specific recvfrom sender validation at the caller.
    pub async fn read_with<T>(&self, mut receive: impl FnMut(&File) -> io::Result<T>) -> io::Result<T> {
        loop {
            let mut ready = self.inner.readable().await?;
            match ready.try_io(|file| receive(file.get_ref().file())) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}

#[allow(dead_code)]
impl Fd {
    /// Reads once the descriptor is readable; 0 is end of file.
    pub async fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.inner.readable().await?;
            match ready.try_io(|file| { let mut file = file.get_ref().file(); file.read(buffer) }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// Writes once the descriptor is writable.
    pub async fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            let mut ready = self.inner.writable().await?;
            match ready.try_io(|file| { let mut file = file.get_ref().file(); file.write(bytes) }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}
