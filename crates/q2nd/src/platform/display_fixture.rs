//! Generated bridge callbacks: ownership/readiness controls, no display access.
use super::*;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
#[derive(Default)]
struct Counts { begin: AtomicUsize, enable: AtomicUsize, close: AtomicUsize }
struct Fake {
    reader: UnixStream,
    counts: Arc<Counts>,
    enabled: bool,
    gate: Option<(tokio::sync::oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}
unsafe extern "C" fn close(pointer: *mut c_void) {
    let fake = unsafe { Box::from_raw(pointer.cast::<Fake>()) };
    fake.counts.close.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn descriptor(pointer: *mut c_void) -> c_int {
    unsafe { &*pointer.cast::<Fake>() }.reader.as_raw_fd()
}
unsafe extern "C" fn enable(pointer: *mut c_void, enabled: c_int) -> c_int {
    let fake = unsafe { &mut *pointer.cast::<Fake>() };
    fake.counts.enable.fetch_add(1, Ordering::SeqCst);
    if let Some((entered, release)) = fake.gate.take() {
        let _ = entered.send(());
        if release.recv_timeout(Duration::from_secs(5)).is_err() { return -1; }
    }
    fake.enabled = enabled != 0;
    0
}
unsafe extern "C" fn begin(pointer: *mut c_void, _: *const u32, count: u32) -> c_int {
    if count != 375 * 320 { unsafe { *libc::__errno_location() = libc::EINVAL; } return -1; }
    let fake = unsafe { &mut *pointer.cast::<Fake>() };
    fake.counts.begin.fetch_add(1, Ordering::SeqCst);
    i32::from(fake.enabled)
}
unsafe extern "C" fn finish(pointer: *mut c_void) -> c_int {
    let fake = unsafe { &mut *pointer.cast::<Fake>() };
    let mut byte = [0];
    match fake.reader.read(&mut byte) {
        Ok(1) if byte[0] == 1 => 0,
        Err(error) => { unsafe { *libc::__errno_location() = error.raw_os_error().unwrap_or(libc::EIO); } -1 }
        _ => { unsafe { *libc::__errno_location() = libc::EIO; } -1 }
    }
}
fn fixture(gate: Option<(tokio::sync::oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>) -> (Display, UnixStream, Arc<Counts>) {
    let (reader, writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    let counts = Arc::new(Counts::default());
    let fake = Box::new(Fake { reader, counts: counts.clone(), enabled: true, gate });
    let handle = Handle { pointer: NonNull::new(Box::into_raw(fake).cast()).unwrap(), close, enable, begin, finish, descriptor, _library: None, _exclusive: PhantomData };
    (Display { handle: Some(handle), mode: None, enabled: true, readiness: None, pending: None }, writer, counts)
}

pub(in crate::platform) fn dark_resume_fixture() -> (Display, UnixStream) {
    let (mut display, writer, _) = fixture(None);
    display.enable(false).unwrap();
    (display, writer)
}

#[test]
fn cancelled_flip_retains_readiness_and_precedes_modeset_without_recopy() {
    let (mut display, mut writer, counts) = fixture(None);
    let pixels = vec![0; 375 * 320];
    crate::rt::Ui::new().unwrap().block_on(async {
        tokio::select! {
            biased;
            _ = display.present(&pixels) => panic!("flip completed before event"),
            _ = tokio::task::yield_now() => {},
        }
        let deadline = display.pending.unwrap();
        assert_eq!(counts.begin.load(Ordering::SeqCst), 1);
        tokio::select! {
            biased;
            _ = display.enable_async(false) => panic!("modeset overtook pending flip"),
            _ = tokio::task::yield_now() => {},
        }
        assert_eq!(counts.enable.load(Ordering::SeqCst), 0);
        assert_eq!(display.pending, Some(deadline));
        assert_eq!(crate::rt::io().spawn(async { 7 }).await.unwrap(), 7);
        writer.write_all(&[1]).unwrap();
        display.enable_async(false).await.unwrap();
        display.enable_async(false).await.unwrap();
        assert_eq!(counts.enable.load(Ordering::SeqCst), 1);
        assert_eq!(counts.begin.load(Ordering::SeqCst), 1);
        display.present(&pixels).await.unwrap(); // disabled scanout completes inline
        assert!(display.pending.is_none());
    });
    drop(display);
    assert_eq!(counts.close.load(Ordering::SeqCst), 1);
}

#[test]
fn cancelled_modeset_retains_exclusive_handle_and_does_not_repeat() {
    let (entered, arrived) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (mut display, _writer, counts) = fixture(Some((entered, gate)));
    crate::rt::Ui::new().unwrap().block_on(async {
        tokio::select! {
            biased;
            _ = display.enable_async(false) => panic!("held modeset completed"),
            result = arrived => result.unwrap(),
        }
        assert!(display.handle.is_none() && display.mode.is_some());
        assert_eq!(crate::rt::io().spawn(async { 9 }).await.unwrap(), 9);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), display.enable_async(false)).await.unwrap().unwrap();
        assert!(display.handle.is_some() && display.mode.is_none());
        assert_eq!(counts.enable.load(Ordering::SeqCst), 1);
    });
    drop(display);
    assert_eq!(counts.close.load(Ordering::SeqCst), 1);
}

#[test]
fn expired_flip_deadline_and_bad_completion_are_errors_not_presentations() {
    let (mut display, mut writer, counts) = fixture(None);
    crate::rt::Ui::new().unwrap().block_on(async {
        assert_eq!(display.present(&[]).await.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let pixels = vec![0; 375 * 320];
        tokio::select! {
            biased;
            _ = display.present(&pixels) => panic!("flip completed before event"),
            _ = tokio::task::yield_now() => {},
        }
        writer.write_all(&[2]).unwrap();
        assert!(display.settle().await.is_err());
        assert!(display.pending.is_some());
        display.pending = Some(tokio::time::Instant::now());
        assert_eq!(display.settle().await.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(counts.begin.load(Ordering::SeqCst), 1);
    });
    drop(display);
    assert_eq!(counts.close.load(Ordering::SeqCst), 1);
}
