//! Generated control sockets only; no configured network, daemon or credentials.
use super::*;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time::{timeout, sleep};

struct Fixture { root: PathBuf, remote: PathBuf, daemon: UnixDatagram, system: Arc<System> }
impl Fixture {
    fn new() -> Self {
        let root = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../../build/perf"))
            .join(format!("wr-{:08x}", fastrand::u32(..)));
        std::fs::create_dir_all(&root).unwrap();
        let remote = root.join("wlan0");
        let daemon = UnixDatagram::bind(&remote).unwrap();
        let system = Arc::new(System::new(root.join("unused-helper"), (remote.clone(), root.clone())));
        Self { root, remote, daemon, system }
    }
    async fn finish(self) {
        let Self { root, system, daemon, .. } = self;
        drop(system); drop(daemon);
        timeout(Duration::from_secs(3), async {
            loop {
                if std::fs::read_dir(&root).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("q2nd-wpa-")) { break; }
                sleep(Duration::from_millis(1)).await;
            }
        }).await.expect("all owned request sockets must retire");
        std::fs::remove_dir_all(root).unwrap();
    }
}
async fn receive(daemon: &UnixDatagram) -> (String, PathBuf) {
    let mut bytes = [0; 256];
    let (n, peer) = timeout(Duration::from_secs(4), daemon.recv_from(&mut bytes)).await.unwrap().unwrap();
    (String::from_utf8(bytes[..n].to_vec()).unwrap(), peer.as_pathname().unwrap().to_owned())
}
fn request(system: &Arc<System>, cmd: &'static str) -> tokio::task::JoinHandle<Option<String>> {
    let system = system.clone();
    rt::io().spawn(async move { system.wpa(cmd).await })
}

#[test]
fn requests_reuse_socket_and_serialize_unanswered_commands() {
    rt::Ui::new().unwrap().block_on(async {
        let f = Fixture::new();
        let first = request(&f.system, "FIRST");
        let (cmd, peer) = receive(&f.daemon).await;
        assert_eq!(cmd, "FIRST");
        // Poll the competing exchange while the first owns the connection.
        // It cannot send or take a late reply before the first completes.
        let mut second = Box::pin(f.system.wpa("SECOND"));
        use std::future::Future;
        assert!(std::future::poll_fn(|cx| std::task::Poll::Ready(second.as_mut().poll(cx))).await.is_pending());
        f.daemon.send_to(b"one", &peer).await.unwrap();
        assert_eq!(first.await.unwrap().as_deref(), Some("one"));
        let serve = async {
            let (cmd, next) = receive(&f.daemon).await;
            assert_eq!(cmd, "SECOND"); assert_eq!(next, peer);
            f.daemon.send_to(b"two", &next).await.unwrap();
        };
        let (result, ()) = tokio::join!(second, serve);
        assert_eq!(result.as_deref(), Some("two"));
        f.finish().await;
    });
}

#[test]
fn cancelled_queued_request_releases_reservation_without_sending_or_retiring_socket() {
    rt::Ui::new().unwrap().block_on(async {
        use std::future::Future;
        for cancel_after_release in [false, true] {
            let f = Fixture::new();
            let first = request(&f.system, "OWNER");
            let (cmd, peer) = receive(&f.daemon).await;
            assert_eq!(cmd, "OWNER"); // Owner has the mutex and unanswered socket.
            let mut cancelled = Box::pin(f.system.wpa("MUST-NOT-SEND"));
            let mut survivor = Box::pin(f.system.wpa("SURVIVOR"));
            for waiter in [&mut cancelled, &mut survivor] {
                assert!(std::future::poll_fn(|cx|
                    std::task::Poll::Ready(waiter.as_mut().poll(cx))).await.is_pending());
            }
            let mut unexpected = [0;256];
            assert_eq!(f.daemon.try_recv(&mut unexpected).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock);
            if cancel_after_release {
                f.daemon.send_to(b"owner-reply", &peer).await.unwrap();
                assert_eq!(first.await.unwrap().as_deref(), Some("owner-reply"));
                // It was notified/reserved, but has not been polled again.
                drop(cancelled);
            } else {
                drop(cancelled);
                f.daemon.send_to(b"owner-reply", &peer).await.unwrap();
                assert_eq!(first.await.unwrap().as_deref(), Some("owner-reply"));
            }
            let serve = async {
                let (cmd, next) = receive(&f.daemon).await;
                assert_eq!(cmd, "SURVIVOR");
                assert_eq!(next, peer, "queued cancellation must keep the reusable connection");
                f.daemon.send_to(b"survivor-reply", &next).await.unwrap();
            };
            let (result, ()) = timeout(Duration::from_secs(4), async {
                tokio::join!(survivor, serve)
            }).await.expect("cancelled waiter must pass ownership to its successor");
            assert_eq!(result.as_deref(), Some("survivor-reply"));
            assert_eq!(f.daemon.try_recv(&mut unexpected).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock);
            f.finish().await;
        }
    });
}

#[test]
fn cancelled_request_discards_late_reply_and_reconnects() {
    rt::Ui::new().unwrap().block_on(async {
        let f = Fixture::new();
        let old = request(&f.system, "OLD");
        let (_, previous) = receive(&f.daemon).await;
        old.abort(); assert!(old.await.unwrap_err().is_cancelled());
        let _ = f.daemon.send_to(b"stale", &previous).await;
        let current = request(&f.system, "NEW");
        let (cmd, peer) = receive(&f.daemon).await;
        assert_eq!(cmd, "NEW"); assert_ne!(peer, previous);
        f.daemon.send_to(b"current", &peer).await.unwrap();
        assert_eq!(current.await.unwrap().as_deref(), Some("current"));
        f.finish().await;
    });
}

#[test]
fn unsolicited_events_cannot_extend_request_deadline_or_reuse_timed_out_socket() {
    rt::Ui::new().unwrap().block_on(async {
        let f = Fixture::new();
        let mut old = request(&f.system, "MISSING");
        let (_, previous) = receive(&f.daemon).await;
        let mut events = 0;
        let result = timeout(Duration::from_secs(4), async {
            loop {
                tokio::select! {
                    result = &mut old => break result.unwrap(),
                    _ = sleep(Duration::from_millis(100)) => {
                        let _ = f.daemon.send_to(b"<3>CTRL-EVENT-SCAN-RESULTS", &previous).await;
                        events += 1;
                    }
                }
            }
        }).await;
        if result.is_err() { old.abort(); let _ = old.await; }
        assert_eq!(result.unwrap(), None); assert!(events > 1);
        let current = request(&f.system, "AFTER");
        let (cmd, peer) = receive(&f.daemon).await;
        assert_eq!(cmd, "AFTER"); assert_ne!(peer, previous);
        f.daemon.send_to(b"new", &peer).await.unwrap();
        assert_eq!(current.await.unwrap().as_deref(), Some("new"));
        f.finish().await;
    });
}

#[test]
fn replacement_daemon_is_used_even_while_previous_daemon_is_alive() {
    rt::Ui::new().unwrap().block_on(async {
        let f = Fixture::new();
        let first = request(&f.system, "BEFORE");
        let (_, old_peer) = receive(&f.daemon).await;
        f.daemon.send_to(b"old", &old_peer).await.unwrap();
        assert_eq!(first.await.unwrap().as_deref(), Some("old"));
        std::fs::remove_file(&f.remote).unwrap();
        let replacement = UnixDatagram::bind(&f.remote).unwrap();
        let next = request(&f.system, "AFTER");
        let (cmd, peer) = receive(&replacement).await;
        assert_eq!(cmd, "AFTER"); assert_ne!(peer, old_peer);
        replacement.send_to(b"replacement", &peer).await.unwrap();
        assert_eq!(next.await.unwrap().as_deref(), Some("replacement"));
        let mut bytes = [0; 16];
        assert_eq!(f.daemon.try_recv(&mut bytes).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        drop(replacement); f.finish().await;
    });
}

#[test]
fn connection_failure_retires_prepared_path_and_next_request_recovers() {
    rt::Ui::new().unwrap().block_on(async {
        let f = Fixture::new();
        std::fs::remove_file(&f.remote).unwrap();
        std::fs::write(&f.remote, b"not a socket").unwrap();
        assert_eq!(f.system.wpa("REFUSED").await, None);
        std::fs::remove_file(&f.remote).unwrap();
        let replacement = UnixDatagram::bind(&f.remote).unwrap();
        let task = request(&f.system, "RECOVERED");
        let (cmd, peer) = receive(&replacement).await;
        assert_eq!(cmd, "RECOVERED");
        replacement.send_to(b"ok", &peer).await.unwrap();
        assert_eq!(task.await.unwrap().as_deref(), Some("ok"));
        drop(replacement); f.finish().await;
    });
}

// The original System::wpa method from product6c31a237, retained verbatim apart
// from its function name and explicit self type. Algorithm baseline only: not
// an execution of the installed V071 app or a whole-app wakeup/power comparison.
impl System {
    async fn legacy_wpa(&self, cmd: &str) -> Option<String> {
        let local = self.local.join(format!("q2nd-wpa-{}-{}", std::process::id(), fastrand::u32(..)));
        let ctrl = self.ctrl.clone();
        let (sock, mut path) = rt::spawn_blocking(move |blocking| {
            let bound = super::socket_path::bind(blocking, &local)?;
            bound.0.connect(&ctrl)?;
            Ok::<_, std::io::Error>(bound)
        }).await.ok()?.ok()?;
        let result = async {
        sock.send(cmd.as_bytes()).await.ok()?;
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf)).await.ok()?.ok()?;
            // Unsolicited events start with '<'; skip them.
            if !buf[..n].starts_with(b"<") {
                return Some(String::from_utf8_lossy(&buf[..n]).into_owned());
            }
        }
        }.await;
        drop(sock);
        path.close().await;
        result
    }

}

#[test]
fn generated_abba_request_cost_and_socket_count() {
    rt::Ui::new().unwrap().block_on(async {
        const REQUESTS: usize = 32;
        let mut arms = Vec::new();
        for (arm, legacy) in [true, false, false, true].into_iter().enumerate() {
            let f = Fixture::new();
            let system = f.system.clone();
            let client = rt::io().spawn(async move {
                let mut times = Vec::new();
                for sequence in 0..REQUESTS {
                    let command = format!("GENERATED-{sequence}");
                    let started = std::time::Instant::now();
                    let result = if legacy { system.legacy_wpa(&command).await } else { system.wpa(&command).await };
                    times.push(started.elapsed().as_nanos() as u64);
                    assert_eq!(result.as_deref(), Some(command.as_str()));
                }
                times
            });
            let mut peers = HashSet::new();
            for sequence in 0..REQUESTS {
                let (command, peer) = receive(&f.daemon).await;
                assert_eq!(command, format!("GENERATED-{sequence}"));
                f.daemon.send_to(command.as_bytes(), &peer).await.unwrap();
                peers.insert(peer);
            }
            let times = client.await.unwrap();
            let distinct_sockets = peers.len();
            assert_eq!(distinct_sockets, if legacy { REQUESTS } else { 1 });
            f.finish().await;
            arms.push(serde_json::json!({"arm":arm,"legacy":legacy,"requests":REQUESTS,
                "distinct_peer_sockets":distinct_sockets,"request_ns":times,"owned_cleanup":true}));
        }
        println!("Q2_WPA_REQUEST_ABBA {}", serde_json::json!({"schema":1,
            "scope":"generated request algorithm comparison; not installed-app wakeups or energy",
            "baseline_source":"fixture-baseline","arms":arms}));
    });
}
