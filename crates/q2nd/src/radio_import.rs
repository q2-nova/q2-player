#![forbid(unsafe_code)]
//! Explicit station administration, run before logging/UI initialization.
//! Saved credentials stay in this process; never print config, URLs or errors
//! from authenticated HTTP requests (which may contain a token).
use crate::config::Config;
use serde::Deserialize;
use std::path::Path;
use subsonic::{Client, RadioStation, calls};

#[derive(Deserialize)]
pub struct Station {
    name: String,
    stream_url: String,
    home_page_url: String,
    #[serde(default)]
    aliases: Vec<String>,
}
fn name_key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
fn url_key(url: &str) -> &str {
    url.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('?')
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
}
fn matches(target: &Station, existing: &RadioStation) -> bool {
    url_key(&target.stream_url) == url_key(&existing.stream_url)
        || name_key(&target.name) == name_key(&existing.name)
        || target
            .aliases
            .iter()
            .any(|a| name_key(a) == name_key(&existing.name))
}
fn safe_error(e: subsonic::Error) -> String {
    match e {
        subsonic::Error::Api { code, .. } => {
            format!("Navidrome API error {code}; station editing may require an administrator")
        }
        subsonic::Error::Http(code) => format!("Navidrome HTTP error {code}"),
        subsonic::Error::Network(_) | subsonic::Error::Io(_) => "Unable to reach Navidrome".into(),
        subsonic::Error::Json(_) => "Unexpected Navidrome response".into(),
    }
}
pub fn run(_blocking: &crate::rt::Blocking, manifest: &Path, config: &Path, apply: bool) -> Result<(), String> {
    let targets: Vec<Station> = serde_json::from_slice(
        &std::fs::read(manifest).map_err(|_| "Cannot read public station manifest")?,
    )
    .map_err(|_| "Invalid public station manifest")?;
    if targets.is_empty()
        || targets.len() > 32
        || targets.iter().any(|s| {
            s.name.trim().is_empty()
                || s.name.chars().any(char::is_control)
                || !(s.stream_url.starts_with("https://") || s.stream_url.starts_with("http://"))
                || !(s.home_page_url.starts_with("https://")
                    || s.home_page_url.starts_with("http://"))
        })
    {
        return Err("Invalid station fields (HTTP(S) URLs and nonempty names required)".into());
    }
    let cfg = Config::from_toml(
        &std::fs::read_to_string(config).map_err(|_| "Cannot open saved login on this device")?,
    )
    .map_err(|_| "Cannot parse saved configuration; no private details displayed")?;
    if !cfg.server.is_configured() {
        return Err("No Navidrome account configured".into());
    }
    let client = Client::with_proxy(
        cfg.server.credentials(),
        "q2-radio-import",
        cfg.tailscale.enabled.then_some(crate::tailscale::PROXY),
    )
    .map_err(safe_error)?;
    crate::rt::Ui::new().map_err(|_| "Unable to start station administration")?
        .block_on(async {
            let mut existing = client
                .get_async(&calls::internet_radio_stations()).await
                .map_err(safe_error)?
                .internet_radio_station;
            println!("Existing station count: {}", existing.len());
            let mut added = 0;
            for target in &targets {
                if existing.iter().any(|s| matches(target, s)) {
                    println!("Already present: {}", target.name);
                    continue;
                }
                if !apply {
                    println!("Would add: {}", target.name);
                    continue;
                }
                let call = calls::create_radio_station(&target.name, &target.stream_url);
                client
                    .fetch_async(&call.request.param("homePageUrl", &target.home_page_url)).await
                    .map_err(safe_error)?;
                existing = client
                    .get_async(&calls::internet_radio_stations()).await
                    .map_err(safe_error)?
                    .internet_radio_station;
                if !existing
                    .iter()
                    .any(|s| s.stream_url == target.stream_url && s.name == target.name)
                {
                    return Err(format!("Could not verify creation of {}", target.name));
                }
                println!("Added and verified: {}", target.name);
                added += 1;
            }
            if apply {
                let verified = client
                    .get_async(&calls::internet_radio_stations()).await
                    .map_err(safe_error)?
                    .internet_radio_station;
                if targets
                    .iter()
                    .any(|t| !verified.iter().any(|s| matches(t, s)))
                {
                    return Err("Final station verification failed".into());
                }
                println!(
                    "Verified {} requested stations; {} newly added.",
                    targets.len(),
                    added
                );
            }
            Ok(())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_detection_keeps_distinct_fip_channels() {
        let t = Station {
            name: "FIP".into(),
            stream_url: "https://example.test/fip.mp3".into(),
            home_page_url: "https://example.test".into(),
            aliases: vec!["FIP Radio".into()],
        };
        for s in [
            RadioStation {
                name: "Fip Radio".into(),
                ..Default::default()
            },
            RadioStation {
                stream_url: "http://example.test/fip.mp3?player=app".into(),
                ..Default::default()
            },
        ] {
            assert!(matches(&t, &s));
        }
        assert!(!matches(
            &t,
            &RadioStation {
                name: "FIP Jazz".into(),
                stream_url: "https://example.test/jazz.mp3".into(),
                ..Default::default()
            }
        ));
    }
    #[test]
    fn errors_cannot_expose_credentials_or_response_text() {
        assert!(!safe_error(subsonic::Error::Network("secret-token".into())).contains("secret"));
        assert!(
            !safe_error(subsonic::Error::Api {
                code: 50,
                message: "secret-password".into()
            })
            .contains("secret")
        );
    }
}
