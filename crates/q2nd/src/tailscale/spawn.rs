//! Keep Linux's parent thread alive without doing fork/exec on an executor.
//! This owner exists only while the enabled Tailscale worker needs it. Child
//! leases also retain the parent if an async caller is cancelled unexpectedly.
use crate::rt::Blocking;
use std::io;
use std::ops::{Deref, DerefMut};
use std::process::Command;
use std::sync::mpsc;

struct Request {
    command: Command,
    lease: mpsc::Sender<Request>,
    reply: tokio::sync::oneshot::Sender<io::Result<Child>>,
}

pub(super) struct Owner {
    tx: mpsc::Sender<Request>,
    thread: std::thread::JoinHandle<()>,
}

pub(super) struct Child {
    // Drop the reactor child (kill-on-drop and orphan reaping) before releasing
    // the parent lease. Normal shutdown explicitly waits before either drops.
    inner: tokio::process::Child,
    _parent: mpsc::Sender<Request>,
}
impl Deref for Child {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target { &self.inner }
}
impl DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.inner }
}

impl Owner {
    pub(super) async fn start() -> io::Result<Self> {
        crate::rt::spawn_blocking(Self::start_blocking).await
            .map_err(|_| io::Error::other("Tailscale spawn owner stopped"))?
    }

    fn start_blocking(_blocking: &Blocking) -> io::Result<Self> {
        let reactor = tokio::runtime::Handle::current();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("ts-spawn".into()).spawn(move || {
            let blocking = Blocking::dedicated_thread();
            // Registration belongs to the existing I/O reactor. This thread
            // never polls async tasks or reserves a general blocking-pool slot.
            let _entered = reactor.enter();
            serve(&blocking, rx);
        })?;
        Ok(Self { tx, thread })
    }

    pub(super) async fn spawn(&self, command: Command) -> io::Result<Child> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.tx.send(Request { command, lease: self.tx.clone(), reply })
            .map_err(|_| io::Error::other("Tailscale spawn owner stopped"))?;
        result.await.map_err(|_| io::Error::other("Tailscale spawn owner stopped"))?
    }

    /// Call only after both children are joined. Closing the channel makes
    /// this a finite OS-thread join with no dependency on async task progress.
    pub(super) async fn stop(self) {
        let Self { tx, thread } = self;
        drop(tx);
        let _ = crate::rt::spawn_blocking(move |_blocking| thread.join()).await;
    }
}

fn serve(_blocking: &Blocking, rx: mpsc::Receiver<Request>) {
    while let Ok(request) = rx.recv() {
        // Cancellation before dispatch needs no process at all. Cancellation
        // during exec drops the returned child with kill-on-drop enabled.
        if request.reply.is_closed() { continue; }
        let result = tokio::process::Command::from(request.command).kill_on_drop(true)
            .spawn().map(|inner| Child { inner, _parent: request.lease });
        let _ = request.reply.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    #[test]
    fn stalled_exec_leaves_io_and_pool_free_and_child_retains_parent() {
        let (mut barrier, child_barrier) = std::os::unix::net::UnixStream::pair().unwrap();
        barrier.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut command = super::super::command(std::path::Path::new("/bin/sleep"), 0);
        command.arg("30");
        crate::platform::process::generated::exec_barrier(&mut command, child_barrier);
        let task = crate::rt::io().spawn(async move {
            let owner = Owner::start().await.unwrap();
            let child = owner.spawn(command).await.unwrap();
            let Owner { tx, thread } = owner;
            drop(tx);
            (child, thread)
        });
        let mut ready = [0u8; 4];
        barrier.read_exact(&mut ready).unwrap();
        assert!(i32::from_ne_bytes(ready) > 0);
        let (io_tx, io_rx) = mpsc::channel();
        let (pool_tx, pool_rx) = mpsc::channel();
        crate::rt::io().spawn(async move { let _ = io_tx.send(()); });
        crate::rt::spawn_blocking(move |_| { let _ = pool_tx.send(()); });
        let io_progress = io_rx.recv_timeout(Duration::from_secs(1));
        let pool_progress = pool_rx.recv_timeout(Duration::from_secs(1));
        // Release before asserting so a regression cannot strand this child.
        barrier.write_all(&[1]).unwrap();
        let (mut child, thread) = super::super::generated_wait(task).unwrap().unwrap();
        let running = child.try_wait().unwrap().is_none();
        let parent_running = !thread.is_finished();
        super::super::generated_wait(async move { child.kill().await.unwrap(); }).unwrap();
        thread.join().unwrap();
        assert!(io_progress.is_ok() && pool_progress.is_ok());
        assert!(running && parent_running, "dropping owner must not kill its live child");
    }

    #[test]
    fn failed_exec_and_cancelled_queued_spawn_leave_owner_joinable() {
        super::super::generated_wait(crate::rt::io().spawn(async {
            let owner = Owner::start().await.unwrap();
            let missing = Command::new("/q2-generated-nonexistent-program");
            assert!(owner.spawn(missing).await.is_err());
            let (reply, result) = tokio::sync::oneshot::channel();
            drop(result);
            owner.tx.send(Request { command: Command::new("/q2-generated-nonexistent-program"),
                lease: owner.tx.clone(), reply }).unwrap();
            tokio::time::timeout(Duration::from_secs(2), owner.stop()).await.unwrap();
        })).unwrap().unwrap();
    }

    #[test]
    fn abandoned_exec_reply_kills_and_reaps_child_before_owner_is_reused() {
        let (mut barrier, child_barrier) = std::os::unix::net::UnixStream::pair().unwrap();
        barrier.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let owner = std::sync::Arc::new(super::super::generated_wait(Owner::start()).unwrap().unwrap());
        let mut command = super::super::command(std::path::Path::new("/bin/sleep"), 0);
        command.arg("30");
        crate::platform::process::generated::exec_barrier(&mut command, child_barrier);
        let caller = owner.clone();
        let task = crate::rt::io().spawn(async move { caller.spawn(command).await });
        let mut ready = [0u8; 4];
        barrier.read_exact(&mut ready).unwrap();
        let pid = i32::from_ne_bytes(ready);
        task.abort();
        assert!(super::super::generated_wait(task).unwrap().is_err());
        barrier.write_all(&[1]).unwrap();
        let owner = std::sync::Arc::try_unwrap(owner).ok().unwrap();
        super::super::generated_wait(async move {
            tokio::time::timeout(Duration::from_secs(2), owner.stop()).await.unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while crate::platform::process::generated::exists(pid) {
                assert!(tokio::time::Instant::now() < deadline, "abandoned child was not reaped");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).unwrap();
        assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }
}
