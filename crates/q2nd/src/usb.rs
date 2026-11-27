#![forbid(unsafe_code)]
//! Request the installed OS's existing exclusive card handoff.
//!
//! Whether a request is open is the machine `q2_machines::usb` (proved
//! there; docs/state-machines.md); `pending` holds its state.
use q2_machines::usb::{self as fsm, Usb};
use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

#[derive(Clone)]
pub struct UsbDrive {
    udc: PathBuf,
    helper: PathBuf,
    log: PathBuf,
    pending: Arc<AtomicBool>,
    #[cfg(test)]
    availability_probe: Option<Arc<dyn Fn() + Send + Sync>>,
}
struct Pending(Arc<AtomicBool>);
impl Drop for Pending {
    fn drop(&mut self) {self.0.store(false,Ordering::Release);}
}
impl Default for UsbDrive {
    fn default() -> Self {
        Self { udc: "/sys/class/udc".into(), helper: "/sbin/q2-usb-storage".into(),
            log: "/run/log/usb-drive-request".into(), pending: Arc::new(AtomicBool::new(false)),
            #[cfg(test)] availability_probe: None }
    }
}
impl UsbDrive {
    /// A charger alone is not a computer connection. No idle polling thread.
    pub fn available(&self, _blocking: &crate::rt::Blocking) -> bool {
        #[cfg(test)] if let Some(probe) = &self.availability_probe { probe(); }
        use std::os::unix::fs::PermissionsExt;
        if !self.helper.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            return false;
        }
        std::fs::read_dir(&self.udc).is_ok_and(|entries| entries.take(8).flatten().any(|e|
            std::fs::read_to_string(e.path().join("state")).is_ok_and(|s| s.trim() == "configured")))
    }

    async fn available_async(&self) -> bool {
        let this = self.clone();
        crate::rt::jobs().run_isolated(move |blocking| this.available(blocking)).await.unwrap_or(false)
    }

    /// The helper owns quiescing, unmounting and export. It must survive the
    /// supervisor stopping this app, without keeping any of our card FDs open.
    pub fn busy(&self) -> bool { self.pending.load(Ordering::Acquire) }

    #[cfg(test)]
    pub fn start(&self, prepare: impl FnOnce() -> Result<(), &'static str> + Send + 'static,
        notify: impl FnOnce(&'static str) + Send + 'static) -> Result<(), &'static str> {
        self.start_async(move || async move {
            crate::rt::spawn_blocking(move |_| prepare()).await
                .unwrap_or(Err("USB drive could not prepare; try again"))
        }, notify)
    }

    /// Preparation may await remote work, but local card/player operations
    /// still use finite blocking jobs. The atomic claim covers both phases.
    pub fn start_async<F: std::future::Future<Output = Result<(), &'static str>> + Send + 'static>(
        &self, prepare: impl FnOnce() -> F + Send + 'static,
        notify: impl FnOnce(&'static str) + Send + 'static) -> Result<(), &'static str> {
        // Test-and-set reads the machine's state and claims the request in
        // one step; a refusal leaves the flag as it was (set).
        let state = if self.pending.swap(true, Ordering::AcqRel) { Usb::Opening } else { Usb::Idle };
        match fsm::transition(state, fsm::Event::Start { available: true }).1 {
            fsm::Effect::Spawn => {}
            fsm::Effect::RefuseBusy => return Err("USB drive is already opening"),
            _ => unreachable!("an available start either spawns or is busy"),
        }
        let this = self.clone();
        let pending=Pending(self.pending.clone());
        crate::rt::io().spawn(async move {
            let _pending=pending;
            if !this.available_async().await {
                notify("Connect USB to a computer first");
                return;
            }
            match crate::rt::io().spawn(async move { prepare().await }).await {
                Err(_) => notify("USB drive could not prepare; try again"),
                Ok(Err(message)) => notify(message),
                Ok(Ok(())) => if this.run_async().await.is_err() {
                    notify("USB drive could not open; reconnect USB and try again");
                },
            }
        });
        Ok(())
    }

    async fn run_async(&self) -> std::io::Result<()> {
        let this=self.clone();
        let mut child=crate::rt::spawn_blocking(move |blocking| {
            // Check/prepare/spawn in one turn: no extra queued gap or open
            // log descriptors are carried between preparation and spawn.
            let command = this.command(blocking)?;
            crate::platform::process::spawn_reactor_child(blocking, command)
        }).await.map_err(|_|std::io::Error::other("USB command preparation stopped"))??;
        // The helper must survive this app's orderly stop: deliberately no
        // kill-on-drop or parent-death signal. Its exit waits on the reactor.
        if !child.wait().await?.success() {return Err(std::io::Error::other("handoff failed"));}
        Ok(())
    }

    fn command(&self, blocking: &crate::rt::Blocking) -> std::io::Result<tokio::process::Command> {
        use std::process::{Command, Stdio};
        if !self.available(blocking) { return Err(std::io::Error::other("USB disconnected")); }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&self.log)?;
        let mut command = Command::new(&self.helper);
        command.arg("start").current_dir("/").env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        crate::platform::process::detached_handoff(&mut command);
        Ok(command.into())
    }

    #[cfg(test)]
    pub(crate) fn fixture(root: &std::path::Path) -> Self {
        Self { udc: root.join("udc"), helper: root.join("helper"), log: root.join("request.log"),
            pending: Arc::new(AtomicBool::new(false)), availability_probe: None }
    }
    #[cfg(test)]
    pub(crate) fn with_availability_probe(mut self, probe: impl Fn() + Send + Sync + 'static) -> Self {
        self.availability_probe = Some(Arc::new(probe)); self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn setup() -> (PathBuf, UsbDrive) {
        let base=std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(||PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/usb-drive"));
        let root = base.join(format!("{}-{}",
            std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(root.join("udc/test")).unwrap();
        std::fs::write(root.join("helper"), "#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$PWD\"\n").unwrap();
        std::fs::set_permissions(root.join("helper"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let drive = UsbDrive::fixture(&root);
        (root, drive)
    }
    #[test]
    fn only_configured_computer_and_executable_helper_offer_card_export() {
        let (root, drive) = setup();
        for state in ["", "not attached", "powered", "default", "addressed", "configured", "suspended"] {
            std::fs::write(root.join("udc/test/state"), state).unwrap();
            assert_eq!(drive.available(&crate::rt::Blocking::dedicated_thread()), state == "configured");
        }
        std::fs::write(root.join("udc/test/state"), "configured\n").unwrap();
        std::fs::set_permissions(root.join("helper"), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!drive.available(&crate::rt::Blocking::dedicated_thread()));
        let (notice, result) = std::sync::mpsc::channel();
        drive.start(|| panic!("unavailable USB cannot prepare"), move |message| { let _ = notice.send(message); }).unwrap();
        assert_eq!(result.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), "Connect USB to a computer first");
        assert!(!drive.log.exists());
    }
    #[test]
    fn owned_availability_preflight_keeps_ui_live_and_refuses_disconnect() {
        use std::time::Duration;
        let (root, drive) = setup();
        std::fs::write(root.join("udc/test/state"), "configured").unwrap();
        let (entered, observed) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(Some(wait));
        let drive = drive.with_availability_probe(move || {
            assert!(crate::rt::on_pool() && !crate::rt::on_executor());
            if let Some(wait) = wait.lock().unwrap().take() {
                let _ = entered.send(());
                let _ = wait.recv_timeout(Duration::from_secs(3));
            }
        });
        let prepared = Arc::new(AtomicBool::new(false));
        let worker_prepared = prepared.clone();
        let (notice, result) = std::sync::mpsc::channel();
        crate::rt::Ui::new().unwrap().block_on(async {
            drive.start_async(move || async move {
                worker_prepared.store(true, Ordering::Release); Ok(())
            }, move |message| { let _ = notice.send(message); }).unwrap();
        });
        let reached = observed.recv_timeout(Duration::from_secs(3));
        let busy = drive.busy();
        let duplicate = drive.start_async(|| async { Ok(()) }, |_| {});
        crate::rt::Ui::new().unwrap().block_on(async { tokio::task::yield_now().await; });
        std::fs::write(root.join("udc/test/state"), "not attached").unwrap();
        let _ = release.send(());
        reached.unwrap();
        assert!(busy && duplicate.is_err(), "the accepted preflight owns the busy claim");
        assert_eq!(result.recv_timeout(Duration::from_secs(3)).unwrap(), "Connect USB to a computer first");
        crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while drive.busy() { tokio::task::yield_now().await; }
            }).await.unwrap();
        });
        assert!(!prepared.load(Ordering::Acquire));
        assert!(!drive.log.exists());
    }

    #[test]
    fn held_usb_spawn_rechecks_disconnect_and_owned_launch_survives_caller_drop() {
        use std::time::Duration;
        for disconnected in [false, true] {
            let (root, drive) = setup();
            std::fs::write(root.join("udc/test/state"), "configured").unwrap();
            std::fs::write(&drive.helper, "#!/bin/sh\nprintf '%s\\n' \"$$\"\n").unwrap();
            let log = drive.log.clone();
            let pending = drive.pending.clone();
            let (entered, observed) = std::sync::mpsc::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let wait = std::sync::Mutex::new(Some(wait));
            let calls = std::sync::atomic::AtomicUsize::new(0);
            let drive = drive.with_availability_probe(move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    assert!(crate::rt::on_pool() && !crate::rt::on_executor());
                    let _ = entered.send(());
                    if let Some(wait) = wait.lock().unwrap().take() { let _ = wait.recv_timeout(Duration::from_secs(5)); }
                }
            });
            let (notice, refusal) = std::sync::mpsc::channel();
            drive.start_async(|| async { Ok(()) }, move |message| { let _ = notice.send(message); }).unwrap();
            drop(drive); // Accepted handoff, unlike a menu probe, owns completion.
            let reached = observed.recv_timeout(Duration::from_secs(3));
            let ui = crate::rt::Ui::new().unwrap();
            let live = ui.block_on(async {
                tokio::time::timeout(Duration::from_secs(1), crate::rt::io().spawn(async { 19 })).await
            });
            if disconnected { std::fs::write(root.join("udc/test/state"), "not attached").unwrap(); }
            let _ = release.send(());
            ui.block_on(async {
                tokio::time::timeout(Duration::from_secs(3), async {
                    while pending.load(Ordering::Acquire) { tokio::task::yield_now().await; }
                }).await.unwrap();
            });
            reached.unwrap(); assert_eq!(live.unwrap().unwrap(), 19);
            if disconnected {
                assert!(refusal.recv_timeout(Duration::from_secs(3)).unwrap().contains("could not open"));
                assert!(!log.exists(), "disconnect at spawn admission must not launch a helper");
            } else {
                let pid: i32 = std::fs::read_to_string(log).unwrap().trim().parse().unwrap();
                assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
            }
        }
    }

    #[tokio::test]
    async fn handoff_is_detached_and_does_not_inherit_private_descriptors_or_environment() {
        use std::os::fd::AsRawFd;
        let (root, drive) = setup();
        std::fs::write(root.join("udc/test/state"), "configured").unwrap();
        let card = std::fs::File::create(root.join("generated-card" )).unwrap();
        let fd = card.as_raw_fd();
        crate::platform::process::generated::make_inheritable(&card).unwrap();
        std::fs::write(&drive.helper, format!("#!/bin/sh\n[ ! -e /proc/self/fd/{fd} ] || exit 11\n[ -z \"$CARGO_MANIFEST_DIR\" ] || exit 12\n[ \"$PWD\" = / ] || exit 13\n[ \"$1\" = start ] || exit 14\n[ \"$#\" = 1 ] || exit 15\nprintf 'handoff-ok\\n'\n")).unwrap();
        drive.run_async().await.unwrap();
        assert_eq!(std::fs::read_to_string(&drive.log).unwrap(), "handoff-ok\n");
        // A stale menu cannot launch after disconnect.
        std::fs::write(root.join("udc/test/state"), "not attached").unwrap();
        assert!(drive.run_async().await.is_err());
    }
    #[test]
    fn checkpoint_failure_prevents_helper_and_busy_requests_cannot_overlap() {
        let (root, drive) = setup();
        std::fs::write(root.join("udc/test/state"), "configured").unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (notice_tx, notice_rx) = std::sync::mpsc::channel();
        drive.start(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            Err("Listening position could not be saved")
        }, move |message| notice_tx.send(message).unwrap()).unwrap();
        entered_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert!(drive.busy());
        assert!(drive.start(|| panic!("duplicate preflight"), |_| panic!("duplicate helper")).is_err());
        release_tx.send(()).unwrap();
        assert_eq!(notice_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), "Listening position could not be saved");
        // Notification happens before the worker clears pending, so join via a
        // bounded predicate rather than assume scheduler ordering.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while drive.busy() && std::time::Instant::now() < end { std::thread::yield_now(); }
        assert!(!drive.busy());
        assert!(!drive.log.exists(), "failed preflight never launches helper");
    }

    #[test]
    fn failed_background_handoff_reports_failure_and_can_retry() {
        let (root, drive) = setup();
        std::fs::write(root.join("udc/test/state"), "configured").unwrap();
        std::fs::write(&drive.helper, "#!/bin/sh\nexit 1\n").unwrap();
        for _ in 0..2 {
            let (tx, rx) = std::sync::mpsc::channel();
            drive.start(|| Ok(()), move |m| tx.send(m).unwrap()).unwrap();
            assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap().contains("could not open"));
            let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while drive.busy() && std::time::Instant::now() < end { std::thread::yield_now(); }
            assert!(!drive.busy());
        }
    }

    #[test]
    #[cfg_attr(panic = "abort", ignore = "recovers from a panic; a panic = \"abort\" build, as on the Q2, aborts")]
    fn panicking_preparation_releases_busy_and_does_not_launch_helper() {
        let (root,drive)=setup();
        std::fs::write(root.join("udc/test/state"),"configured").unwrap();
        let (tx,rx)=std::sync::mpsc::channel();
        drive.start(||panic!("generated USB preparation failure"),move |message|tx.send(message).unwrap()).unwrap();
        assert!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap().contains("could not prepare"));
        let end=std::time::Instant::now()+std::time::Duration::from_secs(2);
        while drive.busy() && std::time::Instant::now()<end {std::thread::yield_now();}
        assert!(!drive.busy());
        assert!(!drive.log.exists());
        // Even a never-polled owner clears its claim when destroyed.
        let pending=drive.pending.clone();pending.store(true,Ordering::Release);
        let guard=Pending(pending.clone());
        let unpolled=async move {let _guard=guard;std::future::pending::<()>().await;};
        drop(unpolled);assert!(!pending.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn async_preparation_keeps_claim_and_yields_without_launching_after_failure() {
        let (root, drive) = setup();
        std::fs::write(root.join("udc/test/state"), "configured").unwrap();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let (notice, result) = tokio::sync::oneshot::channel();
        drive.start_async(move || async move {
            entered.send(()).unwrap();
            wait.await.unwrap();
            Err("Generated queue persistence failure")
        }, move |message| { notice.send(message).unwrap(); }).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), started).await.unwrap().unwrap();
        assert!(drive.busy());
        assert!(drive.start_async(|| async { Ok(()) }, |_| {}).is_err());
        tokio::time::timeout(std::time::Duration::from_secs(3), crate::rt::spawn_blocking(|_| 42)).await.unwrap().unwrap();
        assert!(!drive.log.exists());
        release.send(()).unwrap();
        assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(3), result).await.unwrap().unwrap(), "Generated queue persistence failure");
        tokio::time::timeout(std::time::Duration::from_secs(3), async { while drive.busy() { tokio::task::yield_now().await; } }).await.unwrap();
        assert!(!drive.log.exists());
    }

    #[tokio::test]
    async fn helper_exit_wait_leaves_io_and_blocking_workers_available() {
        let (root,drive)=setup();
        std::fs::write(root.join("udc/test/state"),"configured").unwrap();
        std::fs::write(&drive.helper,"#!/bin/sh\nprintf 'started\\n'\nsleep 1\nprintf 'finished\\n'\n").unwrap();
        let log=drive.log.clone();
        let task=crate::rt::io().spawn(async move {drive.run_async().await});
        tokio::time::timeout(std::time::Duration::from_secs(3),async {
            while !std::fs::read_to_string(&log).is_ok_and(|s|s.contains("started")) {tokio::task::yield_now().await;}
        }).await.unwrap();
        assert!(!task.is_finished());
        let tick=crate::rt::io().spawn(async {tokio::time::sleep(std::time::Duration::from_millis(1)).await;});
        tokio::time::timeout(std::time::Duration::from_millis(500),tick).await.unwrap().unwrap();
        let workers:Vec<_>=(0..crate::rt::BLOCKING_THREADS).map(|_|crate::rt::spawn_blocking(|_|42)).collect();
        for worker in workers {assert_eq!(tokio::time::timeout(std::time::Duration::from_millis(500),worker).await.unwrap().unwrap(),42);}
        tokio::time::timeout(std::time::Duration::from_secs(3),task).await.unwrap().unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(log).unwrap(),"started\nfinished\n");
    }
}
