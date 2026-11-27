//! The monitor's I/O on the device: the supplicant's control socket, the
//! network helper and three small files. Everything here is async on the I/O
//! runtime, or runs on its blocking pool with the witness.
use super::events::Subscription;
#[cfg(test)]
use super::socket_path;
use super::owner::Backend;
use super::{RX_SIGNAL, kick, sync_autostart};
use crate::rt::{self, Blocking};
use std::path::{Path, PathBuf};
use std::os::unix::fs::MetadataExt;
use tokio::net::UnixDatagram;
use tokio::sync::Mutex;
use std::time::Duration;

pub(super) struct System {
    /// The network helper (`/usr/libexec/q2-network/wifi`).
    script: PathBuf,
    /// The supplicant's control socket, and the directory our end binds in.
    ctrl: PathBuf,
    local: PathBuf,
    // One in-flight exchange. Taking the connection out before awaiting means
    // cancellation cannot put an unanswered request back into the cache.
    request: Mutex<Option<RequestSocket>>,
}

struct RequestSocket {
    socket: UnixDatagram,
    local: super::socket_path::SocketPath,
    identity: (u64, u64),
}

impl RequestSocket {
    fn prepare(blocking: &Blocking, previous: Option<Self>, remote: &Path, directory: &Path) -> std::io::Result<Self> {
        // A connected Unix socket retains the old daemon after pathname
        // replacement. Check the current inode before every exchange, even
        // when the old daemon remains alive and willing to answer.
        let metadata = std::fs::metadata(remote)?;
        let identity = (metadata.dev(), metadata.ino());
        if let Some(previous) = previous {
            if previous.identity == identity { return Ok(previous); }
            drop(previous);
        }
        let path = directory.join(format!("q2nd-wpa-{}-{}", std::process::id(), fastrand::u32(..)));
        let (socket, local) = super::socket_path::bind(blocking, &path)?;
        socket.connect(remote)?;
        Ok(Self { socket, local, identity })
    }

    async fn close(self) {
        let Self { socket, mut local, .. } = self;
        drop(socket);
        local.close().await;
    }
}

impl System {
    pub fn new(script: PathBuf, (ctrl, local): (PathBuf, PathBuf)) -> System {
        System { script, ctrl, local, request: Mutex::new(None) }
    }
}

fn read_text(_: &Blocking, path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn write_text(_: &Blocking, path: &Path, text: &str) {
    let _ = std::fs::write(path, text);
}

async fn helper_status(command: tokio::process::Command) -> std::io::Result<std::process::ExitStatus> {
    let mut child = rt::spawn_blocking(move |blocking|
        crate::platform::process::spawn_reactor_child(blocking, command)).await
        .map_err(|_| std::io::Error::other("network helper spawn stopped"))??;
    child.wait().await
}

impl Backend for System {
    type Events = Subscription;

    /// A request/response exchange with wpa_supplicant.
    async fn wpa(&self, cmd: &str) -> Option<String> {
        let mut cached = self.request.lock().await;
        let previous = cached.take();
        let (ctrl, directory) = (self.ctrl.clone(), self.local.clone());
        let connection = rt::spawn_blocking(move |blocking|
            RequestSocket::prepare(blocking, previous, &ctrl, &directory)).await.ok()?.ok()?;
        // One deadline for send and all replies; unsolicited packets cannot
        // renew the timeout indefinitely. A failed/missing reply is never
        // retried here because the command may already have taken effect.
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            connection.socket.send(cmd.as_bytes()).await.ok()?;
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = connection.socket.recv(&mut buf).await.ok()?;
                if !buf[..n].starts_with(b"<") {
                    return Some(String::from_utf8_lossy(&buf[..n]).into_owned());
                }
            }
        }).await.ok().flatten();
        if result.is_some() { *cached = Some(connection); }
        else { connection.close().await; }
        result
    }

    async fn helper(&self, action: &'static str) -> bool {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg(&self.script).arg(action);
        match helper_status(command).await {
            Ok(s) if s.success() => return true,
            Ok(s) => log::warn!("wifi helper {action}: {s}"),
            Err(e) => log::warn!("wifi helper: {e}"),
        }
        false
    }

    async fn attach(&self) -> Option<Subscription> {
        Subscription::attach(&self.ctrl, &self.local).await
    }

    async fn rx_signal(&self) -> Option<u8> {
        rt::spawn_blocking(|blocking| read_text(blocking, Path::new(RX_SIGNAL))).await.ok().flatten()
            .and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("signal_strength:").map(|v| v.trim().parse().ok())))
            .flatten()
    }

    async fn hint_age(&self) -> Option<Duration> {
        rt::spawn_blocking(kick::hint_age).await.ok().flatten()
    }

    async fn read_applied(&self, path: PathBuf) -> Option<String> {
        rt::spawn_blocking(move |blocking| read_text(blocking, &path)).await.ok().flatten()
    }

    async fn write_applied(&self, path: PathBuf, digest: String) {
        let _ = rt::spawn_blocking(move |blocking| write_text(blocking, &path, &digest)).await;
    }

    async fn sync_marker(&self, path: PathBuf, wanted: bool) {
        let _ = rt::spawn_blocking(move |blocking| sync_autostart(blocking, &path, wanted)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn helper_exec_handshake_leaves_io_responsive_and_preserves_exit_status() {
        let (mut barrier, child_barrier) = std::os::unix::net::UnixStream::pair().unwrap();
        barrier.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        crate::platform::process::generated::exec_barrier(&mut command, child_barrier);
        let task = rt::io().spawn(helper_status(tokio::process::Command::from(command)));
        let mut ready = [0u8; 4];
        barrier.read_exact(&mut ready).unwrap();
        let pid = i32::from_ne_bytes(ready);
        let (send, receive) = std::sync::mpsc::channel();
        rt::io().spawn(async move { let _ = send.send(()); });
        let progressed = receive.recv_timeout(Duration::from_secs(1));
        barrier.write_all(&[1]).unwrap();
        let status = rt::Ui::new().unwrap().block_on(task).unwrap().unwrap();
        assert!(progressed.is_ok());
        assert_eq!(status.code(), Some(7));
        assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod request_tests;
