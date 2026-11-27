use super::*;
use crate::{config::{Config, Paths}, rt::Blocking};
use std::{io::{BufRead, BufReader, Write}, sync::{Mutex, mpsc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

#[derive(Clone, Copy)]
enum Mode { Normal, Repeat, FailSecond, Oversized }
struct Server {
    url: String,
    offsets: Arc<Mutex<Vec<u32>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(mode: Mode, hold: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let offsets = Arc::new(Mutex::new(Vec::new())); let observed = offsets.clone();
        let stop = Arc::new(AtomicBool::new(false)); let done = stop.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20); let mut hold = hold;
            while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                let Ok((mut socket, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(2)); continue; };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap()); let mut request = String::new();
                if reader.read_line(&mut request).is_err() { continue; }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() { break; }
                }
                // A server without scan markers: every browse builds (the
                // model server in `sync_tests` has them).
                if request.contains("/rest/getScanStatus") {
                    let body = r#"{"subsonic-response":{"status":"failed","error":{"code":0,"message":"Generated: not supported"}}}"#;
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    continue;
                }
                assert!(request.contains("/rest/getAlbumList2") && request.contains("type=alphabeticalByName") && request.contains("size=500"));
                let offset = request.split("offset=").nth(1).unwrap().split(['&',' ']).next().unwrap().parse::<u32>().unwrap();
                observed.lock().unwrap().push(offset);
                if let Some((started, release)) = hold.take() { let _ = started.send(()); let _ = release.recv_timeout(Duration::from_secs(5)); }
                let body = if matches!(mode, Mode::FailSecond) && offset > 0 {
                    serde_json::json!({"status":"failed","error":{"code":70,"message":"Generated failure"}})
                } else {
                    let first = if matches!(mode, Mode::Repeat) { 0 } else { offset };
                    let count = if matches!(mode, Mode::Oversized) { 501 } else if first == 0 { 500 } else { 5 };
                    let albums: Vec<_> = (first..first+count).map(|id| serde_json::json!({"id":format!("id-{id}"),"name":format!("Generated {id}")})).collect();
                    serde_json::json!({"status":"ok","albumList2":{"album":albums}})
                };
                let body = serde_json::json!({"subsonic-response":body}).to_string();
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self { url, offsets, stop, thread:Some(thread) }
    }
}
impl Drop for Server {
    fn drop(&mut self) { self.stop.store(true, Ordering::SeqCst); self.thread.take().unwrap().join().unwrap(); }
}
fn fixture(server: &Server) -> (Arc<Library>, Config) {
    let mut config = Config::default(); config.server.url = server.url.clone();
    config.server.user = "generated".into(); config.server.password = "synthetic-only".into();
    let (lib, _) = Library::create(Paths::new(super::super::generated_test_directory("async-catalog")), &config, Arc::new(|_| {}));
    (lib, config)
}
fn wait<T>(future: impl Future<Output=T>) -> T {
    crate::rt::Ui::new().unwrap().block_on(async { tokio::time::timeout(crate::library::TEST_LIVENESS, future).await.expect("bounded catalog operation") })
}
fn old_catalog(lib: &Library) -> Catalog {
    let b = Blocking::dedicated_thread();
    let mut db = lib.db(); let build = db.catalog_begin(&b).unwrap();
    db.catalog_append(&b, &build, 0, &[(Album { id:"old".into(), name:"Old".into(), ..Default::default() }, "O".into())]).unwrap();
    Catalog { generation:lib.client_generation(), stored:db.catalog_publish(&b, &build).unwrap() }
}

#[test]
fn async_catalog_refresh_uses_bounded_server_pages_and_retains_older_readers() {
    let server = Server::new(Mode::Normal, None); let (lib, _) = fixture(&server); let old = old_catalog(&lib);
    let catalog = wait(lib.refresh_album_catalog(crate::ui::list::index_letter)).unwrap();
    assert_eq!(catalog.len(), 505); assert_eq!(catalog.groups(), 1);
    assert_eq!(catalog.generation(), lib.client_generation());
    let page = wait(lib.album_catalog_page(catalog.clone(), 499, 100)).unwrap();
    assert_eq!(page.iter().map(|a| a.id.clone()).collect::<Vec<_>>(), (499..505).map(|i| format!("id-{i}")).collect::<Vec<_>>());
    assert_eq!(wait(lib.album_catalog_page(old, 0, 1)).unwrap()[0].id, "old");
    assert_eq!(wait(lib.current_album_catalog()).unwrap().unwrap().len(), 505);
    assert_eq!(*server.offsets.lock().unwrap(), [0,500]);
    assert!(lib.db().response(&Blocking::dedicated_thread(), "getAlbumList2?type=alphabeticalByName&size=500&offset=0").is_none(), "no redundant full response-cache copy");
    assert_eq!(wait(lib.browse_album_catalog(crate::ui::list::index_letter)).unwrap().len(), 505);
    assert_eq!(*server.offsets.lock().unwrap(), [0,500], "recent catalog reuse performs no network requests");
    wait(async { *lib.catalog_refresh.lock().await = Some((lib.client_generation(), Instant::now()-Duration::from_secs(301))); });
    assert_eq!(wait(lib.browse_album_catalog(crate::ui::list::index_letter)).unwrap().len(), 505);
    assert_eq!(*server.offsets.lock().unwrap(), [0,500,0,500], "expired catalog is refreshed for a new browsing session");
}

#[test]
fn pending_catalog_network_keeps_reads_and_executor_live_and_rejects_account_changes() {
    let (started, waiting) = mpsc::channel(); let (release, held) = mpsc::channel();
    let server = Server::new(Mode::Normal, Some((started, held))); let (lib, mut config) = fixture(&server);
    let old = old_catalog(&lib);
    let first = crate::rt::io().spawn(lib.refresh_album_catalog(crate::ui::list::index_letter));
    waiting.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    assert_eq!(wait(crate::rt::io().spawn(async { 42 })).unwrap(), 42);
    assert_eq!(wait(lib.album_catalog_page(old.clone(), 0, 1)).unwrap()[0].id, "old");
    assert_eq!(wait(lib.current_album_catalog()).unwrap().unwrap().len(), 1);
    let queued = lib.refresh_album_catalog(crate::ui::list::index_letter);
    config.server.user = "generated-second".into(); wait(lib.configure_async(&config)).unwrap();
    release.send(()).unwrap();
    assert!(wait(first).unwrap().is_err());
    assert!(wait(queued).is_err(), "unpolled requests retain their original account admission");
    assert!(wait(lib.album_catalog_page(old, 0, 1)).is_err());
    assert!(wait(lib.current_album_catalog()).unwrap().is_none());
    assert_eq!(*server.offsets.lock().unwrap(), [0], "no old-account continuation or queued refresh is sent");
}

#[test]
fn cancelled_catalog_refresh_releases_gate_and_reclaims_partial_build() {
    let (started, waiting) = mpsc::channel(); let (release, held) = mpsc::channel();
    let server = Server::new(Mode::Normal, Some((started, held))); let (lib, _) = fixture(&server);
    let old = old_catalog(&lib);
    let first = crate::rt::io().spawn(lib.refresh_album_catalog(crate::ui::list::index_letter));
    waiting.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); first.abort();
    match wait(first) { Err(error) => assert!(error.is_cancelled()), Ok(_) => panic!("refresh must be cancelled") }
    release.send(()).unwrap();
    let next = wait(lib.refresh_album_catalog(crate::ui::list::index_letter)).unwrap();
    assert_eq!(next.len(), 505);
    assert_eq!(wait(lib.album_catalog_page(old, 0, 1)).unwrap()[0].id, "old");
    assert_eq!(*server.offsets.lock().unwrap(), [0,0,500]);
}

#[test]
fn failed_repeated_and_oversized_server_pages_never_publish_partial_catalogs() {
    for mode in [Mode::Repeat, Mode::FailSecond, Mode::Oversized] {
        let server = Server::new(mode, None); let (lib, _) = fixture(&server); let old = old_catalog(&lib);
        assert!(wait(lib.refresh_album_catalog(crate::ui::list::index_letter)).is_err());
        assert_eq!(wait(lib.current_album_catalog()).unwrap().unwrap().len(), 1);
        assert_eq!(wait(lib.album_catalog_page(old, 0, 1)).unwrap()[0].id, "old");
        assert_eq!(server.offsets.lock().unwrap().len(), if matches!(mode, Mode::Oversized) {1} else {2});
    }
}

#[test]
fn catalog_reads_yield_while_the_database_owner_is_busy() {
    use std::{pin::Pin, task::{Context, Waker}};
    let server = Server::new(Mode::Normal, None); let (lib, _) = fixture(&server); let catalog = old_catalog(&lib);
    let (entered, started) = mpsc::channel(); let (release, held) = mpsc::channel();
    lib.db_post(move |_, _| { entered.send(()).unwrap(); held.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); });
    started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    let reader = lib.clone(); let (done, result) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        let mut current = Box::pin(reader.current_album_catalog());
        let mut page = Box::pin(reader.album_catalog_page(catalog.clone(), 0, 100));
        let mut jump = Box::pin(reader.album_catalog_jump(catalog, 0, 1));
        let mut context = Context::from_waker(Waker::noop());
        let pending = [Pin::as_mut(&mut current).poll(&mut context).is_pending(),
            Pin::as_mut(&mut page).poll(&mut context).is_pending(), Pin::as_mut(&mut jump).poll(&mut context).is_pending()];
        assert!(done.send((pending, current, page, jump)).is_ok());
    });
    let observed = result.recv_timeout(Duration::from_secs(3));
    release.send(()).unwrap(); caller.join().unwrap();
    let (pending, current, page, jump) = observed.expect("future construction/first poll cannot wait on SQLite");
    assert_eq!(pending, [true;3]);
    assert_eq!(wait(current).unwrap().unwrap().len(), 1);
    assert_eq!(wait(page).unwrap()[0].id, "old");
    assert_eq!(wait(jump).unwrap().unwrap().label, "O");
    assert!(server.offsets.lock().unwrap().is_empty());
}
