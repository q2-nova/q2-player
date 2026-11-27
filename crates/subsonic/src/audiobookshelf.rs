//! Audiobookshelf direct-file API. See docs/audiobooks.md for pinned contracts.
//! Secrets stay in headers; neither server response bodies nor URLs enter errors.
use hyper::body::Bytes;
use serde_json::{Value, json};
use std::future::Future;
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    /// No answer: the connection, the name or the proxy failed.
    Network,
    /// The server answered but is busy or failing (408, 429, 5xx).
    Server,
    Auth,
    Missing,
    Protocol,
    Storage,
    Changed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration => "Set an Audiobookshelf address and API token",
            Self::Network => "Audiobookshelf unavailable",
            // ux::Code::AbsServerError's message: shown as it is.
            Self::Server => "Audiobookshelf had a problem",
            Self::Auth => "Audiobookshelf token rejected or expired",
            Self::Missing => "Audiobookshelf item unavailable",
            Self::Storage => "Unable to write audiobook to the card",
            Self::Changed => "Audiobookshelf settings changed; select the item again",
            Self::Protocol => "Unsupported Audiobookshelf response",
        })
    }
}
impl std::error::Error for Error {}
/// An Audiobookshelf request. Every request is built by [`Client::request`],
/// so its outcome reaches the client's reachability observer for all traffic
/// (probes, sessions, progress, streamed ranges, downloads, browsing).
pub(crate) struct Request {
    inner: crate::http::Request,
    observer: Option<fn(bool)>,
}
impl Request {
    pub(crate) fn set(self, name: &str, value: &str) -> Self {
        Request { inner: self.inner.set(name, value), observer: self.observer }
    }
    pub(crate) fn query(self, name: &str, value: &str) -> Self {
        Request { inner: with_query(self.inner, name, value), observer: self.observer }
    }
    pub(crate) async fn send(self, body: Bytes) -> Result<crate::http::Response, crate::http::Error> {
        let result = self.inner.send(body).await;
        if let Some(observer) = self.observer { observer(reached(&result)); }
        result
    }
}

/// A URL query parameter (an explicit `http::Request` receiver, so the
/// async inventory sees a value call, not a database query).
fn with_query(request: crate::http::Request, name: &str, value: &str) -> crate::http::Request {
    request.query(name, value)
}

/// The server answered in a way the player can use: a transport failure
/// ([`Error::Network`]) or a 408/429/5xx ([`Error::Server`]) is not.
fn reached(result: &Result<crate::http::Response, crate::http::Error>) -> bool {
    match result {
        Ok(_) => true,
        Err(crate::http::Error::Status(code, _)) => !matches!(code, 408 | 429 | 500..=599),
        Err(crate::http::Error::Transport(_)) => false,
    }
}

fn error(e: crate::http::Error) -> Error {
    match e {
        crate::http::Error::Status(401 | 403, _) => Error::Auth,
        crate::http::Error::Status(404, _) => Error::Missing,
        crate::http::Error::Status(408 | 429 | 500..=599, _) => Error::Server,
        crate::http::Error::Status(_, _) => Error::Protocol,
        _ => Error::Network,
    }
}

/// Compact listening state from the authenticated user's progress list.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaProgress {
    pub library_item_id: String,
    pub episode_id: Option<String>,
    pub last_update: u64,
    #[serde(default)]
    pub is_finished: bool,
    #[serde(default)]
    pub current_time: f64,
    #[serde(default)]
    pub progress: f64,
}

#[derive(Clone)]
pub struct Client {
    base: Url,
    token: String,
    agent: crate::http::Agent,
    namespace: std::sync::OnceLock<String>,
    progress: std::sync::Arc<std::sync::Mutex<Vec<MediaProgress>>>,
    proxy: Option<String>,
    /// Told whether each request reached the server; see [`Client::observing_reachability`].
    reachability: Option<fn(bool)>,
}
impl Client {
    /// None explicitly selects direct networking; Some requires that proxy.
    /// A failed proxy never falls back to direct networking.
    pub fn new(base: &str, token: &str, proxy: Option<&str>) -> Result<Self, Error> {
        let mut base = Url::parse(base.trim()).map_err(|_| Error::Configuration)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || token.is_empty()
            || token.contains(['\r', '\n'])
            || proxy.is_some_and(str::is_empty)
        {
            return Err(Error::Configuration);
        }
        let path = format!("{}/", base.path().trim_end_matches('/'));
        base.set_path(&path);
        let mut builder = crate::http::AgentBuilder::new().try_proxy_from_env(false);
        if let Some(proxy) = proxy {
            builder = builder.proxy(crate::http::Proxy::new(proxy).map_err(|_| Error::Configuration)?);
        }
        let agent = builder
            .redirects(0)
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(15))
            .timeout_write(Duration::from_secs(15))
            .user_agent("q2nd-audiobookshelf/1")
            .build();
        Ok(Self {
            base,
            token: token.into(),
            agent,
            namespace: std::sync::OnceLock::new(),
            progress: Default::default(),
            proxy: proxy.map(str::to_owned),
            reachability: None,
        })
    }
    pub fn configured_as(&self, base: &str, token: &str, proxy: Option<&str>) -> bool {
        let Ok(mut candidate) = Url::parse(base.trim()) else {
            return false;
        };
        candidate.set_path(&format!("{}/", candidate.path().trim_end_matches('/')));
        candidate == self.base && self.token == token && self.proxy.as_deref() == proxy
    }
    pub fn account_cache_key(&self) -> String {
        use md5::{Digest, Md5};
        format!(
            "shelf-account:{:x}",
            Md5::digest(
                format!(
                    "{}|{}",
                    self.base.as_str().trim_end_matches('/'),
                    self.token
                )
                .as_bytes()
            )
        )
    }
    pub fn cached_namespace(&self) -> Option<&str> {
        self.namespace.get().map(String::as_str)
    }
    #[cfg(test)]
    pub fn namespace(&self) -> Result<String, Error> {
        if let Some(value) = self.namespace.get() { return Ok(value.clone()); }
        self.probe()
    }
    /// Returns the cached verified identity, probing when it is absent.
    pub async fn namespace_async(&self) -> Result<String, Error> {
        if let Some(value) = self.namespace.get() { return Ok(value.clone()); }
        self.probe_async().await
    }
    /// A live authenticated identity check, including when already cached.
    pub async fn probe_async(&self) -> Result<String, Error> {
        let request = self.request("GET", "api/me");
        let (user, progress) = scan_json_lists(
            request?.send(Bytes::new()).await.map_err(error)?, USER_LISTS, Vec::new(),
            |progress, list, row| {
                if list == 0 && progress.len() < 8192 {
                    if let Ok(record) = serde_json::from_slice::<MediaProgress>(row) {
                        progress.push(record);
                    }
                }
                Ok(())
            },
        ).await?;
        let namespace = self.bind(&user)?;
        *self.progress.lock().unwrap() = progress;
        Ok(namespace)
    }
    pub fn take_media_progress(&self) -> Vec<MediaProgress> {
        std::mem::take(&mut *self.progress.lock().unwrap())
    }
    fn bind(&self, user: &Value) -> Result<String, Error> {
        use md5::{Digest, Md5};
        let id = user["id"].as_str().filter(|v| !v.is_empty()).ok_or(Error::Protocol)?;
        let namespace = format!("{:x}", Md5::digest(format!("{}|{}", self.base, id).as_bytes()));
        let _ = self.namespace.set(namespace.clone());
        if self.namespace.get() != Some(&namespace) { return Err(Error::Changed); }
        Ok(namespace)
    }
    /// A live authenticated request, even after namespace has been cached.
    /// An identity change cannot silently reuse the previous account's cache.
    #[cfg(test)]
    pub fn probe(&self) -> Result<String, Error> {
        let user = self.get("api/me")?;
        self.bind(&user)
    }
    fn url(&self, path: &str) -> Result<Url, Error> {
        let url = self.base.join(path).map_err(|_| Error::Protocol)?;
        if url.origin() != self.base.origin()
            || !url.path().starts_with(self.base.path())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Protocol);
        }
        Ok(url)
    }
    /// Reports each request's outcome to `observer`: true when the server
    /// answered, false when it could not be reached (the failures that map to
    /// [`Error::Network`]). It runs on the request's task and must not block.
    pub fn observing_reachability(mut self, observer: fn(bool)) -> Self {
        self.reachability = Some(observer);
        self
    }
    pub(crate) fn request(&self, method: &str, path: &str) -> Result<Request, Error> {
        let inner = self
            .agent
            .request(method, self.url(path)?.as_str())
            .set("Authorization", &format!("Bearer {}", self.token));
        Ok(Request { inner, observer: self.reachability })
    }
    #[cfg(test)]
    pub fn get(&self, path: &str) -> Result<Value, Error> {
        wait(self.get_async(path))
    }
    #[cfg(test)]
    pub fn get_query(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, Error> {
        wait(self.get_query_async(path, query, None))
    }
    #[cfg(test)]
    pub fn post(&self, path: &str, body: &Value) -> Result<Value, Error> {
        wait(self.post_async(path, body))
    }
    #[cfg(test)]
    pub fn play(&self, item: &str, episode: Option<&str>, device_id: &str) -> Result<Value, Error> {
        wait(self.play_async(item, episode, device_id))
    }
    #[cfg(test)]
    pub fn close_session(&self, id: &str) -> Result<(), Error> {
        wait(self.close_session_async(id))
    }
    /// Timestamped local sessions preserve server last-update conflict handling.
    #[cfg(test)]
    pub fn sync_local(&self, session: &Value) -> Result<(), Error> {
        wait(self.sync_local_async(session))
    }
    #[cfg(test)]
    pub fn size(&self, path: &str) -> Result<u64, Error> {
        wait(self.size_async(path))
    }
    #[cfg(test)]
    pub fn cover(&self, item: &str, size: u32) -> Result<Vec<u8>, Error> {
        wait(self.cover_async(item, size))
    }
    #[cfg(test)]
    pub fn range(&self, path: &str, offset: u64, length: usize, total: u64) -> Result<Vec<u8>, Error> {
        wait(self.range_async(path, offset, length, total))
    }

    // The requests, async: each returns a future that owns a copy of the
    // client; no caller or parser worker waits for its network I/O.

    pub fn get_async(&self, path: &str) -> impl Future<Output = Result<Value, Error>> + Send + use<> {
        self.get_query_async(path, &[], None)
    }
    /// A JSON document, read whole, or with `lists` parsed as it streams in.
    pub fn get_query_async(&self, path: &str, query: &[(&str, &str)], lists: Option<Lists>) -> impl Future<Output = Result<Value, Error>> + Send + use<> {
        let request = self.request("GET", path).map(|mut r| {
            for (key, value) in query {
                r = r.query(key, value);
            }
            r
        });
        async move {
            let response = request?.send(Bytes::new()).await.map_err(error)?;
            match lists {
                Some(Lists { lists, keep }) => json_lists(response, lists, keep).await,
                None => json_body(response).await,
            }
        }
    }
    pub fn post_async(&self, path: &str, body: &Value) -> impl Future<Output = Result<Value, Error>> + Send + use<> {
        let request = self.request("POST", path).map(|r| r.set("Content-Type", "application/json"));
        let body = Bytes::from(body.to_string());
        async move { json_body(request?.send(body).await.map_err(error)?).await }
    }
    /// The playback session. Its embedded library item's episode, file and
    /// track lists are skipped as the reply streams in: a podcast's whole
    /// episode list comes with every episode played.
    pub fn play_async(&self, item: &str, episode: Option<&str>, device_id: &str) -> impl Future<Output = Result<Value, Error>> + Send + use<> {
        let path = match episode {
            Some(ep) => format!("api/items/{}/play/{}", segment(item), segment(ep)),
            None => format!("api/items/{}/play", segment(item)),
        };
        let request = self.request("POST", &path).map(|r| r.set("Content-Type", "application/json"));
        let body = Bytes::from(json!({"forceDirectPlay":true,"mediaPlayer":"q2nd","deviceInfo":{"deviceId":device_id,"clientName":"Q2 Navidrome"}}).to_string());
        async move { json_lists(request?.send(body).await.map_err(error)?, SESSION_LISTS, None).await }
    }

    /// An item (`api/items/:id`, expanded) without its file and track lists
    /// and with at most one episode, `episode`, parsed as it streams in.
    pub fn item_async(&self, item: &str, episode: Option<&str>) -> impl Future<Output = Result<Value, Error>> + Send + use<> {
        let keep = episode.map(|id| (0, id.to_owned()));
        self.get_query_async(&format!("api/items/{}", segment(item)), &[("expanded", "1")], Some(Lists { lists: ITEM_LISTS, keep }))
    }
    pub fn close_session_async(&self, id: &str) -> impl Future<Output = Result<(), Error>> + Send + use<> {
        self.posted(&format!("api/session/{}/close", segment(id)), Bytes::from_static(b"{}"))
    }
    pub fn sync_local_async(&self, session: &Value) -> impl Future<Output = Result<(), Error>> + Send + use<> {
        self.posted("api/session/local", Bytes::from(session.to_string()))
    }
    /// Position-only changes (e.g. a seek before any audio has played) are not
    /// listening sessions. Callers reconcile the timestamp before admission.
    pub fn update_progress_async(&self, item: &str, episode: Option<&str>, progress: &Value) -> impl Future<Output = Result<(), Error>> + Send + use<> {
        let path = match episode {
            Some(ep) => format!("api/me/progress/{}/{}", segment(item), segment(ep)),
            None => format!("api/me/progress/{}", segment(item)),
        };
        let request = self.request("PATCH", &path).map(|r| r.set("Content-Type", "application/json"));
        let body = Bytes::from(progress.to_string());
        async move { request?.send(body).await.map_err(error)?; Ok(()) }
    }
    /// A POST whose answer is only its status.
    fn posted(&self, path: &str, body: Bytes) -> impl Future<Output = Result<(), Error>> + Send + use<> {
        let request = self.request("POST", path).map(|r| r.set("Content-Type", "application/json"));
        async move {
            request?.send(body).await.map_err(error)?;
            Ok(())
        }
    }
    pub fn size_async(&self, path: &str) -> impl Future<Output = Result<u64, Error>> + Send + use<> {
        let request = self.request("GET", path).map(|r| r.set("Range", "bytes=0-0"));
        async move {
            let r = request?.send(Bytes::new()).await.map_err(error)?;
            if r.status() != 206 {
                return Err(Error::Protocol);
            }
            let total = r.header("Content-Range")
                .and_then(|range| range.strip_prefix("bytes 0-0/"))
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .ok_or(Error::Protocol)?;
            let bytes = r.bytes(2).await.map_err(read_error)?;
            if bytes.len() != 1 {
                return Err(Error::Protocol);
            }
            Ok(total)
        }
    }
    pub fn cover_async(&self, item: &str, size: u32) -> impl Future<Output = Result<Vec<u8>, Error>> + Send + use<> {
        let request = self.request("GET", &format!("api/items/{}/cover", segment(item)))
            .map(|r| r.query("width", &size.clamp(1, 1024).to_string()));
        async move { request?.send(Bytes::new()).await.map_err(error)?.bytes(4 * 1024 * 1024).await.map_err(read_error) }
    }
    pub fn range_async(&self, path: &str, offset: u64, length: usize, total: u64) -> impl Future<Output = Result<Vec<u8>, Error>> + Send + use<> {
        let checked = if offset >= total || length == 0 || length > 256 * 1024 { Err(Error::Protocol) } else { Ok(()) };
        let end = offset.saturating_add((length as u64).saturating_sub(1)).min(total.saturating_sub(1));
        let request = self.request("GET", path).map(|r| r.set("Range", &format!("bytes={offset}-{end}")));
        async move {
            checked?;
            let response = request?.send(Bytes::new()).await.map_err(error)?;
            if response.status() != 206 || response.header("Content-Range") != Some(format!("bytes {offset}-{end}/{total}").as_str()) {
                return Err(Error::Protocol);
            }
            let data = response.bytes(length).await.map_err(read_error)?;
            if data.len() as u64 != end - offset + 1 {
                return Err(Error::Protocol);
            }
            Ok(data)
        }
    }
    /// The download owner validates the returned range before appending.
    pub fn download_response_async(&self,path:&str,offset:u64,validator:Option<&str>)->impl Future<Output=Result<crate::http::Response,Error>>+Send+use<> {
        let mut request=self.request("GET",path).map(|r|r.set("Accept-Encoding","identity"));
        if offset>0 {if let Some(tag)=validator {request=request.map(|r|r.set("Range",&format!("bytes={offset}-")).set(if tag.starts_with('"') {"If-Range"} else {"If-Unmodified-Since"},tag));}}
        async move {match request?.send(Bytes::new()).await {
            Ok(response)=>Ok(response),
            Err(crate::http::Error::Status(412|416,response)) if offset>0=>Ok(response),
            Err(e)=>Err(error(e)),
        }}
    }
    /// Open a complete direct-file download. Body ownership stays with the
    /// async caller, which must enforce `total` while writing bounded chunks.
    pub fn download_async(&self,path:&str,total:u64)->impl Future<Output=Result<crate::http::DecodedBody,Error>>+Send+use<> {
        let request=self.request("GET",path);
        async move {
            let response=request?.send(Bytes::new()).await.map_err(error)?;
            if response.status()!=200 || response.header("Content-Length")
                .and_then(|v|v.parse::<u64>().ok()).is_some_and(|length|length!=total) {
                return Err(Error::Protocol);
            }
            Ok(response.into_decoded())
        }
    }

}
/// Runs a request future on the HTTP executor and waits (threads only).
#[cfg(test)]
fn wait<T: Send + 'static>(future: impl Future<Output = Result<T, Error>> + Send + 'static) -> Result<T, Error> {
    crate::http::block_on(future).unwrap_or(Err(Error::Network))
}

use crate::document::{Scanner, Seg::{self, Key}};

/// The lists of large Audiobookshelf documents that the player never reads,
/// skipped as the document streams in. `ITEM_LISTS[0]` is the episode list,
/// of which at most one requested episode is kept.
const ITEM_LISTS: &[&[Seg]] = &[&[Key("media"), Key("episodes")], &[Key("media"), Key("audioFiles")],
    &[Key("media"), Key("tracks")], &[Key("libraryFiles")]];
const SESSION_LISTS: &[&[Seg]] = &[&[Key("libraryItem"), Key("media"), Key("episodes")], &[Key("libraryItem"), Key("media"), Key("audioFiles")],
    &[Key("libraryItem"), Key("media"), Key("tracks")], &[Key("libraryItem"), Key("libraryFiles")]];
const USER_LISTS: &[&[Seg]] = &[&[Key("mediaProgress")], &[Key("bookmarks")]];
/// Lists of a document skipped as it streams in (`json_lists`), and the one
/// element of `lists[keep.0]` with id `keep.1` that is kept.
pub struct Lists {
    lists: &'static [&'static [Seg]],
    keep: Option<(usize, String)>,
}

/// A safety bound against a broken server; memory does not follow it.
const STREAM_LIMIT: u64 = 64 * 1024 * 1024;

/// A JSON body parsed as it arrives (document.rs), each received piece in a
/// short job: the elements of `lists` are dropped, except one element of
/// `lists[keep.0]` whose `id` is `keep.1`. Peak memory is the rest of the
/// document, one element and one piece.
async fn json_lists(response: crate::http::Response, lists: &'static [&'static [Seg]], keep: Option<(usize, String)>) -> Result<Value, Error> {
    #[derive(serde::Deserialize)]
    struct Id { #[serde(default)] id: String }
    let wanted = keep.clone();
    let (mut value, kept) = scan_json_lists(response, lists, None, move |kept: &mut Option<Value>, list, row| {
        let Some((wanted_list, id)) = &wanted else { return Ok(()) };
        if list == *wanted_list && kept.is_none() {
            let bad = |_| crate::Error::Json("malformed element".into());
            if serde_json::from_slice::<Id>(row).map_err(bad)?.id == *id {
                *kept = Some(parse_value(row).map_err(|_| crate::Error::Json("malformed element".into()))?);
            }
        }
        Ok(())
    }).await?;
    if let (Some(episode), Some((list, _))) = (kept, &keep) { graft(&mut value, lists[*list], episode)?; }
    Ok(value)
}

async fn scan_json_lists<T, F>(response: crate::http::Response, lists: &'static [&'static [Seg]], mut data: T, record: F) -> Result<(Value, T), Error>
where
    T: Send + 'static,
    F: Fn(&mut T, usize, &[u8]) -> crate::Result<()> + Clone + Send + 'static,
{
    let mut scanner = Scanner::with_limit(None, lists, STREAM_LIMIT);
    let mut body = response.into_decoded();
    let mut piece = Vec::new();
    loop {
        let next = body.chunk().await;
        let end = next.is_none();
        if let Some(chunk) = next {
            piece.extend_from_slice(&chunk.map_err(|_| Error::Network)?);
        }
        if piece.len() >= 32 * 1024 || (end && !piece.is_empty()) {
            let full = std::mem::take(&mut piece);
            let visit = record.clone();
            let (fed, full, updated) = tokio::task::spawn_blocking(move || {
                let result = scanner.feed(&full, &mut |list, _, row| visit(&mut data, list, row)).map(|()| scanner);
                (result, full, data)
            }).await.map_err(|_| Error::Protocol)?;
            scanner = fed.map_err(|_| Error::Protocol)?;
            data = updated;
            piece = full;
            piece.clear();
        }
        if end { break; }
    }
    tokio::task::spawn_blocking(move || {
        let skeleton = scanner.finish(&mut |list, _, row| record(&mut data, list, row)).map_err(|_| Error::Protocol)?;
        Ok((parse_value(skeleton.text())?, data))
    }).await.map_err(|_| Error::Protocol)?
}

/// Puts the one kept episode back into its (emptied) list at `path`.
fn graft(value: &mut Value, path: &[Seg], episode: Value) -> Result<(), Error> {
    let mut at = value;
    // `get_mut`, not `at[key]`: indexing a number or string by key panics,
    // and the document comes from the server.
    for seg in path { if let Key(k) = seg { at = at.get_mut(*k).ok_or(Error::Protocol)?; } }
    at.as_array_mut().ok_or(Error::Protocol)?.push(episode);
    Ok(())
}

/// An 8 MB JSON body.
async fn json_body(response: crate::http::Response) -> Result<Value, Error> {
    let data = response.bytes(8 * 1024 * 1024).await.map_err(read_error)?;
    tokio::task::spawn_blocking(move || parse_value(&data))
        .await.map_err(|_|Error::Protocol)?
}

/// Values one document read into a `serde_json::Value` may hold. A value is
/// 24 bytes on the Q2 (32 on the host) before its text, and an array grows by
/// doubling: `[0,0,...]` in the 8 MiB a body may be is 4.2 million values,
/// over 100 MB. What is parsed whole (libraries, progress, an item's or a
/// session's skeleton without its lists, one kept episode) holds thousands.
const VALUE_LIMIT: usize = 1 << 18;

/// `bytes` as a `Value`, refused before parsing when it holds more than
/// [`VALUE_LIMIT`] values. Every value but the first follows `[`, `{` or `,`
/// outside a string, so their count bounds the values.
fn parse_value(bytes: &[u8]) -> Result<Value, Error> {
    let (mut values, mut string, mut escaped) = (1usize, false, false);
    for &byte in bytes {
        if string {
            if escaped { escaped = false; } else if byte == b'\\' { escaped = true; } else if byte == b'"' { string = false; }
            continue;
        }
        match byte {
            b'"' => string = true,
            b'[' | b'{' | b',' => {
                values += 1;
                if values > VALUE_LIMIT { return Err(Error::Protocol); }
            }
            _ => {}
        }
    }
    serde_json::from_slice(bytes).map_err(|_| Error::Protocol)
}

/// A body over its limit is a protocol error; any other failed read, network.
fn read_error(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::InvalidData { Error::Protocol } else { Error::Network }
}

pub fn segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_and_content_paths_fail_closed() {
        for base in [
            "file:///tmp/a",
            "http://u:p@example.test",
            "http://example.test/?token=secret",
            "http://example.test/#x",
        ] {
            assert!(matches!(
                Client::new(base, "synthetic", Some("socks5://127.0.0.1:1")),
                Err(Error::Configuration)
            ));
        }
        assert!(Client::new("http://example.test", "synthetic", Some("")).is_err());
        let c = Client::new(
            "http://example.test/abs",
            "synthetic",
            Some("socks5://127.0.0.1:1"),
        )
        .unwrap();
        for p in [
            "http://evil.test/abs/a",
            "//evil.test/a",
            "/api/items/a",
            "../private",
            "api/a?token=x",
        ] {
            assert!(c.url(p).is_err(), "{p}");
        }
        assert_eq!(
            c.url("api/items/a").unwrap().as_str(),
            "http://example.test/abs/api/items/a"
        );
        assert_eq!(segment("a/b?c"), "a%2Fb%3Fc");
    }
    #[test]
    fn broken_proxy_cannot_fall_back_to_direct() {
        for scheme in ["socks5","http"] {
        let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        origin.set_nonblocking(true).unwrap();
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_address = format!("{scheme}://{}", proxy.local_addr().unwrap());
        drop(proxy);
        let c = Client::new(
            &format!("http://{}", origin.local_addr().unwrap()),
            "synthetic",
            Some(&proxy_address),
        )
        .unwrap();
        assert_eq!(c.get("api/libraries"), Err(Error::Network));
        assert_eq!(c.play("book", None, "device"), Err(Error::Network));
        assert_eq!(c.close_session("session"), Err(Error::Network));
        assert_eq!(c.size("media"), Err(Error::Network));
        assert_eq!(c.range("media", 0, 1, 1), Err(Error::Network));
        assert_eq!(c.cover("book", 50), Err(Error::Network));
        assert_eq!(c.sync_local(&json!({})), Err(Error::Network));
        assert_eq!(
            wait(async move {c.download_async("media",1).await.map(|_|())}),
            Err(Error::Network)
        );
        assert!(matches!(origin.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
        }
    }
    #[test]
    fn route_changes_replace_transport_but_preserve_account_identity() {
        let direct = Client::new("http://fixture.invalid/abs", "synthetic", None).unwrap();
        let tail = Client::new(
            "http://fixture.invalid/abs",
            "synthetic",
            Some("socks5://127.0.0.1:1055"),
        )
        .unwrap();
        assert_eq!(direct.account_cache_key(), tail.account_cache_key());
        assert!(direct.configured_as(" http://fixture.invalid/abs/ ", "synthetic", None));
        assert!(!direct.configured_as(
            "http://fixture.invalid/abs",
            "synthetic",
            Some("socks5://127.0.0.1:1055")
        ));
        assert!(!tail.configured_as("http://fixture.invalid/abs", "synthetic", None));
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    fn request(socket: &mut std::net::TcpStream) -> (String, Value) {
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            socket.read_exact(&mut b).unwrap();
            header.extend(b);
            assert!(header.len() < 16384);
        }
        let text = String::from_utf8(header).unwrap();
        assert!(text.contains("Authorization: Bearer generated-fixture-token\r\n"));
        assert!(!text.lines().next().unwrap().contains("token="));
        let len = text
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        socket.read_exact(&mut body).unwrap();
        (text, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
    fn response(socket: &mut std::net::TcpStream, status: &str, headers: &str, body: &[u8]) {
        write!(
            socket,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .unwrap();
        socket.write_all(body).unwrap();
    }
    /// A podcast item and a playback session embedding it, with 3,000
    /// A server that repeats a key (the later one wins in the parsed value)
    /// or sends another type where the list's parent belongs gets a protocol
    /// error. Indexing the value by key panicked, which ends the player.
    #[test]
    fn a_kept_episode_whose_parent_is_not_an_object_is_a_protocol_error() {
        let episode = json!({"id": "ep-7"});
        let mut item = json!({"media": {"episodes": []}});
        graft(&mut item, ITEM_LISTS[0], episode.clone()).unwrap();
        assert_eq!(item, json!({"media": {"episodes": [{"id": "ep-7"}]}}));
        // `{"media":{"episodes":[...]},"media":1}` as serde_json reads it.
        for mut hostile in [json!({"media": 1}), json!({"media": "text"}), json!({"media": [1, 2]}), json!({"media": {"episodes": 3}}), json!({"media": {}}), json!([]), json!(null)] {
            let before = hostile.clone();
            assert!(matches!(graft(&mut hostile, ITEM_LISTS[0], episode.clone()), Err(Error::Protocol)), "{before}");
            assert_eq!(hostile, before);
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/abs", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            request(&mut socket);
            response(&mut socket, "200 OK", "Content-Type: application/json\r\n", br#"{"media":{"episodes":[{"id":"ep-7"}]},"media":1}"#);
        });
        let client = Client::new(&base, "generated-fixture-token", None).unwrap();
        assert!(matches!(wait(client.item_async("pod", Some("ep-7"))), Err(Error::Protocol)));
        server.join().unwrap();
    }

    /// Finding 23 (code review round 2). Counted outside strings only, so
    /// text full of brackets and commas is one value.
    #[test]
    fn a_document_is_refused_by_its_value_count_before_it_is_parsed() {
        let zeros = |n: usize| format!("[{}]", vec!["0"; n].join(","));
        assert_eq!(parse_value(zeros(VALUE_LIMIT - 1).as_bytes()).unwrap().as_array().unwrap().len(), VALUE_LIMIT - 1);
        assert!(matches!(parse_value(zeros(VALUE_LIMIT).as_bytes()), Err(Error::Protocol)));
        assert!(matches!(parse_value(br#"{"a":[{},{},{}],"b":1}"#), Ok(_)));
        let text = format!(r#"{{"t":"{}\"[,{{","n":1}}"#, "[,{".repeat(VALUE_LIMIT));
        assert_eq!(parse_value(text.as_bytes()).unwrap()["n"], 1);
        // A large real item: 3000 episodes and 500 files are 58,000 values.
        let episodes: Vec<Value> = (0..3000).map(|i| json!({"id": format!("ep-{i}"), "title": format!("Episode {i}"),
            "duration": 60.0, "chapters": [{"start": 0.0, "title": "Start"}], "audioFile": {"ino": "1", "metadata": {"size": 1}}})).collect();
        let files: Vec<Value> = (0..500).map(|i| json!({"ino": i.to_string(), "metadata": {"filename": "f"}})).collect();
        let item = json!({"media": {"episodes": episodes, "audioFiles": files.clone(), "tracks": files.clone()}, "libraryFiles": files});
        assert_eq!(parse_value(&serde_json::to_vec(&item).unwrap()).unwrap(), item);
    }

    /// episodes and long file lists: the item keeps only the episode asked
    /// for, the session none, and both keep everything else.
    #[test]
    fn large_items_and_sessions_stream_without_their_lists() {
        let episodes: Vec<Value> = (0..3000).map(|i| json!({"id": format!("ep-{i}"), "title": format!("Episode {i}"),
            "description": "x".repeat(400), "duration": 60.0, "chapters": [{"start": 0.0, "title": "Start"}]})).collect();
        let files: Vec<Value> = (0..500).map(|i| json!({"ino": i.to_string(), "metadata": {"filename": "f".repeat(200)}})).collect();
        let item = json!({"id": "pod", "libraryId": "lib", "mediaType": "podcast", "libraryFiles": files.clone(),
            "media": {"metadata": {"title": "Generated podcast"}, "episodes": episodes, "audioFiles": files.clone(), "tracks": files, "duration": 5.0}});
        let session = json!({"id": "session", "duration": 60.0, "chapters": [], "audioTracks": [{"contentUrl": "/a"}],
            "displayTitle": "Episode 7", "libraryItem": item.clone()});
        let bodies = [serde_json::to_vec(&item).unwrap(), serde_json::to_vec(&session).unwrap()];
        assert!(bodies[0].len() > 1_500_000);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/abs", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (step, body) in bodies.iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                let (header, _) = request(&mut socket);
                let first = header.lines().next().unwrap();
                assert!(if step == 0 { first.starts_with("GET /abs/api/items/pod?expanded=1 ") } else { first.starts_with("POST /abs/api/items/pod/play/ep-7 ") }, "{first}");
                response(&mut socket, "200 OK", "Content-Type: application/json\r\n", body);
            }
        });
        let client = Client::new(&base, "generated-fixture-token", None).unwrap();
        let fetched = wait(client.item_async("pod", Some("ep-7"))).unwrap();
        assert_eq!(fetched["media"]["episodes"].as_array().unwrap().len(), 1);
        assert_eq!(fetched["media"]["episodes"][0], item["media"]["episodes"][7]);
        assert_eq!(fetched["media"]["audioFiles"], json!([]));
        assert_eq!(fetched["libraryFiles"], json!([]));
        assert_eq!(fetched["media"]["metadata"], item["media"]["metadata"]);
        assert_eq!(fetched["libraryId"], "lib");
        let played = wait(client.play_async("pod", Some("ep-7"), "device")).unwrap();
        assert_eq!(played["libraryItem"]["media"]["episodes"], json!([]));
        for key in ["id", "duration", "chapters", "audioTracks", "displayTitle"] { assert_eq!(played[key], session[key]); }
        server.join().unwrap();
    }

    #[test]
    fn conditional_download_sends_range_and_validates_wire_identity() {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let base=format!("http://{}/abs",listener.local_addr().unwrap());
        let server=std::thread::spawn(move || {
            let (mut socket,_)=listener.accept().unwrap();let (header,_)=request(&mut socket);
            let lower=header.to_ascii_lowercase();
            assert!(lower.contains("range: bytes=4-\r\n"));assert!(lower.contains("if-range: \"v1\"\r\n"));
            assert!(lower.contains("accept-encoding: identity\r\n"));
            response(&mut socket,"206 Partial Content","Content-Range: bytes 4-9/10\r\nETag: \"v1\"\r\n",b"efghij");
        });
        let client=Client::new(&base,"generated-fixture-token",None).unwrap();
        let result=wait(async move {
            let response=client.download_response_async("api/items/book/file/ino",4,Some("\"v1\"")).await?;
            let prior=crate::http::resume::Validator {etag:"\"v1\"".into(),total:10};
            assert_eq!(crate::http::resume::admit(&response,4,Some(&prior)),Ok(crate::http::resume::Admission::Append));
            response.bytes(6).await.map_err(|_|Error::Network)
        }).unwrap();
        assert_eq!(result,b"efghij");server.join().unwrap();
    }
    #[test]
    fn current_server_contracts_use_selected_route_bearer_remote_dns_ranges_and_plain_sync_success()
    {
        for proxied in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let proxy = format!("socks5://{address}");
            let base = if proxied {
                "http://abs.fixture.invalid/abs".to_string()
            } else {
                format!("http://{address}/abs")
            };
            let worker = std::thread::spawn(move || {
                for step in 0..9 {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    if proxied {
                        let mut hello = [0; 3];
                        socket.read_exact(&mut hello).unwrap();
                        assert_eq!(hello, [5, 1, 0]);
                        socket.write_all(&[5, 0]).unwrap();
                        let mut connect = [0; 5];
                        socket.read_exact(&mut connect).unwrap();
                        assert_eq!(&connect[..4], &[5, 1, 0, 3]);
                        let mut host = vec![0; connect[4] as usize];
                        socket.read_exact(&mut host).unwrap();
                        assert_eq!(host, b"abs.fixture.invalid");
                        let mut port = [0; 2];
                        socket.read_exact(&mut port).unwrap();
                        assert_eq!(u16::from_be_bytes(port), 80);
                        socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).unwrap();
                    }
                    let (header, body) = request(&mut socket);
                    let path = header.lines().next().unwrap();
                    match step {
                        0 => {
                            assert!(path.starts_with("GET /abs/api/me "));
                            response(&mut socket, "200 OK", "", br#"{"id":"generated-user"}"#);
                        }
                        1 => {
                            assert!(path.starts_with("POST /abs/api/items/book/play/episode "));
                            assert_eq!(body["forceDirectPlay"], true);
                            response(
                                &mut socket,
                                "200 OK",
                                "",
                                br#"{"id":"generated-session","audioTracks":[]}"#,
                            );
                        }
                        2 => {
                            assert!(
                                path.starts_with("POST /abs/api/session/generated-session/close ")
                            );
                            response(&mut socket, "200 OK", "", b"OK");
                        }
                        3 => {
                            assert!(header.contains("Range: bytes=0-0\r\n"));
                            response(
                                &mut socket,
                                "206 Partial Content",
                                "Content-Range: bytes 0-0/10\r\n",
                                b"a",
                            );
                        }
                        4 => {
                            assert!(header.contains("Range: bytes=3-6\r\n"));
                            response(
                                &mut socket,
                                "206 Partial Content",
                                "Content-Range: bytes 3-6/10\r\n",
                                b"defg",
                            );
                        }
                        5 => {
                            assert!(path.starts_with("POST /abs/api/session/local "));
                            assert_eq!(body["currentTime"], 12.5);
                            assert_eq!(body["updatedAt"], 123456);
                            response(&mut socket, "200 OK", "", b"OK");
                        }
                        6 | 7 => {
                            assert!(path.starts_with("GET /abs/api/items/book/file/ino "));
                            response(&mut socket, "200 OK", "", b"abcdefghij");
                        }
                        _ => {
                            assert!(path.starts_with("GET /abs/api/items/book/cover?width=50 "));
                            response(&mut socket, "200 OK", "", b"generated-cover");
                        }
                    }
                }
            });
            let client = Client::new(
                &base,
                "generated-fixture-token",
                if proxied { Some(&proxy) } else { None },
            )
            .unwrap();
            let first = client.namespace().unwrap();
            assert_eq!(client.namespace().unwrap(), first);
            let session = client
                .play("book", Some("episode"), "generated-device")
                .unwrap();
            client
                .close_session(session["id"].as_str().unwrap())
                .unwrap();
            assert_eq!(client.size("api/items/book/file/ino").unwrap(), 10);
            assert_eq!(
                client.range("api/items/book/file/ino", 3, 4, 10).unwrap(),
                b"defg"
            );
            client
                .sync_local(&json!({"currentTime":12.5,"updatedAt":123456}))
                .unwrap();
            assert_eq!(
                client.range("api/items/book/file/ino", 0, 10, 10),
                Err(Error::Protocol)
            );
            let response=client.download_async("api/items/book/file/ino",10);
            let downloaded=wait(async move {
                let mut body=response.await?;let mut bytes=Vec::new();
                while let Some(chunk)=body.chunk().await {bytes.extend(chunk.map_err(|_|Error::Network)?);}
                Ok(bytes)
            }).unwrap();
            assert_eq!(downloaded, b"abcdefghij");
            assert_eq!(client.cover("book", 50).unwrap(), b"generated-cover");
            worker.join().unwrap();
        }
    }
    fn accept_bounded(listener:&TcpListener)->std::net::TcpStream {
        listener.set_nonblocking(true).unwrap();let deadline=std::time::Instant::now()+Duration::from_secs(5);
        loop {match listener.accept() {
            Ok((stream,_))=>return stream,
            Err(error) if error.kind()==std::io::ErrorKind::WouldBlock && std::time::Instant::now()<deadline=>std::thread::sleep(Duration::from_millis(1)),
            Err(error)=>panic!("generated fixture accept failed: {error}"),
        }}
    }
    #[test]
    fn shared_http_proxy_forwards_public_lan_and_tailnet_names_without_local_dns() {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let proxy=format!("http://{}",listener.local_addr().unwrap());
        let origins=["public.fixture.invalid","abs.lan.fixture.invalid","192.0.2.10","generated.tailnet.ts.net"];
        let worker=std::thread::spawn(move||for host in origins {
            let mut stream=accept_bounded(&listener);let (header,_)=request(&mut stream);
            assert!(header.starts_with(&format!("GET http://{host}/abs/api/me HTTP/1.1\r\n")),"{header}");
            response(&mut stream,"200 OK","",br#"{"id":"generated-user"}"#);
        });
        for host in origins {
            let client=Client::new(&format!("http://{host}/abs"),"generated-fixture-token",Some(&proxy)).unwrap();
            assert!(!client.probe().unwrap().is_empty());
        }
        worker.join().unwrap();
    }
    #[test]
    fn reachability_follows_every_answer_and_network_failures_only() {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let address=format!("http://{}",listener.local_addr().unwrap());
        let statuses=["200 OK","206 Partial Content","401 Unauthorized","404 Not Found","503 Service Unavailable","429 Too Many Requests"];
        let worker=std::thread::spawn(move||for status in statuses {
            let mut stream=accept_bounded(&listener);request(&mut stream);response(&mut stream,status,"",b"{}");
        });
        let client=Client::new(&address,"generated-fixture-token",None).unwrap();
        let outcomes:Vec<bool>=statuses.iter().map(|_| {
            let request=client.request("GET","api/me").unwrap();
            crate::http::block_on(async move { reached(&request.send(Bytes::new()).await) }).unwrap()
        }).collect();
        assert_eq!(outcomes,[true,true,true,true,false,false]);
        worker.join().unwrap();
        let closed=TcpListener::bind("127.0.0.1:0").unwrap();let gone=format!("http://{}",closed.local_addr().unwrap());drop(closed);
        let request=Client::new(&gone,"generated-fixture-token",None).unwrap().request("GET","api/me").unwrap();
        assert!(!crate::http::block_on(async move { reached(&request.send(Bytes::new()).await) }).unwrap(), "a refused connection is unreachable");
    }
    #[test]
    fn live_probe_does_not_mistake_cached_identity_for_connectivity_or_replace_account() {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let address=format!("http://{}",listener.local_addr().unwrap());
        let worker=std::thread::spawn(move||for (status,body) in [
            ("200 OK",br#"{"id":"generated-user"}"#.as_slice()),("503 Service Unavailable",b"{}"),
            ("401 Unauthorized",b"{}"),("200 OK",br#"{"id":"generated-user"}"#),
            ("200 OK",br#"{"id":"generated-other-user"}"#)] {
            let mut stream=accept_bounded(&listener);request(&mut stream);response(&mut stream,status,"",body);
        });
        let client=Client::new(&address,"generated-fixture-token",None).unwrap();
        let namespace=client.namespace().unwrap();assert_eq!(client.namespace().unwrap(),namespace);
        assert_eq!(client.probe(),Err(Error::Server));assert_eq!(client.probe(),Err(Error::Auth));
        assert_eq!(client.probe().unwrap(),namespace);assert_eq!(client.probe(),Err(Error::Changed));
        assert_eq!(client.cached_namespace(),Some(namespace.as_str()));worker.join().unwrap();
    }
    #[test]
    fn server_auth_errors_and_redirects_are_sanitized_without_following() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for status in ["401 Unauthorized", "302 Found", "403 Forbidden", "404 Not Found",
                "408 Request Timeout", "429 Too Many Requests", "500 Internal Server Error",
                "502 Bad Gateway", "503 Service Unavailable", "504 Gateway Timeout"] {
                let (mut socket, _) = listener.accept().unwrap();
                let _ = request(&mut socket);
                response(
                    &mut socket,
                    status,
                    "Location: http://private.invalid/?token=generated-fixture-token\r\n",
                    b"private-server-contents",
                );
            }
        });
        let client = Client::new(
            "http://abs.fixture.invalid",
            "generated-fixture-token",
            Some(&proxy),
        )
        .unwrap();
        assert_eq!(client.get("api/me"), Err(Error::Auth));
        assert_eq!(client.get("api/me"), Err(Error::Protocol));
        assert_eq!(client.get("api/me"), Err(Error::Auth));
        assert_eq!(client.get("api/me"), Err(Error::Missing));
        // The server answered: busy or failing, not unreachable.
        for _ in 0..6 {
            let failure = client.get("api/me").unwrap_err();
            assert_eq!(failure, Error::Server);
            assert_eq!(failure.to_string(), "Audiobookshelf had a problem");
        }
        assert!(!Error::Auth.to_string().contains("private"));
        worker.join().unwrap();
    }
}
