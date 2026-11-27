//! One bounded, low-priority album warmer. Never competes with song downloads
//! or runs with the screen off. Results use the existing persistent SQLite
//! response cache, so opening a warmed album is a local indexed lookup.
use super::{Library, Policy};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use subsonic::calls;
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct Warmer {
    queue: Mutex<VecDeque<String>>,
    wake: Arc<Notify>,
    browsing: AtomicBool,
}
impl Warmer {
    pub(super) fn wake(&self) { self.wake.notify_one(); }
    pub fn set_browsing(&self, on: bool) {
        // A wake while the task is busy is kept for its next wait.
        if self.browsing.swap(on, Ordering::Relaxed) != on { self.wake.notify_one(); }
    }
    pub fn enqueue(&self, ids: impl IntoIterator<Item=String>) {
        let mut q = self.queue.lock().unwrap();
        // Closest item first; replace stale speculative work after a fast spin.
        q.clear();
        for id in ids.into_iter().take(8) {
            if !q.contains(&id) { q.push_back(id); }
        }
        drop(q);
        self.wake.notify_one();
    }
    /// The next album to warm, when browsing, online and no song is
    /// downloading.
    fn next(lib: &Library) -> Option<String> {
        let mut q = lib.metadata.queue.lock().unwrap();
        let ready = !q.is_empty() && lib.metadata.browsing.load(Ordering::Relaxed)
            && lib.is_online() && lib.maintenance_link_ready() && lib.inflight.lock().unwrap().is_empty();
        if ready { q.pop_front() } else { None }
    }
    /// A task on the I/O runtime; it holds the library only while it works.
    pub fn start(lib: &Arc<Library>) {
        crate::rt::io().spawn(Self::run(Arc::downgrade(lib), lib.metadata.wake.clone()));
    }
    async fn run(weak: Weak<Library>, wake: Arc<Notify>) {
        loop {
            let notified = wake.notified();
            let id = {
                let Some(lib) = weak.upgrade() else { return };
                Warmer::next(&lib)
            };
            let Some(id) = id else {
                // Queue, browsing, connectivity, transfer retirement and
                // final Library drop all wake this single owner directly.
                notified.await;
                continue;
            };
            let Some(lib) = weak.upgrade() else { return };
            // Speculative: its failures are not evidence about the server.
            let _ = super::health::with_priority(q2_machines::server::Priority::Low, lib.fetch_async(calls::album(&id), Policy::PreferCache)).await;
            // Yield bandwidth/CPU to visible art, input and playback.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

impl Drop for Warmer {
    fn drop(&mut self) { self.wake(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    #[test]
    fn idle_warmer_has_no_timer_and_library_drop_wakes_retirement() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let (lib, _) = Library::create(Paths::new(super::super::generated_test_directory("metadata-retirement")), &Config::default(), Arc::new(|_| {}));
        let weak = Arc::downgrade(&lib);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
        runtime.block_on(async {
            let wakes = Arc::new(super::super::readiness_tests::Wakes::default());
            let waker = Waker::from(wakes.clone());
            let mut cx = Context::from_waker(&waker);
            let worker = Warmer::run(weak.clone(), lib.metadata.wake.clone());
            tokio::pin!(worker);
            assert!(worker.as_mut().poll(&mut cx).is_pending());
            tokio::time::advance(Duration::from_secs(4000)).await;
            assert_eq!(wakes.take(), 0, "no hourly dark readiness poll");
            lib.set_browsing(true);
            assert!(wakes.take() > 0);
            assert!(worker.as_mut().poll(&mut cx).is_pending());
            tokio::time::advance(Duration::from_secs(20)).await;
            assert_eq!(wakes.take(), 0, "no ten-second browsing readiness poll");
            drop(lib);
            assert!(weak.upgrade().is_none(), "an idle warmer must not own the library");
            assert!(wakes.take() > 0, "final owner drop wakes the idle task");
            assert_eq!(worker.as_mut().poll(&mut cx), Poll::Ready(()));
        });
    }

    #[test]
    fn warmer_pauses_offscreen_then_caches_album_without_repeat_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "fixture".into();
        config.server.password = "fixture".into();
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let (done, requests) = (stop.clone(), count.clone());
        let server = std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let (mut socket, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(_) => { std::thread::sleep(Duration::from_millis(5)); continue; }
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with("GET /rest/getAlbum?"));
                loop { line.clear(); reader.read_line(&mut line).unwrap(); if line == "\r\n" || line.is_empty() { break; } }
                requests.fetch_add(1, Ordering::Relaxed);
                let body = r#"{"subsonic-response":{"status":"ok","album":{"id":"fixture-album","name":"Fixture","song":[{"id":"fixture-song","title":"Fixture"}]}}}"#;
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        let root = super::super::generated_test_directory("metadata-cache");
        let lib = Library::open(Paths::new(root.clone()), &config, Arc::new(|_| {}));
        lib.set_online(false);
        lib.prefetch_albums(["fixture-album".into()]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count.load(Ordering::Relaxed), 0, "offscreen has no speculative network work");
        lib.set_browsing(true);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count.load(Ordering::Relaxed), 0, "offline also holds queued work");
        lib.set_online(true);
        let end = Instant::now() + crate::library::TEST_LIVENESS;
        let call = calls::album("fixture-album");
        while !lib.db().response_fresh(&crate::rt::Blocking::dedicated_thread(), &call.request.cache_key(), 3600) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        let album = lib.fetch(&call, Policy::PreferCache).unwrap().value;
        assert_eq!(album.song[0].id, "fixture-song");
        assert_eq!(count.load(Ordering::Relaxed), 1, "warm album opens locally");
        lib.set_browsing(false);
        lib.set_offline_mode(true);
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap();
        // Unique fixtures only; never touches the owner's data directory.
        std::fs::remove_dir_all(root).unwrap();
    }
}
