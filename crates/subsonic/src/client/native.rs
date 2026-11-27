//! Native credentials remain inside their immutable account client. No native
//! response, password or bearer token is returned in diagnostics.
use super::*;
use crate::{Album, AlbumListType, calls};

#[derive(Default)]
pub(super) struct State {
    navidrome: Option<bool>,
    token: Option<String>,
}

impl Client {
    /// An explicit seed belongs to a browsing session, not the server account.
    /// Serialize login/token renewal without holding a blocking mutex over I/O.
    pub async fn random_album_page_async(&self, seed: u32, offset: u32, count: u32) -> Result<(Vec<Album>, bool)> {
        if count == 0 || count > 100 { return Err(Error::Json("invalid random page size".into())); }
        let end = offset.checked_add(count).ok_or_else(|| Error::Json("random page offset overflow".into()))?;
        let mut state = self.native.lock().await;
        let native = match state.navidrome {
            Some(value) => value,
            None => {
                let ping = self.fetch_async(&calls::ping().request).await?;
                let value = ping.get("type").and_then(Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("navidrome"));
                state.navidrome = Some(value);
                value
            }
        };
        if !native {
            drop(state);
            return Ok((self.get_async(&calls::album_list(AlbumListType::Random, count, 0)).await?.album, false));
        }
        self.check_clock_async().await?;
        for attempt in 0..2 {
            if state.token.is_none() {
                let Auth::Password(password) = &self.creds.auth else {
                    return Err(Error::Api { code: 0, message: "Navidrome Random Albums requires password authentication".into() });
                };
                let body = serde_json::json!({"username":self.creds.user,"password":password});
                let response = self.native_agent.post(&format!("{}/auth/login", self.creds.server))
                    .set("Content-Type", "application/json").send(body.to_string().into()).await.map_err(native_error)?;
                let login = native_json(response).await?;
                let token = login.get("token").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 16384)
                    .ok_or_else(|| Error::Json("invalid native login response".into()))?;
                state.token = Some(token.to_string());
            }
            let request = self.native_agent.get(&format!("{}/api/album", self.creds.server))
                .set("X-ND-Authorization", &format!("Bearer {}", state.token.as_ref().unwrap()))
                .query("_sort", "random")
                .query("_order", "ASC")
                .query("seed", &seed.to_string())
                .query("_start", &offset.to_string())
                .query("_end", &end.to_string());
            let response = match request.send(Default::default()).await {
                Err(crate::http::Error::Status(401, _)) if attempt == 0 => { state.token = None; continue; }
                Err(crate::http::Error::Status(401, _)) => { state.token = None; return Err(Error::Http(401)); }
                other => other.map_err(native_error)?,
            };
            let value = native_json(response).await?;
            return tokio::task::spawn_blocking(move || {
                let array = value.as_array().filter(|a| a.len() <= count as usize)
                    .ok_or_else(|| Error::Json("invalid native album page".into()))?;
                let mut albums = Vec::with_capacity(array.len());
                for value in array {
                    let mut normalized = value.clone();
                    if let Some(object) = normalized.as_object_mut() {
                        object.remove("starred");
                        if let Some(created) = value.get("createdAt") { object.insert("created".into(), created.clone()); }
                    }
                    if let Some(duration) = value.get("duration").and_then(Value::as_f64) {
                        normalized["duration"] = Value::from(duration.clamp(0.0, u32::MAX as f64) as u32);
                    }
                    let mut album: Album = serde_json::from_value(normalized).map_err(|_| Error::Json("invalid native album".into()))?;
                    if album.id.is_empty() || album.name.is_empty() { return Err(Error::Json("invalid native album identity".into())); }
                    // Native albums use albumArtist rather than Subsonic artist;
                    // artwork is addressed by Subsonic album identifier.
                    album.artist = value.get("albumArtist").and_then(Value::as_str).unwrap_or(&album.artist).to_string();
                    album.cover_art = Some(format!("al-{}", album.id));
                    albums.push(album);
                }
                Ok((albums, true))
            }).await.map_err(|_| Error::Network("native album parser stopped".into()))?;
        }
        Err(Error::Http(401))
    }
}

fn native_error(error: crate::http::Error) -> Error {
    match error {
        crate::http::Error::Status(code, _) => Error::Http(code),
        crate::http::Error::Transport(_) => Error::Network("native request failed".into()),
    }
}
async fn native_json(response: crate::http::Response) -> Result<Value> {
    if !(200..300).contains(&response.status()) { return Err(Error::Http(response.status())); }
    let bytes = media_bytes(response, 1024 * 1024).await?;
    tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes).map_err(|_| Error::Json("invalid native response".into())))
        .await.map_err(|_| Error::Network("native response parser stopped".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::{BufRead, BufReader, Read, Write}, net::TcpListener};
    struct Worker(Option<std::thread::JoinHandle<()>>);
    impl Worker {
        fn join(mut self) -> std::thread::Result<()> { self.0.take().unwrap().join() }
    }
    impl Drop for Worker {
        fn drop(&mut self) { if let Some(worker) = self.0.take() { let _ = worker.join(); } }
    }
    fn fixture(steps: Vec<(&'static str, u16, &'static str)>, auth: Auth) -> (Client, Worker) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/music", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            for (expected, status, body) in steps {
                let deadline = std::time::Instant::now() + Duration::from_secs(4);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(socket) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                        Err(error) => panic!("generated fixture accept failed or timed out: {error}"),
                    }
                };
                socket.set_write_timeout(Some(Duration::from_secs(4))).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" { break; }
                    headers.push_str(&line);
                }
                assert!(headers.contains(expected), "unexpected generated request");
                let length = headers.lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").and_then(|v| v.parse::<usize>().ok())).unwrap_or(0);
                let mut payload = vec![0; length]; reader.read_exact(&mut payload).unwrap();
                if expected.contains("auth/login") {
                    let value: Value = serde_json::from_slice(&payload).unwrap();
                    assert_eq!(value, serde_json::json!({"username":"generated","password":"fixture-password"}));
                } else if headers.contains("/api/album?") {
                    assert!(headers.to_lowercase().contains("x-nd-authorization: bearer fixture-token"));
                    assert!(!headers.contains("fixture-password"));
                }
                write!(socket, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (Client::new(Credentials { server:base, user:"generated".into(), auth }, "fixture"), Worker(Some(worker)))
    }
    const PING: &str = r#"{"subsonic-response":{"status":"ok","type":"navidrome"}}"#;
    const LOGIN: &str = r#"{"token":"fixture-token"}"#;
    const ALBUM: &str = r#"[{"id":"generated","name":"Fixture","albumArtist":"Artist","songCount":2,"duration":4.5}]"#;
    fn password() -> Auth { Auth::Password("fixture-password".into()) }
    #[test]
    fn native_seeded_pages_cache_login_and_renew_once() {
        let (client, worker) = fixture(vec![
            ("GET /music/rest/ping?",200,PING), ("POST /music/auth/login ",200,LOGIN),
            ("seed=42&_start=0&_end=100",200,ALBUM),
            ("seed=42&_start=100&_end=200",401,"{}"), ("POST /music/auth/login ",200,LOGIN),
            ("seed=42&_start=100&_end=200",200,"[]"),
            ("seed=42&_start=0&_end=100",200,ALBUM),
        ], password());
        super::super::wait(async move {
            let (albums, native) = client.random_album_page_async(42,0,100).await?;
            assert!(native); assert_eq!(albums[0].artist,"Artist"); assert_eq!(albums[0].duration,4);
            assert!(client.random_album_page_async(42,100,100).await?.0.is_empty());
            assert_eq!(client.random_album_page_async(42,0,100).await?.0, albums);
            Ok(())
        }).unwrap(); worker.join().unwrap();
    }
    #[test]
    fn unknown_server_uses_standard_random_and_never_native_login() {
        let (client, worker) = fixture(vec![
            ("GET /music/rest/ping?",200,r#"{"subsonic-response":{"status":"ok","type":"other"}}"#),
            ("type=random&size=100&offset=0",200,r#"{"subsonic-response":{"status":"ok","albumList2":{"album":[]}}}"#),
        ], password());
        assert!(!super::super::wait(async move { client.random_album_page_async(9,100,100).await }).unwrap().1);
        worker.join().unwrap();
    }
    #[test]
    fn api_key_only_native_is_explicit_and_auth_error_has_no_server_body() {
        let (client, worker) = fixture(vec![("GET /music/rest/ping?",200,PING)], Auth::ApiKey("generated-key".into()));
        let error = super::super::wait(async move { client.random_album_page_async(9,0,100).await }).unwrap_err();
        assert!(error.to_string().contains("requires password authentication")); worker.join().unwrap();
        let (client, worker) = fixture(vec![("GET /music/rest/ping?",200,PING),("POST /music/auth/login ",401,r#"{"error":"fixture-private"}"#)], password());
        let error = super::super::wait(async move { client.random_album_page_async(9,0,100).await }).unwrap_err();
        assert!(error.is_auth()); assert!(!error.to_string().contains("fixture-private")); worker.join().unwrap();
    }
    #[test]
    fn native_status_malformed_and_exhausted_auth_are_bounded() {
        for (status, body) in [(503, "{}"), (200, "{}"), (200, "[{\"id\":\"x\"}]"), (302, "{}"), (200, "invalid")] {
            let (client, worker) = fixture(vec![
                ("GET /music/rest/ping?",200,PING), ("POST /music/auth/login ",200,LOGIN),
                ("seed=1&_start=0&_end=100",status,body),
            ], password());
            let result = super::super::wait(async move { client.random_album_page_async(1,0,100).await });
            assert!(result.is_err()); worker.join().unwrap();
        }
        let (client, worker) = fixture(vec![
            ("GET /music/rest/ping?",200,PING), ("POST /music/auth/login ",200,LOGIN),
            ("seed=1&_start=0&_end=100",401,"{}"), ("POST /music/auth/login ",200,LOGIN),
            ("seed=1&_start=0&_end=100",401,"{}"),
        ], password());
        assert!(super::super::wait(async move { client.random_album_page_async(1,0,100).await }).unwrap_err().is_auth());
        worker.join().unwrap();
    }

}
