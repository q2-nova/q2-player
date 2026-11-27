#![forbid(unsafe_code)]
//! Authorization for synchronous foreign MediaSource callbacks. The source
//! remains transferable; permission belongs only to the current caller stack.
use std::cell::Cell;
use std::io;
use crate::rt::Blocking;

thread_local! { static DEPTH: Cell<usize> = const { Cell::new(0) }; }
struct Restore<'a> { previous: usize, _blocking: &'a Blocking }
impl Drop for Restore<'_> {
    fn drop(&mut self) { DEPTH.with(|depth| depth.set(self.previous)); }
}

pub(crate) fn with<R>(_blocking: &Blocking, operation: impl FnOnce() -> R) -> R {
    assert!(!crate::rt::on_executor(), "media callback scope on executor");
    let previous = DEPTH.with(|depth| {
        let old = depth.get();
        depth.set(old.checked_add(1).expect("media callback nesting overflow"));
        old
    });
    let _restore = Restore { previous, _blocking };
    operation()
}

pub(crate) fn require() -> io::Result<()> {
    if crate::rt::on_executor() || !DEPTH.with(|depth| depth.get() != 0) {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "media callback outside blocking scope"))
    } else { Ok(()) }
}

pub(crate) fn assert_allowed() {
    assert!(require().is_ok(), "media metadata callback outside blocking scope");
}

#[cfg(test)]
pub(crate) fn test_scope<R>(operation: impl FnOnce() -> R) -> R {
    assert!(crate::rt::may_wait_for_tasks(), "ordinary reader fixture entry");
    let blocking = Blocking::dedicated_thread();
    with(&blocking, operation)
}

#[cfg(test)]
pub(crate) async fn test_job<T: Send + 'static, R: Send + 'static>(
    mut owner: T, operation: impl FnOnce(&mut T) -> R + Send + 'static,
) -> (T, R) {
    crate::rt::jobs().run_isolated(move |blocking| with(blocking, || {
        let result = operation(&mut owner);
        (owner, result)
    })).await.unwrap()
}
