//! The HTTP/1.1 client every request in this crate goes through: hyper 1 on
//! a tokio runtime, TLS by tokio-rustls with ring and the webpki roots, HTTP
//! and SOCKS proxies, one idle keep-alive connection per host.
//!
//! Requests use async send and bounded decoded chunks. Redirect, proxy,
//! authentication and HTTP-error policy remain common to every service.
//! Connection tasks use the executor supplied to [`set_executor`], or the
//! calling Tokio runtime. Production never starts a hidden HTTP runtime.
//! Blocking reader/request adapters exist only for retained wire tests.
pub mod resume;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::client::conn::http1::{self, SendRequest};
use hyper::header::{HeaderMap, HeaderValue};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
#[cfg(test)]
use std::io::Read;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use url::Url;

pub use rustls;
mod decoded;
mod date_validator;
pub use decoded::DecodedBody;

/// A task for the executor.
pub type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

static EXECUTOR: OnceLock<Box<dyn Fn(Task) + Send + Sync>> = OnceLock::new();

/// Runs this crate's connection tasks on `spawn`'s runtime. Call once before
/// requests; false when an executor was already explicitly chosen. Without
/// registration, async callers use their current runtime and own its lifetime.
pub fn set_executor(spawn: impl Fn(Task) + Send + Sync + 'static) -> bool {
    EXECUTOR.set(Box::new(spawn)).is_ok()
}

fn spawn(task: Task) {
    if let Some(executor) = EXECUTOR.get() { executor(task); }
    else {
        #[cfg(test)]
        if tokio::runtime::Handle::try_current().is_err() {
            test_runtime().spawn(task);
            return;
        }
        tokio::spawn(task);
    }
}

// Ordinary-thread wire-test compatibility only. Production has no fallback
// executor thread, blocking body bridge or streaming HTTP decoder pool.
#[cfg(test)]
fn test_runtime() -> &'static tokio::runtime::Handle {
    static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("http test runtime");
        let handle = runtime.handle().clone();
        std::thread::Builder::new().name("http".into())
            .spawn(move || runtime.block_on(std::future::pending::<()>())).expect("http test thread");
        handle
    })
}
#[cfg(test)]
fn test_decoders() -> &'static tokio::runtime::Runtime {
    static DECODERS: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    DECODERS.get_or_init(|| tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(3).thread_name("http-decode").build().expect("HTTP test decoder pool"))
}

/// Runs `future` on the executor and waits for its result. For threads
/// outside the runtime: on a runtime thread tokio's `blocking_recv` panics
/// rather than wait forever for a task that thread would have to run.
#[cfg(test)]
pub(crate) fn block_on<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    test_runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.blocking_recv().ok()
}

// ---- errors ----

/// What went wrong below HTTP. The names are ureq 2's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidUrl,
    UnknownScheme,
    Dns,
    ConnectionFailed,
    TooManyRedirects,
    BadStatus,
    BadHeader,
    Io,
    InvalidProxyUrl,
    ProxyConnect,
    ProxyUnauthorized,
}

#[derive(Debug, Clone)]
pub struct Transport {
    kind: ErrorKind,
    message: String,
}

impl Transport {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

pub enum Error {
    /// The server answered 400 or above; the response is readable.
    Status(u16, Response),
    Transport(Transport),
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Status(..) => ErrorKind::BadStatus,
            Error::Transport(t) => t.kind,
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Status(code, _) => write!(f, "Status({code})"),
            Error::Transport(t) => write!(f, "Transport({t})"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Status(code, response) => write!(f, "{}: status code {code}", response.url),
            Error::Transport(t) => t.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

fn fail(kind: ErrorKind, message: impl fmt::Display) -> Error {
    Error::Transport(Transport { kind, message: message.to_string() })
}

fn io_fail(kind: ErrorKind, e: io::Error) -> Error {
    fail(kind, e)
}

// ---- proxies ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyKind {
    Http,
    Socks4,
    Socks4a,
    Socks5,
}

/// `[scheme://][user:password@]host[:port]`; schemes `http`, `socks4`,
/// `socks4a`, `socks5` (`socks` is `socks5`). Host names go to the proxy
/// unresolved except for SOCKS4, which has no field for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    kind: ProxyKind,
    host: String,
    port: u16,
    auth: Option<(String, String)>,
}

impl Proxy {
    pub fn new(proxy: &str) -> Result<Proxy, Error> {
        let bad = || fail(ErrorKind::InvalidProxyUrl, "invalid proxy");
        let text = if proxy.contains("://") { proxy.to_string() } else { format!("http://{proxy}") };
        let url = Url::parse(&text).map_err(|_| bad())?;
        let kind = match url.scheme() {
            "http" => ProxyKind::Http,
            "socks4" => ProxyKind::Socks4,
            "socks4a" => ProxyKind::Socks4a,
            "socks" | "socks5" => ProxyKind::Socks5,
            _ => return Err(bad()),
        };
        let host = url.host_str().filter(|h| !h.is_empty()).ok_or_else(bad)?;
        let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let port = url.port().unwrap_or(if kind == ProxyKind::Http { 80 } else { 1080 });
        let auth = (!url.username().is_empty()).then(|| {
            (unescape(url.username()), unescape(url.password().unwrap_or("")))
        });
        Ok(Proxy { kind, host, port, auth })
    }
}

/// Percent-decoding of a URL's user or password.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- agent ----

#[derive(Clone)]
struct Config {
    connect_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    timeout: Option<Duration>,
    user_agent: String,
    proxy: Option<Proxy>,
    tls: Arc<rustls::ClientConfig>,
    redirects: u32,
    wifi_source: Option<std::net::Ipv4Addr>,
}

/// Builds an [`Agent`]. Defaults: no timeouts, five redirects, the webpki
/// roots, no proxy (the environment is never read).
pub struct AgentBuilder {
    config: Config,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBuilder {
    pub fn new() -> AgentBuilder {
        AgentBuilder {
            config: Config {
                connect_timeout: None,
                read_timeout: None,
                write_timeout: None,
                timeout: None,
                user_agent: concat!("q2nd/", env!("CARGO_PKG_VERSION")).into(),
                proxy: None,
                tls: default_tls(),
                redirects: 5,
                wifi_source: None,
            },
        }
    }
    pub fn timeout_connect(mut self, d: Duration) -> Self {
        self.config.connect_timeout = Some(d);
        self
    }
    /// The longest wait for the response head or for one piece of the body.
    pub fn timeout_read(mut self, d: Duration) -> Self {
        self.config.read_timeout = Some(d);
        self
    }
    /// The longest time sending the request may take.
    pub fn timeout_write(mut self, d: Duration) -> Self {
        self.config.write_timeout = Some(d);
        self
    }
    /// The whole request, body included.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.config.timeout = Some(d);
        self
    }
    pub fn user_agent(mut self, agent: &str) -> Self {
        self.config.user_agent = agent.into();
        self
    }
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.config.proxy = Some(proxy);
        self
    }
    /// Kept for the ureq call sites: the environment is never read.
    pub fn try_proxy_from_env(self, _: bool) -> Self {
        self
    }
    pub fn tls_config(mut self, tls: Arc<rustls::ClientConfig>) -> Self {
        self.config.tls = tls;
        self
    }
    /// Keep captive-portal exchanges on this Wi-Fi lease, including after DNS resolution.
    pub fn wifi_source(mut self, ip: std::net::Ipv4Addr) -> Self {
        self.config.wifi_source = Some(ip);
        self
    }
    pub fn redirects(mut self, n: u32) -> Self {
        self.config.redirects = n;
        self
    }
    pub fn build(self) -> Agent {
        Agent { config: Arc::new(self.config), pool: Arc::new(Pool::default()) }
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    let mut provider = rustls::crypto::ring::default_provider();
    // The X2000 has no AES instructions, so ring's AES-GCM runs in software
    // (`aes_nohw`, `gcm::fallback`). Offer ChaCha20-Poly1305 first there;
    // a server that honours the client's order then uses it
    // (`q2nd --bench` kind "aead" measures both on the Q2).
    if cfg!(target_arch = "mips") {
        prefer_chacha(&mut provider.cipher_suites);
    }
    Arc::new(provider)
}

/// Moves the ChaCha20-Poly1305 suites to the front, keeping the order otherwise.
fn prefer_chacha(suites: &mut [rustls::SupportedCipherSuite]) {
    use rustls::CipherSuite::*;
    suites.sort_by_key(|s| !matches!(s.suite(), TLS13_CHACHA20_POLY1305_SHA256
        | TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 | TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256));
}

fn default_tls() -> Arc<rustls::ClientConfig> {
    static TLS: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    TLS.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        with_roots(roots)
    })
    .clone()
}

fn with_roots(roots: rustls::RootCertStore) -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// A TLS configuration trusting only `der`, one DER certificate.
pub fn tls_trusting(der: Vec<u8>) -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(rustls::pki_types::CertificateDer::from(der))?;
    Ok(with_roots(roots))
}

/// Settings and kept-alive connections shared by requests. Cheap to clone.
#[derive(Clone)]
pub struct Agent {
    config: Arc<Config>,
    pool: Arc<Pool>,
}

impl Agent {
    pub fn get(&self, url: &str) -> Request {
        self.request("GET", url)
    }
    pub fn post(&self, url: &str) -> Request {
        self.request("POST", url)
    }
    pub fn request(&self, method: &str, url: &str) -> Request {
        Request { agent: self.clone(), method: method.to_ascii_uppercase(), url: url.into(), headers: Vec::new(), query: Vec::new(), timeout: None }
    }
}

// ---- pool ----

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    https: bool,
    host: String,
    port: u16,
}

type Sender = SendRequest<Full<Bytes>>;

/// Bumped when the network link changes (address, association, doze end):
/// idle connections from an older link are never reused. Their sockets can be
/// dead without a close the client has seen, and a request on one would wait
/// out the whole read timeout.
static NETWORK_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The link changed: every agent drops its idle connections at next use.
pub fn network_changed() {
    NETWORK_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn network_epoch() -> u32 { NETWORK_EPOCH.load(std::sync::atomic::Ordering::Relaxed) }

/// A reused connection that has not answered within this is presumed dead;
/// an idempotent request is then sent once more on a new connection.
#[cfg(not(test))]
const REUSED_HEAD_LIMIT: Duration = Duration::from_secs(4);
#[cfg(test)]
const REUSED_HEAD_LIMIT: Duration = Duration::from_millis(300);

/// At most one idle connection per host, as ureq kept.
#[derive(Default)]
struct Pool {
    idle: Mutex<HashMap<Key, (u32, Sender)>>,
}

impl Pool {
    fn take(&self, key: &Key) -> Option<Sender> {
        let epoch = network_epoch();
        self.idle.lock().unwrap().remove(key).filter(|(e, s)| *e == epoch && !s.is_closed()).map(|(_, s)| s)
    }
    fn give(&self, key: Key, sender: Sender) {
        if !sender.is_closed() {
            self.idle.lock().unwrap().insert(key, (network_epoch(), sender));
        }
    }
}

// ---- streams ----

enum Stream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// The earlier of `limit` from now and `deadline`.
fn bound(limit: Option<Duration>, deadline: Option<Instant>) -> Option<Duration> {
    let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    match (limit, left) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

async fn within<T>(limit: Option<Duration>, future: impl Future<Output = T>) -> Result<T, ()> {
    match limit {
        Some(limit) => tokio::time::timeout(limit, future).await.map_err(|_| ()),
        None => Ok(future.await),
    }
}

/// A TCP connection to `host`, all of it within `limit` (a wait for
/// `getaddrinfo` included). Each address gets an equal share of what is left,
/// as ureq divided it, so one unreachable address cannot use it all.
async fn tcp(host: &str, port: u16, limit: Option<Duration>, wifi_source: Option<std::net::Ipv4Addr>) -> Result<TcpStream, Error> {
    let start = Instant::now();
    let timed_out = || fail(ErrorKind::ConnectionFailed, "connect timed out");
    let addrs: Vec<_> = within(limit, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| timed_out())?
        .map_err(|e| io_fail(ErrorKind::Dns, e))?
        .collect();
    let mut last = None;
    for (i, addr) in addrs.iter().enumerate() {
        let share = limit.map(|l| l.saturating_sub(start.elapsed()) / (addrs.len() - i) as u32);
        let connect = async {
            if let Some(ip) = wifi_source {
                let std::net::SocketAddr::V4(peer) = addr else {
                    return Err(io::Error::from(io::ErrorKind::AddrNotAvailable));
                };
                let dst = peer.ip();
                if dst == &ip || dst.is_loopback() || dst.is_link_local() || dst.is_multicast()
                    || dst.is_broadcast() || dst.octets()[0] == 0 {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
                let socket = tokio::net::TcpSocket::new_v4()?;
                #[cfg(target_os = "linux")]
                socket.bind_device(Some(b"wlan0"))?;
                socket.bind((ip, 0).into())?;
                socket.connect(*addr).await
            } else {
                TcpStream::connect(addr).await
            }
        };
        match within(share, connect).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last = Some(io_fail(ErrorKind::ConnectionFailed, e)),
            Err(()) => last = Some(timed_out()),
        }
    }
    Err(last.unwrap_or_else(|| fail(ErrorKind::Dns, format!("no address for {host}"))))
}

/// A host as it goes in a request line or `Host`: IPv6 in brackets.
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

fn basic(user: &str, password: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = format!("{user}:{password}").into_bytes();
    let mut out = String::from("Basic ");
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { TABLE[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Reads up to the blank line ending a proxy's CONNECT answer.
async fn connect_head(s: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || s.read(&mut byte).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

async fn tunnel(proxy: &Proxy, s: &mut TcpStream, host: &str, port: u16) -> Result<(), Error> {
    let proxy_err = |e: io::Error| io_fail(ErrorKind::ProxyConnect, e);
    match proxy.kind {
        ProxyKind::Http => {
            let target = authority(host, port);
            let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
            if let Some((u, p)) = &proxy.auth {
                request += &format!("Proxy-Authorization: {}\r\n", basic(u, p));
            }
            request += "\r\n";
            s.write_all(request.as_bytes()).await.map_err(proxy_err)?;
            let head = connect_head(s).await.map_err(proxy_err)?;
            let code = head.split(' ').nth(1).unwrap_or("");
            match code {
                "200" => Ok(()),
                "407" => Err(fail(ErrorKind::ProxyUnauthorized, "proxy authentication required")),
                _ => Err(fail(ErrorKind::ProxyConnect, format!("proxy answered {code}"))),
            }
        }
        ProxyKind::Socks5 => {
            let greeting: &[u8] = if proxy.auth.is_some() { &[5, 2, 0, 2] } else { &[5, 1, 0] };
            s.write_all(greeting).await.map_err(proxy_err)?;
            let mut choice = [0u8; 2];
            s.read_exact(&mut choice).await.map_err(proxy_err)?;
            match (choice, &proxy.auth) {
                ([5, 0], _) => {}
                ([5, 2], Some((u, p))) if u.len() < 256 && p.len() < 256 => {
                    let mut m = vec![1, u.len() as u8];
                    m.extend_from_slice(u.as_bytes());
                    m.push(p.len() as u8);
                    m.extend_from_slice(p.as_bytes());
                    s.write_all(&m).await.map_err(proxy_err)?;
                    let mut ok = [0u8; 2];
                    s.read_exact(&mut ok).await.map_err(proxy_err)?;
                    if ok[1] != 0 {
                        return Err(fail(ErrorKind::ProxyUnauthorized, "SOCKS5 authentication refused"));
                    }
                }
                _ => return Err(fail(ErrorKind::ProxyConnect, "SOCKS5 method refused")),
            }
            let mut m = vec![5, 1, 0];
            match host.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(ip)) => {
                    m.push(1);
                    m.extend_from_slice(&ip.octets());
                }
                Ok(std::net::IpAddr::V6(ip)) => {
                    m.push(4);
                    m.extend_from_slice(&ip.octets());
                }
                Err(_) if host.len() < 256 => {
                    m.extend_from_slice(&[3, host.len() as u8]);
                    m.extend_from_slice(host.as_bytes());
                }
                Err(_) => return Err(fail(ErrorKind::InvalidUrl, "host name too long")),
            }
            m.extend_from_slice(&port.to_be_bytes());
            s.write_all(&m).await.map_err(proxy_err)?;
            let mut reply = [0u8; 4];
            s.read_exact(&mut reply).await.map_err(proxy_err)?;
            if reply[0] != 5 || reply[1] != 0 {
                return Err(fail(ErrorKind::ProxyConnect, format!("SOCKS5 reply {}", reply[1])));
            }
            let rest = match reply[3] {
                1 => 4 + 2,
                4 => 16 + 2,
                3 => {
                    let mut n = [0u8; 1];
                    s.read_exact(&mut n).await.map_err(proxy_err)?;
                    n[0] as usize + 2
                }
                _ => return Err(fail(ErrorKind::ProxyConnect, "SOCKS5 reply address")),
            };
            let mut skip = vec![0u8; rest];
            s.read_exact(&mut skip).await.map_err(proxy_err)?;
            Ok(())
        }
        ProxyKind::Socks4 | ProxyKind::Socks4a => {
            let mut m = vec![4, 1];
            m.extend_from_slice(&port.to_be_bytes());
            let ip = match host.parse::<std::net::Ipv4Addr>() {
                Ok(ip) => Some(ip),
                Err(_) if proxy.kind == ProxyKind::Socks4 => {
                    let resolved = tokio::net::lookup_host((host, port)).await.map_err(|e| io_fail(ErrorKind::Dns, e))?;
                    Some(resolved.filter_map(|a| match a.ip() { std::net::IpAddr::V4(v4) => Some(v4), _ => None }).next()
                        .ok_or_else(|| fail(ErrorKind::Dns, "no IPv4 address for SOCKS4"))?)
                }
                Err(_) => None,
            };
            m.extend_from_slice(&ip.map_or([0, 0, 0, 1], |ip| ip.octets()));
            if let Some((u, _)) = &proxy.auth {
                m.extend_from_slice(u.as_bytes());
            }
            m.push(0);
            if ip.is_none() {
                m.extend_from_slice(host.as_bytes());
                m.push(0);
            }
            s.write_all(&m).await.map_err(proxy_err)?;
            let mut reply = [0u8; 8];
            s.read_exact(&mut reply).await.map_err(proxy_err)?;
            if reply[1] != 0x5a {
                return Err(fail(ErrorKind::ProxyConnect, format!("SOCKS4 reply {}", reply[1])));
            }
            Ok(())
        }
    }
}

/// A new connection to `key`, through the proxy when there is one. The
/// connect timeout bounds the TCP connection, as in ureq; the proxy's answer
/// and the TLS and HTTP handshakes are reads, bounded by the read timeout.
async fn open(config: &Config, key: &Key, deadline: Option<Instant>) -> Result<Sender, Error> {
    let connect = bound(config.connect_timeout, deadline);
    let tcp = match &config.proxy {
        None => tcp(&key.host, key.port, connect, config.wifi_source).await?,
        Some(proxy) => {
            let mut s = tcp(&proxy.host, proxy.port, connect, config.wifi_source).await.map_err(|e| match e {
                Error::Transport(t) => fail(ErrorKind::ProxyConnect, t.message),
                other => other,
            })?;
            // Plain HTTP through an HTTP proxy is sent to the proxy as is.
            if proxy.kind != ProxyKind::Http || key.https {
                within(bound(config.read_timeout, deadline), tunnel(proxy, &mut s, &key.host, key.port))
                    .await
                    .map_err(|_| fail(ErrorKind::ProxyConnect, "proxy timed out"))??;
            }
            s
        }
    };
    let _ = tcp.set_nodelay(true);
    let setup = async {
        let stream = if key.https {
            let name = rustls::pki_types::ServerName::try_from(key.host.clone())
                .map_err(|_| fail(ErrorKind::InvalidUrl, "invalid TLS server name"))?;
            let tls = tokio_rustls::TlsConnector::from(config.tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|e| io_fail(ErrorKind::ConnectionFailed, e))?;
            Stream::Tls(Box::new(tls))
        } else {
            Stream::Plain(tcp)
        };
        // A 64 KB read buffer: hyper's default grows to about 400 KB per
        // connection, which a 60 MB device does not have to spare.
        let (sender, connection) = http1::Builder::new()
            .title_case_headers(true)
            .max_buf_size(64 * 1024)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(|e| fail(ErrorKind::ConnectionFailed, e))?;
        spawn(Box::pin(async move {
            let _ = connection.await;
        }));
        Ok(sender)
    };
    within(bound(config.read_timeout, deadline), setup).await.map_err(|_| fail(ErrorKind::Io, "timed out in the TLS or HTTP handshake"))?
}

// ---- requests ----

/// One request, built like ureq's.
pub struct Request {
    agent: Agent,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
    timeout: Option<Duration>,
}

impl Request {
    /// Sets a header, replacing an earlier one of the same name.
    pub fn set(mut self, name: &str, value: &str) -> Self {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self.headers.push((name.into(), value.into()));
        self
    }
    /// Appends a query parameter (form encoding).
    pub fn query(mut self, name: &str, value: &str) -> Self {
        self.query.push((name.into(), value.into()));
        self
    }
    /// The whole request, body included; overrides the agent's.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Sends with no body and waits. For threads outside the runtime.
    #[cfg(test)]
    pub fn call(self) -> Result<Response, Error> {
        self.blocking(Bytes::new())
    }
    #[cfg(test)]
    pub fn send_string(self, body: &str) -> Result<Response, Error> {
        self.blocking(Bytes::copy_from_slice(body.as_bytes()))
    }
    #[cfg(test)]
    fn blocking(self, body: Bytes) -> Result<Response, Error> {
        block_on(self.send(body)).unwrap_or_else(|| Err(fail(ErrorKind::Io, "HTTP runtime stopped")))
    }

    /// Sends `body` (empty for none). Async; the response body is read
    /// with [`Response::chunk`] or [`Response::bytes`].
    pub async fn send(self, body: Bytes) -> Result<Response, Error> {
        let config = self.agent.config.clone();
        let deadline = self.timeout.or(config.timeout).map(|t| Instant::now() + t);
        let mut url = Url::parse(&self.url).map_err(|e| fail(ErrorKind::InvalidUrl, e))?;
        if !self.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in &self.query {
                pairs.append_pair(k, v);
            }
        }
        let gzip = self.header("accept-encoding").is_none() && self.header("range").is_none();
        let mut method = self.method.clone();
        let mut headers = self.headers.clone();
        let mut body = body;
        let mut redirects = 0;
        loop {
            let response = self.agent.exchange(&method, &url, &headers, gzip, body.clone(), deadline).await?;
            let location = match response.status {
                301 | 302 | 303 | 307 | 308 if redirects < config.redirects => response.header("location").map(str::to_owned),
                301 | 302 | 303 | 307 | 308 if config.redirects > 0 => {
                    return Err(fail(ErrorKind::TooManyRedirects, "too many redirects"));
                }
                _ => None,
            };
            let next = match (response.status, location) {
                (301..=303, Some(l)) => {
                    if method != "GET" && method != "HEAD" {
                        method = "GET".into();
                    }
                    Some(l)
                }
                (307 | 308, Some(l)) if matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE") => Some(l),
                _ => None,
            };
            let Some(location) = next else {
                return if response.status >= 400 { Err(Error::Status(response.status, response)) } else { Ok(response) };
            };
            // Read the redirect's own body so its connection can serve the
            // next request.
            let mut response = response;
            let mut left: usize = 64 * 1024;
            while let Some(Ok(piece)) = response.body.chunk().await {
                left = match left.checked_sub(piece.len()) { Some(l) => l, None => break };
            }
            drop(response);
            url = url.join(&location).map_err(|e| fail(ErrorKind::InvalidUrl, format!("bad redirection: {e}")))?;
            redirects += 1;
            body = Bytes::new();
            headers.retain(|(n, _)| !["content-length", "cookie", "authorization"].iter().any(|h| n.eq_ignore_ascii_case(h)));
        }
    }
}

impl Agent {
    /// One request and response head on a pooled or new connection.
    async fn exchange(&self, method: &str, url: &Url, headers: &[(String, String)], gzip: bool, body: Bytes, deadline: Option<Instant>) -> Result<Response, Error> {
        let config = &self.config;
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            other => return Err(fail(ErrorKind::UnknownScheme, other)),
        };
        let host = url.host_str().filter(|h| !h.is_empty()).ok_or_else(|| fail(ErrorKind::InvalidUrl, "no host"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let port = url.port_or_known_default().unwrap_or(if https { 443 } else { 80 });
        let key = Key { https, host, port };
        let through_proxy = config.proxy.as_ref().is_some_and(|p| p.kind == ProxyKind::Http) && !https;
        let target = if through_proxy {
            // Absolute form, the URL as given.
            let mut absolute = url.clone();
            absolute.set_fragment(None);
            let _ = absolute.set_username("");
            let _ = absolute.set_password(None);
            absolute.to_string()
        } else {
            match url.query() {
                Some(q) => format!("{}?{q}", url.path()),
                None => url.path().to_string(),
            }
        };
        let host_header = match url.port() {
            Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let build = || -> Result<hyper::Request<Full<Bytes>>, Error> {
            let mut b = hyper::Request::builder().method(method).uri(&target);
            // An invalid method or target (a URI over 64 KiB, for one) is an
            // error for this request, not a panic of the I/O thread.
            let Some(h) = b.headers_mut() else {
                return Err(fail(ErrorKind::InvalidUrl, format!("request target not accepted ({} bytes)", target.len())));
            };
            let put = |h: &mut HeaderMap, name: &str, value: &str| -> Result<(), Error> {
                let name = hyper::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| fail(ErrorKind::BadHeader, e))?;
                let value = HeaderValue::from_str(value).map_err(|e| fail(ErrorKind::BadHeader, e))?;
                h.append(name, value);
                Ok(())
            };
            put(h, "host", &host_header)?;
            for (n, v) in headers {
                put(h, n, v)?;
            }
            if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("user-agent")) {
                put(h, "user-agent", &config.user_agent)?;
            }
            if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("accept")) {
                put(h, "accept", "*/*")?;
            }
            if gzip {
                put(h, "accept-encoding", "gzip")?;
            }
            // Credentials in the URL become Basic authorization, as in ureq.
            let (user, password) = (url.username(), url.password().unwrap_or(""));
            if (!user.is_empty() || !password.is_empty()) && !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("authorization")) {
                put(h, "authorization", &basic(user, password))?;
            }
            if through_proxy {
                if let Some((u, p)) = config.proxy.as_ref().and_then(|p| p.auth.as_ref()) {
                    put(h, "proxy-authorization", &basic(u, p))?;
                }
            }
            if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
                put(h, "content-length", &body.len().to_string())?;
            }
            b.body(Full::new(body.clone())).map_err(|e| fail(ErrorKind::InvalidUrl, e))
        };
        let idempotent = matches!(method, "GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE");
        let mut pooled = self.pool.take(&key);
        loop {
            let reused = pooled.is_some();
            let mut sender = match pooled.take() {
                Some(s) => s,
                None => open(config, &key, deadline).await?,
            };
            let full_limit = bound(config.read_timeout.max(config.write_timeout), deadline);
            let fast_retry = reused && idempotent;
            let head_limit = if fast_retry { Some(full_limit.map_or(REUSED_HEAD_LIMIT, |l| l.min(REUSED_HEAD_LIMIT))) } else { full_limit };
            let sent = within(head_limit, async {
                if sender.ready().await.is_err() {
                    return Sent::Closed;
                }
                let request = match build() {
                    Ok(r) => r,
                    Err(e) => return Sent::Refused(e),
                };
                match sender.try_send_request(request).await {
                    Ok(r) => Sent::Answered(r),
                    Err(mut e) => Sent::Failed { unsent: e.take_message().is_some(), error: e.into_error() },
                }
            })
            .await;
            let response = match sent {
                // A silent reused connection: once more on a new one.
                Err(()) if fast_retry && full_limit.is_none_or(|l| l > REUSED_HEAD_LIMIT) => continue,
                Err(()) => return Err(fail(ErrorKind::Io, "timed out reading response")),
                Ok(Sent::Answered(r)) => r,
                // A kept-alive connection the server had closed: once more
                // on a new one, when the request cannot have been acted on.
                Ok(Sent::Closed) if reused => continue,
                Ok(Sent::Failed { unsent, .. }) if reused && (unsent || idempotent) => continue,
                Ok(Sent::Closed) => return Err(fail(ErrorKind::Io, "connection closed")),
                Ok(Sent::Failed { error, .. }) => return Err(fail(ErrorKind::Io, error)),
                Ok(Sent::Refused(e)) => return Err(e),
            };
            let (parts, incoming) = response.into_parts();
            let status = parts.status.as_u16();
            let mut headers = parts.headers;
            // Any gzip body is decompressed, asked for or not; the length and
            // encoding then describe bytes the caller never sees (ureq
            // removed both too).
            let gzipped = headers.get_all("content-encoding").iter().any(|v| {
                v.to_str().is_ok_and(|v| v.trim().eq_ignore_ascii_case("gzip") || v.trim().eq_ignore_ascii_case("x-gzip"))
            });
            if gzipped {
                headers.remove("content-encoding");
                headers.remove("content-length");
            }
            return Ok(Response {
                url: url.clone(),
                status,
                status_text: parts.status.canonical_reason().unwrap_or("").to_string(),
                headers,
                body: Body { incoming: Some(incoming), sender: Some((key, sender, self.pool.clone())), read_timeout: config.read_timeout, deadline, gzip: gzipped },
            });
        }
    }
}

enum Sent {
    Answered(hyper::Response<Incoming>),
    Closed,
    Failed { unsent: bool, error: hyper::Error },
    Refused(Error),
}

// ---- responses ----

struct Body {
    incoming: Option<Incoming>,
    /// Back to the pool once the body has been read to its end.
    sender: Option<(Key, Sender, Arc<Pool>)>,
    read_timeout: Option<Duration>,
    deadline: Option<Instant>,
    gzip: bool,
}

impl Body {
    async fn chunk(&mut self) -> Option<io::Result<Bytes>> {
        loop {
            let incoming = self.incoming.as_mut()?;
            let frame = within(bound(self.read_timeout, self.deadline), incoming.frame()).await;
            match frame {
                Err(()) => {
                    self.finish(false);
                    return Some(Err(io::Error::new(io::ErrorKind::TimedOut, "timed out reading response")));
                }
                Ok(None) => {
                    self.finish(true);
                    return None;
                }
                Ok(Some(Err(e))) => {
                    self.finish(false);
                    return Some(Err(io::Error::other(e)));
                }
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        if !data.is_empty() {
                            return Some(Ok(data));
                        }
                    }
                }
            }
        }
    }
    fn finish(&mut self, complete: bool) {
        self.incoming = None;
        if let Some((key, sender, pool)) = self.sender.take() {
            if complete {
                pool.give(key, sender);
            }
        }
    }
}

pub struct Response {
    url: Url,
    status: u16,
    status_text: String,
    headers: HeaderMap,
    body: Body,
}

impl Response {
    /// Decode bounded chunks without a worker waiting for the network. Gzip
    /// inflation runs in short pool jobs; plain media needs no decoder job.
    pub fn into_decoded(self) -> DecodedBody { DecodedBody::new(self.body) }

    pub fn status(&self) -> u16 {
        self.status
    }
    pub fn status_text(&self) -> &str {
        &self.status_text
    }
    /// A 304: the validator the request sent names the current version.
    pub fn not_modified(&self) -> bool { self.status == 304 }
    /// Text headers, including repeated Set-Cookie fields.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers.iter().filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
    }
    /// The first value of a header, when it is text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
    }
    /// The URL that answered, after redirects.
    pub fn get_url(&self) -> &str {
        self.url.as_str()
    }

    /// The next piece of the body as sent (still compressed if the server
    /// compressed it); None at its end.
    pub async fn chunk(&mut self) -> Option<io::Result<Bytes>> {
        self.body.chunk().await
    }

    /// The whole body, refused past `limit` wire or decoded bytes. Inflation
    /// uses finite jobs and the only complete buffer is the decoded output.
    pub async fn bytes(self, limit: usize) -> io::Result<Vec<u8>> {
        let mut body=self.into_decoded().with_wire_limit(limit);
        let mut output=Vec::new();
        while let Some(chunk)=body.chunk().await {
            let chunk=chunk?;
            if chunk.len()>limit.saturating_sub(output.len()) {
                return Err(io::Error::new(io::ErrorKind::InvalidData,"response too large"));
            }
            output.extend_from_slice(&chunk);
        }
        Ok(output)
    }

    /// A blocking reader of the decompressed body. The body is read by a
    /// task that stays at most four pieces ahead.
    #[cfg(test)]
    pub fn into_reader(self) -> Box<dyn Read + Send + Sync + 'static> {
        let gzip = self.body.gzip;
        let (tx, reader) = body_channel();
        spawn(Box::pin(pump_body(self.body, tx)));
        decode_reader(gzip, reader)
    }

    /// Decode a streaming body off the executor, keeping at most four body
    /// pieces queued. The caller owns the async pump: dropping this future
    /// closes the channel and connection, waking a decoder blocked in Read.
    /// Gzip inflation and synchronous parsing run on the blocking pool; no
    /// whole response buffer is constructed here.
    #[cfg(test)]
    pub async fn read_with<T: Send + 'static>(
        self,
        decode: impl FnOnce(Box<dyn Read + Send + Sync>) -> T + Send + 'static,
    ) -> io::Result<T> {
        self.read_with_on(test_decoders().handle(), decode).await
    }

    /// Metadata has independent bounded decode capacity. Its caller retains
    /// admission until worker completion, including after async cancellation.
    #[cfg(test)]
    pub(crate) async fn read_with_on<T: Send + 'static>(
        self,
        executor: &tokio::runtime::Handle,
        decode: impl FnOnce(Box<dyn Read + Send + Sync>) -> T + Send + 'static,
    ) -> io::Result<T> {
        let gzip = self.body.gzip;
        let (tx, reader) = body_channel();
        // Gzip's constructor reads its header: initialize it on the worker
        // after the pump can run, never synchronously on the executor.
        let worker = executor.spawn_blocking(move || decode(decode_reader(gzip, reader)));
        pump_body(self.body, tx).await;
        worker.await.map_err(io::Error::other)
    }

    /// The body as text, at most 10 MB, invalid UTF-8 replaced, as ureq
    /// read it.
    #[cfg(test)]
    pub fn into_string(self) -> io::Result<String> {
        const LIMIT: u64 = 10 * 1024 * 1024;
        let mut bytes = Vec::new();
        self.into_reader().take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::other("response too big for into_string"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A response with no connection, for tests of error mapping.
    pub fn synthetic(status: u16) -> Response {
        Response {
            url: Url::parse("http://synthetic.invalid/").unwrap(),
            status,
            status_text: String::new(),
            headers: HeaderMap::new(),
            body: Body { incoming: None, sender: None, read_timeout: None, deadline: None, gzip: false },
        }
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Response[status: {}, url: {}]", self.status, self.url)
    }
}

#[cfg(test)]
enum Piece {
    Data(Bytes),
    End,
    Failed(io::Error),
}

#[cfg(test)]
fn body_channel() -> (tokio::sync::mpsc::Sender<Piece>, Reader) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Piece>(4);
    (tx, Reader { rx, piece: Bytes::new(), end: None })
}
#[cfg(test)]
fn decode_reader(gzip: bool, reader: Reader) -> Box<dyn Read + Send + Sync> {
    if gzip { Box::new(flate2::read::MultiGzDecoder::new(reader)) }
    else { Box::new(reader) }
}

#[cfg(test)]
async fn pump_body(mut body: Body, tx: tokio::sync::mpsc::Sender<Piece>) {
    loop {
        let next = tokio::select! {
            next = body.chunk() => next,
            _ = tx.closed() => return,
        };
        let (piece, last) = match next {
            Some(Ok(data)) => (Piece::Data(data), false),
            Some(Err(e)) => (Piece::Failed(e), true),
            None => (Piece::End, true),
        };
        if tx.send(piece).await.is_err() || last { return; }
    }
}

/// Reads what the body task sends. A failure is returned again on every
/// later read, and a task that stops without the body's end (the runtime
/// shut down) is an error, never a short body taken for a whole one.
#[cfg(test)]
struct Reader {
    rx: tokio::sync::mpsc::Receiver<Piece>,
    piece: Bytes,
    end: Option<Result<(), (io::ErrorKind, String)>>,
}

#[cfg(test)]
impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.piece.is_empty() {
            match &self.end {
                Some(Ok(())) => return Ok(0),
                Some(Err((kind, message))) => return Err(io::Error::new(*kind, message.clone())),
                None => {}
            }
            self.end = match self.rx.blocking_recv() {
                Some(Piece::Data(piece)) => {
                    self.piece = piece;
                    None
                }
                Some(Piece::End) => Some(Ok(())),
                Some(Piece::Failed(e)) => Some(Err((e.kind(), e.to_string()))),
                None => Some(Err((io::ErrorKind::UnexpectedEof, "response body ended early".into()))),
            };
        }
        let n = buf.len().min(self.piece.len());
        buf[..n].copy_from_slice(&self.piece.split_to(n));
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream as StdStream};

    /// The request head, lower-cased, or None when the peer closed.
    fn head(reader: &mut BufReader<StdStream>) -> Option<String> {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            if line == "\r\n" {
                return Some(head.to_lowercase());
            }
            head.push_str(&line);
        }
    }

    fn reply(socket: &mut StdStream, extra: &str, body: &[u8]) {
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra}\r\n", body.len()).unwrap();
        socket.write_all(body).unwrap();
    }

    fn agent() -> Agent {
        AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout_read(Duration::from_secs(5)).build()
    }

    /// Tests that bump or depend on the global network epoch run one at a time.
    static EPOCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn chacha_preference_moves_only_the_chacha_suites_forward() {
        let original = rustls::crypto::ring::default_provider().cipher_suites;
        let mut suites = original.clone();
        prefer_chacha(&mut suites);
        let names = |s: &[rustls::SupportedCipherSuite]| s.iter().map(|c| format!("{:?}", c.suite())).collect::<Vec<_>>();
        let (got, all) = (names(&suites), names(&original));
        let chacha: Vec<_> = all.iter().filter(|n| n.contains("CHACHA20")).cloned().collect();
        let rest: Vec<_> = all.iter().filter(|n| !n.contains("CHACHA20")).cloned().collect();
        assert!(!chacha.is_empty());
        assert_eq!(got, [chacha, rest].concat());
    }

    /// A queue sync of 5,000 songs built a GET target over hyper's 64 KiB
    /// URI limit and the I/O thread panicked (host, 2026-11-21).
    #[test]
    fn an_overlong_target_is_an_error_not_a_panic() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        // The server keeps the connection open, so the request is refused
        // before anything is sent rather than by a closed connection.
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut byte = [0u8; 1];
            let _ = std::io::Read::read(&mut socket, &mut byte);
        });
        let url = format!("http://{address}/rest/savePlayQueue?{}", "id=generated-song-identifier&".repeat(3000));
        let error = agent().get(&url).call().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidUrl, "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_kept_alive_connection_serves_the_next_request() {
        let _epoch = EPOCH.lock().unwrap_or_else(|e| e.into_inner());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            for body in [&b"one"[..], b"two"] {
                head(&mut reader).unwrap();
                reply(&mut socket, "", body);
            }
            listener.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            listener.accept().is_err()
        });
        let agent = agent();
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "one");
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "two");
        assert!(server.join().unwrap(), "a second connection was opened");
    }

    /// UXI-003: after a link change, idle connections are not reused.
    #[test]
    fn a_network_change_drops_idle_connections() {
        let _epoch = EPOCH.lock().unwrap_or_else(|e| e.into_inner());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(first.try_clone().unwrap());
            head(&mut reader).unwrap();
            reply(&mut first, "", b"one");
            // The old connection stays open and silent, as on a dead link.
            let (mut second, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(second.try_clone().unwrap());
            head(&mut reader).unwrap();
            reply(&mut second, "", b"two");
            drop(first);
        });
        let agent = agent();
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "one");
        network_changed();
        let started = Instant::now();
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "two");
        assert!(started.elapsed() < Duration::from_millis(250), "{:?}", started.elapsed());
        server.join().unwrap();
    }

    /// A reused connection that never answers costs a short wait, not the
    /// whole read timeout, and the request succeeds on a new connection.
    #[test]
    fn a_silent_reused_connection_is_retried_quickly() {
        let _epoch = EPOCH.lock().unwrap_or_else(|e| e.into_inner());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(first.try_clone().unwrap());
            head(&mut reader).unwrap();
            reply(&mut first, "", b"one");
            head(&mut reader).unwrap(); // read and ignored: the link is "dead"
            let (mut second, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(second.try_clone().unwrap());
            head(&mut reader).unwrap();
            reply(&mut second, "", b"two");
            drop(first);
        });
        let agent = agent();
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "one");
        let started = Instant::now();
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "two");
        let waited = started.elapsed();
        assert!(waited >= REUSED_HEAD_LIMIT && waited < Duration::from_secs(2), "{waited:?}");
        server.join().unwrap();
    }

    #[test]
    fn a_closed_kept_alive_connection_is_replaced() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for body in [&b"one"[..], b"two"] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                head(&mut reader).unwrap();
                reply(&mut socket, "", body);
            }
        });
        let agent = agent();
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "one");
        // The server has closed the first connection by now or will.
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "two");
        server.join().unwrap();
    }

    #[test]
    fn gzip_is_asked_for_and_undone_except_for_ranges() {
        use flate2::{Compression, write::GzEncoder};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let first = head(&mut reader).unwrap();
            assert!(first.contains("accept-encoding: gzip\r\n"), "{first}");
            let mut gz = GzEncoder::new(Vec::new(), Compression::default());
            gz.write_all(b"plain text").unwrap();
            reply(&mut socket, "Content-Encoding: gzip\r\n", &gz.finish().unwrap());
            let second = head(&mut reader).unwrap();
            assert!(!second.contains("accept-encoding"), "{second}");
            reply(&mut socket, "", b"raw");
        });
        let agent = agent();
        assert_eq!(agent.get(&url).call().unwrap().into_string().unwrap(), "plain text");
        assert_eq!(agent.get(&url).set("Range", "bytes=0-").call().unwrap().into_string().unwrap(), "raw");
        server.join().unwrap();
    }

    #[test]
    fn a_redirect_is_followed_without_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/a", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let first = head(&mut reader).unwrap();
            assert!(first.starts_with("post /a ") && first.contains("authorization: secret"), "{first}");
            let mut body = [0u8; 4];
            std::io::Read::read_exact(&mut reader, &mut body).unwrap();
            write!(socket, "HTTP/1.1 303 See Other\r\nLocation: /b\r\nContent-Length: 0\r\n\r\n").unwrap();
            let second = head(&mut reader).unwrap();
            assert!(second.starts_with("get /b ") && !second.contains("authorization") && !second.contains("content-length"), "{second}");
            reply(&mut socket, "", b"moved");
        });
        let response = agent().post(&url).set("Authorization", "secret").send_string("body").unwrap();
        assert!(response.get_url().ends_with("/b"));
        assert_eq!(response.into_string().unwrap(), "moved");
        server.join().unwrap();
    }

    #[test]
    fn errors_carry_status_and_a_readable_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            head(&mut reader).unwrap();
            write!(socket, "HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\ngone").unwrap();
        });
        match agent().get(&url).call() {
            Err(Error::Status(404, response)) => assert_eq!(response.into_string().unwrap(), "gone"),
            other => panic!("{other:?}"),
        }
        server.join().unwrap();
        let unused = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}/x", unused.local_addr().unwrap());
        drop(unused);
        assert_eq!(agent().get(&closed).call().unwrap_err().kind(), ErrorKind::ConnectionFailed);
        assert_eq!(agent().get("ftp://example.invalid/").call().unwrap_err().kind(), ErrorKind::UnknownScheme);
    }

    #[test]
    fn socks5_hands_the_host_name_to_the_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = Proxy::new(&format!("socks5://{}", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut greeting = [0u8; 3];
            std::io::Read::read_exact(&mut socket, &mut greeting).unwrap();
            assert_eq!(greeting, [5, 1, 0]);
            socket.write_all(&[5, 0]).unwrap();
            let mut request = [0u8; 5 + 13 + 2];
            std::io::Read::read_exact(&mut socket, &mut request).unwrap();
            assert_eq!(&request[..5], &[5, 1, 0, 3, 13]);
            assert_eq!(&request[5..18], b"music.invalid");
            assert_eq!(&request[18..], &4533u16.to_be_bytes());
            socket.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let first = head(&mut reader).unwrap();
            assert!(first.starts_with("get /rest/ping ") && first.contains("host: music.invalid:4533"), "{first}");
            reply(&mut socket, "", b"pong");
        });
        let agent = AgentBuilder::new().proxy(proxy).timeout_read(Duration::from_secs(5)).build();
        assert_eq!(agent.get("http://music.invalid:4533/rest/ping").call().unwrap().into_string().unwrap(), "pong");
        server.join().unwrap();
    }

    #[test]
    fn a_stalled_body_times_out_and_a_dropped_reader_closes_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            head(&mut reader).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nab").unwrap();
            // Stalls; then a stream that never ends, until the client goes.
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            head(&mut reader).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            let start = Instant::now();
            while socket.write_all(b"4\r\ndata\r\n").is_ok() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            closed_tx.send(start.elapsed()).unwrap();
        });
        let agent = AgentBuilder::new().timeout_read(Duration::from_millis(300)).build();
        let mut bytes = Vec::new();
        let error = agent.get(&url).call().unwrap().into_reader().read_to_end(&mut bytes).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(bytes, b"ab");
        let mut reader = agent.get(&url).call().unwrap().into_reader();
        let mut some = [0u8; 8];
        reader.read_exact(&mut some).unwrap();
        assert_eq!(&some, b"datadata");
        drop(reader);
        assert!(closed_rx.recv_timeout(Duration::from_secs(5)).unwrap() < Duration::from_secs(5));
        server.join().unwrap();
    }

    #[test]
    fn async_decoder_cancellation_wakes_reader_and_closes_stalled_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            head(&mut BufReader::new(socket.try_clone().unwrap())).unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\na").unwrap();
            let mut byte = [0];
            match socket.read(&mut byte) {
                Ok(0) => (),
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => (),
                other => panic!("cancelled decoder left socket open: {other:?}"),
            }
        });
        let runtime = tokio::runtime::Builder::new_current_thread().max_blocking_threads(1).enable_all().build().unwrap();
        runtime.block_on(async {
            let response = agent().get(&url).send(Bytes::new()).await.unwrap();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (ended_tx, ended_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(response.read_with(move |mut reader| {
                let mut byte = [0];
                reader.read_exact(&mut byte).unwrap();
                entered_tx.send(()).unwrap();
                let result = reader.read_exact(&mut byte);
                ended_tx.send(result.is_err()).unwrap();
            }));
            tokio::time::timeout(Duration::from_secs(3), entered_rx).await.unwrap().unwrap();
            // The single-thread runtime progressed while the decoder awaited
            // the body. Aborting its pump must also retire the blocked reader.
            // Even a one-thread application pool remains usable while a
            // decoder is waiting for more network data.
            assert!(tokio::time::timeout(Duration::from_secs(1),
                tokio::task::spawn_blocking(|| true)).await.unwrap().unwrap());
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(tokio::time::timeout(Duration::from_secs(3), ended_rx).await.unwrap().unwrap());
        });
        server.join().unwrap();
    }

    #[test]
    fn async_decoder_streams_gzip_and_preserves_incomplete_body_errors() {
        for (gzip, truncated) in [(false, false), (true, false), (false, true)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/x", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                head(&mut BufReader::new(socket.try_clone().unwrap())).unwrap();
                let plain = b"generated streaming body";
                let bytes = if gzip {
                    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    encoder.write_all(plain).unwrap();
                    encoder.finish().unwrap()
                } else { plain.to_vec() };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}\r\n", bytes.len()+usize::from(truncated),
                    if gzip { "Content-Encoding: gzip\r\n" } else { "" }).unwrap();
                socket.write_all(&bytes).unwrap();
            });
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let response = agent().get(&url).send(Bytes::new()).await.unwrap();
                let result = response.read_with(|mut reader| {
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes).map(|_| bytes)
                }).await.unwrap();
                if truncated { assert!(result.is_err()); }
                else { assert_eq!(result.unwrap(), b"generated streaming body"); }
            });
            server.join().unwrap();
        }
    }

    #[test]
    fn async_requests_read_bodies_on_the_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            head(&mut reader).unwrap();
            reply(&mut socket, "", b"0123456789");
        });
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let agent = agent();
        let body = runtime.block_on(async move { agent.get(&url).send(Bytes::new()).await.unwrap().bytes(10).await.unwrap() });
        assert_eq!(body, b"0123456789");
        server.join().unwrap();
    }

    #[test]
    fn any_gzip_body_is_undone_and_its_length_dropped() {
        use flate2::{Compression, write::GzEncoder};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            head(&mut reader).unwrap();
            let mut gz = GzEncoder::new(Vec::new(), Compression::default());
            gz.write_all(b"text \xff").unwrap();
            reply(&mut socket, "Content-Encoding: x-gzip\r\n", &gz.finish().unwrap());
        });
        // A range request does not ask for gzip; a server may send it anyway.
        let response = agent().get(&url).set("Range", "bytes=0-").call().unwrap();
        assert_eq!((response.header("content-length"), response.header("content-encoding")), (None, None));
        assert_eq!(response.into_string().unwrap(), "text \u{fffd}");
        server.join().unwrap();
    }

    #[test]
    fn url_credentials_become_basic_auth_and_never_reach_a_proxy_request_line() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = Proxy::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let first = head(&mut reader).unwrap();
            assert!(first.starts_with("get http://radio.invalid/live "), "{first}");
            assert!(first.contains("authorization: basic dxnlcjpwyxnz\r\n"), "{first}");
            reply(&mut socket, "", b"ok");
        });
        let agent = AgentBuilder::new().proxy(proxy).timeout_read(Duration::from_secs(5)).build();
        assert_eq!(agent.get("http://user:pass@radio.invalid/live").call().unwrap().into_string().unwrap(), "ok");
        server.join().unwrap();
    }

    #[test]
    fn connect_bounds_the_connection_and_the_proxy_answer_is_a_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = Proxy::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            line
        });
        let agent = AgentBuilder::new().proxy(proxy).timeout_connect(Duration::from_millis(200)).timeout_read(Duration::from_millis(800)).build();
        let start = Instant::now();
        let error = agent.get("https://[::1]:8443/x").call().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ProxyConnect);
        assert!(start.elapsed() >= Duration::from_millis(700), "{:?}", start.elapsed());
        assert_eq!(server.join().unwrap(), "CONNECT [::1]:8443 HTTP/1.1\r\n");
    }

    #[test]
    fn a_failed_body_stays_failed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            head(&mut reader).unwrap();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nab").unwrap();
            std::thread::sleep(Duration::from_millis(800));
        });
        let agent = AgentBuilder::new().timeout_read(Duration::from_millis(200)).build();
        let mut reader = agent.get(&url).call().unwrap().into_reader();
        let mut buf = [0u8; 16];
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert_eq!(reader.read(&mut buf).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(reader.read(&mut buf).unwrap_err().kind(), io::ErrorKind::TimedOut);
        server.join().unwrap();
    }

    #[test]
    #[should_panic]
    fn a_blocking_call_on_a_runtime_thread_is_refused() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async { let _ = agent().get("http://127.0.0.1:9/").call(); });
    }

    #[test]
    fn proxies_parse_as_ureq_did() {
        assert_eq!(Proxy::new("h").unwrap().port, 80);
        let p = Proxy::new("127.0.0.1:1055").unwrap();
        assert_eq!((p.kind, p.host.as_str(), p.port), (ProxyKind::Http, "127.0.0.1", 1055));
        assert_eq!(Proxy::new("socks5://h").unwrap().port, 1080);
        assert_eq!(Proxy::new("socks://u:p%40w@h:9").unwrap().auth, Some(("u".into(), "p@w".into())));
        assert!(Proxy::new("ftp://h:1").is_err());
        assert_eq!(basic("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(basic("a", ""), "Basic YTo=");
    }
}
