//! Real subprocess boundaries keep process-lifetime sampler state independent.
//! Every input below is generated; no installed paths, config or hardware.
use super::*;
const PRIOR_BOOT: &str = "00000000-0000-4000-8000-000000000001";
const PRIOR_RUN: &str = "00000000-0000-4000-8000-000000000002";
const CRASH_ID: &str = "00000000-0000-4000-8000-0000000000a0";

fn fixture() -> PathBuf {
    let root = crate::library::generated_test_directory("telemetry-import");
    std::fs::create_dir_all(&root).unwrap();
    root
}
fn settings(endpoint: String) -> Telemetry {
    Telemetry {
        enabled: true,
        endpoint: Some(endpoint),
        token: Some("a".repeat(64)),
        ..Telemetry::default()
    }
}
fn report() -> Report {
    Report {
        id: uuid(),
        build: "a".repeat(64),
        boot: PRIOR_BOOT.into(),
        runtime: "process".into(),
        kind: "crash".into(),
        thread: 0,
        task: 0,
        poll: 0,
        monotonic_us: 0,
        elapsed_us: 0,
        dropped_reports: 0,
        frames: vec![],
        stack_status: "not_requested".into(),
        context: Context::default(),
        process: ProcessIdentity {
            pid: 42,
            start_ticks: 123,
        },
        event: Some(crash::Event::AppPanic {}),
    }
}
fn dead_journal(directory: &Path) -> PathBuf {
    let crash_dir = directory.join("crashes");
    std::fs::create_dir_all(&crash_dir).unwrap();
    let header = crash::Header {
        build: "b".repeat(64),
        boot: PRIOR_BOOT.into(),
        process: ProcessIdentity {
            pid: 43,
            start_ticks: 124,
        },
        context: Context {
            stream: Stream::AutomatedTest,
            labels: vec!["automated-test".into()],
            run_id: Some(PRIOR_RUN.into()),
            ..Context::default()
        },
    };
    let header = serde_json::to_vec(&header).unwrap();
    let mut bytes = [0u8; 4096 + 32];
    bytes[..8].copy_from_slice(b"Q2CRSH01");
    bytes[8..12].copy_from_slice(&(header.len() as u32).to_le_bytes());
    bytes[12..12 + header.len()].copy_from_slice(&header);
    bytes[4096..4100].copy_from_slice(b"Q2EV");
    bytes[4100] = 1;
    bytes[4108..4112].copy_from_slice(b"DONE");
    let path = crash_dir.join(format!("{CRASH_ID}.crash"));
    std::fs::write(&path, bytes).unwrap();
    path
}
fn child(mode: &str) {
    let root = fixture();
    let executable = root.join("stripped-child");
    let status = std::process::Command::new("strip")
        .arg("--strip-debug")
        .arg("-o")
        .arg(&executable)
        .arg(std::env::current_exe().unwrap())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "could not prepare generated stripped child"
    );
    let mut process = std::process::Command::new(&executable)
        .args([
            "--exact",
            "diagnostics::import_tests::child_fixture",
            "--nocapture",
        ])
        .env("Q2_GENERATED_IMPORT_CHILD", mode)
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(root.join("child.stdout")).unwrap())
        .stderr(std::fs::File::create(root.join("child.stderr")).unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(
                status.success(),
                "generated import child failed; retained evidence at {}",
                root.display()
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            panic!("generated import child timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn full_outbox_does_not_disable_delivery_and_ack_retries_original_context_import() {
    child("full");
}
#[test]
fn opt_out_cleanup_failure_still_disables_process_exit_capture_authorization() {
    child("off");
}
#[test]
fn interrupted_owned_pending_import_is_recovered_without_duplicate_or_relabel() {
    child("pending");
}

#[test]
fn child_fixture() {
    let Some(mode) = std::env::var_os("Q2_GENERATED_IMPORT_CHILD") else {
        return;
    };
    let root = fixture();
    let blocking = Blocking::dedicated_thread();
    let outbox = root.join("outbox-v2");
    std::fs::create_dir_all(&outbox).unwrap();
    if mode == "off" {
        reconfigure(&blocking, &root, &settings("http://127.0.0.1:8766".into())).unwrap();
        let state = STATE.get().unwrap();
        let denied = outbox.join(format!("{}.json", uuid()));
        // A generated directory with a report name is rejected by cleanup.
        std::fs::create_dir(&denied).unwrap();
        assert!(reconfigure(&blocking, &root, &Telemetry::default()).is_err());
        assert!(!ACTIVE.load(Ordering::Acquire));
        assert!(!state.capture.lock().unwrap().enabled);
        let prepared = supervisor::prepare_exit(
            &blocking,
            &root,
            &state.boot,
            state.process.pid,
            state.process.start_ticks,
            139,
            supervisor::Reason::UnexpectedExit,
        )
        .unwrap();
        assert!(matches!(
            prepared,
            supervisor::Preparation::Skipped(supervisor::SkipReason::Disabled)
        ));
        let marker: serde_json::Value = serde_json::from_slice(
            &bounded(&blocking, &root.join("capture-context.json"), 4096).unwrap(),
        )
        .unwrap();
        assert_eq!(marker["enabled"], false);
        std::fs::remove_dir(denied).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        return;
    }
    let journal = dead_journal(&root);
    if mode == "pending" {
        let interrupted = outbox.join(format!("{CRASH_ID}.pending"));
        std::fs::write(&interrupted, b"incomplete generated imported record").unwrap();
        reconfigure(&blocking, &root, &settings("http://127.0.0.1:8766".into())).unwrap();
        assert!(ACTIVE.load(Ordering::Acquire));
        assert!(!journal.exists());
        assert!(!interrupted.exists());
        let rows = pending(&blocking, &outbox).unwrap();
        assert_eq!(rows.len(), 1);
        let first = bounded(&blocking, &rows[0], REPORT_BYTES).unwrap();
        let imported: Report = serde_json::from_slice(&first).unwrap();
        assert_eq!(imported.id, CRASH_ID);
        assert_eq!(imported.context.stream, Stream::AutomatedTest);
        assert_eq!(imported.context.run_id.as_deref(), Some(PRIOR_RUN));
        let _capture = STATE.get().unwrap().capture.lock().unwrap();
        import_crashes(&blocking, STATE.get().unwrap()).unwrap();
        assert_eq!(pending(&blocking, &outbox).unwrap().len(), 1);
        assert_eq!(bounded(&blocking, &rows[0], REPORT_BYTES).unwrap(), first);
        std::fs::remove_dir_all(root).unwrap();
        return;
    }
    assert_eq!(mode, "full");
    for _ in 0..LIMIT {
        retain(&blocking, &outbox, &report()).unwrap();
    }
    assert_eq!(pending(&blocking, &outbox).unwrap().len(), LIMIT);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(18);
        let mut received = std::collections::HashSet::new();
        let mut saw_prior = false;
        while received.len() < LIMIT + 1 {
            assert!(
                Instant::now() < deadline,
                "generated server did not receive recovered backlog"
            );
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("generated accept failed: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() < 16 * 1024);
            }
            let header = String::from_utf8(header).unwrap().to_ascii_lowercase();
            let length = header
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert!(length <= 64 * 1024);
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["schema"], 2);
            let selected = body["stream"].as_str().unwrap();
            assert!(header.starts_with(&format!("post /v2/{selected}/reports ")));
            let reports = body["reports"].as_array().unwrap();
            assert!(!reports.is_empty() && reports.len() <= 16);
            let mut ids = Vec::new();
            for report in reports {
                let id = report["id"].as_str().unwrap().to_owned();
                assert!(received.insert(id.clone()));
                assert_eq!(report["context"]["stream"], selected);
                if id == CRASH_ID {
                    assert_eq!(selected, "automated-test");
                    assert_eq!(report["context"]["run_id"], PRIOR_RUN);
                    assert_eq!(report["boot"], PRIOR_BOOT);
                    assert_eq!(report["process"]["pid"], 43);
                    assert_eq!(report["event"], serde_json::json!({"type":"app-panic"}));
                    saw_prior = true;
                }
                ids.push(id);
            }
            let body = serde_json::to_vec(
                &serde_json::json!({"schema":2,"stream":selected,"accepted":ids}),
            )
            .unwrap();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
        assert!(saw_prior);
    });
    reconfigure(&blocking, &root, &settings(format!("http://{address}"))).unwrap();
    let state = STATE.get().unwrap().clone();
    assert!(ACTIVE.load(Ordering::Acquire));
    assert!(state.capture.lock().unwrap().enabled);
    assert!(
        journal.exists(),
        "full outbox must retain unimported journal"
    );
    let online = AtomicBool::new(true);
    let runtime = crate::rt::Ui::new().unwrap();
    runtime.block_on(async {
        let agent = subsonic::http::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(3))
            .build();
        for _ in 0..40 {
            if pending(&blocking, &outbox).unwrap().is_empty() {
                break;
            }
            deliver(&agent, state.clone(), &online).await.unwrap();
        }
    });
    assert!(pending(&blocking, &outbox).unwrap().is_empty());
    assert!(!journal.exists());
    server.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
