use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// The server could not be reached, or the connection failed.
    #[error("network: {0}")]
    Network(String),
    /// An HTTP status other than 2xx.
    #[error("HTTP {0}")]
    Http(u16),
    /// A malformed or unexpected response.
    #[error("bad response: {0}")]
    Json(String),
    /// The server answered with a Subsonic error.
    #[error("{message} ({code})")]
    Api { code: u32, message: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Whether the failure means "can't reach the server" (as opposed to the
    /// server refusing the request), so the app can fall back to its cache.
    pub fn is_offline(&self) -> bool {
        matches!(self, Error::Network(_) | Error::Io(_)) || matches!(self, Error::Http(s) if *s >= 500)
    }

    /// Wrong user name, password or API key.
    pub fn is_auth(&self) -> bool {
        matches!(self, Error::Api { code: 40 | 41 | 42 | 43 | 44, .. }) || matches!(self, Error::Http(401))
    }
}

/// How a request that could not be served failed, for reachability
/// decisions and numeric telemetry. Network messages are this crate's own
/// (`http::Transport`'s `Kind: message`), never server text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Failure {
    /// No answer in time: connect, proxy answer, headers or body.
    Timeout = 1,
    /// The connection was refused or the name did not resolve.
    Refused = 2,
    /// The proxy (Tailscale's local proxy) failed or refused the tunnel.
    Proxy = 3,
    /// The server answered 5xx.
    Server = 4,
    /// Any other transport failure: reset, closed, TLS.
    Network = 5,
}

impl Error {
    /// The failure class of an [`is_offline`](Self::is_offline) error.
    pub fn failure(&self) -> Option<Failure> {
        let timed_out = |text: &str| text.contains("timed out") || text.contains("timeout");
        Some(match self {
            Error::Http(status) if *status >= 500 => Failure::Server,
            Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => Failure::Timeout,
            Error::Io(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => Failure::Refused,
            Error::Io(_) => Failure::Network,
            Error::Network(text) if text.starts_with("ProxyConnect") || text.starts_with("ProxyUnauthorized") => {
                if timed_out(text) { Failure::Timeout } else { Failure::Proxy }
            }
            Error::Network(text) if timed_out(text) => Failure::Timeout,
            Error::Network(text) if text.starts_with("Dns") || text.contains("refused") => Failure::Refused,
            Error::Network(_) => Failure::Network,
            _ => return None,
        })
    }
}

#[test]
fn failures_are_classified_from_the_transport_kind() {
    for (error, class) in [
        (Error::Http(503), Some(Failure::Server)),
        (Error::Http(404), None),
        (Error::Network("Io: timed out reading response".into()), Some(Failure::Timeout)),
        (Error::Network("ConnectionFailed: connect timed out".into()), Some(Failure::Timeout)),
        (Error::Network("ConnectionFailed: Connection refused (os error 111)".into()), Some(Failure::Refused)),
        (Error::Network("Dns: failed to lookup address information".into()), Some(Failure::Refused)),
        (Error::Network("ProxyConnect: proxy answered 502".into()), Some(Failure::Proxy)),
        (Error::Network("ProxyConnect: proxy timed out".into()), Some(Failure::Timeout)),
        (Error::Network("Io: connection closed".into()), Some(Failure::Network)),
        (Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out reading response")), Some(Failure::Timeout)),
        (Error::Api { code: 70, message: "not found".into() }, None),
    ] {
        assert_eq!(error.failure(), class, "{error}");
        if class.is_some() { assert!(error.is_offline()); }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
