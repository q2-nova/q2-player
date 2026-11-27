use crate::{Error, Request, Result, API_VERSION};
#[cfg(test)]
use crate::Call;
use md5::{Digest, Md5};
use crate::document::Parser;
use crate::document::Streamed;
use serde_json::Value;
#[cfg(test)]
use std::io::Read;
use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// Sent as a salted token (`t`, `s`), never in the clear.
    Password(String),
    /// OpenSubsonic API key (`apiKey`).
    ApiKey(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The server's base URL, e.g. `https://music.example.com` or
    /// `http://192.168.1.10:4533`. A trailing `/` is ignored.
    pub server: String,
    pub user: String,
    pub auth: Auth,
}

/// What the server should send for a stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamOptions {
    /// Target format (`mp3`, `opus`, `aac`, …), or `raw` for the original
    /// file. `None` leaves it to the server's transcoding settings.
    pub format: Option<String>,
    /// Maximum bit rate in kbit/s; 0 or `None` for no limit.
    pub max_bit_rate: Option<u32>,
}

#[cfg(test)]
pub struct StreamResponse {
    pub reader: Box<dyn Read + Send>,
    pub content_type: Option<String>,
    /// Bytes in this response (from the requested offset).
    pub content_length: Option<u64>,
    /// Whether the server honoured the requested byte offset.
    pub partial: bool,
}

/// The same checked media response, with an async body owner. Callers choose
/// bounded decoded chunks or the compatibility blocking reader explicitly.
pub struct AsyncStreamResponse {
    pub response: crate::http::Response,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub partial: bool,
}

mod native;

pub struct Client {
    native: tokio::sync::Mutex<native::State>,
    agent: crate::http::Agent,
    native_agent: crate::http::Agent,
    stream_agent: crate::http::Agent,
    creds: Credentials,
    client_name: String,
    proxy: Option<String>,
}

impl Client {
    pub fn new(creds: Credentials, client_name: &str) -> Self {
        Self::with_proxy(creds, client_name, None).expect("no proxy")
    }

    /// Both API and media requests use this proxy, including DNS resolution.
    /// A failed proxy never falls back to a direct connection.
    pub fn with_proxy(creds: Credentials, client_name: &str, proxy: Option<&str>) -> Result<Self> {
        let proxy_config = proxy.map(crate::http::Proxy::new).transpose().map_err(|e| Error::Network(e.to_string()))?;
        // Optional explicitly provisioned DER trust root for a private server.
        // A missing/bad file fails closed; never disable certificate validation.
        let tls = if let Some(path) = std::env::var_os("Q2ND_TLS_CA") {
            let cert = std::fs::read(path).map_err(|e| Error::Network(format!("TLS trust root: {e}")))?;
            Some(crate::http::tls_trusting(cert).map_err(|e| Error::Network(format!("TLS trust root: {e}")))?)
        } else { None };
        let builder = || {
            let mut b = crate::http::AgentBuilder::new();
            if let Some(tls) = &tls { b = b.tls_config(tls.clone()); }
            match &proxy_config {
                Some(p) => b.proxy(p.clone()),
                None => b,
            }
        };
        let native_agent = native_agent(builder(), client_name);
        let agent = builder()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(20))
            .timeout_write(Duration::from_secs(20))
            .user_agent(client_name)
            .build();
        // Streams can stall while the server transcodes; the player decides
        // when to give up, so only the connect has a deadline.
        let stream_agent = builder()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(60))
            .user_agent(client_name)
            .build();
        let mut creds = creds;
        while creds.server.ends_with('/') {
            creds.server.pop();
        }
        Ok(Client { native: Default::default(), native_agent, agent, stream_agent, creds, client_name: client_name.to_string(), proxy: proxy.map(str::to_owned) })
    }

    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    pub fn credentials(&self) -> &Credentials {
        &self.creds
    }

    fn auth_params(&self) -> Vec<(&'static str, String)> {
        let mut p = vec![
            ("v", API_VERSION.to_string()),
            ("c", self.client_name.clone()),
            ("f", "json".to_string()),
        ];
        match &self.creds.auth {
            Auth::Password(pw) => {
                let salt: String = (0..12).map(|_| fastrand::alphanumeric()).collect();
                let token = Md5::digest(format!("{pw}{salt}").as_bytes());
                p.push(("u", self.creds.user.clone()));
                p.push(("t", hex(&token)));
                p.push(("s", salt));
            }
            Auth::ApiKey(key) => p.push(("apiKey", key.clone())),
        }
        p
    }

    /// The full URL for a request, credentials included. For URLs handed to
    /// something else (an image or audio fetch); never log it.
    pub fn url(&self, request: &Request) -> String {
        let mut url = format!("{}/rest/{}", self.creds.server, request.endpoint);
        let mut first = true;
        for (k, v) in self.auth_params().iter().chain(request.params.iter()) {
            url.push(if first { '?' } else { '&' });
            first = false;
            url.push_str(k);
            url.push('=');
            url.push_str(&encode(v));
        }
        url
    }

    // Mainline service policy: a build-date floor is not trusted time.
    fn clock_status_path(&self) -> Option<std::ffi::OsString> {
        url::Url::parse(&self.creds.server).is_ok_and(|url| url.scheme() == "https")
            .then(|| std::env::var_os("Q2ND_CLOCK_STATUS")).flatten()
    }

    fn check_clock_async(&self) -> impl Future<Output = Result<()>> + Send + use<> {
        let path = self.clock_status_path();
        async move {
            if let Some(path) = path {
                tokio::task::spawn_blocking(move || check_clock_path(path)).await
                    .map_err(|_| Error::Network("Unable to check synchronized clock".into()))??;
            }
            Ok(())
        }
    }

    /// Fetches the checked object inside `subsonic-response`.
    #[cfg(test)]
    pub fn fetch(&self, request: &Request) -> Result<Value> {
        wait(self.fetch_async(request))
    }

    /// The checked body of a small reply (ping, actions) as JSON. Network
    /// waits stay async; parsing runs in short jobs off the executor,
    /// including HTTP-error envelopes.
    pub fn fetch_async(&self, request: &Request) -> impl Future<Output = Result<Value>> + Send + use<> {
        let endpoint = request.endpoint;
        let parser = Parser::<Value>::new(None, endpoint);
        let clock = self.check_clock_async();
        let request = self.agent.get(&self.url(request));
        async move {
            clock.await?;
            match request.send(Default::default()).await {
                Ok(response) => stream(response, parser).await.map(|(value, _)| value),
                Err(error) => Err(map_err_async(error).await),
            }
        }
    }

    /// Fetches and parses in one step.
    #[cfg(test)]
    pub fn get<T: Streamed + Send + 'static>(&self, call: &Call<T>) -> Result<T> {
        wait(self.get_async_owned(call))
    }

    /// Fetches a typed call, parsed as it arrives (`document.rs`).
    pub async fn get_async<T: Streamed + Send + 'static>(&self, call: &crate::Call<T>) -> Result<T> {
        self.document_async(call).await.map(|(value, _)| value)
    }

    #[cfg(test)]
    fn get_async_owned<T: Streamed + Send + 'static>(&self, call: &crate::Call<T>) -> impl Future<Output = Result<T>> + Send + use<T> {
        let document = self.document_async(call);
        async move { document.await.map(|(value, _)| value) }
    }

    /// A typed call's result and the bytes of its document. Each received
    /// piece is scanned in a short job; no complete text is kept.
    pub fn document_async<T: Streamed + Send + 'static>(&self, call: &crate::Call<T>) -> impl Future<Output = Result<(T, u64)>> + Send + use<T> {
        let parser = call.parser();
        let clock = self.check_clock_async();
        let request = self.agent.get(&self.url(&call.request));
        async move {
            clock.await?;
            match request.send(Default::default()).await {
                Ok(response) => stream(response, parser).await,
                Err(error) => Err(map_err_async(error).await),
            }
        }
    }

    pub fn stream_request(&self, id: &str, opts: &StreamOptions) -> Request {
        Request::new("stream")
            .param("id", id)
            .opt("format", opts.format.as_deref())
            .opt("maxBitRate", opts.max_bit_rate.filter(|b| *b > 0))
    }

    /// The original file, for downloads.
    pub fn download_request(&self, id: &str) -> Request {
        Request::new("download").param("id", id)
    }

    pub fn cover_art_request(&self, id: &str, size: Option<u32>) -> Request {
        Request::new("getCoverArt").param("id", id).opt("size", size)
    }

    /// Opens a binary response (a stream, download or image), from `offset`
    /// bytes in when the server supports ranges.
    #[cfg(test)]
    pub fn open(&self, request: &Request, offset: u64) -> Result<StreamResponse> {
        let response = wait(self.open_async(request, offset))?;
        Ok(StreamResponse { reader: response.response.into_reader(), content_type: response.content_type,
                            content_length: response.content_length, partial: response.partial })
    }

    pub fn open_async(&self, request: &Request, offset: u64) -> impl Future<Output = Result<AsyncStreamResponse>> + Send + use<> {
        let clock = self.check_clock_async();
        let mut req = self.stream_agent.get(&self.url(request));
        if offset > 0 {
            req = req.set("Range", &format!("bytes={offset}-"));
        }
        async move {
            clock.await?;
            let resp = match req.send(Default::default()).await {
                Ok(response) => response,
                Err(error) => return Err(map_err_async(error).await),
            };
            let content_type = resp.header("Content-Type").map(str::to_string);
            // Error documents retain the existing bounded off-executor parser.
            if content_type.as_deref().is_some_and(|t| t.contains("json") || t.contains("xml")) {
                document(resp).await?;
                return Err(Error::Json("expected media, got a document".into()));
            }
            let partial = resp.status() == 206;
            let content_length = resp.header("Content-Length").and_then(|v| v.parse().ok());
            Ok(AsyncStreamResponse { response: resp, content_type, content_length, partial })
        }
    }

    /// Direct download response with optional conditional range; caller must
    /// validate status, validator, encoding and Content-Range before writing.
    pub fn download_response_async(&self, request:&Request, offset:u64, validator:Option<&str>) -> impl Future<Output=Result<crate::http::Response>> + Send + use<> {
        let clock=self.check_clock_async();
        let mut req=self.stream_agent.get(&self.url(request)).set("Accept-Encoding","identity");
        if offset>0 { if let Some(tag)=validator {req=req.set("Range",&format!("bytes={offset}-")).set(if tag.starts_with('"') {"If-Range"} else {"If-Unmodified-Since"},tag);} }
        async move {
            clock.await?;
            let resp=match req.send(Default::default()).await {
                Ok(response)=>response,
                Err(crate::http::Error::Status(412|416,response)) if offset>0=>response,
                Err(error)=>return Err(map_err_async(error).await),
            };
            let kind=resp.header("Content-Type").map(str::to_string);
            if kind.as_deref().is_some_and(|t|t.contains("json") || t.contains("xml")) {document(resp).await?;return Err(Error::Json("expected media, got a document".into()));}
            Ok(resp)
        }
    }

    /// A bounded random-access read of an original stream. Never interpret a
    /// server's full-file response as bytes at the requested offset.
    #[cfg(test)]
    pub fn read_range(&self, request: &Request, offset: u64, length: usize) -> Result<Option<Vec<u8>>> {
        wait(self.read_range_async(request, offset, length))
    }
    pub fn read_range_async(&self, request: &Request, offset: u64, length: usize) -> impl Future<Output = Result<Option<Vec<u8>>>> + Send + use<> {
        let clock=self.check_clock_async();
        let request=self.stream_agent.get(&self.url(request));
        async move {
            clock.await?;
            if length == 0 || length > 256 * 1024 {
                return Err(Error::Json("invalid range size".into()));
            }
            let end = offset.checked_add(length as u64 - 1).ok_or_else(|| Error::Json("range overflow".into()))?;
            let resp = match request.set("Range", &format!("bytes={offset}-{end}"))
                .set("Accept-Encoding", "identity").timeout(Duration::from_secs(10))
                .send(Default::default()).await {
                    Ok(response)=>response,
                    Err(error)=>return Err(map_err_async(error).await),
                };
            if resp.status() == 200 { return Ok(None); }
            let range = resp.header("Content-Range").and_then(|s| s.strip_prefix("bytes "))
                .and_then(|s| s.split_once('/'))
                .and_then(|(span, total)| {
                    let (start, end) = span.split_once('-')?;
                    Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
                });
            let Some((start, last, _total)) = range.filter(|&(start, last, total)|
                resp.status() == 206 && start == offset && last >= start && last <= end && last < total) else {
                return Err(Error::Json("invalid media Content-Range".into()));
            };
            let expected = (last - start + 1) as usize;
            // Content-Range counts decoded media bytes. Some servers still
            // gzip despite identity preference; wire overhead is not a bound
            // on the decoded range. Keep the decoded cap and exact-length check.
            let bytes = decoded_media_bytes(resp,expected+1).await?;
            if bytes.len() != expected { return Err(Error::Json("incomplete media range".into())); }
            Ok(Some(bytes))
        }
    }

    /// Reads a small binary response (cover art) whole.
    #[cfg(test)]
    pub fn fetch_bytes(&self, request: &Request, limit: usize) -> Result<Vec<u8>> {
        wait(self.fetch_bytes_async(request, limit))
    }
    pub fn fetch_bytes_async(&self, request: &Request, limit: usize) -> impl Future<Output = Result<Vec<u8>>> + Send + use<> {
        let clock = self.check_clock_async();
        let request = self.stream_agent.get(&self.url(request));
        async move {
            clock.await?;
            let response = match request.send(Default::default()).await {
                Ok(response) => response,
                Err(error) => return Err(map_err_async(error).await),
            };
            if response.header("Content-Type").is_some_and(|t| t.contains("json") || t.contains("xml")) {
                document(response).await?;
                return Err(Error::Json("expected media, got a document".into()));
            }
            media_bytes(response, limit).await
        }
    }

    /// Offline downloads distinguish genuine absence from failed transfers,
    /// authentication or unsupported images. Error bodies stay private.
    #[cfg(test)]
    pub fn cover_art(&self, id: &str, size: u32) -> Result<Option<Vec<u8>>> {
        wait(self.cover_art_async(id, size))
    }
    pub fn cover_art_async(&self, id: &str, size: u32) -> impl Future<Output = Result<Option<Vec<u8>>>> + Send + use<> {
        let valid = !id.is_empty();
        let clock = self.check_clock_async();
        let request = self.agent.get(&self.url(&self.cover_art_request(id, Some(size.clamp(1,1024)))));
        async move {
            if !valid { return Err(Error::Json("missing cover identity".into())); }
            clock.await?;
            let response = match request.send(Default::default()).await {
                Ok(response) | Err(crate::http::Error::Status(404, response)) => response,
                Err(crate::http::Error::Status(status, _)) => return Err(Error::Http(status)),
                Err(_) => return Err(Error::Network("Cover art unavailable".into())),
            };
            let missing = response.status() == 404;
            let bytes = media_bytes(response, 4*1024*1024).await?;
            // XML/JSON error inspection is CPU work too, not an executor poll.
            tokio::task::spawn_blocking(move || match super::cover::error_code(&bytes) {
                Some(70) => Ok(None),
                Some(code) => Err(Error::Api {code, message:"Cover art request failed".into()}),
                None if missing => Ok(None),
                None => Ok(Some(bytes)),
            }).await.map_err(|_| Error::Network("Cover art processing failed".into()))?
        }
    }

    /// Fetch an image URL supplied by the server. It may be a signed URL;
    /// never append API credentials. Preserve media proxy/TLS policy.
    #[cfg(test)]
    pub fn fetch_image_url(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        wait(self.fetch_image_url_async(url, limit))
    }
    pub fn fetch_image_url_async(&self, url: &str, limit: usize) -> impl Future<Output = Result<Vec<u8>>> + Send + use<> {
        let valid = url.starts_with("https://") || url.starts_with("http://");
        let request = self.stream_agent.get(url);
        async move {
            if !valid { return Err(Error::Network("unsupported image URL scheme".into())); }
            let response = request.send(Default::default()).await
                .map_err(|_| Error::Network("image request failed".into()))?;
            media_bytes(response, limit).await
        }
    }

}

/// The agent for Navidrome's native API: no redirects, the API deadlines.
fn native_agent(builder: crate::http::AgentBuilder, client_name: &str) -> crate::http::Agent {
    builder.redirects(0)
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .timeout_write(Duration::from_secs(20)).user_agent(client_name).build()
}

/// Explicit mainline time provenance; NTP acceptance does not disable TLS
/// certificate checks or claim that unauthenticated NTP is authenticated time.
fn check_clock_path(path: std::ffi::OsString) -> Result<()> {
    let status = std::fs::read_to_string(path).unwrap_or_default();
    if !clock_status_ready(status.trim()) {
        return Err(Error::Network("HTTPS needs a synchronized clock".into()));
    }
    Ok(())
}

fn clock_status_ready(status: &str) -> bool {
    matches!(status, "provisioned" | "trusted-rtc" | "network-ntp" | "network-ntp-rtc")
}

#[test]
fn clock_provenance_requires_completed_synchronization() {
    for status in ["provisioned", "trusted-rtc", "network-ntp", "network-ntp-rtc"] {
        assert!(clock_status_ready(status));
    }
    for status in ["", "rtc-or-system", "build-floor-unsynchronized", "network", "nts"] {
        assert!(!clock_status_ready(status));
    }
}

#[cfg(test)]
fn wait<T: Send + 'static>(future: impl Future<Output = Result<T>> + Send + 'static) -> Result<T> {
    crate::http::block_on(future).unwrap_or_else(|| Err(Error::Network("request cancelled".into())))
}

async fn media_bytes(response: crate::http::Response, limit: usize) -> Result<Vec<u8>> {
    if response.header("Content-Length").and_then(|n| n.parse::<u64>().ok()).is_some_and(|n| n > limit as u64) {
        return Err(Error::Json("response too large".into()));
    }
    decoded_media_bytes(response,limit).await
}
async fn decoded_media_bytes(response: crate::http::Response, limit: usize) -> Result<Vec<u8>> {
    let mut body=response.into_decoded();
    let mut bytes=Vec::new();
    while let Some(chunk)=body.chunk().await {
        let chunk=chunk.map_err(|_|Error::Network("media transfer failed".into()))?;
        if chunk.len()>limit.saturating_sub(bytes.len()) {return Err(Error::Json("response too large".into()));}
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Received bytes scanned per job: hyper's pieces are coalesced to this.
const PIECE: usize = 32 * 1024;

/// Parses a response as it arrives: each piece in a short blocking job that
/// owns only the parser and the piece, never the socket.
async fn stream<T: Streamed + Send + 'static>(response: crate::http::Response, mut parser: Parser<T>) -> Result<(T, u64)> {
    if response.header("Content-Length").and_then(|n| n.parse::<u64>().ok()).is_some_and(|n| n > crate::document::DOCUMENT_LIMIT) {
        return Err(Error::Json("response too large".into()));
    }
    let stopped = |_| Error::Network("document decode failed".into());
    let mut body = response.into_decoded();
    let mut piece = Vec::new();
    loop {
        let next = body.chunk().await;
        let end = next.is_none();
        if let Some(chunk) = next {
            let chunk = chunk.map_err(|_| Error::Network("document transfer failed".into()))?;
            piece.extend_from_slice(&chunk);
        }
        if piece.len() >= PIECE || (end && !piece.is_empty()) {
            let full = std::mem::take(&mut piece);
            let (fed, full) = tokio::task::spawn_blocking(move || { let fed = parser.feed(&full).map(|()| parser); (fed, full) })
                .await.map_err(stopped)?;
            parser = fed?;
            piece = full;
            piece.clear();
        }
        if end { break; }
    }
    drop(piece);
    let bytes = parser.bytes();
    let value = tokio::task::spawn_blocking(move || parser.finish()).await.map_err(stopped)??;
    Ok((value, bytes))
}

/// An error envelope or a document where media was expected: its Subsonic
/// error, if it has one.
async fn document(response: crate::http::Response) -> Result<()> {
    stream(response, Parser::<Value>::with_limit(None, "document", ERROR_DOCUMENT_LIMIT)).await.map(drop)
}

async fn map_err_async(error: crate::http::Error) -> Error {
    match error {
        crate::http::Error::Status(401, _) => Error::Http(401),
        crate::http::Error::Status(code, response) => match document(response).await {
            Err(api @ Error::Api {..}) => api,
            _ => Error::Http(code),
        },
        crate::http::Error::Transport(error) => Error::Network(error.to_string()),
    }
}

/// The largest error document read.
const ERROR_DOCUMENT_LIMIT: u64 = 1024 * 1024;

/// The reference parse of a complete document text into its body tree, for
/// tests that compare the streamed parse with it.
#[cfg(test)]
pub(crate) fn parse_envelope(text: &str) -> Result<Value> {
    let mut v: Value = serde_json::from_str(text).map_err(|e| Error::Json(e.to_string()))?;
    let body = v
        .get_mut("subsonic-response")
        .map(Value::take)
        .ok_or_else(|| Error::Json("no subsonic-response".into()))?;
    if body.get("status").and_then(Value::as_str) == Some("ok") {
        return Ok(body);
    }
    let err = body.get("error");
    let code = err.and_then(|e| e.get("code")).and_then(Value::as_u64).unwrap_or(0) as u32;
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("request failed")
        .to_string();
    Err(Error::Api { code, message })
}


fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Percent-encodes a query value (RFC 3986 unreserved characters pass).
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
