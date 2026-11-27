//! The reachability shell against a generated server (docs/server-status.md):
//! a slow server that drops requests keeps the status online; a dead one goes
//! offline within the probe's bound. Generated data only.
use super::*;
use q2_machines::server::{Priority, Status};
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU8, AtomicUsize};

/// How the generated server answers.
const OK: u8 = 0;
/// Pages and covers reset after `delay`; `ping` answers after the same delay.
const DROP_PAGES: u8 = 1;
/// Pages reset; `ping` never answers (a black hole).
const DEAD: u8 = 2;
/// Controlled positive probe. The test releases the reply after admission.
const HOLD_PING: u8 = 3;

struct Server {
    url: String,
    mode: Arc<AtomicU8>,
    pings: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    listener: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(delay: Duration) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (mode, pings, requests, stop) = (Arc::new(AtomicU8::new(OK)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let (m, p, r, s) = (mode.clone(), pings.clone(), requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let Ok((socket, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(5)); continue };
                let (m, p, r, s) = (m.clone(), p.clone(), r.clone(), s.clone());
                std::thread::spawn(move || serve(socket, delay, &m, &p, &r, &s));
            }
        });
        Server { url, mode, pings, requests, stop, listener: Some(thread) }
    }
    fn set(&self, mode: u8) { self.mode.store(mode, Ordering::Relaxed); }
    fn pings(&self) -> usize { self.pings.load(Ordering::Relaxed) }
    /// Stops listening: new connections are refused.
    fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.listener.take() { thread.join().unwrap(); }
    }
}

impl Drop for Server {
    fn drop(&mut self) { self.close(); }
}

fn serve(mut socket: TcpStream, delay: Duration, mode: &AtomicU8, pings: &AtomicUsize, requests: &AtomicUsize, stop: &AtomicBool) {
    socket.set_nonblocking(false).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || stop.load(Ordering::Relaxed) { return; }
        loop { let mut header = String::new(); if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" { break; } }
        let ping = line.contains("/rest/ping");
        if ping { pings.fetch_add(1, Ordering::Relaxed); } else { requests.fetch_add(1, Ordering::Relaxed); }
        let mut current = mode.load(Ordering::Relaxed);
        if ping && current == HOLD_PING {
            let until = Instant::now() + crate::library::TEST_LIVENESS;
            while current == HOLD_PING && !stop.load(Ordering::Relaxed) {
                if Instant::now() >= until { return; }
                std::thread::sleep(Duration::from_millis(1));
                current = mode.load(Ordering::Relaxed);
            }
            if stop.load(Ordering::Relaxed) { return; }
        }
        if current != OK { std::thread::sleep(delay); }
        if current == DEAD && ping {
            // No answer while the server is dead; the client times out.
            while !stop.load(Ordering::Relaxed) && mode.load(Ordering::Relaxed) == DEAD { std::thread::sleep(Duration::from_millis(20)); }
            return;
        }
        if current != OK && !ping {
            // The connection closes unanswered: the request fails at once.
            let _ = socket.shutdown(std::net::Shutdown::Both);
            return;
        }
        let body = if line.contains("getAlbumList2") {
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","albumList2":{"album":[{"id":"generated-album","name":"Generated"}]}}}"#
        } else { r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"# };
        if write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).is_err() { return; }
    }
}

struct Fixture {
    lib: Arc<Library>,
    offline_events: Arc<AtomicUsize>,
    root: PathBuf,
}

impl Fixture {
    fn new(server: &Server, label: &str) -> Fixture {
        let root = generated_test_directory(label);
        let mut config = Config::default();
        config.server.url = server.url.clone();
        config.server.user = "generated".into();
        config.server.password = "synthetic-only".into();
        let offline_events = Arc::new(AtomicUsize::new(0));
        let seen = offline_events.clone();
        let (lib, _) = Library::create(Paths::new(root.clone()), &config, Arc::new(move |event| {
            if event == LibEvent::Online(false) { seen.fetch_add(1, Ordering::Relaxed); }
        }));
        Fixture { lib, offline_events, root }
    }
    fn page(&self, priority: Priority) -> Result<(), LibError> {
        let lib = self.lib.clone();
        let call = calls::album_list(subsonic::AlbumListType::Newest, 10, 0);
        super::shelf_browse::wait(async move { with_priority(priority, lib.fetch_async(call, Policy::NetworkOnly)).await.map(drop) })
    }
    fn until(&self, status: Status, within: Duration) -> Duration {
        let start = Instant::now();
        while self.lib.server_status() != status {
            assert!(start.elapsed() < within, "status {:?} not {status:?} within {within:?}", self.lib.server_status());
            std::thread::sleep(Duration::from_millis(5));
        }
        start.elapsed()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
}

/// The probe's worst case: every attempt times out.
fn probe_bound() -> Duration { health::PROBE_ATTEMPT * 2 + Duration::from_millis(50) }

#[test]
fn a_slow_server_that_drops_requests_stays_online() {
    let server = Server::start(Duration::from_millis(150));
    let fx = Fixture::new(&server, "health-slow");
    fx.page(Priority::Foreground).unwrap();
    assert_eq!(fx.lib.server_status(), Status::Online);
    server.set(DROP_PAGES);
    // Cover prefetch and page refreshes behind cached rows: never evidence,
    // never a probe.
    for _ in 0..5 { assert!(fx.page(Priority::Low).is_err()); }
    assert_eq!((fx.lib.server_status(), server.pings()), (Status::Online, 0));
    // A page the listener waits for fails: checking, then the probe answers.
    assert!(fx.page(Priority::Foreground).is_err());
    assert_ne!(fx.lib.server_status(), Status::Offline);
    fx.until(Status::Online, Duration::from_secs(3));
    assert_eq!(server.pings(), 1, "one probe settles the doubt");
    // Background work too; the status never went offline.
    assert!(fx.page(Priority::Background).is_err());
    fx.until(Status::Online, Duration::from_secs(3));
    assert!(fx.lib.is_online());
    assert_eq!(fx.offline_events.load(Ordering::Relaxed), 0, "nothing told the UI the server was gone");
}

#[test]
fn a_dead_server_goes_offline_within_the_probe_bound() {
    let server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-dead");
    fx.page(Priority::Foreground).unwrap();
    server.set(DEAD);
    assert!(fx.page(Priority::Foreground).is_err());
    let waited = fx.until(Status::Offline, probe_bound() + Duration::from_secs(1));
    assert!(waited <= probe_bound() + Duration::from_millis(500), "{waited:?}");
    assert_eq!(server.pings(), 2, "two attempts before giving up");
    assert_eq!(fx.offline_events.load(Ordering::Relaxed), 1);
    // Any answer brings it back.
    server.set(OK);
    fx.page(Priority::Foreground).unwrap();
    assert_eq!(fx.lib.server_status(), Status::Online);
}

#[test]
fn a_refused_server_goes_offline_at_once() {
    let mut server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-refused");
    fx.page(Priority::Foreground).unwrap();
    server.close();
    assert!(fx.page(Priority::Foreground).is_err());
    let waited = fx.until(Status::Offline, Duration::from_secs(2));
    assert!(waited < Duration::from_millis(800), "{waited:?}");
}

#[test]
fn a_recheck_finds_a_server_that_failed_while_nothing_asked() {
    // Connections opened: the last answer was a success, then the server
    // stopped with no request in between.
    let mut server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-recheck");
    fx.page(Priority::Foreground).unwrap();
    fx.lib.recheck_server();
    fx.until(Status::Online, Duration::from_secs(2));
    assert_eq!(server.pings(), 1, "a working server answers the recheck");
    server.close();
    assert_eq!(fx.lib.server_status(), Status::Online, "nothing has asked yet");
    fx.lib.recheck_server();
    assert_eq!(fx.lib.server_status(), Status::Checking, "not shown as connected while untested");
    fx.lib.recheck_server();
    fx.until(Status::Offline, Duration::from_secs(2));
    assert_eq!(fx.offline_events.load(Ordering::Relaxed), 1);
    // Offline Mode: nothing is asked.
    fx.lib.set_offline_mode(true);
    fx.lib.recheck_server();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fx.lib.server_status(), Status::Offline);
}

#[test]
fn three_foreground_failures_go_offline_before_the_probe_ends() {
    let server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-three");
    fx.page(Priority::Foreground).unwrap();
    server.set(DEAD);
    let start = Instant::now();
    for _ in 0..2 { assert!(fx.page(Priority::Foreground).is_err()); assert_eq!(fx.lib.server_status(), Status::Checking); }
    assert!(fx.page(Priority::Foreground).is_err());
    assert_eq!(fx.lib.server_status(), Status::Offline);
    assert!(start.elapsed() < health::PROBE_ATTEMPT, "the third failure did not wait for the probe");
}

#[test]
fn transport_down_is_offline_at_once_and_its_return_probes() {
    let server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-transport");
    fx.page(Priority::Foreground).unwrap();
    fx.lib.set_transport(false);
    assert_eq!(fx.lib.server_status(), Status::Offline);
    // The offline cadence does not probe without a transport.
    super::shelf_browse::wait({ let lib = fx.lib.clone(); async move { lib.probe_if_due().await; Ok(()) } }).unwrap();
    assert_eq!(server.pings(), 0);
    fx.lib.set_transport(true);
    fx.until(Status::Online, Duration::from_secs(3));
    assert_eq!(server.pings(), 1);
}

#[test]
fn transport_recovery_drains_queued_updates_as_soon_as_the_server_answers() {
    use std::{future::Future, task::{Context, Waker}};

    let server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-transport-mailbox");
    fx.page(Priority::Foreground).unwrap();
    fx.lib.db().push_pending(&crate::rt::Blocking::dedicated_thread(),
        &PendingCall::Scrobble { id: "generated-played".into(), time_ms: 123 });
    fx.lib.set_transport(false);
    let mut context = Context::from_waker(Waker::noop());
    // Consume any initial setup permit before observing this recovery.
    let _ = Box::pin(fx.lib.sync_wake.notified()).as_mut().poll(&mut context);
    server.set(HOLD_PING);
    fx.lib.set_transport(true);
    let deadline = Instant::now() + TEST_LIVENESS;
    while server.pings() == 0 {
        assert!(Instant::now() < deadline, "missing recovery probe");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(Box::pin(fx.lib.sync_wake.notified()).as_mut().poll(&mut context).is_pending());
    assert_eq!(server.requests.load(Ordering::Relaxed), 1, "only the initial page has been sent");

    server.set(OK);
    let lib = fx.lib.clone();
    super::shelf_browse::wait(async move {
        tokio::time::timeout(TEST_LIVENESS, lib.sync_wake.notified()).await.expect("recovered server did not kick sync");
        assert!(lib.sync_urgent.swap(false, Ordering::Acquire));
        assert_eq!(lib.server_status(), Status::Online);
        assert_eq!(lib.sync_round(true).await, SYNC_IDLE);
        Ok(())
    }).unwrap();
    assert_eq!(server.pings(), 1);
    assert_eq!(server.requests.load(Ordering::Relaxed), 2, "exactly one completed listen drained");
    assert!(fx.lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 1).is_empty());
    fx.page(Priority::Foreground).unwrap();
    assert!(Box::pin(fx.lib.sync_wake.notified()).as_mut().poll(&mut context).is_pending(),
        "ordinary online success does not wake maintenance");
}

#[test]
fn a_request_the_player_cancels_is_not_evidence() {
    let server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-cancel");
    fx.page(Priority::Foreground).unwrap();
    server.set(DEAD);
    // A request that waits (pings black-hole) and that its caller drops.
    let lib = fx.lib.clone();
    let task = crate::rt::io().spawn(async move { lib.fetch_net(&calls::ping().request).await.map(drop) });
    std::thread::sleep(Duration::from_millis(100));
    task.abort();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fx.lib.server_status(), Status::Online);
    assert_eq!(server.pings(), 1, "only the cancelled request reached the server");
}

#[test]
fn background_transfers_yield_to_a_waiting_request_and_the_probe() {
    let server = Server::start(Duration::from_millis(300));
    let fx = Fixture::new(&server, "health-yield");
    assert!(!fx.lib.background_should_yield());
    let guard = fx.lib.foreground_request(Priority::Foreground);
    assert!(fx.lib.background_should_yield());
    drop(guard);
    assert!(fx.lib.foreground_request(Priority::Background).is_none());
    assert!(!fx.lib.background_should_yield());
    fx.page(Priority::Foreground).unwrap();
    server.set(DROP_PAGES);
    assert!(fx.page(Priority::Background).is_err());
    // The probe (ping delayed 300 ms) is running.
    assert!(fx.lib.background_should_yield());
    fx.until(Status::Online, Duration::from_secs(3));
    let end = Instant::now() + crate::library::TEST_LIVENESS;
    while fx.lib.background_should_yield() && Instant::now() < end { std::thread::sleep(Duration::from_millis(5)); }
    assert!(!fx.lib.background_should_yield());
}

#[test]
fn a_probe_on_its_kept_connection_still_ends_within_its_bound() {
    let server = Server::start(Duration::ZERO);
    let fx = Fixture::new(&server, "health-kept");
    fx.lib.set_transport(true);
    fx.until(Status::Online, Duration::from_secs(3));
    assert_eq!(server.pings(), 1);
    server.set(DEAD);
    assert!(fx.page(Priority::Foreground).is_err());
    let waited = fx.until(Status::Offline, probe_bound() + Duration::from_secs(2));
    assert!(waited <= probe_bound() + Duration::from_millis(500), "{waited:?}");
}
