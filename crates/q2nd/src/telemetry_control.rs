//! Acknowledged live telemetry-context boundary for installed-service tests.
//!
//! The fixed control endpoint accepts only `reload <32 lowercase hex>\n`.
//! The callback owns effective settings, capture/delivery serialization and
//! durable crash context. It must finish that bounded transition before returning.
//! This module never reads/saves user configuration or changes queued reports.
use crate::rt::Blocking;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixListener,
        },
    },
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const OVERLAY: &str = "/run/q2/telemetry-test.toml";
const SOCKET: &str = "/run/q2/telemetry-test.sock";
const REQUEST_BYTES: usize = 128;
const REPLY_BYTES: usize = 2048;
const IO_DEADLINE: Duration = Duration::from_secs(2);
const CLIENT_DEADLINE: Duration = Duration::from_secs(15);
type Apply = Arc<dyn Fn(&Blocking) -> io::Result<Applied> + Send + Sync>;

/// Whitelisted projection returned only after the effective transition completes.
/// Never include transport credentials, destination, main config or its digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub enabled: bool,
    pub stream: String,
    pub run_id: Option<String>,
    pub overlay_present: bool,
}
impl Applied {
    pub fn validate(&self) -> io::Result<()> {
        if !matches!(self.stream.as_str(), "production" | "automated-test")
            || self.run_id.as_ref().is_some_and(|id| !valid_uuid(id))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema: u32,
    pub request_id: String,
    pub pid: u32,
    pub start_ticks: u64,
    pub generation: u64,
    pub enabled: bool,
    pub stream: String,
    pub labels: Vec<String>,
    pub run_id: Option<String>,
    pub overlay_present: bool,
}
impl Receipt {
    fn new(
        request_id: String,
        process: (u32, u64),
        generation: u64,
        applied: Applied,
    ) -> io::Result<Self> {
        applied.validate()?;
        let labels = if applied.stream == "automated-test" {
            vec!["automated-test".into()]
        } else {
            vec![]
        };
        Ok(Self {
            schema: 1,
            request_id,
            pid: process.0,
            start_ticks: process.1,
            generation,
            enabled: applied.enabled,
            stream: applied.stream,
            labels,
            run_id: applied.run_id,
            overlay_present: applied.overlay_present,
        })
    }
    fn validate(&self, nonce: &str) -> io::Result<()> {
        let applied = Applied {
            enabled: self.enabled,
            stream: self.stream.clone(),
            run_id: self.run_id.clone(),
            overlay_present: self.overlay_present,
        };
        let expected = Self::new(
            nonce.into(),
            (self.pid, self.start_ticks),
            self.generation,
            applied,
        )?;
        if !valid_nonce(nonce)
            || self.pid == 0
            || self.start_ticks == 0
            || self.start_ticks > i64::MAX as u64
            || self.generation == 0
            || self != &expected
        {
            return Err(invalid());
        }
        Ok(())
    }
}
fn invalid() -> io::Error {
    io::Error::other("invalid telemetry control exchange")
}
fn valid_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
            }
        })
}
fn request(bytes: &[u8]) -> io::Result<String> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| invalid())?
        .strip_prefix("reload ")
        .and_then(|v| v.strip_suffix('\n'))
        .ok_or_else(invalid)?;
    if !valid_nonce(value) {
        return Err(invalid());
    }
    Ok(value.to_owned())
}
fn process_identity(blocking: &Blocking) -> io::Result<(u32, u64)> {
    let _ = blocking;
    let mut bytes = Vec::new();
    File::open("/proc/self/stat")?
        .take(4097)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(invalid());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
    let (pid, rest) = text.split_once(' ').ok_or_else(invalid)?;
    let (_, fields) = rest.rsplit_once(')').ok_or_else(invalid)?;
    let pid: u32 = pid.parse().map_err(|_| invalid())?;
    let ticks: u64 = fields
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    if pid != std::process::id() || ticks == 0 || ticks > i64::MAX as u64 {
        return Err(invalid());
    }
    Ok((pid, ticks))
}

/// Acquire a stable adjacent lock before retiring a stale socket. Another live
/// process's listener is never unlinked. The flock dies with the process; the
/// fixed lock inode remains for the next supervised restart.
fn prepare(_blocking: &Blocking, socket: &Path) -> io::Result<(UnixListener, File)> {
    let parent = socket.parent().ok_or_else(invalid)?;
    let metadata = std::fs::symlink_metadata(parent)?;
    // SAFETY: geteuid has no pointer arguments or mutable process effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(invalid());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(socket.with_extension("lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(invalid());
    }
    // SAFETY: lock owns a live descriptor for the entire listener lifetime.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == uid => {
            std::fs::remove_file(socket)?
        }
        Ok(_) => return Err(invalid()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok((listener, lock))
}

/// Process-lifetime listener; no extra native thread and no idle polling.
/// Start even with reporting disabled so tests can enable a scoped context.
pub fn start(
    blocking: &Blocking,
    apply: impl Fn(&Blocking) -> io::Result<Applied> + Send + Sync + 'static,
) -> io::Result<()> {
    start_at(blocking, Path::new(SOCKET), Arc::new(apply)).map(|_| ())
}
fn start_at(
    blocking: &Blocking,
    socket: &Path,
    apply: Apply,
) -> io::Result<tokio::task::JoinHandle<()>> {
    let (listener, lock) = prepare(blocking, socket)?;
    let process = process_identity(blocking)?;
    Ok(crate::rt::io().spawn(async move {
        let _lock = lock;
        let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
            return;
        };
        let mut generation = 0u64;
        while let Ok((mut stream, _)) = listener.accept().await {
            // Peer credentials prevent even a temporary permissive bind mode
            // from admitting another user. No authentication secret is sent.
            if !same_user(&stream) {
                continue;
            }
            let Ok(Ok(bytes)) =
                tokio::time::timeout(IO_DEADLINE, line(&mut stream, REQUEST_BYTES)).await
            else {
                continue;
            };
            let Ok(nonce) = request(&bytes) else {
                continue;
            };
            let Some(next) = generation.checked_add(1) else {
                continue;
            };
            let apply = apply.clone();
            // One finite worker at a time. Do not detach/cancel this transition
            // on client disconnect: a later cleanup request must follow it.
            let Ok(Ok(applied)) = crate::rt::spawn_blocking(move |blocking| apply(blocking)).await
            else {
                continue;
            };
            let Ok(receipt) = Receipt::new(nonce, process, next, applied) else {
                continue;
            };
            generation = next;
            let Ok(mut bytes) = serde_json::to_vec(&receipt) else {
                continue;
            };
            if bytes.len() >= REPLY_BYTES {
                continue;
            }
            bytes.push(b'\n');
            let _ = tokio::time::timeout(IO_DEADLINE, stream.write_all(&bytes)).await;
        }
    }))
}
fn same_user(stream: &tokio::net::UnixStream) -> bool {
    // SAFETY: geteuid reads only this process's effective numeric identity.
    let uid = unsafe { libc::geteuid() };
    matches!(stream.peer_cred(), Ok(peer) if peer.uid() == uid)
}
async fn line(stream: &mut tokio::net::UnixStream, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit);
    let mut buffer = [0u8; 256];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if bytes.len() + count > limit {
            return Err(invalid());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.iter().position(|b| *b == b'\n') {
            if index + 1 != bytes.len() {
                return Err(invalid());
            }
            return Ok(bytes);
        }
    }
}

/// Await the process-bound applied receipt before stimulus or after removing
/// the test overlay. A timed-out exchange is unknown, never an applied receipt.
pub async fn request_reload(request_id: &str) -> io::Result<Receipt> {
    request_at(Path::new(SOCKET), request_id).await
}
async fn request_at(socket: &Path, request_id: &str) -> io::Result<Receipt> {
    if !valid_nonce(request_id) {
        return Err(invalid());
    }
    tokio::time::timeout(CLIENT_DEADLINE, async {
        // A Unix socket refuses with EAGAIN while its listen backlog is full;
        // retry within the deadline rather than failing the request.
        let mut stream = loop {
            match tokio::net::UnixStream::connect(socket).await {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                other => break other?,
            }
        };
        if !same_user(&stream) {
            return Err(invalid());
        }
        stream
            .write_all(format!("reload {request_id}\n").as_bytes())
            .await?;
        let receipt: Receipt = serde_json::from_slice(&line(&mut stream, REPLY_BYTES).await?)
            .map_err(|_| invalid())?;
        receipt.validate(request_id)?;
        if !matches!(stream.peer_cred(), Ok(peer) if peer.pid().is_some_and(|pid| pid > 0 && pid as u32 == receipt.pid)) {
            return Err(invalid());
        }
        Ok(receipt)
    })
    .await
    .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
    };
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const RUN: &str = "12345678-1234-1234-1234-123456789abc";
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            // Short relative names fit AF_UNIX; target fixtures may name their
            // admitted writable scratch root. All native data stays in build/.
            let base = std::env::var_os("Q2_GENERATED_TEST_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("../../build/perf"));
            let path = base.join(format!("tc-{:08x}", fastrand::u32(..)));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn socket(&self) -> PathBuf {
            self.0.join("s")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn applied(enabled: bool) -> Applied {
        Applied {
            enabled,
            stream: if enabled {
                "automated-test"
            } else {
                "production"
            }
            .into(),
            run_id: enabled.then(|| RUN.into()),
            overlay_present: enabled,
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn stop(task: tokio::task::JoinHandle<()>) {
        task.abort();
        let _ = runtime().block_on(task);
    }

    #[test]
    fn strict_request_and_receipt_allowlist_rejects_unbounded_or_secret_fields() {
        assert_eq!(
            request(format!("reload {NONCE}\n").as_bytes()).unwrap(),
            NONCE
        );
        for text in [
            "reload ../../secret\n",
            "reload 123\n",
            "apply abc\n",
            "reload A123456789abcdef0123456789abcdef\n",
            "reload 0123456789abcdef0123456789abcdef\nextra",
        ] {
            assert!(request(text.as_bytes()).is_err());
        }
        let receipt = Receipt::new(NONCE.into(), (1, 1), 1, applied(true)).unwrap();
        receipt.validate(NONCE).unwrap();
        let mut value = serde_json::to_value(&receipt).unwrap();
        value["token"] = "secret-canary".into();
        assert!(serde_json::from_value::<Receipt>(value).is_err());
        let serialized = serde_json::to_string(&receipt).unwrap();
        assert!(!serialized.contains("endpoint"));
        assert!(!serialized.contains("token"));
        let mut wrong = receipt.clone();
        wrong.request_id = "f".repeat(32);
        assert!(wrong.validate(NONCE).is_err());
        wrong = receipt;
        wrong.labels.push("secret-canary".into());
        assert!(wrong.validate(NONCE).is_err());
    }

    #[test]
    fn live_off_on_off_receipts_follow_completed_callback_without_config_files() {
        let fixture = Fixture::new();
        let enabled = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (next, seen) = (enabled.clone(), observed.clone());
        let task = start_at(
            &Blocking::dedicated_thread(),
            &fixture.socket(),
            Arc::new(move |_| {
                let effective = applied(next.load(Ordering::Acquire));
                seen.lock().unwrap().push(effective.clone());
                Ok(effective)
            }),
        )
        .unwrap();
        let rt = runtime();
        let baseline = rt.block_on(request_at(&fixture.socket(), NONCE)).unwrap();
        assert!(!baseline.enabled);
        assert!(!baseline.overlay_present);
        assert_eq!(baseline.generation, 1);
        enabled.store(true, Ordering::Release);
        let active = rt.block_on(request_at(&fixture.socket(), NONCE)).unwrap();
        assert!(active.enabled);
        assert_eq!(active.labels, ["automated-test"]);
        assert_eq!(active.run_id.as_deref(), Some(RUN));
        assert_eq!(active.pid, std::process::id());
        assert!(active.start_ticks > 0);
        assert_eq!(observed.lock().unwrap().len(), 2); // callback complete before receipt
        enabled.store(false, Ordering::Release);
        let cleared = rt.block_on(request_at(&fixture.socket(), NONCE)).unwrap();
        assert!(!cleared.enabled);
        assert_eq!(cleared.stream, "production");
        assert!(cleared.labels.is_empty());
        assert_eq!(cleared.generation, 3);
        assert_eq!(cleared.start_ticks, active.start_ticks);
        // A completed receipt is a value; clearing cannot relabel prior evidence.
        assert_eq!(active.run_id.as_deref(), Some(RUN));
        stop(task);
    }

    #[test]
    fn active_listener_is_not_unlinked_and_stale_socket_is_recovered_after_stop() {
        let fixture = Fixture::new();
        let blocking = Blocking::dedicated_thread();
        let apply: Apply = Arc::new(|_| Ok(applied(false)));
        let task = start_at(&blocking, &fixture.socket(), apply.clone()).unwrap();
        assert!(start_at(&blocking, &fixture.socket(), apply.clone()).is_err());
        assert_eq!(
            std::fs::symlink_metadata(fixture.socket()).unwrap().mode() & 0o777,
            0o600
        );
        assert!(
            !runtime()
                .block_on(request_at(&fixture.socket(), NONCE))
                .unwrap()
                .enabled
        );
        stop(task);
        let restarted = start_at(&blocking, &fixture.socket(), apply).unwrap();
        assert_eq!(
            runtime()
                .block_on(request_at(&fixture.socket(), NONCE))
                .unwrap()
                .generation,
            1
        );
        stop(restarted);
    }

    #[test]
    fn regular_symlink_and_hardlinked_ownership_paths_are_refused() {
        let fixture = Fixture::new();
        let blocking = Blocking::dedicated_thread();
        let socket = fixture.socket();
        let target = fixture.0.join("sentinel");
        std::fs::write(&target, b"unchanged").unwrap();
        std::fs::write(&socket, b"ordinary file").unwrap();
        assert!(prepare(&blocking, &socket).is_err());
        std::fs::remove_file(&socket).unwrap();
        std::os::unix::fs::symlink(&target, &socket).unwrap();
        assert!(prepare(&blocking, &socket).is_err());
        std::fs::remove_file(&socket).unwrap();
        let lock = socket.with_extension("lock");
        std::fs::remove_file(&lock).unwrap();
        std::fs::hard_link(&target, &lock).unwrap();
        assert!(prepare(&blocking, &socket).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
    }

    #[test]
    fn malformed_and_oversized_clients_do_not_apply_context() {
        let fixture = Fixture::new();
        let calls = Arc::new(AtomicU32::new(0));
        let count = calls.clone();
        let task = start_at(
            &Blocking::dedicated_thread(),
            &fixture.socket(),
            Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(applied(false))
            }),
        )
        .unwrap();
        for body in [b"unrecognized\n".to_vec(), vec![b'x'; REQUEST_BYTES + 1]] {
            let mut client = std::os::unix::net::UnixStream::connect(fixture.socket()).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            client.write_all(&body).unwrap();
            let mut byte = [0];
            let _ = client.read(&mut byte);
        }
        let receipt = runtime()
            .block_on(request_at(&fixture.socket(), NONCE))
            .unwrap();
        assert_eq!(receipt.generation, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        stop(task);
    }

    #[test]
    fn failed_transition_has_no_applied_receipt_and_next_request_can_restore() {
        let fixture = Fixture::new();
        let fail = Arc::new(AtomicBool::new(true));
        let flag = fail.clone();
        let task = start_at(
            &Blocking::dedicated_thread(),
            &fixture.socket(),
            Arc::new(move |_| {
                if flag.load(Ordering::Acquire) {
                    Err(io::Error::other("generated failure"))
                } else {
                    Ok(applied(false))
                }
            }),
        )
        .unwrap();
        let rt = runtime();
        assert!(rt.block_on(request_at(&fixture.socket(), NONCE)).is_err());
        fail.store(false, Ordering::Release);
        assert_eq!(
            rt.block_on(request_at(&fixture.socket(), NONCE))
                .unwrap()
                .generation,
            1
        );
        stop(task);
    }
    #[test]
    fn disconnected_apply_is_joined_before_a_later_clear_receipt() {
        let fixture = Fixture::new();
        let calls = Arc::new(AtomicU32::new(0));
        let count = calls.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let task = start_at(
            &Blocking::dedicated_thread(),
            &fixture.socket(),
            Arc::new(move |_| {
                let first = count.fetch_add(1, Ordering::SeqCst) == 0;
                if first {
                    entered_tx.send(()).map_err(|_| invalid())?;
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(3))
                        .map_err(|_| invalid())?;
                }
                Ok(applied(first))
            }),
        )
        .unwrap();
        let mut abandoned = std::os::unix::net::UnixStream::connect(fixture.socket()).unwrap();
        abandoned
            .write_all(format!("reload {NONCE}\n").as_bytes())
            .unwrap();
        drop(abandoned);
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let socket = fixture.socket();
        let clear = crate::rt::io().spawn(async move { request_at(&socket, NONCE).await });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        let receipt = runtime().block_on(clear).unwrap().unwrap();
        assert_eq!(receipt.generation, 2);
        assert!(!receipt.enabled);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        stop(task);
    }
}
