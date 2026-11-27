use super::{Credentials, EncodeError, Listen, now_playing, scrobbles};
use serde_json::Value;

const ENDPOINT: &str = "https://ws.audioscrobbler.com/2.0/";
const MAX_RESPONSE: usize = 256 * 1024;

/// Item filtering is an acknowledged outcome, not an accepted listen or a
/// transport failure. Preserve unknown future codes for explicit worker policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Filtered(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Input(EncodeError),
    Authentication,
    RateLimited { retry_after_seconds: Option<u64> },
    Unavailable,
    Rejected(u64),
    Protocol,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration => "Invalid Last.fm configuration",
            Self::Input(_) => "Invalid Last.fm listen",
            Self::Authentication => "Last.fm authorization required",
            Self::RateLimited { .. } => "Last.fm rate limit reached",
            Self::Unavailable => "Last.fm unavailable",
            Self::Rejected(_) => "Last.fm request rejected",
            Self::Protocol => "Unsupported Last.fm response",
        })
    }
}
impl std::error::Error for Error {}

/// Async and opt-in. Construction performs no requests. The durable worker
/// owns retry scheduling, per-account ordering and individual acknowledgements.
/// No Debug implementation: application credentials/session are private.
pub struct Client {
    api_key: String,
    secret: String,
    session: String,
    agent: crate::http::Agent,
    endpoint: String,
}
impl Client {
    pub fn new(
        credentials: &Credentials<'_>,
        proxy: Option<&str>,
        contact_url: &str,
    ) -> Result<Self, Error> {
        super::base(credentials, "track.scrobble").map_err(|_| Error::Configuration)?;
        Ok(Self {
            api_key: credentials.api_key.into(),
            secret: credentials.shared_secret.into(),
            session: credentials.session.into(),
            endpoint: ENDPOINT.into(),
            agent: crate::scrobble_http::agent(proxy, contact_url)
                .map_err(|_| Error::Configuration)?,
        })
    }
    /// Generated app-tier controls only; production always uses the HTTPS API.
    #[cfg(feature = "scrobble-fixtures")]
    pub fn generated_loopback(credentials: &Credentials<'_>, endpoint: std::net::SocketAddrV4) -> Self {
        assert!(endpoint.ip().is_loopback());
        let mut client = Self::new(credentials, None, "https://contact.invalid/q2").unwrap();
        client.endpoint = format!("http://{endpoint}/2.0/");
        client
    }

    #[cfg(test)]
    fn now_playing(&self, listen: &Listen<'_>) -> Result<Outcome, Error> {
        crate::scrobble_http::wait(self.now_playing_async(listen))
    }
    #[cfg(test)]
    fn scrobble(&self, listens: &[Listen<'_>]) -> Result<Vec<Outcome>, Error> {
        crate::scrobble_http::wait(self.scrobble_async(listens))
    }
    pub async fn now_playing_async(&self, listen: &Listen<'_>) -> Result<Outcome, Error> {
        let body = self.encode(vec![OwnedListen::from(listen)], false).await?;
        let value = self.post_async(body).await?;
        item(value.get("nowplaying").ok_or(Error::Protocol)?)
    }
    pub async fn scrobble_async(&self, listens: &[Listen<'_>]) -> Result<Vec<Outcome>, Error> {
        // Reject the encoder's invalid batch before allocating owned copies.
        if listens.is_empty() || listens.len() > 50 { return Err(Error::Input(EncodeError::BatchSize)); }
        let body = self.encode(listens.iter().map(OwnedListen::from).collect(), true).await?;
        let value = self.post_async(body).await?;
        acknowledgements(&value, listens)
    }
    async fn encode(&self, listens: Vec<OwnedListen>, batch: bool) -> Result<String, Error> {
        let (api_key, shared_secret, session) = (self.api_key.clone(), self.secret.clone(), self.session.clone());
        tokio::task::spawn_blocking(move || {
            let credentials = Credentials { api_key: &api_key, shared_secret: &shared_secret, session: &session };
            let listens: Vec<_> = listens.iter().map(OwnedListen::borrow).collect();
            let body = if batch { scrobbles(&credentials, &listens) }
                else { now_playing(&credentials, &listens[0]) };
            body.map(|body| body.as_str().to_owned()).map_err(Error::Input)
        }).await.map_err(|_| Error::Unavailable)?
    }
    async fn post_async(&self, body: String) -> Result<Value, Error> {
        let response = match self
            .agent
            .post(&self.endpoint)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .set("Accept", "application/json")
            .send(body.into()).await
        {
            Ok(response) | Err(crate::http::Error::Status(_, response)) => response,
            Err(_) => return Err(Error::Unavailable),
        };
        let status = response.status();
        let retry = response
            .header("Retry-After")
            .and_then(|v| decimal(&Value::String(v.trim().into())));
        match status {
            401 | 403 => return Err(Error::Authentication),
            429 => {
                return Err(Error::RateLimited {
                    retry_after_seconds: retry,
                });
            }
            408 | 500..=599 => return Err(Error::Unavailable),
            200 | 400 => {}
            _ => return Err(Error::Protocol), // redirects never followed
        }
        let value = crate::scrobble_http::json(response, MAX_RESPONSE).await.map_err(|error| match error {
            crate::scrobble_http::BodyError::Transport => Error::Unavailable,
            crate::scrobble_http::BodyError::Protocol => Error::Protocol,
        })?;
        if let Some(code) = value.get("error") {
            return Err(match decimal(code).ok_or(Error::Protocol)? {
                4 | 9 => Error::Authentication,
                10 | 13 | 26 => Error::Configuration,
                11 | 16 => Error::Unavailable,
                29 => Error::RateLimited {
                    retry_after_seconds: retry,
                },
                code => Error::Rejected(code),
            });
        }
        if status != 200 {
            return Err(Error::Protocol);
        }
        Ok(value)
    }
}

// Owned only across finite request encoding, never a socket wait on a worker.
struct OwnedListen {
    artist: String,
    title: String,
    album: Option<String>,
    duration_seconds: Option<u64>,
    started_at: u64,
    is_broadcast: bool,
}
impl From<&Listen<'_>> for OwnedListen {
    fn from(value: &Listen<'_>) -> Self {
        Self {
            artist: value.artist.to_owned(),
            title: value.title.to_owned(),
            album: value.album.map(str::to_owned),
            duration_seconds: value.duration_seconds,
            started_at: value.started_at,
            is_broadcast: value.is_broadcast,
        }
    }
}
impl OwnedListen {
    fn borrow(&self) -> Listen<'_> {
        Listen {
            artist: &self.artist,
            title: &self.title,
            album: self.album.as_deref(),
            duration_seconds: self.duration_seconds,
            started_at: self.started_at,
            is_broadcast: self.is_broadcast,
        }
    }
}

fn decimal(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    let text = value.as_str()?;
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}
fn item(value: &Value) -> Result<Outcome, Error> {
    let code = value
        .get("ignoredMessage")
        .and_then(|v| v.get("code"))
        .and_then(decimal)
        .ok_or(Error::Protocol)?;
    Ok(if code == 0 {
        Outcome::Accepted
    } else {
        Outcome::Filtered(code)
    })
}
fn acknowledgements(value: &Value, sent: &[Listen<'_>]) -> Result<Vec<Outcome>, Error> {
    let batch = value.get("scrobbles").ok_or(Error::Protocol)?;
    // JSON translations use @attr for the parent attributes. Also accept the
    // direct attribute representation documented by the REST translation, but
    // reject conflicting duplicate summaries rather than choose one silently.
    let attrs = batch.get("@attr").unwrap_or(batch);
    let accepted = attrs
        .get("accepted")
        .and_then(decimal)
        .ok_or(Error::Protocol)?;
    let ignored = attrs
        .get("ignored")
        .and_then(decimal)
        .ok_or(Error::Protocol)?;
    if attrs != batch {
        for (name, expected) in [("accepted", accepted), ("ignored", ignored)] {
            if batch
                .get(name)
                .is_some_and(|v| decimal(v) != Some(expected))
            {
                return Err(Error::Protocol);
            }
        }
    }
    if accepted.checked_add(ignored) != Some(sent.len() as u64) {
        return Err(Error::Protocol);
    }
    let entries = batch.get("scrobble").ok_or(Error::Protocol)?;
    let entries: Vec<&Value> = match entries.as_array() {
        Some(entries) => entries.iter().collect(),
        None if sent.len() == 1 => vec![entries],
        None => return Err(Error::Protocol),
    };
    if entries.len() != sent.len() {
        return Err(Error::Protocol);
    }
    let mut outcomes = Vec::with_capacity(sent.len());
    for (entry, listen) in entries.into_iter().zip(sent) {
        if entry.get("timestamp").and_then(decimal) != Some(listen.started_at) {
            return Err(Error::Protocol);
        }
        outcomes.push(item(entry)?);
    }
    if outcomes.iter().filter(|o| **o == Outcome::Accepted).count() as u64 != accepted {
        return Err(Error::Protocol);
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use super::*;
    use serde_json::json;
    use std::{
        io::Write,
        net::TcpListener,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    fn credentials() -> Credentials<'static> {
        Credentials {
            api_key: "generated-key",
            shared_secret: "generated-secret",
            session: "generated-session",
        }
    }
    fn listen(at: u64) -> Listen<'static> {
        Listen {
            artist: "Synthetic Café",
            title: "A+B 🎵",
            album: None,
            duration_seconds: None,
            started_at: at,
            is_broadcast: true,
        }
    }
    fn entry(at: u64, code: u64) -> Value {
        json!({"timestamp":at.to_string(),"ignoredMessage":{"code":code.to_string(),"#text":"generated private response"}})
    }
    fn single() -> Value {
        json!({"scrobbles":{"@attr":{"accepted":"1","ignored":"0"},"scrobble":entry(100,0)}})
    }
    fn server(
        status: u16,
        headers: &str,
        body: &str,
    ) -> (Client, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut client =
            Client::new(&credentials(), None, "https://example.invalid/q2-generated").unwrap();
        client.endpoint = format!("http://{}/2.0/", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status} Generated\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        );
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(3);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("generated accept deadline: {e}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
                assert!(bytes.len() < 8192);
            }
            let header = String::from_utf8(bytes.clone()).unwrap();
            let length = header
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap();
            assert!(length < 65536);
            let mut body = vec![0; length];
            socket.read_exact(&mut body).unwrap();
            bytes.extend(body);
            tx.send(String::from_utf8(bytes).unwrap()).unwrap();
            let _ = socket.write_all(response.as_bytes());
        });
        (client, rx, worker)
    }
    #[test]
    fn wire_request_and_individual_acknowledgements_preserve_order() {
        let reply = json!({"scrobbles":{"@attr":{"accepted":"1","ignored":"1"},"scrobble":[entry(100,0),entry(101,2)]}});
        let (client, rx, worker) = server(200, "", &reply.to_string());
        let library = Listen {
            album: Some("Synthetic Album"),
            duration_seconds: Some(120),
            is_broadcast: false,
            ..listen(100)
        };
        assert_eq!(
            client.scrobble(&[library, listen(101)]),
            Ok(vec![Outcome::Accepted, Outcome::Filtered(2)])
        );
        let request = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /2.0/ HTTP/1.1\r\n"));
        assert!(!head.contains("generated-session"));
        assert!(!body.contains("generated-secret"));
        let values: std::collections::BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(values["artist[1]"], "Synthetic Café");
        assert_eq!(values["track[0]"], "A+B 🎵");
        assert_eq!(values["album[0]"], "Synthetic Album");
        assert_eq!(values["duration[0]"], "120");
        assert!(!values.contains_key("chosenByUser[0]"));
        assert_eq!(values["chosenByUser[1]"], "0");
        // Python hashlib over the mixed batch's literal raw name/value pairs.
        assert_eq!(values["api_sig"], "6e21f864f136e7910339c833fb73dc8c");
        assert_eq!(values["sk"], "generated-session");
        let (client, rx, worker) = server(200, "", &single().to_string());
        assert_eq!(client.scrobble(&[listen(100)]), Ok(vec![Outcome::Accepted]));
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        let (client, rx, worker) =
            server(200, "", r#"{"nowplaying":{"ignoredMessage":{"code":"0"}}}"#);
        assert_eq!(client.now_playing(&listen(100)), Ok(Outcome::Accepted));
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn malformed_or_mismatched_batch_never_acknowledges_any_item() {
        let sent = [listen(100)];
        assert_eq!(
            acknowledgements(&single(), &sent),
            Ok(vec![Outcome::Accepted])
        );
        for bad in [
            json!({}),
            json!({"scrobbles":{"@attr":{"accepted":"1","ignored":"0"},"scrobble":entry(101,0)}}),
            json!({"scrobbles":{"@attr":{"accepted":"1","ignored":"0"},"scrobble":entry(100,2)}}),
            json!({"scrobbles":{"@attr":{"accepted":"1","ignored":"0"},"accepted":"0","scrobble":entry(100,0)}}),
            json!({"scrobbles":{"accepted":"1","ignored":"0","scrobble":[entry(100,0),entry(100,0)]}}),
        ] {
            assert_eq!(acknowledgements(&bad, &sent), Err(Error::Protocol));
        }
        let reversed = json!({"scrobbles":{"@attr":{"accepted":"2","ignored":"0"},"scrobble":[entry(101,0),entry(100,0)]}});
        assert_eq!(
            acknowledgements(&reversed, &[listen(100), listen(101)]),
            Err(Error::Protocol)
        );
        for code in 0..=50 {
            let reply = json!({"scrobbles":{"accepted":u64::from(code==0),"ignored":u64::from(code!=0),"scrobble":entry(100,code)}});
            assert_eq!(
                acknowledgements(&reply, &sent),
                Ok(vec![if code == 0 {
                    Outcome::Accepted
                } else {
                    Outcome::Filtered(code)
                }])
            );
        }
    }
    #[test]
    fn http_success_does_not_hide_api_failures_or_leak_private_text() {
        for (status, body, expected) in [
            (
                200,
                json!({"error":9,"message":"private"}),
                Error::Authentication,
            ),
            (200, json!({"error":11}), Error::Unavailable),
            (200, json!({"error":16}), Error::Unavailable),
            (400, json!({"error":13}), Error::Configuration),
            (
                200,
                json!({"error":29}),
                Error::RateLimited {
                    retry_after_seconds: None,
                },
            ),
            (200, json!({"error":6}), Error::Rejected(6)),
            (503, json!({"message":"private"}), Error::Unavailable),
            (400, single(), Error::Protocol),
            (200, json!({"error":"bad"}), Error::Protocol),
        ] {
            let (client, rx, worker) = server(status, "", &body.to_string());
            assert_eq!(client.scrobble(&[listen(100)]), Err(expected));
            rx.recv_timeout(Duration::from_secs(3)).unwrap();
            worker.join().unwrap();
            assert!(!expected.to_string().contains("private"));
        }
    }
    #[test]
    fn redirects_caps_rate_headers_and_proxy_failure_are_bounded() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let address = format!("http://{}/2.0/", target.local_addr().unwrap());
        for (status, headers, body, expected) in [
            (
                307,
                format!("Location: {address}\r\n"),
                String::new(),
                Error::Protocol,
            ),
            (
                429,
                "Retry-After: 31\r\n".into(),
                String::new(),
                Error::RateLimited {
                    retry_after_seconds: Some(31),
                },
            ),
            (
                200,
                String::new(),
                "x".repeat(MAX_RESPONSE + 1),
                Error::Protocol,
            ),
        ] {
            let (client, rx, worker) = server(status, &headers, &body);
            assert_eq!(client.scrobble(&[listen(100)]), Err(expected));
            rx.recv_timeout(Duration::from_secs(3)).unwrap();
            worker.join().unwrap();
        }
        let (proxy, rx, worker) = server(502, "", "");
        let proxy = url::Url::parse(&proxy.endpoint)
            .unwrap()
            .origin()
            .ascii_serialization();
        let mut client = Client::new(
            &credentials(),
            Some(&proxy),
            "https://example.invalid/q2-generated",
        )
        .unwrap();
        client.endpoint = address;
        assert_eq!(client.scrobble(&[listen(100)]), Err(Error::Unavailable));
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
