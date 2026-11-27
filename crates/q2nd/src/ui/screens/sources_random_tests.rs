use super::*;
use crate::config::{Config, Paths};
use std::{io::{BufRead, BufReader, Read, Write}, sync::atomic::{AtomicBool, Ordering}, time::{Duration, Instant}};

/// Generated transport serving both API variants. All sockets and the owned
/// thread are bounded and joined even when a browsing assertion fails.
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<(String, u32)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(native: bool) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false)); let done = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new())); let observed = requests.clone();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                let Ok((mut socket, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(1)); continue; };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap()); let mut request = String::new();
                if reader.read_line(&mut request).is_err() { continue; }
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.is_empty() || line == "\r\n" { break; }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") { length = v.trim().parse::<usize>().unwrap(); }
                }
                let mut body = vec![0; length]; if reader.read_exact(&mut body).is_err() { continue; }
                let number = |key: &str| request.split(key).nth(1).unwrap().split(['&', ' ']).next().unwrap().parse::<u32>().unwrap();
                let body = if request.contains("/rest/ping") {
                    serde_json::json!({"subsonic-response":{"status":"ok","type":if native {"navidrome"} else {"other"}}})
                } else if request.contains("/auth/login") {
                    serde_json::json!({"token":"generated-token"})
                } else if request.contains("/api/album?") {
                    let start = number("_start="); let seed = number("seed=");
                    observed.lock().unwrap().push(("native".into(), start));
                    assert_eq!(seed, 17);
                    let albums: Vec<_> = (start..start.saturating_add(100).min(3101))
                        .map(|i| serde_json::json!({"id":format!("id-{i}"),"name":format!("Generated {i}"),"albumArtist":"Generated"})).collect();
                    serde_json::json!(albums)
                } else if request.contains("/rest/getAlbumList2") {
                    assert!(request.contains("type=random"));
                    let offset = number("offset="); observed.lock().unwrap().push(("generic".into(), offset));
                    serde_json::json!({"subsonic-response":{"status":"ok","albumList2":{"album":[{"id":"a","name":"A"},{"id":"b","name":"B"}]}}})
                } else { serde_json::json!({"subsonic-response":{"status":"failed","error":{"code":70,"message":"Generated missing endpoint"}}}) };
                let body = body.to_string();
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self { url, requests, stop, worker:Some(worker) }
    }
}
impl Drop for Server {
    fn drop(&mut self) { self.stop.store(true, Ordering::SeqCst); self.worker.take().unwrap().join().unwrap(); }
}
fn fixture(server: &Server) -> (Arc<Library>, Config, Arc<Albums>) {
    let mut config = Config::default(); config.server.url = server.url.clone();
    config.server.user = "generated-random".into(); config.server.password = "synthetic-only".into();
    let lib = Library::open(Paths::new(crate::library::generated_test_directory("random-source")), &config, Arc::new(|_| {}));
    let mut source = Albums::new(AlbumListType::Random, "Random Albums".into()); source.random_seed = 17;
    (lib, config, Arc::new(source))
}
#[test]
fn native_random_source_replays_evicted_pages_without_enumerating_a_catalog() {
    let server = Server::new(true); let (lib, _, source) = fixture(&server);
    let runtime = crate::rt::Ui::new().unwrap(); let mut pages = Vec::new();
    for n in 0..32 {
        let crate::ui::screens::menu::WindowPage { rows, more, .. } = runtime.block_on(source.clone().window_page(lib.clone(), n, Policy::NetworkOnly)).unwrap();
        assert_eq!(more, n<31); assert!(rows.len()<=100);
        pages.push(rows.into_iter().map(|r|r.key).collect::<Vec<_>>());
    }
    assert!(source.refetch_history()); assert!(!source.refresh_retained());
    // Simulate a first-page draw cached by an earlier player version. The
    // online menu probes CacheOnly before refetching an evicted page zero.
    lib.db().put_response(&crate::rt::Blocking::dedicated_thread(),
        &calls::album_list(AlbumListType::Random,100,0).request.cache_key(),
        &serde_json::json!({"albumList2":{"album":[{"id":"old-draw","name":"Old draw"}]}}).to_string());
    assert!(runtime.block_on(source.clone().window_page(lib.clone(),0,Policy::CacheOnly)).is_err(),
        "old unseeded cache must never masquerade as native backward history");
    assert!(source.refetch_history(), "a failed cache preview cannot revoke native history");
    for n in [30, 0, 29, 1] {
        let crate::ui::screens::menu::WindowPage { rows, .. } = runtime.block_on(source.clone().window_page(lib.clone(), n, Policy::NetworkOnly)).unwrap();
        assert_eq!(rows.into_iter().map(|r|r.key).collect::<Vec<_>>(), pages[n as usize]);
    }
    assert!(runtime.block_on(lib.current_album_catalog()).unwrap().is_none(), "random browsing does not build a local catalog");
    let requests = server.requests.lock().unwrap(); assert_eq!(requests.len(),36);
    assert!(requests.iter().all(|(kind,_)|kind=="native"));
}
#[test]
fn generic_random_source_keeps_repeated_draw_occurrences_and_has_no_reverse_refetch() {
    let server = Server::new(false); let (lib, _, source) = fixture(&server);
    let runtime = crate::rt::Ui::new().unwrap(); let mut keys = std::collections::HashSet::new();
    for n in 0..31 {
        let crate::ui::screens::menu::WindowPage { rows, more, .. } = runtime.block_on(source.clone().window_page(lib.clone(), n, Policy::NetworkOnly)).unwrap();
        assert!(more, "short random samples are not library exhaustion"); assert_eq!(rows.len(),2);
        for row in rows { assert!(keys.insert(row.key)); }
    }
    assert!(!source.refetch_history()); assert!(!source.refresh_retained());
    assert!(runtime.block_on(lib.current_album_catalog()).unwrap().is_none());
    assert_eq!(*server.requests.lock().unwrap(), vec![("generic".into(),0);31]);
}
