//! Async, explicit-opt-in ListenBrainz transport. No listening policy here.
//!
//! Submit only after a listen has been qualified. Tokens,
//! track metadata and server bodies never appear in this module's errors/logs.
use serde_json::json;
use tokio::sync::{Mutex, MutexGuard};
use std::{sync::Arc, time::{Duration, Instant}};

const ENDPOINT: &str = "https://api.listenbrainz.org/1/submit-listens";
const MAX_RESPONSE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Configuration,
    InvalidListen,
    Authentication,
    RateLimited { retry_after_seconds: Option<u64> },
    Unavailable,
    Protocol,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration => "Invalid ListenBrainz configuration",
            Self::InvalidListen => "Incomplete listen metadata or timestamp",
            Self::Authentication => "ListenBrainz token rejected",
            Self::RateLimited { .. } => "ListenBrainz rate limit reached",
            Self::Unavailable => "ListenBrainz unavailable",
            Self::Protocol => "Unsupported ListenBrainz response",
        })
    }
}
impl std::error::Error for Error {}

/// Structured source metadata, not a station name or a guessed filename.
/// The caller retains the original spelling, including Unicode.
pub struct Track<'a> {
    pub artist: &'a str,
    pub title: &'a str,
    pub album: Option<&'a str>,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Copy)]
pub enum Submission {
    PlayingNow,
    /// A newly completed, qualified listen; Unix seconds, not milliseconds.
    Single {
        started_at: u64,
    },
    /// One previously persisted listen. The durable worker owns retry/order.
    Cached {
        started_at: u64,
    },
}

pub struct Client {
    endpoint: String,
    token: String,
    agent: crate::http::Agent,
    budget: Budget,
    #[cfg(test)]
    response_seen: tokio::sync::Notify,
}
/// One submission budget across route and token changes.
#[derive(Clone)]
pub struct Budget(Arc<Mutex<Rate>>);
impl Default for Budget {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Rate { since: Instant::now(), wait: Duration::ZERO })))
    }
}

struct Rate {
    since: Instant,
    wait: Duration,
}

/// If a request is cancelled before headers, its slot still imposes the
/// ordinary one-second retry delay from cancellation. Once response headers
/// establish a deadline, cancellation while reading never erases that deadline.
struct Attempt<'a> {
    rate: MutexGuard<'a, Rate>,
    responded: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.responded { self.rate.since = Instant::now(); }
    }
}

impl Client {
    /// No environment proxy and no fallback from an explicitly selected proxy.
    /// Construction alone makes no network request or authentication change.
    /// Supply the project's real maintainer contact URL, never another client's
    /// identity. Keep this client alive so its rate state survives retries.
    pub fn new(token: &str, proxy: Option<&str>, contact_url: &str) -> Result<Self, Error> {
        Self::with_budget(token, proxy, contact_url, Budget::default())
    }

    pub fn with_budget(token: &str, proxy: Option<&str>, contact_url: &str, budget: Budget) -> Result<Self, Error> {
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::Configuration);
        }
        let agent =
            crate::scrobble_http::agent(proxy, contact_url).map_err(|_| Error::Configuration)?;
        Ok(Self {
            endpoint: ENDPOINT.into(),
            token: token.into(),
            agent,
            #[cfg(test)]
            response_seen: tokio::sync::Notify::new(),
            budget,
        })
    }

    /// Generated app-tier controls only; production always uses the HTTPS API.
    #[cfg(feature = "scrobble-fixtures")]
    pub fn generated_loopback(token: &str, endpoint: std::net::SocketAddrV4) -> Self {
        assert!(endpoint.ip().is_loopback());
        let mut client = Self::new(token, None, "https://contact.invalid/q2").unwrap();
        client.endpoint = format!("http://{endpoint}/1/submit-listens");
        client
    }

    #[cfg(test)]
    fn submit(&self, track: &Track<'_>, submission: Submission) -> Result<(), Error> {
        crate::scrobble_http::wait(self.submit_async(track, submission))
    }

    pub async fn submit_async(&self, track: &Track<'_>, submission: Submission) -> Result<(), Error> {
        let (artist, title, album, duration_ms) = (track.artist.to_owned(), track.title.to_owned(), track.album.map(str::to_owned), track.duration_ms);
        let body = tokio::task::spawn_blocking(move || payload(&Track {
            artist: &artist, title: &title, album: album.as_deref(), duration_ms,
        }, submission)).await.map_err(|_| Error::Unavailable)??;
        // Serialize submissions and enforce the service's maximum one call per
        // second without sleeping. The durable worker schedules the next attempt.
        let mut rate = self.budget.0.lock().await;
        let remaining = rate.wait.saturating_sub(rate.since.elapsed());
        if !remaining.is_zero() {
            return Err(Error::RateLimited {
                retry_after_seconds: Some(
                    remaining
                        .as_secs()
                        .saturating_add(u64::from(remaining.subsec_nanos() != 0)),
                ),
            });
        }
        rate.since = Instant::now();
        rate.wait = Duration::from_secs(1);
        let mut attempt = Attempt { rate, responded: false };
        let request = self
            .agent
            .post(&self.endpoint)
            .set("Authorization", &format!("Token {}", self.token))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        let response = match request.send(body.into()).await {
            Ok(response) => response,
            Err(crate::http::Error::Status(_, response)) => response,
            Err(_) => {
                return Err(Error::Unavailable);
            }
        };
        let seconds = |name| {
            response.header(name).and_then(|value| {
                let value = value.trim();
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    None
                } else {
                    value.parse::<u64>().ok()
                }
            })
        };
        let limited = response.status() == 429 || seconds("X-RateLimit-Remaining") == Some(0);
        let advertised = if limited {
            seconds("Retry-After")
                .into_iter()
                .chain(seconds("X-RateLimit-Reset-In"))
                .max()
        } else {
            None
        };
        attempt.rate.since = Instant::now();
        attempt.rate.wait = Duration::from_secs(advertised.unwrap_or(1).max(1));
        attempt.responded = true;
        #[cfg(test)]
        self.response_seen.notify_one();
        match response.status() {
            200 => {}
            400 => return Err(Error::InvalidListen),
            401 | 403 => return Err(Error::Authentication),
            429 => {
                return Err(Error::RateLimited {
                    // Relative reset headers work even when wall time is untrusted.
                    // HTTP-date Retry-After remains unspecified; never invent a clock.
                    retry_after_seconds: advertised,
                });
            }
            408 | 500..=599 => return Err(Error::Unavailable),
            _ => return Err(Error::Protocol),
        }
        let value = crate::scrobble_http::json(response, MAX_RESPONSE).await.map_err(|error| match error {
            crate::scrobble_http::BodyError::Transport => Error::Unavailable,
            crate::scrobble_http::BodyError::Protocol => Error::Protocol,
        })?;
        if value.get("status").and_then(|value| value.as_str()) != Some("ok") {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

fn payload(track: &Track<'_>, submission: Submission) -> Result<String, Error> {
    if track.artist.trim().is_empty()
        || track.title.trim().is_empty()
        || track.duration_ms == Some(0)
    {
        return Err(Error::InvalidListen);
    }
    let mut metadata = json!({"artist_name": track.artist, "track_name": track.title,
        "additional_info": {"media_player": "q2nd", "submission_client": "q2nd"}});
    if let Some(album) = track.album.filter(|album| !album.trim().is_empty()) {
        metadata["release_name"] = json!(album);
    }
    if let Some(duration) = track.duration_ms {
        metadata["additional_info"]["duration_ms"] = json!(duration);
    }
    let mut listen = json!({"track_metadata": metadata});
    let kind = match submission {
        Submission::PlayingNow => "playing_now",
        Submission::Single { started_at } | Submission::Cached { started_at } => {
            if started_at == 0 || started_at > i64::MAX as u64 {
                return Err(Error::InvalidListen);
            }
            listen["listened_at"] = json!(started_at);
            if matches!(submission, Submission::Cached { .. }) {
                "import"
            } else {
                "single"
            }
        }
    };
    Ok(json!({"listen_type": kind, "payload": [listen]}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{Read, Write}, net::TcpListener, sync::mpsc, thread};

    fn client(token: &str, proxy: Option<&str>) -> Result<Client, Error> {
        Client::new(token, proxy, "https://example.invalid/generated-q2-contact")
    }

    fn track() -> Track<'static> {
        Track {
            artist: "Synthetic café & orchestra",
            title: "Generated \"A+B\" 🎵",
            album: Some("Fixture album"),
            duration_ms: Some(123456),
        }
    }

    // One generated request only. No public endpoint or owner token is used.
    fn server(
        status: u16,
        headers: &str,
        body: &str,
    ) -> (Client, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = client("generated-test-token", None).unwrap();
        client.endpoint = format!("http://{}/1/submit-listens", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status} Generated\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        );
        let (sent, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let until = Instant::now() + Duration::from_secs(3);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(value) => break value,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < until =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("generated request deadline: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0u8];
            while !bytes.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let header = String::from_utf8(bytes.clone()).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap();
            let mut body = vec![0; length];
            socket.read_exact(&mut body).unwrap();
            bytes.extend(body);
            sent.send(String::from_utf8(bytes).unwrap()).unwrap();
            let _ = socket.write_all(response.as_bytes());
        });
        (client, received, worker)
    }

    #[test]
    fn stalled_submission_cancels_without_blocking_pool_or_erasing_rate_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // One finite worker: a parser parked on the socket would prevent the
        // independent job below from completing. Both pre-head and body waits
        // are controlled by observed request/header events, never a sleep.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all()
            .max_blocking_threads(1).build().unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                for headers in [false, true] {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let mut client = client("generated-cancel-token", None).unwrap();
                    client.endpoint = format!("http://{}/submit", listener.local_addr().unwrap());
                    let client = std::sync::Arc::new(client);
                    let (seen, received) = tokio::sync::oneshot::channel();
                    let server = tokio::spawn(async move {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        let mut head = Vec::new();
                        while !head.ends_with(b"\r\n\r\n") {
                            head.push(socket.read_u8().await.unwrap());
                            assert!(head.len() < 8192);
                        }
                        let head = String::from_utf8(head).unwrap();
                        let length: usize = head.lines().find_map(|line| line.to_ascii_lowercase()
                            .strip_prefix("content-length:").map(|value| value.trim().parse().unwrap())).unwrap();
                        let mut body = vec![0; length];
                        socket.read_exact(&mut body).await.unwrap();
                        if headers {
                            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-In: 23\r\n\r\n").await.unwrap();
                        }
                        seen.send(()).unwrap();
                        match socket.read(&mut [0; 1]).await {
                            Ok(0) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                            result => panic!("cancelled transport did not close: {result:?}"),
                        }
                    });
                    let owner = client.clone();
                    let first = tokio::spawn(async move { owner.submit_async(&track(), Submission::PlayingNow).await });
                    received.await.unwrap();
                    if headers { client.response_seen.notified().await; }
                    let owner = client.clone();
                    let next = tokio::spawn(async move { owner.submit_async(&track(), Submission::PlayingNow).await });
                    assert_eq!(tokio::task::spawn_blocking(|| 17).await.unwrap(), 17);
                    first.abort();
                    assert!(first.await.unwrap_err().is_cancelled());
                    let retry = next.await.unwrap();
                    let expected = if headers { 23 } else { 1 };
                    assert!(matches!(retry, Err(Error::RateLimited { retry_after_seconds: Some(left) }) if left > 0 && left <= expected));
                    let rate = client.budget.0.lock().await;
                    assert_eq!(rate.wait, Duration::from_secs(expected));
                    assert!(rate.since.elapsed() < rate.wait);
                    drop(rate);
                    server.await.unwrap();
                }
            }).await.unwrap();
        });
    }

    #[test]
    fn actual_requests_keep_auth_in_header_and_preserve_unicode_and_timestamp_units() {
        for (submission, kind, at) in [
            (Submission::PlayingNow, "playing_now", None),
            (
                Submission::Single {
                    started_at: 1793721600,
                },
                "single",
                Some(1793721600),
            ),
            (
                Submission::Cached {
                    started_at: 1793721600,
                },
                "import",
                Some(1793721600),
            ),
        ] {
            let (client, request, worker) = server(200, "", r#"{"status":"ok"}"#);
            assert_eq!(client.submit(&track(), submission), Ok(()));
            let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
            worker.join().unwrap();
            let (header, body) = request.split_once("\r\n\r\n").unwrap();
            assert!(header.starts_with("POST /1/submit-listens HTTP/1.1\r\n"));
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: token generated-test-token")
            );
            assert!(header.contains("https://example.invalid/generated-q2-contact"));
            assert!(!body.contains("generated-test-token"));
            let value: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(value["listen_type"], kind);
            assert_eq!(value["payload"].as_array().unwrap().len(), 1);
            assert_eq!(
                value["payload"][0]
                    .get("listened_at")
                    .and_then(|v| v.as_u64()),
                at
            );
            assert_eq!(
                value["payload"][0]["track_metadata"]["artist_name"],
                track().artist
            );
            assert_eq!(
                value["payload"][0]["track_metadata"]["track_name"],
                track().title
            );
            assert_eq!(
                value["payload"][0]["track_metadata"]["additional_info"]["duration_ms"],
                123456
            );
        }
    }

    #[test]
    fn response_errors_are_typed_without_retaining_private_server_content() {
        for (status, headers, body, expected) in [
            (401, "", "private rejection", Error::Authentication),
            (403, "", "private rejection", Error::Authentication),
            (
                429,
                "Retry-After: 23\r\n",
                "private details",
                Error::RateLimited {
                    retry_after_seconds: Some(23),
                },
            ),
            (
                429,
                "Retry-After: unknown\r\n",
                "private details",
                Error::RateLimited {
                    retry_after_seconds: None,
                },
            ),
            (503, "", "private details", Error::Unavailable),
            (400, "", "private details", Error::InvalidListen),
            (
                200,
                "",
                r#"{"status":"failed","error":"private details"}"#,
                Error::Protocol,
            ),
            (200, "", "not JSON", Error::Protocol),
        ] {
            let (client, request, worker) = server(status, headers, body);
            let result = client.submit(
                &track(),
                Submission::Single {
                    started_at: 1793721600,
                },
            );
            let _ = request.recv_timeout(Duration::from_secs(3)).unwrap();
            worker.join().unwrap();
            assert_eq!(result, Err(expected));
            assert!(!expected.to_string().contains("private"));
        }
    }

    #[test]
    fn redirects_do_not_forward_tokens_and_oversized_success_is_not_accepted() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let header = format!(
            "Location: http://{}/stolen\r\n",
            target.local_addr().unwrap()
        );
        let (client, request, worker) = server(307, &header, "");
        assert_eq!(
            client.submit(&track(), Submission::PlayingNow),
            Err(Error::Protocol)
        );
        let _ = request.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let body = format!(
            "{{\"status\":\"ok\",\"padding\":\"{}\"}}",
            "x".repeat(MAX_RESPONSE)
        );
        let (client, request, worker) = server(200, "", &body);
        assert_eq!(
            client.submit(&track(), Submission::PlayingNow),
            Err(Error::Protocol)
        );
        let _ = request.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn selected_proxy_failure_never_falls_back_to_direct_network() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let (proxy, request, worker) = server(502, "", "generated proxy failure");
        let proxy_url = url::Url::parse(&proxy.endpoint)
            .unwrap()
            .origin()
            .ascii_serialization();
        let mut client = client("generated", Some(&proxy_url)).unwrap();
        client.endpoint = format!("http://{}/1/submit-listens", target.local_addr().unwrap());
        assert_eq!(
            client.submit(&track(), Submission::PlayingNow),
            Err(Error::Unavailable)
        );
        let request = request.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        assert!(request.starts_with(&format!("POST {} HTTP/1.1", client.endpoint)));
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn incomplete_input_is_rejected_without_any_network_request() {
        for token in ["", "generated\r\ninjection", "generated token", "é"] {
            assert!(matches!(client(token, None), Err(Error::Configuration)));
        }
        for proxy in [
            "",
            "http://127.0.0.1:9000/incorrect-path",
            "http://localhost?secret=x",
            "https://localhost",
        ] {
            assert!(matches!(
                client("generated", Some(proxy)),
                Err(Error::Configuration)
            ));
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client = client("generated", None).unwrap();
        client.endpoint = format!("http://{}/1/submit-listens", listener.local_addr().unwrap());
        for (artist, title, duration, at) in [
            ("", "Title", None, 1),
            ("Artist", "  ", None, 1),
            ("Artist", "Title", Some(0), 1),
            ("Artist", "Title", None, 0),
            ("Artist", "Title", None, u64::MAX),
        ] {
            let track = Track {
                artist,
                title,
                album: None,
                duration_ms: duration,
            };
            assert_eq!(
                client.submit(&track, Submission::Single { started_at: at }),
                Err(Error::InvalidListen)
            );
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn rate_state_blocks_early_retries_and_honors_success_response_limits() {
        for (headers, expected) in [
            ("", 1),
            (
                "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-In: 23\r\n",
                23,
            ),
        ] {
            let (client, request, worker) = server(200, headers, r#"{"status":"ok"}"#);
            assert_eq!(client.submit(&track(), Submission::PlayingNow), Ok(()));
            let _ = request.recv_timeout(Duration::from_secs(3)).unwrap();
            worker.join().unwrap();
            // The one-shot listener is now closed. A second HTTP call would be
            // Unavailable, so this also proves that no eager retry is attempted.
            assert_eq!(
                client.submit(&track(), Submission::PlayingNow),
                Err(Error::RateLimited {
                    retry_after_seconds: Some(expected)
                })
            );
            let mut rate = client.budget.0.blocking_lock();
            assert_eq!(rate.wait, Duration::from_secs(expected));
            rate.since = Instant::now() - rate.wait;
            drop(rate);
            assert_eq!(
                client.submit(&track(), Submission::PlayingNow),
                Err(Error::Unavailable)
            );
        }
        for contact in [
            "",
            "http://example.invalid",
            "https://user:secret@example.invalid",
            "https://example.invalid/?token=secret",
        ] {
            assert!(matches!(
                Client::new("generated", None, contact),
                Err(Error::Configuration)
            ));
        }
    }
}
