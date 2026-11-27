//! Generated queue durability/remote-sync boundaries; no owner state.
use super::*;

fn queued(lib: &Library) {
    *lib.queue_save.lock().unwrap() = Some(QueueSave {
        songs: vec![Song {
            id: "generated-queue".into(),
            ..Default::default()
        }],
        index: 0,
        position_ms: 7654,
    });
}

#[test]
fn async_save_keeps_local_durability_and_optional_remote_sync_off_the_executor() {
    use std::io::{BufRead, BufReader, Write};
    for status in [200, 503] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "generated".into();
        config.playback.sync_queue = true;
        // No sync/prefetch owner: only the explicitly invoked barrier can send.
        let (lib, _) = Library::create(
            Paths::new(generated_test_directory("queue-async")),
            &config,
            Arc::new(|_| {}),
        );
        lib.set_online(true);
        queued(&lib);
        let (entered, received) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + crate::library::TEST_LIVENESS;
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing generated savePlayQueue");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(
                line.contains("/rest/savePlayQueue")
                    && line.contains("position=7654")
                    && line.contains("id=generated-queue")
            );
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            entered.send(()).unwrap();
            wait.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let body = r#"{"subsonic-response":{"status":"ok"}}"#;
            write!(socket, "HTTP/1.1 {status} Generated\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        crate::rt::Ui::new().unwrap().block_on(async {
            let save = lib.save_now_async();
            tokio::pin!(save);
            tokio::select! {
                result = &mut save => panic!("save returned before generated remote reply: {result:?}"),
                result = tokio::time::timeout(crate::library::TEST_LIVENESS, received) => result.unwrap().unwrap(),
            }
            // Same UI executor keeps polling while remote headers are stalled.
            tokio::select! {
                result = &mut save => panic!("save did not retain optional request: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            assert_eq!(lib.db_run(|_blocking, db| db.get(&crate::rt::Blocking::dedicated_thread(), "queue")).await.map(|text| serde_json::from_str::<SavedQueue>(&text).unwrap().position_ms), Some(7654));
            release.send(()).unwrap();
            tokio::time::timeout(crate::library::TEST_LIVENESS, save).await.unwrap().unwrap();
        });
        server.join().unwrap();
        assert_eq!(lib.saved_queue().unwrap().2, 7654);
        assert!(lib.queue_save.lock().unwrap().is_none());
    }
}

#[test]
fn async_save_drains_prior_database_work_and_preserves_failed_queue_for_retry() {
    let (lib, _) = Library::create(
        Paths::new(generated_test_directory("queue-fence")),
        &Config::default(),
        Arc::new(|_| {}),
    );
    queued(&lib);
    lib.db().conn_for_sleep_fault(&crate::rt::Blocking::dedicated_thread());
    let (entered, started) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    lib.db_post(move |_, _| {
        entered.send(()).unwrap();
        wait.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    });
    started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    crate::rt::Ui::new().unwrap().block_on(async {
        let save = lib.save_now_async();
        tokio::pin!(save);
        tokio::select! {
            result = &mut save => panic!("barrier skipped queued database work: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        release.send(()).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(3), save)
            .await
            .unwrap()
            .is_err());
    });
    assert_eq!(
        lib.queue_save.lock().unwrap().as_ref().unwrap().position_ms,
        7654
    );
    lib.db().clear_sleep_fault(&crate::rt::Blocking::dedicated_thread());
    crate::rt::Ui::new()
        .unwrap()
        .block_on(lib.save_now_async())
        .unwrap();
    assert_eq!(lib.saved_queue().unwrap().2, 7654);
}
