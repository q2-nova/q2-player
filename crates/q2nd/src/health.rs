#![forbid(unsafe_code)]
//! Optional mainline service health. Only the actual UI/audio threads send pulses.
//! Without `Q2ND_HEALTH_SOCKET` nothing is opened and the UI wait is not capped.
use std::{cell::RefCell, os::unix::net::UnixDatagram, time::{Duration, Instant}};
struct Health { socket: UnixDatagram, path: String, role: u8, last: Instant }
thread_local! { static HEALTH: RefCell<Option<Health>> = const { RefCell::new(None) }; }
pub fn register(role: u8) {
    // The TLS sender belongs to the actual UI/player owner, initialized before
    // executor polling. Socket creation and nonblocking setup stay outside it.
    assert!(crate::rt::may_wait_for_tasks(), "register health on the ordinary UI/player owner");
    let Ok(path) = std::env::var("Q2ND_HEALTH_SOCKET") else { return };
    if let Ok(health) = Health::prepare(path, role, |health| health.socket.set_nonblocking(true)) {
        HEALTH.with(|h| *h.borrow_mut() = Some(health));
    }
    pulse();
}
pub fn pulse() {
    HEALTH.with(|h| {
        if let Some(health) = h.borrow_mut().as_mut() { health.pulse(); }
    });
}
impl Health {
    fn prepare(path: String, role: u8,
        nonblocking: impl FnOnce(&Self) -> std::io::Result<()>) -> std::io::Result<Self> {
        // Never publish a sender unless the kernel accepted nonblocking mode.
        // A saturated supervisor must not stop either the UI or PCM owner.
        let health = Self { socket: UnixDatagram::unbound()?, path, role, last: Instant::now() - Duration::from_secs(2) };
        nonblocking(&health)?;
        Ok(health)
    }
    fn pulse(&mut self) {
        if self.last.elapsed() < Duration::from_secs(1) { return; }
        // Fault injection deliberately stops the actual role, not a helper.
        // It remains enabled only by the disposable guest harness.
        if std::env::var("Q2_EMULATOR_TEST").as_deref() == Ok("1") {
            let name = if self.role == b'U' { "ui" } else { "audio" };
            while std::path::Path::new(&format!("/run/q2nd/freeze-{name}")).exists() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let _ = self.socket.send_to(&[self.role], &self.path);
        self.last = Instant::now();
    }
}
pub fn wait(duration: Duration) -> Duration {
    HEALTH.with(|h| if h.borrow().is_some() { duration.min(Duration::from_secs(1)) } else { duration })
}


/// Install an isolated generated sender on this ordinary test thread only.
/// Restore its previous TLS value even if the body unwinds; never mutate the
/// process environment or register a different thread as the audio owner.
#[cfg(test)]
pub(crate) fn with_generated_sender<R>(
    path: &std::path::Path, role: u8, body: impl FnOnce() -> R,
) -> std::io::Result<(R, Instant)> {
    assert!(crate::rt::may_wait_for_tasks());
    let path = path.to_str().ok_or_else(|| std::io::Error::other("generated health path is not UTF-8"))?;
    let health = Health::prepare(path.into(), role, |h| h.socket.set_nonblocking(true))?;
    struct Restore(Option<Health>);
    impl Drop for Restore {
        fn drop(&mut self) {
            HEALTH.with(|h| *h.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(HEALTH.with(|h| h.borrow_mut().replace(health)));
    let value = body();
    let last = HEALTH.with(|h| h.borrow().as_ref().unwrap().last);
    Ok((value, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io, path::PathBuf};

    #[test]
    #[cfg(panic = "unwind")]
    fn registration_refuses_executor_before_environment_or_socket_setup() {
        let runtime = crate::rt::Ui::new().unwrap();
        runtime.block_on(async {
            assert!(std::panic::catch_unwind(|| super::register(b'U')).is_err());
        });
    }

    #[test]
    fn failed_nonblocking_setup_never_admits_a_health_sender() {
        let result = Health::prepare("generated-unused".into(), b'U', |_| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "generated setup refusal"))
        });
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn full_health_queue_drops_pulses_without_blocking_or_changing_roles() {
        // Same short relative host socket-root convention as Tailscale's
        // generated fixture; target runners supply their owned writable root.
        let root = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../../build/perf"))
            .join(format!("health-{:08x}", fastrand::u32(..)));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("health.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver.set_nonblocking(true).unwrap();
        for role in [b'U', b'A'] {
            let mut health = Health::prepare(path.to_str().unwrap().into(), role, |health| {
                // Bound a broken implementation too, independently of the
                // actual kernel O_NONBLOCK assertion below.
                health.socket.set_write_timeout(Some(Duration::from_millis(100)))?;
                health.socket.set_nonblocking(true)
            }).unwrap();
            assert!(crate::platform::system::descriptor_is_nonblocking(&health.socket).unwrap(),
                "the admitted descriptor must actually have kernel nonblocking mode");
            let mut full = false;
            for _ in 0..4096 {
                match health.socket.send_to(&[0; 8192], &path) {
                    Ok(_) => {},
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => { full = true; break; },
                    Err(error) => panic!("generated health send failed: {error}"),
                }
            }
            assert!(full, "bounded generated receiver must reach backpressure");
            let before = Instant::now();
            // This is the actual pulse implementation, on the UI runtime.
            crate::rt::Ui::new().unwrap().block_on(async {
                health.pulse();
                tokio::task::yield_now().await;
            });
            assert!(health.last >= before, "failed send still advances the original pulse cadence");
            let mut bytes = [0; 8192];
            while receiver.recv(&mut bytes).is_ok() {}
            health.pulse();
            assert_eq!(receiver.recv(&mut bytes).unwrap_err().kind(), io::ErrorKind::WouldBlock,
                "an immediate retry must not defeat the one-second cadence");
            health.last = Instant::now() - Duration::from_secs(2);
            health.pulse();
            assert_eq!(receiver.recv(&mut bytes).unwrap(), 1);
            assert_eq!(bytes[0], role, "actual role remains the wire identity");
        }
        drop(receiver);
        std::fs::remove_dir_all(root).unwrap();
    }
}
