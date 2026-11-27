//! Radio France's public LiveMeta feed, also used by its web player.
//! FIP's MP3 stream does not carry ICY titles. No account or API key is needed.
use std::io;
use std::time::Duration;
use std::sync::{Arc, OnceLock};
use serde_json::Value;

const ENDPOINT: &str = "https://api.radiofrance.fr/livemeta/pull/7";
const MAX_RESPONSE: u64 = 128 * 1024;

pub fn matches_stream(stream: &str) -> bool {
    let Ok(url) = url::Url::parse(stream) else { return false; };
    matches!(url.scheme(), "http" | "https")
        && url.host_str() == Some("icecast.radiofrance.fr")
        && matches!(url.path(), "/fip-midfi.mp3" | "/fip-lofi.mp3" | "/fip-hifi.aac")
}

pub struct Update {
    pub title: Option<String>,
    pub expires: u64,
    pub refresh: Duration,
}

#[cfg(test)]
pub fn fetch(proxy: Option<&str>) -> io::Result<Update> {
    fetch_from(ENDPOINT, proxy)
}

/// Explicit endpoint for generated fixture callers. Normal radio always uses
/// `fetch_async`, which retains the public FIP endpoint and known-station admission.
#[cfg(test)]
pub fn fetch_from(endpoint: &str, proxy: Option<&str>) -> io::Result<Update> {
    let endpoint = endpoint.to_owned();
    let proxy = proxy.map(str::to_owned);
    crate::http::block_on(async move { fetch_from_async(&endpoint, proxy.as_deref()).await })
        .unwrap_or_else(|| Err(io::Error::other("Radio metadata runtime stopped")))
}

pub async fn fetch_async(proxy: Option<&str>) -> io::Result<Update> {
    fetch_from_async(ENDPOINT, proxy).await
}

struct Decoder {
    // No executor owner: only one demand-created blocking worker, retiring
    // after10s. Static production ownership avoids dropping a runtime on I/O.
    runtime: tokio::runtime::Runtime,
    admission: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    started: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    entered: Arc<std::sync::atomic::AtomicUsize>,
}
impl Decoder {
    fn new() -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(1).thread_name("fip-decode")
                .thread_keep_alive(Duration::from_secs(10)).build().expect("metadata decoder lane"),
            admission: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)]
            started: Arc::default(),
            #[cfg(test)]
            entered: Arc::default(),
        }
    }
}
fn decoder() -> &'static Decoder {
    static DECODER: OnceLock<Decoder> = OnceLock::new();
    DECODER.get_or_init(Decoder::new)
}

/// Async counterpart of the explicit generated-fixture endpoint entry.
pub async fn fetch_from_async(endpoint: &str, proxy: Option<&str>) -> io::Result<Update> {
    fetch_with(endpoint, proxy, decoder()).await
}

async fn fetch_with(endpoint: &str, proxy: Option<&str>, decoder: &Decoder) -> io::Result<Update> {
    // The old dedicated owner bounded transport to8s. Include decoder
    // admission/body/parsing in that limit too; never retry into an unbounded
    // queue when a cancelled worker has not retired yet.
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut builder = crate::http::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(8));
        if let Some(proxy) = proxy {
            builder = builder.proxy(crate::http::Proxy::new(proxy).map_err(|_| io::Error::other("Invalid radio proxy"))?);
        }
        let response = builder.build().get(endpoint).send(Default::default()).await
            .map_err(|e| match e {
                crate::http::Error::Status(code, _) => io::Error::other(format!("Radio metadata HTTP {code}")),
                crate::http::Error::Transport(e) => io::Error::other(format!("Radio metadata connection failed ({:?})", e.kind())),
            })?;
        // Bound admitted bodies too. Network waiting owns no decoder worker;
        // the permit moves into the finite parser job once the body completes.
        // Cancelling that job's waiter cannot prematurely admit another parser.
        #[cfg(test)]
        decoder.entered.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
        let permit = decoder.admission.clone().acquire_owned().await
            .map_err(|_| io::Error::other("Radio metadata decoder stopped"))?;
        #[cfg(test)]
        let started = decoder.started.clone();
        let mut body=response.into_decoded();
        let mut bytes=Vec::new();
        while let Some(chunk)=body.chunk().await {
            let chunk=chunk?;
            if chunk.len()>(MAX_RESPONSE as usize).saturating_sub(bytes.len()) {
                return Err(io::Error::other("Radio metadata too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        decoder.runtime.spawn_blocking(move || {
            let _permit = permit;
            #[cfg(test)]
            {
                assert_eq!(std::thread::current().name(),Some("fip-decode"));
                started.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            }
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            parse(&bytes, now)
        }).await.map_err(io::Error::other)?

    }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut,"Radio metadata timed out"))?
}

fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(256)
        .collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse(bytes: &[u8], now: u64) -> io::Result<Update> {
    let data: Value = serde_json::from_slice(bytes).map_err(|_| io::Error::other("Invalid radio metadata"))?;
    if data["stationId"].as_u64() != Some(7) { return Err(io::Error::other("Unexpected radio station")); }
    let steps = data["steps"].as_object().ok_or_else(|| io::Error::other("Invalid radio metadata"))?;
    // The feed includes previous and upcoming songs. Never select a future
    // entry merely because it is last in the response or at a reported index.
    let song = steps.values().filter(|s| s["stationId"].as_u64() == Some(7)
        && s["embedType"].as_str() == Some("song")
        && s["start"].as_u64().is_some_and(|t| t <= now)
        && s["end"].as_u64().is_some_and(|t| t > now))
        .max_by_key(|s| s["start"].as_u64());
    let Some(song) = song else {
        return Ok(Update { title: None, expires: now, refresh: Duration::from_secs(15) });
    };
    let title = clean(song["title"].as_str().unwrap_or(""));
    let artists = song["highlightedArtists"].as_array().map(|artists|
        artists.iter().filter_map(Value::as_str).take(8).map(clean)
            .filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let artist = if artists.is_empty() { clean(song["authors"].as_str().unwrap_or("")) } else { clean(&artists) };
    let expires = song["end"].as_u64().unwrap();
    Ok(Update {
        title: (!title.is_empty()).then(|| if artist.is_empty() { title } else { format!("{artist} - {title}") }),
        expires,
        refresh: Duration::from_secs(expires.saturating_sub(now).clamp(5, 60)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_known_main_fip_mounts_use_the_public_feed() {
        assert!(matches_stream("https://icecast.radiofrance.fr/fip-midfi.mp3"));
        assert!(!matches_stream("https://icecast.radiofrance.fr/fiprock-midfi.mp3"));
        assert!(!matches_stream("https://icecast.radiofrance.fr.example/fip-midfi.mp3"));
        assert!(!matches_stream("https://example.test/fip-midfi.mp3"));
    }

    #[test]
    fn chooses_current_song_not_past_future_or_program_and_expires_it() {
        let song = |title, start, end| json!({"stationId":7,"embedType":"song","title":title,"highlightedArtists":["Björk"],"start":start,"end":end});
        let bytes = serde_json::to_vec(&json!({"stationId":7,"steps":{
            "past":song("Past",1,100), "current":song("Jóga",100,200),
            "future":song("Future",200,300),
            "program":{"stationId":7,"embedType":"program","title":"Program","start":110,"end":400}
        }})).unwrap();
        let current = parse(&bytes, 150).unwrap();
        assert_eq!(current.title.as_deref(), Some("Björk - Jóga"));
        assert_eq!(current.refresh, Duration::from_secs(50));
        assert_eq!(current.expires, 200);
        assert_eq!(parse(&bytes, 200).unwrap().title.as_deref(), Some("Björk - Future"));
        assert_eq!(parse(&bytes, 300).unwrap().title, None);
        assert!(parse(br#"{"stationId":8,"steps":{}}"#, 150).is_err());
        assert!(parse(b"not json", 150).is_err());
    }
}

#[cfg(test)]
#[path = "france_wire_tests.rs"]
mod wire_tests;
