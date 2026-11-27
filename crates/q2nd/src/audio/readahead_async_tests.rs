//! Typed reader controls: generated files and loopback HTTP only.
use crate::audio::interrupt::Interrupt;
use super::*;
use crate::audio::source::{Progress, ProgressiveReader};
use std::io::Write;
use std::sync::mpsc;

fn generated(bytes: &[u8]) -> std::path::PathBuf {
    let root = std::env::var_os("Q2_GENERATED_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"))
        .join("test-async-readahead");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("{}-{}", std::process::id(), fastrand::u64(..)));
    std::fs::write(&path, bytes).unwrap();
    path
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < until, "owned operation did not complete");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn progressive(path: &std::path::Path, progress: Arc<Progress>) -> ReadAhead {
    let stop = Arc::new(Interrupt::new(false));
    let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), path, progress.clone(), stop.clone()).unwrap();
    ReadAhead::progressive(source, progress, Arc::new(Interrupt::new(false)), stop).unwrap()
}

#[test]
fn fragmented_progress_wakes_all_readers_and_retains_bytes_before_failure() {
        crate::audio::callback::test_scope(|| {
    let path = generated(&[]);
    let progress = Progress::new();
    let mut a = progressive(&path, progress.clone());
    let mut b = progressive(&path, progress.clone());
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    for byte in 0..64u8 {
        writer.write_all(&[byte]).unwrap();
        progress.advance(u64::from(byte) + 1);
        if byte == 32 {
            progress.set_total(Some(64));
        }
        for reader in [&mut a, &mut b] {
            let mut out = [0];
            reader.read_exact(&mut out).unwrap();
            assert_eq!(out, [byte]);
        }
    }
    // A notification before first polling must work as well as registered waits.
    let mut c = progressive(&path, progress.clone());
    progress.fail("generated terminal".into());
    let mut prefix = [0; 64];
    c.read_exact(&mut prefix).unwrap();
    assert_eq!(prefix, std::array::from_fn(|i| i as u8));
    for reader in [&mut a, &mut b, &mut c] {
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().to_string(),
            "generated terminal"
        );
    }
    drop((a, b, c));
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn obsolete_stalled_card_read_cannot_delay_a_new_reader_or_publish_after_seek() {
        crate::audio::callback::test_scope(|| {
    let bytes: Vec<u8> = (0..CAP * 3).map(|n| (n % 251) as u8).collect();
    let path = generated(&bytes);
    let progress = Progress::new();
    progress.advance(bytes.len() as u64);
    progress.finish();
    let mut old = progressive(&path, progress);
    super::progressive_tests::full(&old);
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    let entered_tx = Mutex::new(Some(entered_tx));
    let release = Mutex::new(release);
    *old.shared.card_hook.0.lock().unwrap() = Some(Arc::new(move |at| {
        if at >= CAP as u64 {
            if let Some(tx) = entered_tx.lock().unwrap().take() {
                tx.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
        }
    }));
    old.read_exact(&mut vec![0; CAP / 2 + 1]).unwrap();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    old.seek(SeekFrom::Start(17)).unwrap();
    let mut new =
        ReadAhead::source(&crate::rt::Blocking::dedicated_thread(), File::open(&path).unwrap(), Arc::new(Interrupt::new(false))).unwrap();
    let (done_tx, done) = mpsc::channel();
    let first = std::thread::spawn(move || { crate::audio::callback::test_scope(|| {
        let mut out = [0; 128];
        let result = new.read_exact(&mut out);
        done_tx.send((result, out)).unwrap();
     }) });
    let independent = done.recv_timeout(Duration::from_secs(2));
    release_tx.send(()).unwrap();
    first.join().unwrap();
    let (result, out) = independent.expect("new reader queued behind obsolete card operation");
    result.unwrap();
    assert_eq!(&out, &bytes[..128]);
    let mut after_seek = [0; 128];
    old.read_exact(&mut after_seek).unwrap();
    assert_eq!(
        &after_seek,
        &bytes[17..145],
        "stale card bytes survived the new seek"
    );
    drop(old);
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn drop_frees_buffer_but_joins_owned_card_work_before_retirement() {
        crate::audio::callback::test_scope(|| {
    let path = generated(&vec![9; CAP * 2]);
    let progress = Progress::new();
    progress.advance((CAP * 2) as u64);
    progress.finish();
    let mut reader = progressive(&path, progress);
    super::progressive_tests::full(&reader);
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release) = mpsc::channel();
    let entered_tx = Mutex::new(Some(entered_tx));
    let release = Mutex::new(release);
    *reader.shared.card_hook.0.lock().unwrap() = Some(Arc::new(move |_| {
        if let Some(tx) = entered_tx.lock().unwrap().take() {
            tx.send(()).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
    }));
    reader.read_exact(&mut vec![0; CAP / 2 + 1]).unwrap();
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let weak = Arc::downgrade(&reader.shared);
    drop(reader);
    let retained = weak.upgrade().expect("owner must join its finite read");
    assert!(retained.buffer.lock().unwrap().closed);
    assert_eq!(retained.buffer.lock().unwrap().data.capacity(), 0);
    drop(retained);
    release_tx.send(()).unwrap();
    wait_until(|| weak.upgrade().is_none());
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn local_card_lane_progresses_with_general_blocking_pool_saturated() {
        crate::audio::callback::test_scope(|| {
    crate::rt::test_support::isolated_pool_test(
            "audio::readahead::async_tests::local_card_lane_progresses_with_general_blocking_pool_saturated",
            "Q2_READER_POOL_ISOLATED_TEST",
        || {
            let (result, consumer, path) =
                crate::rt::test_support::with_saturated_general_pool(Duration::from_secs(5), || {
                    let path = generated(b"independent card bytes");
                    let mut reader =
                        ReadAhead::source(&crate::rt::Blocking::dedicated_thread(), File::open(&path).unwrap(), Arc::new(Interrupt::new(false)))
                            .unwrap();
                    let (done_tx, done) = mpsc::channel();
                    let consumer = std::thread::spawn(move || { crate::audio::callback::test_scope(|| {
                        let mut out = Vec::new();
                        let result = reader.read_to_end(&mut out);
                        done_tx.send((result, out)).unwrap();
                     }) });
                    let result = done.recv_timeout(Duration::from_secs(2));
                    (result, consumer, path)
                });
            // Even a wrong-pool regression can finish once the synthetic blockers
            // have joined; never join its consumer while retaining those blockers.
            consumer.join().unwrap();
            let (result, out) = result.expect("card read was queued on the saturated general pool");
            result.unwrap();
            assert_eq!(out, b"independent card bytes");
            std::fs::remove_file(path).unwrap();
        },
        );
        })
    }

fn headers(socket: &mut std::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 8192);
    }
    String::from_utf8(bytes).unwrap()
}
#[test]
fn stalled_http_seek_is_cancelled_by_new_seek_and_drop_without_stale_bytes() {
        crate::audio::callback::test_scope(|| {
    for close in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let (entered_tx, entered) = mpsc::channel();
        let (eof_tx, eof) = mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            first
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            assert!(headers(&mut first)
                .to_lowercase()
                .contains("range: bytes=100-262243"));
            entered_tx.send(()).unwrap();
            let mut out = [0];
            assert_eq!(first.read(&mut out).unwrap(), 0);
            eof_tx.send(()).unwrap();
            if !close {
                let (mut second, _) = listener.accept().unwrap();
                second
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                assert!(headers(&mut second)
                    .to_lowercase()
                    .contains("range: bytes=999-263142"));
                second.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 999-1002/1003\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlast").unwrap();
            }
          });
        let client = subsonic::Client::new(
            subsonic::Credentials {
                server,
                user: "generated".into(),
                auth: subsonic::Auth::Password("generated".into()),
            },
            "generated-read-ahead",
        );
        let request = client.stream_request("generated-song", &Default::default());
        let path = generated(&[]);
        let progress = Progress::new();
        progress.set_total(Some(1003));
        let stop = Arc::new(Interrupt::new(false));
        let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), stop.clone())
            .unwrap()
            .with_async_ranges(Arc::new(move |at, len| {
                let request = client.read_range_async(&request, at, len);
                Box::pin(async move {
                    request
                        .await
                        .map_err(|_| io::Error::other("generated range failed"))
                })
            }));
        let mut reader = ReadAhead::progressive(
            source,
            progress.clone(),
            Arc::new(Interrupt::new(false)),
            stop,
        )
        .unwrap();
        reader.seek(SeekFrom::Start(100)).unwrap();
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let weak = Arc::downgrade(&reader.shared);
        if close {
            drop(reader);
        } else {
            reader.seek(SeekFrom::Start(999)).unwrap();
            let mut bytes = [0; 4];
            reader.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"last");
            assert_eq!(reader.read(&mut bytes).unwrap(), 0);
            drop(reader);
        }
        eof.recv_timeout(Duration::from_secs(2)).unwrap();
        server_thread.join().unwrap();
        wait_until(|| weak.upgrade().is_none());
        assert_eq!(
            progress.snapshot().written,
            0,
            "range reads never publish sequential cache holes"
        );
        std::fs::remove_file(path).unwrap();
    }
        })
    }

#[test]
fn unsupported_range_falls_back_to_progress_and_is_requested_only_once() {
        crate::audio::callback::test_scope(|| {
    let path = generated(b"abcdefgh");
    let progress = Progress::new();
    let stop = Arc::new(Interrupt::new(false));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), stop.clone())
        .unwrap()
        .with_async_ranges(Arc::new(move |_, _| {
            observed.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(None) })
        }));
    let mut reader = ReadAhead::progressive(
        source,
        progress.clone(),
        Arc::new(Interrupt::new(false)),
        stop,
    )
    .unwrap();
    reader.seek(SeekFrom::Start(4)).unwrap();
    wait_until(|| calls.load(Ordering::Relaxed) == 1);
    assert!(reader.shared.buffer.lock().unwrap().data.is_empty());
    progress.advance(8);
    progress.finish();
    let mut tail = Vec::new();
    reader.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, b"efgh");
    reader.seek(SeekFrom::Start(2)).unwrap();
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"cdefgh");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    drop(reader);
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn typed_file_reads_match_seeded_seek_read_oracle() {
        crate::audio::callback::test_scope(|| {
    let bytes: Vec<_> = (0..CAP + 173).map(|n| (n % 251) as u8).collect();
    let path = generated(&bytes);
    let mut reader =
        ReadAhead::source(&crate::rt::Blocking::dedicated_thread(), File::open(&path).unwrap(), Arc::new(Interrupt::new(false))).unwrap();
    let mut oracle = std::io::Cursor::new(&bytes);
    let mut rng = fastrand::Rng::with_seed(0x51ee);
    for _ in 0..200 {
        let offset = rng.usize(..bytes.len() + 100) as u64;
        assert_eq!(
            reader.seek(SeekFrom::Start(offset)).unwrap(),
            oracle.seek(SeekFrom::Start(offset)).unwrap()
        );
        let count = rng.usize(1..4097);
        let mut actual = vec![0; count];
        let mut expected = vec![0; count];
        // Read implementations may legally split at different chunk boundaries.
        let n = oracle.read(&mut expected).unwrap();
        reader.read_exact(&mut actual[..n]).unwrap();
        assert_eq!(&actual[..n], &expected[..n]);
        if n < count {
            assert_eq!(reader.read(&mut actual).unwrap(), 0);
        }
    }
    drop(reader);
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn unpolled_owner_exit_completes_a_pending_seek_instead_of_hanging() {
        crate::audio::callback::test_scope(|| {
    let shared = Arc::new(Shared {
        buffer: Mutex::new(Buf {
            data: VecDeque::new(),
            start: 0,
            eof: false,
            error: None,
            seek: Some(9),
            closed: false,
        }),
        ready: Arc::default(),
        changed: tokio::sync::Notify::new(),
        card_hook: CardHook::default(),
    });
    let path = generated(b"generated");
    let task = worker(
        Input::File(crate::audio::source::ReadAheadFile::new(File::open(&path).unwrap())),
        shared.clone(),
        CardLane::new().unwrap(),
    );
    drop(task);
    let mut reader = ReadAhead {
        shared,
        pos: 9,
        len: Length::Fixed(9),
        interrupt: Arc::new(Interrupt::new(false)),
        stop_io: None,
    };
    assert_eq!(
        reader.read(&mut [0]).unwrap_err().to_string(),
        "Read-ahead owner stopped"
    );
    drop(reader);
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn progress_wait_never_enters_card_lane_before_published_bytes() {
        crate::audio::callback::test_scope(|| {
    let path = generated(b"published");
    let progress = Progress::new();
    let mut reader = progressive(&path, progress.clone());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    *reader.shared.card_hook.0.lock().unwrap() = Some(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
    }));
    // An io task scheduled after the owner proves that the pending read has
    // yielded; a finite worker can simultaneously execute unrelated work.
    let (tx, rx) = mpsc::channel();
    crate::rt::io().spawn(async move {
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let (tx, rx) = mpsc::channel();
    crate::rt::spawn_blocking(move |_| tx.send(()).unwrap());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    progress.advance(9);
    progress.finish();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"published");
    assert!(calls.load(Ordering::Relaxed) > 0);
    drop(reader);
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn progressive_seek_read_oracle_preserves_range_and_download_boundaries() {
        crate::audio::callback::test_scope(|| {
    let bytes: Vec<u8> = (0..CAP + 173).map(|n| (n % 251) as u8).collect();
    let path = generated(&bytes);
    let progress = Progress::new();
    progress.advance(7);
    progress.set_total(Some(bytes.len() as u64));
    let stop = Arc::new(Interrupt::new(false));
    let ranges = bytes.clone();
    let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), stop.clone())
        .unwrap()
        .with_async_ranges(Arc::new(move |at, len| {
            let end = (at as usize + len).min(ranges.len());
            let bytes = ranges[at as usize..end].to_vec();
            Box::pin(async move { Ok(Some(bytes)) })
        }));
    let mut reader = ReadAhead::progressive(
        source,
        progress.clone(),
        Arc::new(Interrupt::new(false)),
        stop,
    )
    .unwrap();
    let mut oracle = std::io::Cursor::new(&bytes);
    let mut rng = fastrand::Rng::with_seed(0x511cee);
    for step in 0..200 {
        let offset = rng.usize(8..bytes.len() + 100) as u64;
        reader.seek(SeekFrom::Start(offset)).unwrap();
        oracle.seek(SeekFrom::Start(offset)).unwrap();
        let count = rng.usize(1..4097);
        let mut actual = vec![0; count];
        let mut expected = vec![0; count];
        let n = oracle.read(&mut expected).unwrap();
        reader.read_exact(&mut actual[..n]).unwrap();
        assert_eq!(&actual[..n], &expected[..n]);
        if n < count {
            assert_eq!(reader.read(&mut actual).unwrap(), 0);
        }
        progress.advance(((step + 1) * bytes.len() / 200) as u64);
    }
    progress.finish();
    drop(reader);
    std::fs::remove_file(path).unwrap();
        })
    }

#[test]
fn malformed_range_results_surface_without_card_reads_or_unbounded_buffer() {
        crate::audio::callback::test_scope(|| {
    for size in [0, CHUNK + 1] {
        let path = generated(&[]);
        let progress = Progress::new();
        let stop = Arc::new(Interrupt::new(false));
        let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), stop.clone())
            .unwrap()
            .with_async_ranges(Arc::new(move |_, _| {
                Box::pin(async move { Ok(Some(vec![0; size])) })
            }));
        let mut reader =
            ReadAhead::progressive(source, progress, Arc::new(Interrupt::new(false)), stop)
                .unwrap();
        reader.seek(SeekFrom::Start(100)).unwrap();
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().to_string(),
            "invalid seek block"
        );
        assert!(reader.shared.buffer.lock().unwrap().data.is_empty());
        drop(reader);
        std::fs::remove_file(path).unwrap();
    }
        })
    }


#[test]
fn abandoned_card_reply_retires_last_file_off_executor() {
    let expected = b"generated read-ahead bytes";
    let path = generated(expected);
    let ui = crate::rt::Ui::new().unwrap();
    for progressive in [false, true] {
        for on_executor in [false, true] {
            let (closed, retired) = mpsc::channel();
            let mut input = if progressive {
                let progress = Progress::new();
                progress.advance(expected.len() as u64);
                let mut source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path,
                    progress, Arc::new(Interrupt::new(false))).unwrap().into_async();
                source.observe_file_close(closed);
                Input::Progressive(source)
            } else {
                let mut file = crate::audio::source::ReadAheadFile::new(File::open(&path).unwrap());
                file.observe_close(closed);
                Input::File(file)
            };
            let mut bytes = vec![0; expected.len()];
            let ready = ui.block_on(input.prepare_read(0, &mut bytes)).unwrap();
            let Ready::Card(card, length, _) = &ready else { panic!("expected ready file") };
            assert_eq!(*length, bytes.len());
            assert_eq!(card.read_at(&crate::rt::Blocking::dedicated_thread(), &mut bytes, 0).unwrap(), bytes.len());
            assert_eq!(bytes.as_slice(), expected);
            // Abandon the async controller first. The prepared card reply owns
            // the last clone and must keep the actual descriptor alive.
            ui.block_on(async { drop(input); });
            assert!(matches!(retired.try_recv(), Err(mpsc::TryRecvError::Empty)));
            if on_executor { ui.block_on(async { drop(ready); }); }
            else { drop(ready); }
            assert_eq!(retired.recv_timeout(Duration::from_secs(3)).unwrap(), (false, on_executor),
                "final descriptor close must be inline ordinary or on a finite worker");
        }
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn unpolled_reader_owner_retires_file_on_finite_worker() {
    let path = generated(b"unpolled generated reader");
    let (closed, retired) = mpsc::channel();
    let mut file = crate::audio::source::ReadAheadFile::new(File::open(&path).unwrap());
    file.observe_close(closed);
    let shared = Arc::new(Shared {
        buffer: Mutex::new(Buf { data: VecDeque::new(), start: 0, eof: false,
            error: None, seek: None, closed: false }),
        ready: Arc::default(), changed: tokio::sync::Notify::new(), card_hook: CardHook::default(),
    });
    // Exercise the real controller's before-first-poll ownership, including
    // its Exit token and independent lane. No card read is dispatched.
    let owner = worker(Input::File(file), shared.clone(), CardLane::new().unwrap());
    crate::rt::Ui::new().unwrap().block_on(async { drop(owner); });
    assert_eq!(retired.recv_timeout(Duration::from_secs(3)).unwrap(), (false, true));
    let state = shared.buffer.lock().unwrap();
    assert!(state.closed);
    assert_eq!(state.error.as_ref().map(|error| error.1.as_str()), Some("Read-ahead owner stopped"));
    drop(state);
    std::fs::remove_file(path).unwrap();
}
