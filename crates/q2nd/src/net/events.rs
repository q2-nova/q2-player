//! Supplicant events accelerate association/DHCP; periodic status is still truth.
//! A missing socket, failed ATTACH, lost event or daemon replacement always
//! retains the ordinary polling fallback. Event contents are never logged.
//!
//! The subscription is a datagram socket on the I/O runtime's reactor: the
//! monitor task waits on it together with its commands and its deadline.
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixDatagram;
use crate::rt::{self, Blocking};
use super::socket_path::SocketPath;

/// What one wait on the event socket produced.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Packet {
    /// Connected, disconnected or terminating: status work is due.
    Relevant,
    /// A hinted first scan found no saved network (see net/kick.rs).
    NotFound,
    /// A survey completed; only an outstanding scan needs result work.
    ScanResults,
    /// Signal changes and other unrelated events: no status work.
    Other,
    /// The socket failed; the subscription is gone.
    Lost,
}

/// A source of supplicant events, as the monitor uses it.
pub(super) trait Events: Send {
    /// The daemon attached to is still the one at the control path.
    fn current(&self) -> impl Future<Output = bool> + Send;
    /// Waits for packets and classifies what arrived.
    fn recv(&mut self) -> impl Future<Output = Packet> + Send;
}

/// A hinted first scan found no saved network (see net/kick.rs).
fn not_found(bytes: &[u8]) -> bool {
    let Some(end) = bytes.iter().position(|&b| b == b'>') else { return false };
    let event = &bytes[end + 1..];
    let name = b"CTRL-EVENT-NETWORK-NOT-FOUND".as_slice();
    bytes.starts_with(b"<") && event.starts_with(name) && event.get(name.len()).is_none_or(|b| b.is_ascii_whitespace())
}

fn relevant(bytes: &[u8]) -> bool {
    let event = if bytes.starts_with(b"<") {
        match bytes.iter().position(|&b| b == b'>') { Some(end) => &bytes[end+1..], None => return false }
    } else { return false; };
    [b"CTRL-EVENT-CONNECTED".as_slice(), b"CTRL-EVENT-DISCONNECTED", b"CTRL-EVENT-TERMINATING"]
        .iter().any(|name| event.starts_with(name) && event.get(name.len()).is_none_or(|b| b.is_ascii_whitespace()))
}

fn scan_results(bytes: &[u8]) -> bool {
    let Some(end) = bytes.iter().position(|&b| b == b'>') else { return false };
    let event = &bytes[end + 1..];
    let name = b"CTRL-EVENT-SCAN-RESULTS".as_slice();
    bytes.starts_with(b"<") && event.starts_with(name) && event.get(name.len()).is_none_or(|b| b.is_ascii_whitespace())
}

/// The flight-recorder kind of a supplicant event (`ux::Mark::WifiEvent`), from
/// its name only: scans and associations show up next to Bluetooth stalls.
fn wifi_event(bytes: &[u8]) -> Option<u32> {
    let end = bytes.iter().position(|&b| b == b'>').filter(|_| bytes.starts_with(b"<"))?;
    let event = &bytes[end + 1..];
    let named = |name: &[u8]| event.starts_with(name) && event.get(name.len()).is_none_or(|b| b.is_ascii_whitespace());
    [(b"CTRL-EVENT-SCAN-STARTED".as_slice(), 0), (b"CTRL-EVENT-SCAN-RESULTS", 1), (b"SME: Trying to authenticate", 2), (b"Trying to associate", 2),
        (b"CTRL-EVENT-CONNECTED", 3), (b"CTRL-EVENT-DISCONNECTED", 4), (b"CTRL-EVENT-NETWORK-NOT-FOUND", 5)]
        .into_iter().find(|(name, _)| named(name)).map(|(_, kind)| kind)
}

/// Records a supplicant event, at most one of each kind a second (a search
/// repeats its scans every few seconds and would fill the recorder).
fn mark_wifi(bytes: &[u8]) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static LAST: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
    let Some(kind) = wifi_event(bytes) else { return };
    // Boot milliseconds + 1, so zero means never.
    let now = (crate::playtrace::boottime_us() / 1000) as u32 | 1;
    let last = &LAST[kind as usize];
    let before = last.load(Ordering::Relaxed);
    if before != 0 && now.wrapping_sub(before) < 1000 { return }
    if last.compare_exchange(before, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() { crate::ux::record(crate::ux::Mark::WifiEvent, kind); }
}

pub(super) struct Subscription {
    socket: UnixDatagram,
    local: SocketPath,
    remote: PathBuf,
    identity: (u64, u64),
    detach: bool,
}
impl Subscription {
    /// How long ATTACH may take before the monitor falls back to polling.
    const ATTACH: Duration = Duration::from_millis(300);

    pub async fn attach(remote: &Path, directory: &Path) -> Option<Self> {
        let (remote, directory) = (remote.to_owned(), directory.to_owned());
        let mut subscription = rt::spawn_blocking(move |blocking| Self::prepare(blocking, &remote, &directory)).await.ok().flatten()?;
        let attached = tokio::time::timeout(Self::ATTACH, async {
            subscription.socket.send(b"ATTACH").await.ok()?;
            let mut bytes = [0u8; 4096];
            loop {
                let n = subscription.socket.recv(&mut bytes).await.ok()?;
                if bytes[..n].starts_with(b"<") { continue; }
                return (bytes[..n].trim_ascii() == b"OK").then_some(());
            }
        }).await;
        if matches!(attached, Ok(Some(()))) { Some(subscription) }
        else { subscription.close().await; None }
    }

    fn prepare(blocking: &Blocking, remote: &Path, directory: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(remote).ok()?;
        sweep_dead(blocking, directory, Path::new("/proc"));
        let local = directory.join(format!("q2nd-events-{}-{}", std::process::id(), fastrand::u32(..)));
        let (socket, local) = super::socket_path::bind(blocking, &local).ok()?;
        // From here the pathname guard owns cleanup, whatever happens.
        let subscription = Self { socket, local, remote: remote.into(), identity: (metadata.dev(), metadata.ino()), detach: true };
        subscription.socket.connect(remote).ok()?;
        Some(subscription)
    }

    async fn close(&mut self) {
        let _ = self.socket.try_send(b"DETACH");
        self.detach = false;
        self.local.close().await;
    }
}
impl Events for Subscription {
    fn current(&self) -> impl Future<Output = bool> + Send {
        let (remote, identity) = (self.remote.clone(), self.identity);
        async move {
            rt::spawn_blocking(move |_blocking| std::fs::metadata(remote).is_ok_and(|m| (m.dev(), m.ino()) == identity))
                .await.unwrap_or(false)
        }
    }
    async fn recv(&mut self) -> Packet {
        if self.socket.readable().await.is_err() { return Packet::Lost; }
        let mut bytes = [0u8; 4096];
        // Bound a noisy event source; unrelated scan/signal events do
        // not cause expensive STATUS/LIST_NETWORKS work on each packet.
        for _ in 0..64 {
            let received = self.socket.try_recv(&mut bytes);
            if let Ok(n) = received { mark_wifi(&bytes[..n]); }
            match received {
                Ok(n) if not_found(&bytes[..n]) => return Packet::NotFound,
                Ok(n) if relevant(&bytes[..n]) => return Packet::Relevant,
                Ok(n) if scan_results(&bytes[..n]) => return Packet::ScanResults,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return Packet::Lost,
            }
        }
        Packet::Other
    }
}
/// Removes event sockets left by players that exited without `Drop` (killed
/// or `exit`ed): `q2nd-events-PID-N` whose PID has no `/proc` entry. Only
/// this client's own names are touched.
fn sweep_dead(_blocking: &Blocking, directory: &Path, proc: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("q2nd-events-")) else { continue };
        let Some((pid, nonce)) = rest.split_once('-') else { continue };
        let (Ok(pid), Ok(_)) = (pid.parse::<u32>(), nonce.parse::<u32>()) else { continue };
        if pid == std::process::id() || proc.join(pid.to_string()).exists() { continue; }
        let _ = std::fs::remove_file(entry.path());
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Never wait for a response while tearing down; a crashed/replaced
        // daemon must not hold up disable/reconnect. Socket removal releases
        // this client's registration in wpa_supplicant.
        if self.detach { let _ = self.socket.try_send(b"DETACH"); }
        // SocketPath retains ownership through cancellation; executor drops
        // schedule removal, while explicit close() awaits it.
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn not_found_is_recognised_exactly() {
        assert!(super::not_found(b"<3>CTRL-EVENT-NETWORK-NOT-FOUND"));
        assert!(super::not_found(b"<3>CTRL-EVENT-NETWORK-NOT-FOUND extra"));
        assert!(!super::not_found(b"<3>CTRL-EVENT-NETWORK-NOT-FOUNDX"));
        assert!(!super::not_found(b"CTRL-EVENT-NETWORK-NOT-FOUND"), "a reply, not an event");
        assert!(!super::not_found(b"<3>CTRL-EVENT-SCAN-RESULTS"));
        assert!(!super::relevant(b"<3>CTRL-EVENT-NETWORK-NOT-FOUND"), "not a connectivity event");
    }

    use super::*;
    #[test]
    fn sweep_removes_only_dead_players_sockets() {
        let root = scratch("sweep");
        let (dir, proc) = (root.join("ctrl"), root.join("proc"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(proc.join("4242")).unwrap();
        let me = std::process::id();
        for name in ["q2nd-events-4242-7", "q2nd-events-999999-3", &format!("q2nd-events-{me}-1"), "q2nd-events-x-1", "wlan0"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let (directory, processes) = (dir.clone(), proc.clone());
        crate::rt::Ui::new().unwrap().block_on(rt::spawn_blocking(move |blocking|
            sweep_dead(blocking, &directory, &processes))).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        let mut want = vec!["q2nd-events-4242-7".to_string(), format!("q2nd-events-{me}-1"), "q2nd-events-x-1".into(), "wlan0".into()];
        want.sort();
        assert_eq!(left, want);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A short directory: a socket path is limited to 107 bytes.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../../build/perf"));
        let dir = root.join(format!("ev-{name}-{:08x}", fastrand::u32(..)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn wait_removed(path: &Path) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while path.exists() { tokio::time::sleep(Duration::from_millis(1)).await; }
        }).await.expect("owned socket cleanup must complete");
    }

    #[test]
    fn cancelled_attach_retires_its_socket_on_the_real_io_runtime() {
        let dir = scratch("cancel");
        crate::rt::Ui::new().unwrap().block_on(async {
            let remote = dir.join("wlan0");
            let daemon = UnixDatagram::bind(&remote).unwrap();
            let (control, directory) = (remote.clone(), dir.clone());
            let task = rt::io().spawn(async move { Subscription::attach(&control, &directory).await });
            let mut packet = [0u8; 32];
            let (count, peer) = tokio::time::timeout(Duration::from_secs(2), daemon.recv_from(&mut packet)).await.unwrap().unwrap();
            assert_eq!(&packet[..count], b"ATTACH");
            let local = peer.as_pathname().unwrap().to_path_buf();
            assert!(local.exists());
            task.abort();
            assert!(task.await.is_err());
            wait_removed(&local).await;
            assert!(remote.exists(), "only the owned client path may be removed");
            drop(daemon);
            std::fs::remove_dir_all(&dir).unwrap();
        });
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn abandoned_preparation_reply_retires_the_bound_socket() {
        let dir = scratch("abandon");
        let remote = dir.join("wlan0");
        let daemon_path = remote.clone();
        let daemon = crate::rt::Ui::new().unwrap().block_on(rt::spawn_blocking(move |_| UnixDatagram::bind(daemon_path))).unwrap().unwrap();
        let (control, directory) = (remote.clone(), dir.clone());
        let (ready, entered) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let task = rt::io().spawn(async move {
            rt::spawn_blocking(move |blocking| {
                let subscription = Subscription::prepare(blocking, &control, &directory).unwrap();
                let local = std::fs::read_dir(&directory).unwrap().map(|e| e.unwrap().path())
                    .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("q2nd-events-")).unwrap();
                ready.send(local).unwrap();
                gate.recv_timeout(Duration::from_secs(3)).unwrap();
                subscription
            }).await
        });
        let local = entered.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(local.exists());
        task.abort();
        assert!(crate::rt::Ui::new().unwrap().block_on(task).is_err());
        release.send(()).unwrap();
        crate::rt::Ui::new().unwrap().block_on(wait_removed(&local));
        assert!(remote.exists());
        drop(daemon);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_bind_never_removes_an_existing_path() {
        let dir = scratch("collision");
        let path = dir.join("occupied");
        std::fs::write(&path, b"existing generated owner").unwrap();
        let local = path.clone();
        let result = crate::rt::Ui::new().unwrap().block_on(rt::spawn_blocking(move |blocking|
            super::super::socket_path::bind(blocking, &local))).unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing generated owner");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A generated daemon end: answers ATTACH with `reply`. Returns the
    /// subscription (if the attach held), the daemon socket and the
    /// client's socket path.
    async fn attached(dir: &Path, reply: &'static [u8]) -> (Option<Subscription>, UnixDatagram, PathBuf) {
        let remote = dir.join("wlan0");
        let daemon = UnixDatagram::bind(&remote).unwrap();
        let attach = Subscription::attach(&remote, dir);
        let serve = async {
            let mut bytes = [0u8; 64];
            let (n, peer) = daemon.recv_from(&mut bytes).await.unwrap();
            assert_eq!(&bytes[..n], b"ATTACH");
            let path = peer.as_pathname().unwrap().to_path_buf();
            // An event queued before the reply is skipped, not taken as the answer.
            daemon.send_to(b"<3>CTRL-EVENT-SCAN-STARTED", &path).await.unwrap();
            daemon.send_to(reply, &path).await.unwrap();
            path
        };
        let (subscription, path) = tokio::join!(attach, serve);
        (subscription, daemon, path)
    }

    #[tokio::test]
    async fn attach_subscribes_and_teardown_detaches_and_removes_the_socket() {
        let dir = scratch("attach");
        let (subscription, daemon, local) = attached(&dir, b"OK\n").await;
        let mut subscription = subscription.expect("attached");
        assert!(local.exists());
        assert!(subscription.current().await);
        subscription.close().await;
        drop(subscription);
        let mut bytes = [0u8; 64];
        let n = daemon.recv(&mut bytes).await.unwrap();
        assert_eq!(&bytes[..n], b"DETACH");
        assert!(!local.exists(), "the client socket file is removed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_refused_attach_falls_back_and_leaves_no_socket_file() {
        let dir = scratch("refused");
        let (subscription, _daemon, local) = attached(&dir, b"FAIL\n").await;
        assert!(subscription.is_none());
        assert!(!local.exists());
        assert!(Subscription::attach(&dir.join("absent"), &dir).await.is_none(), "no control socket");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_daemon_times_the_attach_out() {
        let dir = scratch("silent");
        let remote = dir.join("wlan0");
        let _daemon = UnixDatagram::bind(&remote).unwrap();
        let started = tokio::time::Instant::now();
        assert!(Subscription::attach(&remote, &dir).await.is_none());
        assert_eq!(started.elapsed(), Subscription::ATTACH);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "only the daemon's socket remains");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn packets_are_classified_and_unrelated_ones_are_drained_together() {
        let dir = scratch("packets");
        let (subscription, daemon, path) = attached(&dir, b"OK\n").await;
        let mut subscription = subscription.unwrap();
        // Unrelated signals still drain together without asking for status.
        for _ in 0..5 { daemon.send_to(b"<3>CTRL-EVENT-SIGNAL-CHANGE", &path).await.unwrap(); }
        assert_eq!(subscription.recv().await, Packet::Other);
        daemon.send_to(b"<3>CTRL-EVENT-SCAN-RESULTS", &path).await.unwrap();
        daemon.send_to(b"<3>CTRL-EVENT-CONNECTED generated", &path).await.unwrap();
        assert_eq!(subscription.recv().await, Packet::ScanResults);
        assert_eq!(subscription.recv().await, Packet::Relevant);
        daemon.send_to(b"<3>CTRL-EVENT-NETWORK-NOT-FOUND", &path).await.unwrap();
        assert_eq!(subscription.recv().await, Packet::NotFound);
        // A replaced daemon is noticed by identity.
        drop(daemon);
        std::fs::remove_file(dir.join("wlan0")).unwrap();
        let _replacement = UnixDatagram::bind(dir.join("wlan0")).unwrap();
        assert!(!subscription.current().await);
        drop(subscription);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn generated_event_boundaries_never_promote_non_connectivity_events() {
        let mut random = fastrand::Rng::with_seed(0x51425750);
        for _ in 0..4096 {
            let mut bytes = vec![0; random.usize(0..256)];
            random.fill(&mut bytes);
            assert!(!relevant(&bytes));
            assert!(!scan_results(&bytes));
        }
        for event in ["CONNECTED", "DISCONNECTED", "TERMINATING"] {
            let valid = format!("<3>CTRL-EVENT-{event}");
            assert!(relevant(valid.as_bytes()));
            for extra in ["x", "-FAKE", "_", "0"] {
                assert!(!relevant(format!("{valid}{extra}").as_bytes()));
            }
        }
        assert!(scan_results(b"<3>CTRL-EVENT-SCAN-RESULTS"));
        assert!(scan_results(b"<3>CTRL-EVENT-SCAN-RESULTS generated"));
        for bytes in [b"CTRL-EVENT-SCAN-RESULTS".as_slice(), b"<3>CTRL-EVENT-SCAN-RESULTS-FAKE", b"<3>CTRL-EVENT-SCAN-RESULTS0", b"<malformed"] {
            assert!(!scan_results(bytes));
        }
    }

    #[test]
    fn supplicant_events_map_to_fixed_wifi_marks_by_name_only() {
        for (bytes, kind) in [(b"<3>CTRL-EVENT-SCAN-STARTED ".as_slice(), Some(0)), (b"<3>CTRL-EVENT-SCAN-RESULTS", Some(1)),
            (b"<3>SME: Trying to authenticate with generated", Some(2)), (b"<3>Trying to associate with generated", Some(2)),
            (b"<3>CTRL-EVENT-CONNECTED - Connection to generated", Some(3)), (b"<3>CTRL-EVENT-DISCONNECTED bssid=generated", Some(4)),
            (b"<3>CTRL-EVENT-NETWORK-NOT-FOUND", Some(5)), (b"<3>CTRL-EVENT-SCAN-STARTEDX", None), (b"CTRL-EVENT-SCAN-STARTED", None),
            (b"<3>CTRL-EVENT-SIGNAL-CHANGE above=1", None), (b"<malformed", None)] {
            assert_eq!(super::wifi_event(bytes), kind, "{}", String::from_utf8_lossy(bytes));
        }
    }

    #[test]
    fn only_connectivity_events_wake_status_work() {
        for priority in 0..10 {
            for event in ["CONNECTED", "DISCONNECTED", "TERMINATING"] {
                assert!(relevant(format!("<{priority}>CTRL-EVENT-{event} generated").as_bytes()));
            }
            for event in ["SCAN-RESULTS", "SIGNAL-CHANGE", "CONNECTED-FAKE", "NETWORK-NOT-FOUND"] {
                assert!(!relevant(format!("<{priority}>CTRL-EVENT-{event}").as_bytes()));
            }
        }
        assert!(!relevant(b"OK"));
        assert!(!relevant(b"<malformed"));
    }
}
