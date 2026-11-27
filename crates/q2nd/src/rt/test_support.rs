//! Deliberate global-pool starvation runs in a fresh test process. Ordinary
//! range fixtures may block the same pool, so a mutex between starvation tests
//! alone cannot provide isolation from the rest of the concurrent suite.
use std::sync::{mpsc, Mutex};
use std::time::Duration;

static GENERAL_POOL_SATURATION: Mutex<()> = Mutex::new(());

pub(crate) fn with_saturated_general_pool<T>(
    admission_limit: Duration,
    check: impl FnOnce() -> T,
) -> T {
    let _exclusive = GENERAL_POOL_SATURATION
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (entered, started) = mpsc::channel();
    let mut release = Vec::new();
    let mut jobs = Vec::new();
    // Admission/assertion panic must still release AND join every blocker
    // before a second saturation test can claim the same finite capacity.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for _ in 0..super::BLOCKING_THREADS {
            let (send, gate) = mpsc::channel::<()>();
            release.push(send);
            let entered = entered.clone();
            jobs.push(super::spawn_blocking(move |_| {
                let _ = entered.send(());
                let _ = gate.recv();
            }));
        }
        for _ in 0..super::BLOCKING_THREADS {
            started.recv_timeout(admission_limit).unwrap();
        }
        check()
    }));
    drop(release);
    // Both original fixtures used a three-second drain bound. Joining on this
    // ordinary test thread never occupies a worker needed by the I/O runtime.
    let joined = super::Ui::new().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut failed = false;
            for job in jobs {
                failed |= job.await.is_err();
            }
            !failed
        })
        .await
    });
    match outcome {
        Ok(value) => {
            assert!(
                matches!(joined, Ok(true)),
                "synthetic blocking workers did not join"
            );
            value
        }
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Run a complete starvation fixture in a fresh process, including retirement
/// observations. Never forward nested libtest lines into the outer inventory.
/// Each caller supplies a distinct guard: only that exact selected test recurses.
pub(crate) fn isolated_pool_test(test: &str, guard: &str, body: impl FnOnce()) {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;
    if let Some(selected) = std::env::var_os(guard) {
        assert_eq!(selected, test, "incorrect isolated-test recursion guard");
        body();
        println!("\nQ2_POOL_CHILD_DONE 1");
        return;
    }
    let root = std::env::var_os("Q2_GENERATED_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"))
        .join("test-pool-isolation");
    std::fs::create_dir_all(&root).unwrap();
    let out = root.join(format!("{}-{}", std::process::id(), fastrand::u64(..)));
    std::fs::create_dir(&out).unwrap();
    let stdout = std::fs::File::create(out.join("stdout.log")).unwrap();
    let stderr = std::fs::File::create(out.join("stderr.log")).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test, "--nocapture", "--test-threads=1"]);
    if cfg!(any(target_arch = "mips", target_arch = "mips64")) {
        command.args(["-Z", "unstable-options", "--force-run-in-process"]);
    }
    command
        .env(guard, test)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            // kill is harmless after try_wait has reaped this exact child.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    // Also retire the child if the target's panic-abort parent disappears.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }
    let mut child = OwnedChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {}
            Ok(None) => {
                break Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "isolated pool test exceeded 60s",
                ))
            }
            Err(error) => break Err(error),
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Explicitly kill/join BEFORE asserting: MIPS uses panic=abort, not Drop.
    drop(child);
    let status = status.unwrap_or_else(|error| panic!("{error}; logs: {}", out.display()));
    let mut text = String::new();
    std::fs::File::open(out.join("stdout.log"))
        .unwrap()
        .take(65_537)
        .read_to_string(&mut text)
        .unwrap();
    assert!(
        text.len() <= 65_536,
        "unexpected child output size; logs: {}",
        out.display()
    );
    let mut complete = 0;
    for line in text.lines() {
        let fields: Vec<_> = line
            .split_whitespace()
            .skip_while(|field| !field.starts_with("Q2_"))
            .collect();
        match fields.as_slice() {
            ["Q2_POOL_CHILD_DONE", "1"] => complete += 1,
            ["Q2_USB_SCAN_COST", first, restarted]
                if first
                    .strip_prefix("first_event_us=")
                    .is_some_and(|v| v.parse::<u128>().is_ok())
                    && restarted
                        .strip_prefix("restarted_event_us=")
                        .is_some_and(|v| v.parse::<u128>().is_ok()) =>
            {
                println!("Q2_USB_SCAN_COST {first} {restarted}");
            }
            ["Q2_USB_SCAN_RETIRE", "joined=1", exiting, absent, state, flags]
                if exiting.strip_prefix("exiting=").is_some_and(|v| matches!(v,"0"|"1"))
                    && absent.strip_prefix("absent=").is_some_and(|v| matches!(v,"0"|"1"))
                    && state.strip_prefix("state=").is_some_and(|v| v.parse::<u8>().is_ok())
                    && flags.strip_prefix("flags=").is_some_and(|v| v.parse::<u64>().is_ok()) =>
            {
                println!("Q2_USB_SCAN_RETIRE joined=1 {exiting} {absent} {state} {flags}");
            }
            _ => {}
        }
    }
    assert!(
        status.success(),
        "isolated pool child failed, code {:?}; logs: {}",
        status.code(),
        out.display()
    );
    assert_eq!(
        complete,
        1,
        "isolated test did not complete exactly once; logs: {}",
        out.display()
    );
    assert!(
        text.lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "isolated exact selector did not run one passing test; logs: {}",
        out.display()
    );
}
