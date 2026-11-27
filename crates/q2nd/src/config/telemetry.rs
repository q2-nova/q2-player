//! Optional numeric reporting. Runtime overlays never enter saved user settings.
use serde::{Deserialize, Serialize};
use std::{fmt, io::{self, Read}, os::unix::fs::OpenOptionsExt, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    #[default]
    #[serde(rename = "production")]
    Production,
    #[serde(rename = "automated-test")]
    AutomatedTest,
}
impl Stream {
    pub fn as_str(self) -> &'static str {
        match self { Self::Production => "production", Self::AutomatedTest => "automated-test" }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telemetry {
    pub enabled: bool,
    pub stream: Stream,
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Operator-provisioned defaults are used when routing is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}
impl fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telemetry").field("enabled", &self.enabled)
            .field("stream", &self.stream).finish_non_exhaustive()
    }
}
pub fn valid_uuid(value: &str) -> bool {
    value.len() == 36 && value.bytes().enumerate().all(|(i, b)| {
        if [8, 13, 18, 23].contains(&i) { b == b'-' }
        else { b.is_ascii_hexdigit() && !b.is_ascii_uppercase() }
    })
}
impl Telemetry {
    pub fn validate(&self) -> io::Result<()> {
        let bad = || io::Error::other("invalid telemetry settings");
        if self.labels.iter().any(|label| label != "automated-test") || self.labels.len() > 1
            || self.run_id.as_ref().is_some_and(|id| !valid_uuid(id)) {
            return Err(bad());
        }
        if self.stream == Stream::Production && !self.labels.is_empty() { return Err(bad()); }
        if let Some(endpoint) = &self.endpoint {
            // Endpoint is a base URL; no credentials, query, fragment or paths.
            let authority = endpoint.strip_prefix("https://").or_else(|| endpoint.strip_prefix("http://"))
                .ok_or_else(bad)?.trim_end_matches('/');
            if authority.is_empty() || authority.len() > 256 || authority.bytes().any(|b|
                !(b.is_ascii_alphanumeric() || b".:-[]".contains(&b))) { return Err(bad()); }
        }
        if self.token.as_ref().is_some_and(|token| token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err(bad());
        }
        Ok(())
    }
    /// Capture context labels are canonical even when an explicit test stream
    /// omits its redundant label. Missing overlays are not a runtime error.
    pub fn capture_labels(&self) -> Vec<String> {
        match self.stream { Stream::Production => Vec::new(), Stream::AutomatedTest => vec!["automated-test".into()] }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayFields {
    enabled: Option<bool>, stream: Option<Stream>, labels: Option<Vec<String>>,
    run_id: Option<String>, endpoint: Option<String>, token: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Overlay { telemetry: OverlayFields }

/// Apply only explicitly supplied fields to a separate effective copy. The
/// caller retains the original Config for every save/snapshot operation.
pub fn apply_overlay(base: &Telemetry, text: &str) -> io::Result<Telemetry> {
    if text.len() > 4096 { return Err(io::Error::other("telemetry overlay bound")); }
    let overlay: Overlay = toml::from_str(text).map_err(|_| io::Error::other("invalid telemetry overlay"))?;
    let fields = overlay.telemetry;
    let mut effective = base.clone();
    if let Some(value) = fields.enabled { effective.enabled = value; }
    if let Some(value) = fields.stream { effective.stream = value; }
    if let Some(value) = fields.labels { effective.labels = value; }
    if let Some(value) = fields.run_id { effective.run_id = Some(value); }
    if let Some(value) = fields.endpoint { effective.endpoint = Some(value); }
    if let Some(value) = fields.token { effective.token = Some(value); }
    effective.validate()?;
    Ok(effective)
}

pub fn load_overlay(_blocking: &crate::rt::Blocking, base: &Telemetry, path: &Path) -> io::Result<Telemetry> {
    let mut file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() { return Err(io::Error::other("nonregular telemetry overlay")); }
    let mut text = String::new();
    Read::by_ref(&mut file).take(4097).read_to_string(&mut text)
        .map_err(|_| io::Error::other("invalid telemetry overlay encoding"))?;
    apply_overlay(base, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opt_in_defaults_to_production_and_roundtrips() {
        let off = crate::config::Config::parse("", "generated");
        assert_eq!(off.telemetry, Telemetry::default());
        let on = crate::config::Config::parse("[telemetry]\nenabled=true\n", "generated");
        assert!(on.telemetry.enabled);
        assert_eq!(on.telemetry.stream, Stream::Production);
        assert!(on.telemetry.capture_labels().is_empty());
        let text = toml::to_string(&on).unwrap();
        assert_eq!(crate::config::Config::parse(&text, "generated"), on);
    }
    #[test]
    fn overlay_never_changes_saved_config_and_preserves_explicit_base_routing() {
        let base = crate::config::Config::parse("[telemetry]\nenabled=true\nendpoint='http://127.0.0.1:8766'\n", "generated");
        let original = toml::to_string(&base).unwrap();
        let effective = apply_overlay(&base.telemetry, "[telemetry]\nstream='automated-test'\nrun_id='12345678-1234-1234-1234-123456789abc'\n").unwrap();
        assert!(effective.enabled);
        assert_eq!(effective.endpoint, base.telemetry.endpoint);
        assert_eq!(effective.capture_labels(), ["automated-test"]);
        assert_eq!(toml::to_string(&base).unwrap(), original);
        assert_eq!(apply_overlay(&effective, "[telemetry]\nenabled=false\n").unwrap().enabled, false);
    }
    #[test]
    fn invalid_telemetry_fails_closed_without_losing_other_settings() {
        for field in ["enabled='true'", "stream='typo'", "labels=['secret-canary']", "endpoint='http://user:secret@host'", "token='secret-canary'", "run_id='arbitrary-secret'", "unexpected='secret-canary'"] {
            let text = format!("[playback]\nvolume=37\n[telemetry]\n{field}\n");
            let config = crate::config::Config::parse(&text, "generated");
            assert_eq!(config.playback.volume, 37);
            assert_eq!(config.telemetry, Telemetry::default());
            let error = apply_overlay(&Telemetry::default(), &format!("[telemetry]\n{field}\n")).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
        assert!(apply_overlay(&Telemetry::default(), "[server]\npassword='secret-canary'\n").is_err());
    }
    #[test]
    fn debug_does_not_expose_routing_credentials_or_metadata() {
        let config = Telemetry { enabled: true, token: Some("a".repeat(64)), endpoint: Some("http://private-host".into()), ..Telemetry::default() };
        let text = format!("{config:?}");
        assert!(!text.contains("aaaa")); assert!(!text.contains("private-host"));
    }
}
