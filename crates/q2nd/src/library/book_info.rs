//! Card-owned readable metadata. Cache identity follows the authenticated
//! account, not a transport route or API key; publication follows its generation.
use super::{LibError, Library, Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use subsonic::{Chapter, Song, SpokenSource};

#[derive(Debug, Clone, PartialEq)]
pub enum BookInfoTarget {
    Local {
        relative_path: String,
    },
    Shelf {
        namespace: String,
        library: Option<String>,
        item: String,
        episode: Option<String>,
    },
}
impl BookInfoTarget {
    pub fn from_song(song: &Song) -> Option<Self> {
        match &song.spoken.as_ref()?.source {
            SpokenSource::Local { relative_path } => Some(Self::Local {
                relative_path: relative_path.clone(),
            }),
            SpokenSource::Audiobookshelf {
                namespace,
                item,
                episode,
                ..
            } => Some(Self::Shelf {
                namespace: namespace.clone(),
                library: None,
                item: item.clone(),
                episode: episode.clone(),
            }),
            _ => None,
        }
    }
    pub fn remote(&self) -> bool {
        matches!(self, Self::Shelf { .. })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BookInfo {
    pub title: String,
    pub subtitle: String,
    pub description: String,
    pub authors: Vec<String>,
    pub narrators: Vec<String>,
    pub series: Vec<String>,
    pub duration_ms: u64,
    pub chapters: Vec<Chapter>,
    pub publisher: String,
    pub published_year: String,
    pub language: String,
    pub genres: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Cached {
    info: BookInfo,
    library: Option<String>,
}
fn fail(message: impl std::fmt::Display) -> LibError {
    LibError::Server(message.to_string())
}
fn changed() -> LibError {
    fail(subsonic::audiobookshelf::Error::Changed)
}
fn cache_key(target: &BookInfoTarget) -> String {
    use md5::{Digest, Md5};
    match target {
        BookInfoTarget::Local { relative_path } => format!(
            "book-info:1:local:{:x}",
            Md5::digest(relative_path.as_bytes())
        ),
        BookInfoTarget::Shelf {
            namespace,
            item,
            episode,
            ..
        } => format!(
            "book-info:1:{namespace}:{:x}",
            Md5::digest(serde_json::to_vec(&(item, episode)).expect("string tuple serialization"))
        ),
    }
}

pub(super) fn local_cache_record(relative_path: &str, info: BookInfo) -> Result<(String, String), LibError> {
    let target = BookInfoTarget::Local { relative_path: relative_path.into() };
    Ok((cache_key(&target), serde_json::to_string(&Cached { info, library: None }).map_err(fail)?))
}

impl Library {
    pub fn shelf_info_target(
        &self,
        library: Option<String>,
        item: String,
        episode: Option<String>,
    ) -> Result<BookInfoTarget, LibError> {
        let slot = self.shelf.read().unwrap();
        if slot.namespace.is_empty() {
            return Err(LibError::Offline);
        }
        Ok(BookInfoTarget::Shelf {
            namespace: slot.namespace.clone(),
            library,
            item,
            episode,
        })
    }

    /// Compatibility entry for callers outside runtime/pool/database work.
    pub fn book_info(&self, target: &BookInfoTarget, policy: Policy) -> Result<BookInfo, LibError> {
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        let target = target.clone();
        super::shelf_browse::wait(async move { lib.book_info_async(target, policy).await })
    }

    pub async fn book_info_async(self: &Arc<Self>, target: BookInfoTarget, policy: Policy) -> Result<BookInfo, LibError> {
        let lib = self.clone();
        let (generation, offline) = crate::rt::spawn_blocking(move |_| (lib.shelf_generation(), lib.offline_mode()))
            .await.map_err(fail)?;
        if offline && policy == Policy::NetworkOnly { return Err(LibError::Offline); }
        if policy == Policy::CacheOnly || offline || policy == Policy::PreferCache {
            let cached = self.cached_book_info_async(target.clone(), generation.clone()).await;
            if policy == Policy::CacheOnly || offline || cached.is_ok() { return cached; }
        }
        let result = self.fetch_book_info_async(target.clone(), generation.clone()).await;
        if policy == Policy::NetworkFirst {
            match result {
                Ok(info) => Ok(info),
                Err(error) => self.cached_book_info_async(target, generation).await.or(Err(error)),
            }
        } else { result }
    }

    /// What the background sync's lists hold about `target`, for an item
    /// whose own page was never fetched (UXI-016): its title and its row's
    /// detail (the author, or an episode's date and length). `None` for card
    /// books, an item outside a known library, or without a completed sync.
    pub async fn synced_book_info_async(self: &Arc<Self>, target: &BookInfoTarget) -> Option<BookInfo> {
        let BookInfoTarget::Shelf { namespace, library: Some(library), item, episode } = target else { return None };
        let entry = self.shelf_synced_entry(namespace, library, item, episode.as_deref()).await.ok()??;
        let mut info = BookInfo { title: readable(&entry.title, 4096), ..BookInfo::default() };
        let detail = readable(&entry.detail, 4096);
        if !detail.is_empty() {
            if matches!(entry.kind, super::shelf_browse::EntryKind::Episode { .. }) { info.subtitle = detail; }
            else { info.authors.push(detail); }
        }
        Some(info)
    }

    async fn cached_book_info_async(self: &Arc<Self>, target: BookInfoTarget, generation: Arc<()>) -> Result<BookInfo, LibError> {
        let key = cache_key(&target);
        let remote = target.remote();
        if let BookInfoTarget::Shelf { namespace, .. } = &target {
            let lib = self.clone(); let expected = generation.clone(); let namespace = namespace.clone();
            crate::rt::spawn_blocking(move |_| {
                let slot = lib.shelf.read().unwrap();
                if !Arc::ptr_eq(&slot.generation, &expected) || slot.namespace != namespace { return Err(changed()); }
                Ok(())
            }).await.map_err(fail)??;
        }
        let read = move |blocking: &crate::rt::Blocking, db: &mut super::db::Db| db.spoken_response(blocking, &key).map_err(fail)?.ok_or(LibError::Offline);
        let body = if remote { self.shelf_db_run(generation.clone(), read).await? } else { self.db_run(read).await? };
        let info = crate::rt::spawn_blocking(move |_| {
            let record: Cached = serde_json::from_str(&body).map_err(|_| fail("Cached book information is unreadable"))?;
            if let BookInfoTarget::Shelf { library: Some(library), .. } = target {
                if record.library.as_ref() != Some(&library) { return Err(fail("Book information belongs to another library")); }
            }
            Ok::<_, LibError>(record.info)
        }).await.map_err(fail)??;
        if remote { self.shelf_result(generation, info).await } else { Ok(info) }
    }

    async fn fetch_book_info_async(self: &Arc<Self>, target: BookInfoTarget, generation: Arc<()>) -> Result<BookInfo, LibError> {
        match target {
            BookInfoTarget::Local { relative_path } => {
                let lib = self.clone();
                let (info, key, body) = crate::rt::spawn_blocking(move |blocking| {
                    let path = lib.local_book_path(blocking, &relative_path).map_err(fail)?;
                    let metadata = super::spoken::read_m4b_info(blocking, &path)
                        .map_err(|_| fail("Unable to read embedded book information"))?;
                    let info = BookInfo::from_m4b(&metadata, path.file_stem().unwrap_or_default().to_string_lossy().as_ref());
                    let (key, body) = local_cache_record(&relative_path, info.clone())?;
                    Ok::<_, LibError>((info, key, body))
                }).await.map_err(fail)??;
                self.db_run(move |blocking, db| db.put_spoken_response(blocking, &key, &body).map_err(fail)).await?;
                Ok(info)
            }
            BookInfoTarget::Shelf { namespace, library, item, episode } => {
                if namespace.is_empty() || item.is_empty() || item.len() > 4096
                    || episode.as_ref().is_some_and(|id| id.len() > 4096) {
                    return Err(fail("Invalid book information identity"));
                }
                let client = self.shelf_client_async().await?;
                if client.cached_namespace() != Some(namespace.as_str()) { return Err(changed()); }
                let lib = self.clone();let expected = generation.clone();
                if !crate::rt::spawn_blocking(move |_| lib.shelf_generation_current(&expected)).await.map_err(fail)? { return Err(changed()); }
                // Streamed: of a podcast's episodes only this one is kept.
                let response = client.item_async(&item, episode.as_deref()).await.map_err(fail)?;
                let (info, key, body) = crate::rt::spawn_blocking(move |_| {
                    if response["id"].as_str() != Some(&item)
                        || library.as_ref().is_some_and(|id| response["libraryId"].as_str() != Some(id)) {
                        return Err(fail("Book information belongs to another item or library"));
                    }
                    if response["libraryId"].as_str().is_none_or(str::is_empty)
                        || !matches!(response["mediaType"].as_str(), Some("book" | "podcast"))
                        || (episode.is_some() && response["mediaType"].as_str() != Some("podcast")) {
                        return Err(fail("Unsupported book information response"));
                    }
                    let info = from_shelf(&response, episode.as_deref())?;
                    let record = Cached { info: info.clone(), library: response["libraryId"].as_str().map(str::to_owned) };
                    let target = BookInfoTarget::Shelf { namespace, library, item, episode };
                    let body = serde_json::to_string(&record).map_err(fail)?;
                    Ok::<_, LibError>((info, cache_key(&target), body))
                }).await.map_err(fail)??;
                self.shelf_db_run(generation.clone(), move |blocking, db| {
                    db.put_spoken_response(blocking, &key, &body)
                        .map_err(|_| fail("Unable to cache book information on the card"))
                }).await?;
                self.shelf_result(generation, info).await
            }
        }
    }

}

impl BookInfo {
    pub(super) fn from_m4b(metadata: &super::spoken::M4bInfo, fallback: &str) -> Self {
        Self {
            title: readable(metadata.title.as_deref().unwrap_or(fallback), 4096),
            authors: metadata
                .artist
                .as_deref()
                .map(|v| vec![readable(v, 4096)])
                .unwrap_or_default(),
            description: readable(metadata.description.as_deref().unwrap_or_default(), 32768),
            narrators: metadata
                .narrator
                .as_deref()
                .map(|v| vec![readable(v, 4096)])
                .unwrap_or_default(),
            series: metadata
                .series
                .as_deref()
                .map(|v| vec![readable(v, 4096)])
                .unwrap_or_default(),
            duration_ms: metadata.duration_ms,
            chapters: metadata
                .chapters
                .iter()
                .map(|chapter| Chapter {
                    start_ms: chapter.start_ms,
                    title: readable(&chapter.title, 4096),
                })
                .collect(),
            ..Self::default()
        }
    }
}
fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .take(256)
        .filter_map(|v| v.as_str().or_else(|| v["name"].as_str()))
        .map(|s| readable(s, 4096))
        .filter(|s| !s.is_empty())
        .collect()
}
fn field(value: &Value, name: &str) -> String {
    readable(value[name].as_str().unwrap_or_default(), 4096)
}
fn millis(value: &Value) -> u64 {
    value
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| (v * 1000.0).round().min(u64::MAX as f64) as u64)
        .unwrap_or(0)
}
fn from_shelf(response: &Value, episode: Option<&str>) -> Result<BookInfo, LibError> {
    let media = &response["media"];
    let metadata = &media["metadata"];
    let mut info = BookInfo {
        title: field(metadata, "title"),
        subtitle: field(metadata, "subtitle"),
        description: readable(
            metadata["descriptionPlain"]
                .as_str()
                .filter(|v| !v.is_empty())
                .or_else(|| metadata["description"].as_str())
                .unwrap_or_default(),
            32768,
        ),
        authors: strings(&metadata["authors"]),
        narrators: strings(&metadata["narrators"]),
        duration_ms: millis(&media["duration"]),
        publisher: field(metadata, "publisher"),
        published_year: field(metadata, "publishedYear"),
        language: field(metadata, "language"),
        genres: strings(&metadata["genres"]),
        series: metadata["series"]
            .as_array()
            .into_iter()
            .flatten()
            .take(256)
            .filter_map(|series| {
                let name = field(series, "name");
                if name.is_empty() {
                    return None;
                }
                let sequence = series["sequence"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| series["sequence"].as_f64().map(|v| v.to_string()))
                    .unwrap_or_default();
                Some(if sequence.is_empty() {
                    name
                } else {
                    format!("{name} #{}", readable(&sequence, 128))
                })
            })
            .collect(),
        ..BookInfo::default()
    };
    if info.authors.is_empty() {
        let name = field(metadata, "authorName");
        if !name.is_empty() {
            info.authors.push(name);
        }
    }
    if info.narrators.is_empty() {
        let name = field(metadata, "narratorName");
        if !name.is_empty() {
            info.narrators.push(name);
        }
    }
    if info.series.is_empty() {
        let name = field(metadata, "seriesName");
        if !name.is_empty() {
            info.series.push(name);
        }
    }
    let chapter_source = if let Some(episode) = episode {
        let entry = media["episodes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["id"].as_str() == Some(episode))
            .ok_or_else(|| fail("Episode information unavailable"))?;
        info.title = field(entry, "title");
        info.description = readable(entry["description"].as_str().unwrap_or_default(), 32768);
        info.duration_ms = millis(&entry["duration"]).max(millis(&entry["audioFile"]["duration"]));
        &entry["chapters"]
    } else {
        &media["chapters"]
    };
    info.chapters = chapter_source
        .as_array()
        .into_iter()
        .flatten()
        .take(16384)
        .map(|chapter| Chapter {
            start_ms: millis(&chapter["start"]),
            title: field(chapter, "title"),
        })
        .filter(|chapter| info.duration_ms == 0 || chapter.start_ms < info.duration_ms)
        .collect();
    info.chapters.sort_by_key(|chapter| chapter.start_ms);
    info.chapters.dedup_by_key(|chapter| chapter.start_ms);
    if info.title.is_empty() {
        return Err(fail("Book information has no title"));
    }
    Ok(info)
}

/// Native text only: no markup, active content, links, control escapes or
/// unbounded input enters the reading page. Paragraph boundaries survive.
pub fn readable(input: &str, maximum: usize) -> String {
    let source: String = input.chars().take(65536).collect();
    let mut plain = String::new();
    let mut rest = source.as_str();
    let mut hidden: Option<String> = None;
    while !rest.is_empty() && plain.len() < maximum.saturating_mul(4) {
        if rest.starts_with('<')
            && rest.as_bytes().get(1).is_some_and(|byte| {
                byte.is_ascii_alphabetic() || matches!(byte, b'/' | b'!' | b'?')
            })
        {
            if let Some(end) = rest.find('>') {
                let tag = rest[1..end].trim();
                let closing = tag.starts_with('/');
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "script" | "style" | "iframe" | "object" | "svg"
                ) {
                    if closing && hidden.as_deref() == Some(name.as_str()) {
                        hidden = None;
                    } else if !closing {
                        hidden = Some(name);
                    }
                } else if hidden.is_none()
                    && matches!(
                        name.as_str(),
                        "p" | "br" | "div" | "li" | "h1" | "h2" | "h3" | "blockquote" | "tr"
                    )
                {
                    plain.push('\n');
                }
                rest = &rest[end + 1..];
                continue;
            } else {
                // No closing delimiter exists: consume the remainder once.
                // Re-searching it at every '<' would make malformed text
                // quadratic on the Q2's CPU.
                if hidden.is_none() {
                    plain.extend(rest.chars().take(maximum));
                }
                break;
            }
        }
        let ch = rest.chars().next().unwrap();
        rest = &rest[ch.len_utf8()..];
        if hidden.is_some() {
            continue;
        }
        if ch == '&' {
            if let Some(end) = rest
                .as_bytes()
                .iter()
                .take(17)
                .position(|byte| *byte == b';')
            {
                if let Some(decoded) = entity(&rest[..end]) {
                    plain.push(decoded);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        plain.push(ch);
    }
    let mut result = String::new();
    let mut pending_space = false;
    let mut newlines = 0;
    for ch in plain.chars() {
        if ch == '\n' || ch == '\r' {
            pending_space = false;
            if !result.is_empty() && newlines < 2 {
                result.push('\n');
                newlines += 1;
            }
        } else if ch.is_whitespace() {
            pending_space = true;
        } else if !ch.is_control()
            && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            if pending_space && newlines == 0 && !result.is_empty() {
                result.push(' ');
            }
            result.push(ch);
            pending_space = false;
            newlines = 0;
        }
    }
    result.trim().chars().take(maximum).collect()
}
fn entity(value: &str) -> Option<char> {
    match value {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        "ndash" => Some('–'),
        "mdash" => Some('—'),
        "hellip" => Some('…'),
        "lsquo" => Some('‘'),
        "rsquo" => Some('’'),
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        _ if value.starts_with("#x") || value.starts_with("#X") => {
            u32::from_str_radix(&value[2..], 16)
                .ok()
                .and_then(char::from_u32)
        }
        _ if value.starts_with('#') => value[1..].parse().ok().and_then(char::from_u32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::Duration;
    struct State {
        item: Value,
        fail: bool,
        requests: usize,
        block: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
    }
    struct Fixture {
        lib: Arc<Library>,
        state: Arc<Mutex<State>>,
        config: Config,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
        root: std::path::PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }
    fn fixture() -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(State {
            fail: false,
            requests: 0,
            block: None,
            item: json!({
            "id":"book", "libraryId":"books", "mediaType":"book", "media":{
                "duration":3661.25, "metadata":{"title":"Generated Book", "subtitle":"A subtitle",
                    "authors":[{"name":"Author One"},{"name":"Author Two"}], "narrators":["Narrator"],
                    "series":[{"name":"Series","sequence":"2.5"}], "description":"<p>First &amp; second.</p><script>private-js</script><p>音楽 &#x1f3b5;</p>",
                    "publisher":"Publisher", "publishedYear":"2026", "language":"en", "genres":["Fiction"]},
                "chapters":[{"start":0,"title":"Beginning"},{"start":60,"title":"Next"}]}}),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = state.clone();
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            while !stopping.load(Ordering::Relaxed) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                let shared = shared.clone();
                handlers.push(std::thread::spawn(move || {
                    socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") { let mut byte = [0]; socket.read_exact(&mut byte).unwrap(); request.push(byte[0]); assert!(request.len() < 16384); }
                    let request = String::from_utf8(request).unwrap();
                    let other = request.contains("Bearer generated-other-token");
                    assert!(other || request.contains("Bearer generated-token"));
                    let path = request.lines().next().unwrap().split_whitespace().nth(1).unwrap();
                    let mut state = shared.lock().unwrap(); state.requests += 1;
                    let (status, value) = if path == "/api/me" { ("200 OK", json!({"id":if other {"other"} else {"generated"}})) }
                    else {
                        assert_eq!(path, "/api/items/book?expanded=1");
                        if let Some((entered, release)) = state.block.take() {
                            drop(state); entered.send(()).unwrap(); release.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); state = shared.lock().unwrap();
                        }
                        (if state.fail { "503 Unavailable" } else { "200 OK" }, state.item.clone())
                    };
                    let body = value.to_string();
                    if let Err(error) = write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()) {
                        assert!(matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset));
                    }
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        let root = super::super::generated_test_directory("book-info");
        let mut config = Config::default();
        config.audiobookshelf.url = format!("http://{address}");
        config.audiobookshelf.token = "generated-token".into();
        let lib = Library::open(Paths::new(root.clone()), &config, Arc::new(|_| {}));
        lib.shelf_client().unwrap();
        Fixture {
            lib,
            state,
            config,
            stop,
            worker: Some(worker),
            root,
        }
    }

    #[test]
    fn fetched_info_is_card_cached_offline_and_failure_does_not_replace_good_data() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let target = lib
            .shelf_info_target(Some("books".into()), "book".into(), None)
            .unwrap();
        let info = lib.book_info(&target, Policy::NetworkOnly).unwrap();
        assert_eq!(info.title, "Generated Book");
        assert_eq!(info.authors, ["Author One", "Author Two"]);
        assert_eq!(info.narrators, ["Narrator"]);
        assert_eq!(info.series, ["Series #2.5"]);
        assert_eq!(info.duration_ms, 3661250);
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.description, "First & second.\n\n音楽 🎵");
        let count = fixture.state.lock().unwrap().requests;
        lib.set_offline_mode(true);
        assert_eq!(lib.book_info(&target, Policy::PreferCache).unwrap(), info);
        assert_eq!(fixture.state.lock().unwrap().requests, count);
        let restarted = Library::open(
            Paths::new(fixture.root.clone()),
            &fixture.config,
            Arc::new(|_| {}),
        );
        restarted.set_offline_mode(true);
        assert_eq!(
            restarted.book_info(&target, Policy::CacheOnly).unwrap(),
            info
        );
        assert_eq!(fixture.state.lock().unwrap().requests, count);
        lib.set_offline_mode(false);
        fixture.state.lock().unwrap().fail = true;
        assert_eq!(lib.book_info(&target, Policy::NetworkFirst).unwrap(), info);
        assert!(lib.book_info(&target, Policy::NetworkOnly).is_err());
        fixture.state.lock().unwrap().fail = false;
        fixture.state.lock().unwrap().item["libraryId"] = json!("other-library");
        assert!(lib.book_info(&target, Policy::NetworkOnly).is_err());
        assert_eq!(lib.book_info(&target, Policy::CacheOnly).unwrap(), info);
    }

    #[test]
    fn cache_write_failure_is_visible_and_corrupt_cache_can_refetch() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let target = lib
            .shelf_info_target(Some("books".into()), "book".into(), None)
            .unwrap();
        let info = lib.book_info(&target, Policy::NetworkOnly).unwrap();
        let connection = rusqlite::Connection::open(fixture.root.join("library.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_info BEFORE UPDATE ON spoken_responses WHEN NEW.key LIKE 'book-info:%' BEGIN SELECT RAISE(FAIL,'generated full card'); END;").unwrap();
        fixture.state.lock().unwrap().item["media"]["metadata"]["title"] = json!("New Title");
        assert!(
            lib.book_info(&target, Policy::NetworkOnly)
                .unwrap_err()
                .to_string()
                .contains("cache")
        );
        assert_eq!(lib.book_info(&target, Policy::CacheOnly).unwrap(), info);
        connection.execute_batch("DROP TRIGGER fail_info").unwrap();
        lib.db()
            .put_spoken_response(&crate::rt::Blocking::dedicated_thread(), &cache_key(&target), "corrupt generated cache")
            .unwrap();
        assert!(lib.book_info(&target, Policy::CacheOnly).is_err());
        assert_eq!(
            lib.book_info(&target, Policy::PreferCache).unwrap().title,
            "New Title"
        );
    }

    #[test]
    fn stalled_info_keeps_cached_reads_live_and_cancellation_cannot_publish() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let target = lib.shelf_info_target(Some("books".into()), "book".into(), None).unwrap();
        let original = lib.book_info(&target, Policy::NetworkOnly).unwrap();
        let requests = fixture.state.lock().unwrap().requests;
        assert_eq!(lib.book_info(&target, Policy::PreferCache).unwrap(), original);
        assert_eq!(fixture.state.lock().unwrap().requests, requests);
        let (entered, entry) = mpsc::channel();
        let (release, released) = mpsc::channel();
        fixture.state.lock().unwrap().block = Some((entered, released));
        fixture.state.lock().unwrap().item["media"]["metadata"]["title"] = json!("Cancelled Title");
        let task = lib.clone();let requested = target.clone();
        let pending = crate::rt::io().spawn(async move { task.book_info_async(requested, Policy::NetworkOnly).await });
        entry.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let task = lib.clone();let cached = target.clone();
        let (send, receive) = mpsc::channel();
        crate::rt::io().spawn(async move { send.send(task.book_info_async(cached, Policy::CacheOnly).await).unwrap(); });
        assert_eq!(receive.recv_timeout(crate::library::TEST_LIVENESS).expect("executor/cache must progress while HTTP stalls").unwrap(), original);
        pending.abort();
        super::super::shelf_browse::wait(async move { assert!(pending.await.unwrap_err().is_cancelled()); Ok(()) }).unwrap();
        release.send(()).unwrap();
        assert_eq!(lib.book_info(&target, Policy::CacheOnly).unwrap(), original);
    }

    #[test]
    fn delayed_account_and_route_results_cannot_publish_or_fallback_across_generations() {
        for account_change in [false, true] {
            let fixture = fixture();
            let lib = &fixture.lib;
            let target = lib
                .shelf_info_target(Some("books".into()), "book".into(), None)
                .unwrap();
            let initial = lib.book_info(&target, Policy::NetworkOnly).unwrap();
            fixture.state.lock().unwrap().item["media"]["metadata"]["title"] =
                json!("Delayed Title");
            let (entered, entry) = mpsc::channel();
            let (release, released) = mpsc::channel();
            fixture.state.lock().unwrap().block = Some((entered, released));
            let task = lib.clone();
            let old_target = target.clone();
            let worker =
                std::thread::spawn(move || task.book_info(&old_target, Policy::NetworkFirst));
            entry.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let mut config = fixture.config.clone();
            if account_change {
                config.audiobookshelf.token = "generated-other-token".into();
            } else {
                config.tailscale.enabled = true;
            }
            lib.configure(&config);
            if account_change {
                lib.shelf_client().unwrap();
            }
            release.send(()).unwrap();
            assert!(worker.join().unwrap().is_err());
            if account_change {
                assert!(lib.book_info(&target, Policy::CacheOnly).is_err());
                let new_target = lib
                    .shelf_info_target(Some("books".into()), "book".into(), None)
                    .unwrap();
                assert!(lib.book_info(&new_target, Policy::CacheOnly).is_err());
            } else {
                assert_eq!(
                    lib.book_info(&target, Policy::CacheOnly).unwrap(),
                    initial,
                    "route changes preserve the same account's cached metadata"
                );
            }
        }
    }

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [(body.len() as u32 + 8).to_be_bytes().as_slice(), kind, body].concat()
    }
    fn tagged(kind: &[u8; 4], text: &str) -> Vec<u8> {
        atom(
            kind,
            &atom(b"data", &[&[0; 8][..], text.as_bytes()].concat()),
        )
    }
    #[test]
    fn embedded_summary_narrator_series_are_catalog_cached_without_audio_reads() {
        let fixture = fixture();
        let directory = fixture.root.join("audiobooks");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("generated.m4b");
        let mut mvhd = vec![0; 20];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&120000u32.to_be_bytes());
        let mut tags = [
            tagged(b"\xA9nam", "Embedded Book"),
            tagged(b"\xA9ART", "Embedded Author"),
            tagged(b"desc", "Short summary"),
            tagged(b"ldes", "<p>Long &amp; readable summary.</p>"),
            tagged(b"\xA9wrt", "Composer must not become narrator"),
        ]
        .concat();
        for (name, value) in [
            ("narrator", "Embedded Narrator"),
            ("series", "Embedded Series"),
        ] {
            tags.extend(atom(
                b"----",
                &[
                    atom(b"name", &[&[0; 4][..], name.as_bytes()].concat()),
                    atom(b"data", &[&[0; 8][..], value.as_bytes()].concat()),
                ]
                .concat(),
            ));
        }
        let moov = atom(
            b"moov",
            &[
                atom(b"mvhd", &mvhd),
                atom(
                    b"udta",
                    &atom(b"meta", &[&[0; 4][..], &atom(b"ilst", &tags)].concat()),
                ),
            ]
            .concat(),
        );
        std::fs::write(&path, moov).unwrap();
        let (songs, errors) = fixture.lib.local_audiobooks().unwrap();
        assert!(errors.is_empty());
        let target = BookInfoTarget::from_song(&songs[0]).unwrap();
        std::fs::remove_file(path).unwrap();
        let info = fixture.lib.book_info(&target, Policy::CacheOnly).unwrap();
        assert_eq!(info.title, "Embedded Book");
        assert_eq!(info.authors, ["Embedded Author"]);
        assert_eq!(info.description, "Long & readable summary.");
        assert_eq!(info.narrators, ["Embedded Narrator"]);
        assert_eq!(info.series, ["Embedded Series"]);
        assert_eq!(info.duration_ms, 120000);
        assert!(fixture.lib.book_info(&target, Policy::NetworkOnly).is_err());
        assert_eq!(
            fixture.lib.book_info(&target, Policy::CacheOnly).unwrap(),
            info
        );
    }

    #[test]
    fn readable_summary_properties_bound_output_and_strip_active_content_and_controls() {
        let mut rng = fastrand::Rng::with_seed(0xB00C_1F0);
        let chunks = [
            "text ",
            "音楽",
            "<p>",
            "</p>",
            "<br>",
            "&amp;",
            "&#x1f3b5;",
            "\0\u{1b}\u{202e}",
            "<script>hidden-script</script>",
        ];
        for _ in 0..256 {
            let input = (0..rng.usize(0..200))
                .map(|_| chunks[rng.usize(0..chunks.len())])
                .collect::<String>();
            let maximum = rng.usize(1..1000);
            let output = readable(&input, maximum);
            assert!(output.chars().count() <= maximum);
            assert!(!output.contains("hidden-script"));
            assert!(!output.contains('<'));
            assert!(output.chars().all(|c| !c.is_control() || c == '\n'));
            assert!(!output.contains('\u{202e}'));
        }
        for malformed in ["<".repeat(65536), "&".repeat(65536), "<a".repeat(32768)] {
            assert!(readable(&malformed, 128).chars().count() <= 128);
        }
        assert_eq!(
            readable("Less than 3 < 4 and more than 2 > 1", 128),
            "Less than 3 < 4 and more than 2 > 1"
        );
    }
}
