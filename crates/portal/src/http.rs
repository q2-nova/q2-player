//! A short-lived portal session, separate from the authenticated media client.
use cookie_store::CookieStore;
use q2_portal_trace::Trace;
use serde_json::json;
use std::{collections::HashMap, net::Ipv4Addr, process::Command, time::Duration};
use subsonic::http::{Agent, AgentBuilder, Error};
use url::Url;

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub trait Transport {
    fn decision(&self, _reason: &'static str) {}
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, String>;
    fn cookie(&self, _url: &str, _name: &str) -> Option<String> {
        None
    }
    fn set_cookie(&mut self, _url: &str, _name: &str, _value: &str) -> Result<(), String> {
        Err("Cookies unavailable".into())
    }
}

#[derive(PartialEq, Clone)]
pub struct Link {
    pub ssid: String,
    pub ip: Ipv4Addr,
    pub bssid: String,
}

impl Link {
    pub fn current() -> Result<Self, String> {
        let status = Command::new("/usr/sbin/wpa_cli")
            .args(["-i", "wlan0", "status"])
            .output()
            .map_err(|_| "Couldn’t check Wi-Fi")?;
        let text = std::str::from_utf8(&status.stdout).map_err(|_| "Couldn’t check Wi-Fi")?;
        if !status.status.success() || !text.lines().any(|line| line == "wpa_state=COMPLETED") {
            return Err("Join Wi-Fi first".into());
        }
        let field = |key: &str| text.lines().find_map(|line| line.strip_prefix(key));
        Ok(Self {
            bssid: field("bssid=").ok_or("Join Wi-Fi first")?.to_owned(),
            ssid: field("ssid=").ok_or("Join Wi-Fi first")?.to_owned(),
            ip: field("ip_address=")
                .and_then(|v| v.parse().ok())
                .ok_or("Waiting for a Wi-Fi address")?,
        })
    }
}

pub struct Client<'a> {
    trace: &'a Trace,
    sequence: u32,
    agent: Agent,
    runtime: tokio::runtime::Runtime,
    link: Link,
    // Never transfer login cookies from an untrusted guest portal to another origin,
    // even if a portal sends a wide Domain attribute.
    jars: HashMap<String, CookieStore>,
}

impl<'a> Client<'a> {
    pub fn new(link: Link, trace: &'a Trace) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "Couldn’t start Wi-Fi sign-in")?;
        let agent = AgentBuilder::new()
            .wifi_source(link.ip)
            .redirects(0)
            .timeout_connect(Duration::from_secs(4))
            .timeout(Duration::from_secs(10))
            .build();
        Ok(Self {
            trace,
            sequence: 0,
            agent,
            runtime,
            link,
            jars: HashMap::new(),
        })
    }

    fn exchange(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, String> {
        if self.link != Link::current()? {
            return Err("Wi-Fi changed".into());
        }
        let parsed = Url::parse(url).map_err(|_| "Invalid portal address")?;
        if !matches!(parsed.scheme(), "https" | "http")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.host().is_none()
            || !matches!(method, "GET" | "POST" | "HEAD" | "PUT")
        {
            return Err("Unsupported portal address".into());
        }
        let key = parsed.origin().ascii_serialization();
        if self.jars.len() >= 24 && !self.jars.contains_key(&key) {
            return Err("Too many portal destinations".into());
        }
        let jar = self.jars.entry(key).or_default();
        let mut request = self.agent.request(method, url);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        let cookie_fields = jar
            .get_request_values(&parsed)
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>();
        self.trace.event(
            "dispatch",
            json!({"n":self.sequence,"cookie_count":cookie_fields.len()}),
        );
        let cookies = cookie_fields.join("; ");
        if !cookies.is_empty() {
            request = request.set("Cookie", &cookies);
        }
        let body = body.to_vec().into();
        self.runtime.block_on(async {
            let raw = match request.send(body).await {
                Ok(r) | Err(Error::Status(_, r)) => r,
                Err(error) => {
                    let summary = error.to_string().to_ascii_lowercase();
                    let detail = ["certificate", "tls", "timed out", "connection refused",
                        "network is unreachable", "connection reset", "broken pipe"]
                        .into_iter().find(|label| summary.contains(label));
                    self.trace.event(
                        "transport_error",
                        json!({"n":self.sequence,"kind":format!("{:?}",error.kind()),"detail":detail}),
                    );
                    return Err("Couldn’t reach the Wi-Fi sign-in page".into());
                }
            };
            let status = raw.status();
            let headers: Vec<(String, String)> = raw
                .headers()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            if self.trace.enabled() {
                self.trace.event(
                    "response_headers",
                    json!({"n":self.sequence, "status":status,
                        "http":crate::trace_view::response_headers(url, status, &headers)}),
                );
            }
            let mut set = 0;
            let mut accepted = 0;
            for (name, value) in raw.headers() {
                if name.eq_ignore_ascii_case("set-cookie") {
                    set += 1;
                    if value.len() <= 4096 && jar.parse(value, &parsed).is_ok() { accepted += 1; }
                }
            }
            if set != 0 { self.trace.event("cookies", json!({"n":self.sequence,"set":set,"accepted":accepted})); }
            if jar.iter_any().count() > 128 {
                return Err("Too many portal cookies".into());
            }
            let body = raw
                .bytes(512 * 1024)
                .await
                .map_err(|_| "Couldn’t read the Wi-Fi sign-in page")?;
            Ok(Response {
                status,
                headers,
                body,
            })
        })
    }
}

impl Transport for Client<'_> {
    fn decision(&self, reason: &'static str) {
        self.trace
            .event("decision", json!({"n":self.sequence, "reason":reason}));
    }

    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, String> {
        self.sequence += 1;
        if !self.trace.enabled() {
            return self.exchange(method, url, headers, body);
        }
        self.trace.event(
            "request",
            json!({"n":self.sequence,
            "http":crate::trace_view::request(method, url, headers, body)}),
        );
        let started = std::time::Instant::now();
        let result = self.exchange(method, url, headers, body);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(r) => self.trace.event(
                "response",
                json!({"n":self.sequence,"elapsed_ms":elapsed_ms,
                "http":crate::trace_view::response(url,r.status,&r.headers,&r.body)}),
            ),
            Err(message) => self.trace.event(
                "request_failed",
                json!({"n":self.sequence,"elapsed_ms":elapsed_ms,"reason":message}),
            ),
        }
        result
    }

    fn cookie(&self, url: &str, name: &str) -> Option<String> {
        let url = Url::parse(url).ok()?;
        self.jars
            .get(&url.origin().ascii_serialization())?
            .get_request_values(&url)
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_owned())
    }

    fn set_cookie(&mut self, url: &str, name: &str, value: &str) -> Result<(), String> {
        let url = Url::parse(url).map_err(|_| "Invalid cookie origin")?;
        self.jars
            .entry(url.origin().ascii_serialization())
            .or_default()
            .parse(&format!("{name}={value}; Path=/; Secure"), &url)
            .map(|_| ())
            .map_err(|_| "Invalid session cookie".into())
    }
}
