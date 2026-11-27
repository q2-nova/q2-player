//! Random browsing owns its pages in the UI session, never the shared metadata
//! cache: another random draw cannot stand in for a previously visited page.
use super::{Library, LibError, Policy};
use std::{future::Future, sync::Arc};
use subsonic::Album;

impl Library {
    pub(crate) fn random_album_page(self: &Arc<Self>, seed: u32, page: u32, policy: Policy)
        -> impl Future<Output = Result<(Vec<Album>, bool), LibError>> + Send + 'static + use<> {
        let generation = self.client_generation();
        let owner = self.clone();
        async move {
            if policy == Policy::CacheOnly {
                // Preserve offline first-page browsing of previously cached
                // standard draws. Never use these bytes as native seed history.
                if page != 0 { return Err(LibError::Offline); }
                let albums = owner.clone().fetch_async(subsonic::calls::album_list(subsonic::AlbumListType::Random, 100, 0), Policy::CacheOnly).await?.value.album;
                if owner.client_generation() != generation { return Err(LibError::Offline); }
                return Ok((albums, false));
            }
            let offset = page.checked_mul(100).ok_or_else(|| LibError::Server("Random page is out of range".into()))?;
            owner.music_binding_ready(generation).await.map_err(|_| LibError::Offline)?;
            let prepare = owner.clone();
            let client = crate::rt::spawn_blocking(move |_| {
                let slot = prepare.client.read().unwrap();
                if prepare.client_generation() != generation || prepare.offline_mode() { return Err(LibError::Offline); }
                slot.clone().ok_or(LibError::NotConfigured)
            }).await.map_err(|_| LibError::Offline)??;
            let priority = super::health::current_priority();
            let result = {
                let _foreground = owner.foreground_request(priority);
                client.random_album_page_async(seed, offset, 100).await
            };
            crate::rt::spawn_blocking(move |_| {
                let slot = owner.client.read().unwrap();
                if owner.client_generation() != generation || owner.offline_mode()
                    || !slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &client)) { return Err(LibError::Offline); }
                owner.observe(&result, priority);
                result.map_err(LibError::from)
            }).await.map_err(|_| LibError::Offline)?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use std::{io::{BufRead, BufReader, Read, Write}, net::TcpListener, sync::mpsc, time::{Duration, Instant}};

    /// Owns both rendezvous and worker; assertion failures release the delayed
    /// response and join the bounded server rather than stranding a listener.
    struct Server {
        release: Option<mpsc::Sender<()>>,
        worker: Option<std::thread::JoinHandle<()>>,
        entered: Option<tokio::sync::oneshot::Receiver<()>>,
        url: String,
    }
    impl Server {
        fn delayed(retry: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/generated", listener.local_addr().unwrap());
            let (release, released) = mpsc::channel();
            let (entered, entered_rx) = tokio::sync::oneshot::channel();
            let worker = std::thread::spawn(move || {
                let mut entered = Some(entered);
                for index in 0..if retry { 4 } else { 3 } {
                    let deadline = Instant::now() + crate::library::TEST_LIVENESS;
                    let (mut socket, _) = loop {
                        match listener.accept() {
                            Ok(socket) => break socket,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                            Err(error) => panic!("generated native accept failed or timed out: {error}"),
                        }
                    };
                    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    socket.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut reader = BufReader::new(socket.try_clone().unwrap());
                    let mut headers = String::new();
                    loop {
                        let mut line = String::new();
                        assert!(reader.read_line(&mut line).unwrap() > 0);
                        if line == "\r\n" { break; }
                        headers.push_str(&line);
                    }
                    let length = headers.lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").and_then(|v| v.parse::<usize>().ok())).unwrap_or(0);
                    let mut body = vec![0; length]; reader.read_exact(&mut body).unwrap();
                    let body = match index {
                        0 => { assert!(headers.starts_with("GET /generated/rest/ping?")); r#"{"subsonic-response":{"status":"ok","type":"navidrome"}}"# },
                        1 => { assert!(headers.starts_with("POST /generated/auth/login ")); r#"{"token":"generated-token"}"# },
                        _ => {
                            assert!(headers.starts_with("GET /generated/api/album?"));
                            assert!(headers.to_lowercase().contains("x-nd-authorization: bearer generated-token"));
                            if index == 2 {
                                let _ = entered.take().unwrap().send(());
                                let _ = released.recv_timeout(Duration::from_secs(5));
                            }
                            r#"[{"id":"generated-album","name":"Generated album","albumArtist":"Generated artist"}]"#
                        }
                    };
                    // A cancelled HTTP owner can close its socket before the
                    // delayed response. That closure is an expected condition.
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                }
            });
            Self { release: Some(release), worker: Some(worker), entered: Some(entered_rx), url }
        }
        fn release(&mut self) { if let Some(release) = self.release.take() { let _ = release.send(()); } }
        fn finish(mut self) { self.release(); self.worker.take().unwrap().join().unwrap(); }
    }
    impl Drop for Server {
        fn drop(&mut self) { self.release(); if let Some(worker) = self.worker.take() { let _ = worker.join(); } }
    }
    fn library(url: &str) -> (Arc<Library>, Config) {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../build/perf/tests/native-random-{:016x}", fastrand::u64(..)));
        let mut config = Config::default();
        config.server.url = url.into(); config.server.user = "generated".into(); config.server.password = "generated".into();
        let (library, _) = Library::create(Paths::new(root), &config, Arc::new(|_| {}));
        (library, config)
    }
    #[test]
    fn delayed_native_random_cannot_publish_into_replaced_account() {
        let mut server = Server::delayed(false);
        let (library, mut config) = library(&server.url);
        let entered = server.entered.take().unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(4), async {
                let request = library.random_album_page(7, 0, Policy::NetworkOnly);
                tokio::pin!(request);
                tokio::select! {
                    result = &mut request => panic!("response escaped delayed fixture: {result:?}"),
                    signal = entered => signal.unwrap(),
                }
                config.server.user = "generated-replacement".into();
                library.configure_async(&config).await.unwrap();
                server.release();
                assert!(matches!(request.await, Err(LibError::Offline)));
                assert!(!library.online.load(std::sync::atomic::Ordering::Relaxed));
            }).await.expect("native account replacement exceeded bound");
        });
        server.finish();
    }
    #[test]
    fn cancelled_native_page_releases_transport_owner_for_next_page() {
        let mut server = Server::delayed(true);
        let (library, _) = library(&server.url);
        let entered = server.entered.take().unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(4), async {
                let mut request = Box::pin(library.random_album_page(7, 0, Policy::NetworkOnly));
                tokio::select! {
                    result = &mut request => panic!("response escaped delayed fixture: {result:?}"),
                    signal = entered => signal.unwrap(),
                }
                drop(request);
                assert!(!library.online.load(std::sync::atomic::Ordering::Relaxed));
                server.release();
                let (albums, native) = library.random_album_page(7, 1, Policy::NetworkOnly).await.unwrap();
                assert!(native); assert_eq!(albums.len(), 1);
            }).await.expect("native cancellation/retry exceeded bound");
        });
        server.finish();
    }
}
