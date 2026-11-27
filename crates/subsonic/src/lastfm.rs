//! Last.fm request encoding and async transport. No credentials
//! acquisition or listening policy. Qualify audible listens before submitting;
//! await transport and never log credential-bearing bodies.
mod transport;
use md5::{Digest, Md5};
use std::collections::BTreeMap;
pub use transport::{Client, Error, Outcome};

/// Project-owned application credentials and the user's authorized session.
/// Deliberately not Debug: these must never be included in diagnostic output.
pub struct Credentials<'a> {
    pub api_key: &'a str,
    pub shared_secret: &'a str,
    pub session: &'a str,
}

pub struct Listen<'a> {
    pub artist: &'a str,
    pub title: &'a str,
    pub album: Option<&'a str>,
    pub duration_seconds: Option<u64>,
    pub started_at: u64,
    /// Broadcast programming is selected by the station. For library tracks
    /// the selection is unknown, so the scrobble omits the optional field.
    pub is_broadcast: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    Credentials,
    Metadata,
    Timestamp,
    BatchSize,
}

/// A form body contains a private session key. It has no Debug implementation.
pub struct FormBody(String);
impl FormBody {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn base(
    credentials: &Credentials<'_>,
    method: &str,
) -> Result<BTreeMap<String, String>, EncodeError> {
    if [
        credentials.api_key,
        credentials.shared_secret,
        credentials.session,
    ]
    .iter()
    .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_graphic()))
    {
        return Err(EncodeError::Credentials);
    }
    Ok(BTreeMap::from([
        ("api_key".into(), credentials.api_key.into()),
        ("method".into(), method.into()),
        ("sk".into(), credentials.session.into()),
    ]))
}

fn metadata(
    params: &mut BTreeMap<String, String>,
    listen: &Listen<'_>,
    suffix: &str,
) -> Result<(), EncodeError> {
    if listen.artist.trim().is_empty()
        || listen.title.trim().is_empty()
        || listen.duration_seconds == Some(0)
    {
        return Err(EncodeError::Metadata);
    }
    params.insert(format!("artist{suffix}"), listen.artist.into());
    params.insert(format!("track{suffix}"), listen.title.into());
    if let Some(album) = listen.album.filter(|s| !s.trim().is_empty()) {
        params.insert(format!("album{suffix}"), album.into());
    }
    if let Some(duration) = listen.duration_seconds {
        params.insert(format!("duration{suffix}"), duration.to_string());
    }
    Ok(())
}

fn sign(params: &BTreeMap<String, String>, secret: &str) -> String {
    // Last.fm specifies this MD5 construction; it is not a password hash.
    // Sign raw UTF-8, before form escaping. ASCII parameter sorting deliberately
    // places artist[10] before artist[1], not numeric batch-index order.
    let mut hash = Md5::new();
    for (name, value) in params {
        if matches!(name.as_str(), "format" | "callback" | "api_sig") {
            continue;
        }
        hash.update(name.as_bytes());
        hash.update(value.as_bytes());
    }
    hash.update(secret.as_bytes());
    format!("{:x}", hash.finalize())
}

fn finish(mut params: BTreeMap<String, String>, secret: &str) -> FormBody {
    params.insert("api_sig".into(), sign(&params, secret));
    params.insert("format".into(), "json".into());
    FormBody(
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params)
            .finish(),
    )
}

/// Encode one newly started track. This notification must not be retried
/// as a completed listen. No timestamp or chosenByUser is sent for now-playing.
pub fn now_playing(
    credentials: &Credentials<'_>,
    listen: &Listen<'_>,
) -> Result<FormBody, EncodeError> {
    let mut params = base(credentials, "track.updateNowPlaying")?;
    metadata(&mut params, listen, "")?;
    Ok(finish(params, credentials.shared_secret))
}

/// Preserve caller order (oldest cached first); the worker owns persistence,
/// per-item acknowledgement and retry. Broadcast tracks use chosenByUser=0.
pub fn scrobbles(
    credentials: &Credentials<'_>,
    listens: &[Listen<'_>],
) -> Result<FormBody, EncodeError> {
    if listens.is_empty() || listens.len() > 50 {
        return Err(EncodeError::BatchSize);
    }
    let mut params = base(credentials, "track.scrobble")?;
    for (index, listen) in listens.iter().enumerate() {
        if listen.started_at == 0 || listen.started_at > i64::MAX as u64 {
            return Err(EncodeError::Timestamp);
        }
        let suffix = if listens.len() == 1 {
            String::new()
        } else {
            format!("[{index}]")
        };
        metadata(&mut params, listen, &suffix)?;
        params.insert(format!("timestamp{suffix}"), listen.started_at.to_string());
        if listen.is_broadcast {
            params.insert(format!("chosenByUser{suffix}"), "0".into());
        }
    }
    Ok(finish(params, credentials.shared_secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn credentials() -> Credentials<'static> {
        Credentials {
            api_key: "generated-key",
            shared_secret: "generated-secret",
            session: "generated-session",
        }
    }
    fn listen() -> Listen<'static> {
        Listen {
            artist: "Café + Quartet",
            title: "A&B / 🎵",
            album: None,
            duration_seconds: None,
            started_at: 1793721600,
            is_broadcast: true,
        }
    }
    fn decode(body: &FormBody) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(body.as_str().as_bytes())
            .into_owned()
            .collect()
    }

    #[test]
    fn independent_utf8_signature_and_form_golden() {
        let body = scrobbles(&credentials(), &[listen()]).unwrap();
        let params = decode(&body);
        // Independently computed with Python hashlib over literal raw UTF-8
        // names/values in documented order, not this module's signer.
        assert_eq!(params["api_sig"], "f9ea9e1ef4c5ea6697e118372171bc44");
        assert_eq!(params["artist"], listen().artist);
        assert_eq!(params["track"], listen().title);
        assert_eq!(params["timestamp"], "1793721600");
        assert_eq!(params["chosenByUser"], "0");
        assert_eq!(params["format"], "json");
        assert_eq!(params["sk"], "generated-session");
        assert!(!body.as_str().contains("generated-secret"));
        assert!(body.as_str().contains("A%26B+%2F+%F0%9F%8E%B5"));
        let now = decode(&now_playing(&credentials(), &listen()).unwrap());
        assert_eq!(now["method"], "track.updateNowPlaying");
        assert!(!now.contains_key("timestamp") && !now.contains_key("chosenByUser"));
    }

    #[test]
    fn all_batch_sizes_preserve_identity_and_ascii_sorting() {
        for size in 1..=50 {
            let listens: Vec<_> = (0..size)
                .map(|i| Listen {
                    started_at: 1793721600 + i as u64,
                    ..listen()
                })
                .collect();
            let params = decode(&scrobbles(&credentials(), &listens).unwrap());
            for i in 0..size {
                let suffix = if size == 1 {
                    String::new()
                } else {
                    format!("[{i}]")
                };
                assert_eq!(
                    params[&format!("timestamp{suffix}")],
                    listens[i].started_at.to_string()
                );
                assert_eq!(params[&format!("chosenByUser{suffix}")], "0");
            }
            if size > 10 {
                let keys: Vec<_> = params.keys().collect();
                assert!(
                    keys.iter().position(|k| *k == "artist[10]").unwrap()
                        < keys.iter().position(|k| *k == "artist[1]").unwrap()
                );
            }
            let expected = params["api_sig"].clone();
            if size == 12 {
                assert_eq!(
                    expected, "567bef1b8e79579aac7073fe04544dd4",
                    "independent indexed UTF-8 signature"
                );
            }
            for seed in 0..20 {
                let mut pairs: Vec<_> =
                    params.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                fastrand::Rng::with_seed(seed).shuffle(&mut pairs);
                let mut shuffled: BTreeMap<_, _> = pairs.into_iter().collect();
                shuffled.insert("format".into(), "xml".into());
                shuffled.insert("callback".into(), "generated".into());
                assert_eq!(sign(&shuffled, credentials().shared_secret), expected);
            }
        }
    }

    #[test]
    fn invalid_requests_fail_before_they_can_be_sent() {
        assert!(matches!(
            scrobbles(&credentials(), &[]),
            Err(EncodeError::BatchSize)
        ));
        let oversized: Vec<_> = (0..51).map(|_| listen()).collect();
        assert!(matches!(
            scrobbles(&credentials(), &oversized),
            Err(EncodeError::BatchSize)
        ));
        for at in [0, u64::MAX] {
            assert!(matches!(
                scrobbles(
                    &credentials(),
                    &[Listen {
                        started_at: at,
                        ..listen()
                    }]
                ),
                Err(EncodeError::Timestamp)
            ));
        }
        for artist in ["", "  "] {
            assert!(matches!(
                now_playing(&credentials(), &Listen { artist, ..listen() }),
                Err(EncodeError::Metadata)
            ));
        }
        assert!(matches!(
            now_playing(
                &credentials(),
                &Listen {
                    duration_seconds: Some(0),
                    ..listen()
                }
            ),
            Err(EncodeError::Metadata)
        ));
        assert!(matches!(
            now_playing(
                &Credentials {
                    session: "invalid\nsession",
                    ..credentials()
                },
                &listen()
            ),
            Err(EncodeError::Credentials)
        ));
    }
}
