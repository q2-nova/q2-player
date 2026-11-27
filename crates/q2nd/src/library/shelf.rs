use super::{LibError, Library, Policy};
use md5::{Digest, Md5};
use serde_json::{Value, json};
use std::sync::Arc;
use super::download_intent::{Registry,Intent};
use subsonic::audiobookshelf::{Client, segment};
use subsonic::{Chapter, Song, Spoken, SpokenSource};

mod download;
mod legacy;

#[cfg(test)]
#[path = "shelf_admission_tests.rs"]
mod admission_tests;

fn fail(e: impl std::fmt::Display) -> LibError {
    LibError::Server(e.to_string())
}
fn ms(v: &Value) -> u64 {
    v.as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| (n * 1000.0).round().min(u64::MAX as f64) as u64)
        .unwrap_or(0)
}
fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().into()
}
#[derive(Default)]
pub(super) struct ShelfSlot {
    pub(super) client: Option<Arc<Client>>,
    pub(super) namespace: String,
    config_key: String,
    // Read once during construction; then updated with verification/invalidation
    // under this slot's guard. Configuration must not overtake queued DB writes.
    pub(super) namespaces: Option<std::collections::HashMap<String, String>>,
    downloads: Arc<Registry>,
    pub(super) generation: Arc<()>,
    progress_sync: Arc<tokio::sync::Mutex<()>>,
    pub(super) queue: Arc<download::Queue>,
}
impl ShelfSlot {
    pub(super) fn with_namespaces(namespaces: std::collections::HashMap<String, String>) -> Self {
        Self { namespaces: Some(namespaces), ..Self::default() }
    }
}
pub(super) struct PreparedConfiguration { pub(super) client: Option<Arc<Client>>, pub(super) key: String }
pub(super) fn prepare_configuration(c: &crate::config::Config) -> PreparedConfiguration {
    // UX telemetry's Audiobookshelf state follows every request's outcome.
    let client = Client::new(&c.audiobookshelf.url, &c.audiobookshelf.token, c.http_proxy()).ok()
        .map(|client| Arc::new(client.observing_reachability(|answered| crate::ux::set_abs(if answered { 1 } else { 2 }))));
    let key = client.as_ref().map(|client| client.account_cache_key()).unwrap_or_else(|| {
        format!("invalid:{:x}", Md5::digest(format!("{}|{}", c.audiobookshelf.url, c.audiobookshelf.token).as_bytes()))
    });
    PreparedConfiguration { client, key }
}
struct ProbePublication {
    generation: Arc<()>,
    client: Option<Arc<Client>>,
    saved: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), LibError>> + Send>>,
}
pub(crate) struct SpokenQueueRow {
    pub logical:String,pub item:String,pub episode:Option<String>,pub title:String,pub namespace:String,
    pub failed:bool,pub paused:bool,pub progress:Option<Option<f32>>,
}
pub struct SpokenDownload {intent:Intent,item:String,episode:Option<String>,configuration:Arc<()>,namespace:String,ticket:Option<i64>,automatic:bool}
impl Library {
    /// Snapshot for UI workers whose account identity may survive a route change.
    pub fn shelf_generation(&self) -> Arc<()> {
        self.shelf.read().unwrap().generation.clone()
    }
    pub fn shelf_generation_current(&self, generation: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.shelf.read().unwrap().generation, generation)
    }
    /// Binding is posted while the short shelf write guard excludes new
    /// snapshots. Accepted older jobs precede it; delayed old responses after
    /// it fail. DB jobs never acquire a shelf/client guard.
    fn bind_shelf_generation(&self, generation: Arc<()>) {
        self.db_post(move |_blocking, db| db.bind_shelf_generation(generation));
    }
    pub(super) fn shelf_db_run<R: Send + 'static>(&self, generation: Arc<()>, job: impl FnOnce(&crate::rt::Blocking, &mut super::db::Db) -> Result<R, LibError> + Send + 'static)
        -> impl std::future::Future<Output = Result<R, LibError>> + Send + 'static {
        self.db_run(move |blocking, db| {
            if !db.shelf_generation_current(&generation) { return Err(fail(subsonic::audiobookshelf::Error::Changed)); }
            job(blocking, db)
        })
    }
    pub(super) async fn shelf_admission(self: &Arc<Self>, client: Arc<Client>) -> Result<Arc<()>, LibError> {
        let lib = self.clone();
        crate::rt::spawn_blocking(move |_| {
            let slot = lib.shelf.read().unwrap();
            if !slot.client.as_ref().is_some_and(|active| Arc::ptr_eq(active, &client)) { return Err(fail(subsonic::audiobookshelf::Error::Changed)); }
            Ok(slot.generation.clone())
        }).await.map_err(fail)?
    }
    pub(super) async fn shelf_result<R: Send + 'static>(self: &Arc<Self>, generation: Arc<()>, value: R) -> Result<R, LibError> {
        let lib = self.clone();
        crate::rt::spawn_blocking(move |_| {
            if !lib.shelf_generation_current(&generation) { return Err(fail(subsonic::audiobookshelf::Error::Changed)); }
            Ok(value)
        }).await.map_err(fail)?
    }
    pub(super) fn set_shelf(&self, c: &crate::config::Config) {
        let proxy = c.http_proxy();
        if self
                .shelf
                .read()
                .unwrap()
                .client
                .as_ref()
                .is_some_and(|client| {
                    client.configured_as(&c.audiobookshelf.url, &c.audiobookshelf.token, proxy)
                })
        {
            return;
        }
        let prepared = prepare_configuration(c);
        self.install_shelf(prepared.client, prepared.key);
    }
    pub(super) fn shelf_configuration_matches(&self, c: &crate::config::Config) -> bool {
        self.shelf.read().unwrap().client.as_ref().is_some_and(|client|
            client.configured_as(&c.audiobookshelf.url, &c.audiobookshelf.token, c.http_proxy()))
    }
    pub(super) fn invalidate_shelf_configuration(&self) -> Arc<()> {
        let mut slot = self.shelf.write().unwrap();
        let generation = Arc::new(());let queue=slot.queue.clone();
        self.bind_shelf_generation(generation.clone());
        queue.set_generation(generation.clone());
        slot.generation = generation.clone(); slot.client = None; slot.namespace.clear();
        drop(slot); self.downloads.interrupt();
        self.shelf_auto.wake.notify_one();
        self.notify(super::LibEvent::SpokenChanged);
        generation
    }
    pub(super) fn publish_shelf_configuration(&self, prepared: PreparedConfiguration, generation: &Arc<()>) {
        let mut slot = self.shelf.write().unwrap();
        if !Arc::ptr_eq(&slot.generation, generation) { return; }
        slot.namespace = slot.namespaces.as_ref().and_then(|cache| cache.get(&prepared.key)).cloned().unwrap_or_default();
        if prepared.client.is_none() { crate::ux::set_abs(0); }
        slot.config_key = prepared.key; slot.client = prepared.client;
        drop(slot); self.downloads.interrupt(); self.notify(super::LibEvent::SpokenChanged);
    }
    fn install_shelf(&self, client: Option<Arc<Client>>, key: String) {
        if client.is_none() { crate::ux::set_abs(0); }
        let mut slot=self.shelf.write().unwrap();
        let namespace = slot.namespaces.as_ref().and_then(|cache| cache.get(&key)).cloned().unwrap_or_default();
        // Recheck and replace under one guard. Only initial construction reads
        // card state; subsequent account/route changes use verified memory state.
        if slot.config_key == key
            && match (&slot.client, &client) {
                (None, None) => true,
                (Some(old), Some(new)) => Arc::ptr_eq(old, new),
                _ => false,
            }
        {
            return;
        }
        let initialized = !slot.config_key.is_empty();
        let queue=slot.queue.clone();
        let downloads=slot.downloads.clone();
        let generation = Arc::new(());
        self.bind_shelf_generation(generation.clone());
        queue.set_generation(generation.clone());
        *slot = ShelfSlot {
            downloads,
            client,
            namespace,
            config_key: key,
            namespaces: slot.namespaces.take(),
            progress_sync: slot.progress_sync.clone(),
            queue: slot.queue.clone(),
            generation,
        };
        drop(slot);
        self.downloads.interrupt();
        if initialized {
            (self.notify)(super::LibEvent::SpokenChanged);
        }
    }
    pub(super) fn shelf_client(&self) -> Result<Arc<Client>, LibError> {
        self.verified_shelf_client(false)
    }
    pub fn check_shelf_connection(&self) -> Result<(), LibError> {
        self.verified_shelf_client(true).map(|_|())
    }
    /// Requests and DB ownership are awaited. Only short shelf snapshots and
    /// admission run in the finite pool; no worker waits for SQLite.
    pub async fn check_shelf_connection_async(self: &Arc<Self>) -> Result<(), LibError> {
        self.verified_shelf_client_async(true).await.map(|_| ())
    }
    pub(super) async fn shelf_client_async(self: &Arc<Self>) -> Result<Arc<Client>, LibError> {
        self.verified_shelf_client_async(false).await
    }
    async fn verified_shelf_client_async(self: &Arc<Self>, fresh: bool) -> Result<Arc<Client>, LibError> {
        let owner = self.clone();
        let generation = crate::rt::spawn_blocking(move |_| owner.shelf_generation()).await.map_err(fail)?;
        self.shelf_configuration_ready(&generation).await?;
        let lib = self.clone();
        let client = crate::rt::spawn_blocking(move |blocking| lib.shelf_probe_client(blocking))
            .await.map_err(fail)??;
        let result = if fresh { client.probe_async().await } else { client.namespace_async().await };
        self.finish_shelf_probe_async(client, result).await
    }
    fn shelf_probe_client(&self, _: &crate::rt::Blocking) -> Result<Arc<Client>, LibError> {
        self.configured_shelf_client()
    }
    fn configured_shelf_client(&self) -> Result<Arc<Client>, LibError> {
        if self.offline_mode() {
            return Err(LibError::Offline);
        }
        self.shelf.read().unwrap().client.clone()
            .ok_or_else(|| fail("Set up Audiobookshelf in Settings → Connections"))
    }
    fn verified_shelf_client(&self, fresh: bool) -> Result<Arc<Client>, LibError> {
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        super::shelf_browse::wait(async move { lib.verified_shelf_client_async(fresh).await })
    }
    async fn finish_shelf_probe_async(self: &Arc<Self>, client: Arc<Client>, result: Result<String, subsonic::audiobookshelf::Error>) -> Result<Arc<Client>, LibError> {
        let lib = self.clone();
        let publication = crate::rt::spawn_blocking(move |_| lib.prepare_shelf_probe(client, result)).await.map_err(fail)??;
        let generation = publication.generation.clone();
        let client = publication.client;
        publication.saved.await?;
        let client = client.ok_or_else(|| fail(subsonic::audiobookshelf::Error::Changed))?;
        let client = self.shelf_result(generation.clone(), client).await?;
        let records = client.take_media_progress();
        if !records.is_empty() {
            let lib = self.clone();
            let namespace = client.cached_namespace().unwrap_or_default().to_owned();
            crate::rt::io().spawn(async move {
                let mut changed = false;
                let mut records = records.into_iter();
                loop {
                    let batch: Vec<_> = records.by_ref().take(64).collect();
                    if batch.is_empty() { break; }
                    let namespace = namespace.clone();
                    let result = lib.shelf_db_run(generation.clone(), move |blocking, db| {
                        db.observe_spoken_snapshot(blocking, &namespace, &batch).map_err(fail)
                    }).await;
                    match result {
                        Ok(batch_changed) => changed |= batch_changed,
                        Err(_) => break,
                    }
                }
                if changed { lib.notify(super::LibEvent::SpokenProgress); }
            });
        }
        Ok(client)
    }
    fn prepare_shelf_probe(&self, client: Arc<Client>, result: Result<String, subsonic::audiobookshelf::Error>) -> Result<ProbePublication, LibError> {
        let key = client.account_cache_key();
        let namespace = match result {
            Ok(namespace) => namespace,
            Err(subsonic::audiobookshelf::Error::Changed) => {
                let mut slot = self.shelf.write().unwrap();
                if !slot.client.as_ref().is_some_and(|active| Arc::ptr_eq(active, &client)) {
                    return Err(fail(subsonic::audiobookshelf::Error::Changed));
                }
                let generation = Arc::new(());
                self.bind_shelf_generation(generation.clone());
                slot.queue.set_generation(generation.clone());
                slot.client = None; slot.namespace.clear(); slot.generation = generation.clone();
                slot.namespaces.as_mut().expect("initialized shelf").insert(key.clone(), String::new());
                let publication = self.admit_shelf_probe(generation, key, String::new(), None);
                drop(slot);
                self.downloads.interrupt();
                (self.notify)(super::LibEvent::SpokenChanged);
                return Ok(publication);
            }
            Err(error) => return Err(fail(error)),
        };
        let mut slot = self.shelf.write().unwrap();
        if !slot.client.as_ref().is_some_and(|active| Arc::ptr_eq(active, &client)) {
            return Err(fail(subsonic::audiobookshelf::Error::Changed));
        }
        slot.namespace = namespace.clone();
        slot.namespaces.as_mut().expect("initialized shelf").insert(key.clone(), namespace.clone());
        Ok(self.admit_shelf_probe(slot.generation.clone(), key, namespace, Some(client)))
    }
    // Post before releasing the shelf publication guard. A cancelled observer
    // cannot leave a verified/invalidated namespace without its cache update.
    fn admit_shelf_probe(&self, generation: Arc<()>, key: String, namespace: String, client: Option<Arc<Client>>) -> ProbePublication {
        let saved = Box::pin(self.shelf_db_run(generation.clone(), move |blocking, db| db.put_spoken_response(blocking, &key, &namespace).map_err(fail)));
        ProbePublication { generation, client, saved }
    }
    pub(super) fn shelf_current(&self, client: &Arc<Client>) -> bool {
        self.shelf
            .read()
            .unwrap()
            .client
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, client))
    }
    #[cfg(test)]
    pub(super) fn publish_shelf<T>(
        &self,
        client: &Arc<Client>,
        operation: impl FnOnce() -> Result<T, LibError>,
    ) -> Result<T, LibError> {
        let slot = self.shelf.read().unwrap();
        if !slot
            .client
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, client))
        {
            return Err(fail(subsonic::audiobookshelf::Error::Changed));
        }
        operation()
    }
    pub fn shelf_queue_current(&self, songs: &[Song]) -> bool {
        let slot = self.shelf.read().unwrap();
        let Some(client) = slot.client.as_ref() else {
            return false;
        };
        let Some(namespace) = client.cached_namespace() else {
            return false;
        };
        !songs.is_empty() && songs.iter().all(|song|song.spoken.as_ref().is_some_and(|book|matches!(&book.source,SpokenSource::Audiobookshelf {namespace:account,..} if account==namespace)))
    }
    /// The current account's namespace; empty while unconfigured or changing.
    pub fn shelf_namespace(&self) -> String { self.shelf.read().unwrap().namespace.clone() }

    pub fn shelf_open(&self, item: &str, episode: Option<&str>) -> Result<Vec<Song>, String> {
        let lib = self.me.upgrade().ok_or_else(|| LibError::Offline.to_string())?;
        let item = item.to_owned();
        let episode = episode.map(str::to_owned);
        super::shelf_browse::wait(async move {
            lib.shelf_open_async(item, episode).await.map_err(fail)
        }).map_err(|e| e.to_string())
    }

    pub async fn shelf_open_async(self: &Arc<Self>, item: String, episode: Option<String>) -> Result<Vec<Song>, String> {
        if self.paths.ephemeral_media {
            return Err("Insert a card to save audiobook listening locations".into());
        }
        // UX-005: a downloaded item plays from the card without the server.
        if let Some(songs) = self.shelf_open_local_async(&item, episode.as_deref()).await {
            return Ok(songs);
        }
        let client = self.shelf_client_async().await.map_err(|e| e.to_string())?;
        let namespace = client.namespace_async().await.map_err(|e| e.to_string())?;
        let generation = self.shelf_admission(client.clone()).await.map_err(|e| e.to_string())?;
        let device_id = self.shelf_db_run(generation.clone(), |blocking, db| Self::spoken_device_id(blocking, db).map_err(fail))
            .await.map_err(|e| e.to_string())?;
        let active = client.clone();
        let book = item.clone();
        let ep = episode.clone();
        // Keep play + immediate close together even if the caller is cancelled.
        // Dropping this join handle detaches the bounded request transaction:
        // it owns no library lock and cannot publish a queue or start playback.
        // Direct content URLs remain usable after close; timestamped local
        // progress sessions retain disconnected saves. An ID lost to a network
        // failure cannot be closed here (the same ambiguity as a failed play).
        let session = crate::rt::io().spawn(async move {
            let session = active.play_async(&book, ep.as_deref(), &device_id)
                .await.map_err(|e| e.to_string())?;
            let id = session["id"].as_str().ok_or("Missing Audiobookshelf session ID")?;
            active.close_session_async(id).await.map_err(|e| e.to_string())?;
            Ok::<_, String>(session)
        }).await.map_err(|e| e.to_string())??;
        let book = item.clone();
        let ep = episode.clone();
        let (logical, mut songs) = crate::rt::spawn_blocking(move |_| {
            Self::shelf_session_songs(session, namespace, book, ep)
        }).await.map_err(|e| e.to_string())??;
        for song in &mut songs {
            let Some(Spoken { source: SpokenSource::Audiobookshelf { content_url, .. }, .. }) = song.spoken.as_deref() else {
                unreachable!("projected ABS session has an ABS source")
            };
            song.size = Some(client.size_async(content_url).await.map_err(|e| e.to_string())?);
        }
        let key = logical.clone();
        let songs = self.shelf_db_run(generation.clone(), move |blocking, db| {
            db.replace_spoken_queue(blocking, &key, &songs).map_err(fail)?;
            Ok(songs)
        }).await.map_err(|e| e.to_string())?;
        self.pull_shelf_position_async(client, logical, &item, episode.as_deref(), true)
            .await.map_err(|e| e.to_string())?;
        self.shelf_result(generation, songs).await.map_err(|e| e.to_string())
    }

    /// The stored queue of an item whose every file is downloaded, or None.
    /// Before returning it, a stale local position (none, or older than
    /// [`REMOTE_POSITION_AFTER_MS`]) is refreshed from the server within
    /// [`REMOTE_POSITION_WAIT`] (PLAYBACK-007); a later answer never seeks.
    async fn shelf_open_local_async(self: &Arc<Self>, item: &str, episode: Option<&str>) -> Option<Vec<Song>> {
        let owner = self.clone();
        let namespace = crate::rt::spawn_blocking(move |_| owner.shelf.read().unwrap().namespace.clone()).await.ok()?;
        if namespace.is_empty() { return None; }
        let logical = format!("abs:{namespace}:{item}:{}", episode.unwrap_or_default());
        let paths = self.paths.clone();
        let key = logical.clone();
        let (songs, position) = self.db_run(move |blocking, db| {
            let songs = super::spoken::stored_queue(blocking, db, &key).ok()?;
            if songs.is_empty() { return None; }
            for song in &songs {
                let path = db.spoken_download(blocking, &song.id).ok()??;
                if !super::spoken::downloaded_file(blocking, &paths, &path) { return None; }
            }
            Some((songs, db.spoken_position(blocking, &key).ok().flatten()))
        }).await?;
        let stale = position.is_none_or(|p| super::spoken::epoch_ms().saturating_sub(p.updated_ms) > REMOTE_POSITION_AFTER_MS);
        if stale && !self.offline_mode() {
            let lib = self.clone();
            let (item, episode) = (item.to_owned(), episode.map(str::to_owned));
            let pull = async move {
                let client = lib.shelf_client_async().await.ok()?;
                lib.pull_shelf_position_async(client, logical, &item, episode.as_deref(), true).await.ok()
            };
            if tokio::time::timeout(REMOTE_POSITION_WAIT, pull).await.ok().flatten().is_none() {
                crate::ux::recovered(crate::ux::Code::AbsUnreachable, crate::ux::Journey::PlaySpoken);
            }
        }
        Some(songs)
    }

    /// Hover prefetch (UX-007, PLAYBACK-007): the item's text information
    /// into the card database, and its remote position when the local one is
    /// stale, so opening and playing need no network wait. Each item at most
    /// once per [`PREFETCH_AGAIN`].
    pub(crate) async fn prefetch_spoken_async(self: Arc<Self>, target: super::book_info::BookInfoTarget) {
        let super::book_info::BookInfoTarget::Shelf { namespace, item, episode, .. } = &target else { return };
        if self.offline_mode() || !recently_prefetched(&format!("{namespace}:{item}:{}", episode.as_deref().unwrap_or_default())).is_none() { return; }
        let logical = format!("abs:{namespace}:{item}:{}", episode.as_deref().unwrap_or_default());
        let (item, episode) = (item.clone(), episode.clone());
        let _ = self.book_info_async(target, Policy::PreferCache).await;
        let key = logical.clone();
        let position = self.db_run(move |blocking, db| db.spoken_position(blocking, &key).ok().flatten()).await;
        if position.is_none_or(|p| super::spoken::epoch_ms().saturating_sub(p.updated_ms) > REMOTE_POSITION_AFTER_MS) {
            if let Ok(client) = self.shelf_client_async().await {
                let _ = self.pull_shelf_position_async(client, logical, &item, episode.as_deref(), false).await;
            }
        }
    }

    fn shelf_session_songs(session: Value, namespace: String, item: String, episode: Option<String>) -> Result<(String, Vec<Song>), String> {
        let duration_ms = ms(&session["duration"]);
        if duration_ms == 0 {
            return Err("Audiobookshelf returned no playable duration".into());
        }
        let logical = format!("abs:{namespace}:{item}:{}", episode.as_deref().unwrap_or_default());
        let mut chapters = session["chapters"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Chapter {
                start_ms: ms(&c["start"]),
                title: text(&c["title"]),
            })
            .filter(|c| c.start_ms < duration_ms)
            .collect::<Vec<_>>();
        chapters.sort_by_key(|c| c.start_ms);
        chapters.dedup_by_key(|c| c.start_ms);
        let mut songs = Vec::new();
        for track in session["audioTracks"]
            .as_array()
            .ok_or("Audiobookshelf returned no audio files")?
            .iter()
        {
            let raw = track["contentUrl"]
                .as_str()
                .ok_or("Audiobookshelf returned no file URL")?;
            if raw.contains("://") || raw.starts_with("//") {
                return Err("Audiobookshelf returned an external file URL".into());
            }
            let content_url = raw.trim_start_matches('/').to_string();
            let suffix = track["metadata"]["ext"]
                .as_str()
                .map(|s| s.trim_start_matches('.').to_ascii_lowercase())
                .or_else(|| {
                    track["title"]
                        .as_str()
                        .and_then(|s| s.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()))
                })
                .unwrap_or_else(|| "m4b".into());
            if ![
                "m4b", "m4a", "mp4", "mp3", "flac", "aac", "ogg", "oga", "wav", "alac",
            ]
            .contains(&suffix.as_str())
            {
                return Err(format!("Unsupported audiobook format: {suffix}"));
            }
            let spoken = Spoken {
                id: logical.clone(),
                offset_ms: ms(&track["startOffset"]),
                duration_ms,
                chapters: chapters.clone(),
                source: SpokenSource::Audiobookshelf {
                    namespace: namespace.clone(),
                    item: item.clone(),
                    episode: episode.clone(),
                    content_url: content_url.clone(),
                },
            };
            let song = Song {
                id: format!("{logical}:{:x}", Md5::digest(content_url.as_bytes())),
                title: text(&session["displayTitle"]),
                artist: text(&session["displayAuthor"]),
                album: if episode.is_some() {
                    "Podcast".into()
                } else {
                    "Audiobook".into()
                },
                duration: (ms(&track["duration"]) / 1000).min(u32::MAX as u64) as u32,
                suffix: Some(suffix),
                size: None,
                cover_art: Some(format!("shelf-book-cover:{namespace}:{item}")),
                spoken: Some(Box::new(spoken)),
                ..Default::default()
            };
            songs.push(song);
        }
        if songs.is_empty() {
            return Err("Audiobookshelf returned no audio files".into());
        }
        Ok((logical, songs))
    }

    async fn pull_shelf_position_async(
        self: &Arc<Self>, client: Arc<Client>, logical: String, item: &str, episode: Option<&str>, resolve_dirty: bool,
    ) -> Result<(), subsonic::audiobookshelf::Error> {
        let path = match episode {
            Some(ep) => format!("api/me/progress/{}/{}", segment(item), segment(ep)),
            None => format!("api/me/progress/{}", segment(item)),
        };
        use subsonic::audiobookshelf::Error;
        let generation = self.shelf_admission(client.clone()).await.map_err(|_| Error::Changed)?;
        let progress = match client.get_async(&path).await {
            Ok(progress) => progress,
            Err(Error::Missing) => return Ok(()),
            Err(error) => return Err(error),
        };
        let position = if progress["isFinished"].as_bool() == Some(true) { 0 } else { ms(&progress["currentTime"]) };
        let percent = if progress["isFinished"].as_bool() == Some(true) { Some(100) }
            else { progress["progress"].as_f64().map(|ratio| (ratio.clamp(0.0, 1.0) * 100.0) as u8) };
        let updated = progress["lastUpdate"].as_u64().unwrap_or(0);
        // Keep storage failure distinct from an obsolete account response.
        let stored = self.shelf_db_run(generation.clone(), move |blocking, db| {
            Ok(db.observe_spoken_remote(blocking, &logical, position, updated, resolve_dirty, percent))
        }).await.map_err(|_| Error::Changed)?;
        self.shelf_result(generation, ()).await.map_err(|_| Error::Changed)?;
        if stored.map_err(|_| Error::Storage)? { self.notify(super::LibEvent::SpokenProgress); }
        Ok(())
    }
    /// Refresh before selecting a cached/downloaded book or resuming a paused
    /// instance. Local files stay local; remote identity and writes use the same
    /// generation admission as streaming. Explicit offline mode skips HTTP.
    pub async fn spoken_queue_for_playback_async(self: &Arc<Self>, logical: String) -> Result<Vec<Song>, String> {
        let songs = self.spoken_queue_async(logical.clone()).await?;
        let Some(Spoken { source: SpokenSource::Audiobookshelf { namespace, item, episode, .. }, .. }) = songs.first().and_then(|s| s.spoken.as_deref()) else { return Ok(songs); };
        if !self.shelf_namespace_current(namespace) { return Err("Audiobookshelf changed; select the item again".into()); }
        if self.offline_mode() { return Ok(songs); }
        let client = self.shelf_client_async().await.map_err(|e| e.to_string())?;
        let refreshed=self.pull_shelf_position_async(client.clone(), logical, item, episode.as_deref(), true).await;
        if !self.shelf_current(&client) {return Err(subsonic::audiobookshelf::Error::Changed.to_string());}
        match refreshed {
            // A temporarily unreachable or failing service cannot prevent local
            // playback. Authentication, account changes and storage faults remain errors.
            Ok(()) | Err(subsonic::audiobookshelf::Error::Network | subsonic::audiobookshelf::Error::Server) => {},
            Err(error) => return Err(error.to_string()),
        }
        Ok(songs)
    }

    fn spoken_device_id(blocking: &crate::rt::Blocking, db: &super::db::Db) -> Result<String, String> {
        if let Some(id) = db
            .spoken_response(blocking, "device-id")
            .map_err(|_| "Unable to read audiobook device identity")?
        {
            return Ok(id);
        }
        let mut random = [0u8; 16];
        fastrand::fill(&mut random);
        random[6] = (random[6] & 15) | 64;
        random[8] = (random[8] & 63) | 128;
        let hex = random
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let id = format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        db.put_spoken_response(blocking, "device-id", &id)
            .map_err(|_| "Unable to save audiobook device identity")?;
        Ok(id)
    }
    /// Register before queuing the worker so Remove/requeue does not depend on job scheduling.
    pub fn begin_shelf_download(&self,item:&str,episode:Option<&str>)->Result<SpokenDownload,String> {
        let slot=self.shelf.read().unwrap();
        if slot.namespace.is_empty() || slot.client.is_none() {return Err("Audiobookshelf changed; select the item again".into());}
        let logical=format!("abs:{}:{item}:{}",slot.namespace,episode.unwrap_or_default());
        let intent=slot.downloads.begin(logical.clone())?;slot.queue.discard_saved(&logical);
        Ok(SpokenDownload {intent,item:item.into(),episode:episode.map(str::to_owned),configuration:slot.generation.clone(),namespace:slot.namespace.clone(),ticket:None,automatic:false})
    }
    pub fn enqueue_shelf_download_async(self: &Arc<Self>, request: SpokenDownload, title: String) -> impl std::future::Future<Output=Result<(),String>> + Send + 'static {
        let lib=self.clone();
        let task=crate::rt::io().spawn(async move {
            let gate=lib.offline_art_gate.acquire().await;
            let generation=request.configuration.clone();
            let result=lib.shelf_db_run(generation, move |blocking,db| {
                request.intent.check().map_err(fail)?;
                db.enqueue_spoken(blocking,&request.namespace,&request.intent.logical,&request.item,request.episode.as_deref(),&title).map_err(|_|fail("Unable to queue audiobook (queue may be full)"))
            }).await.map_err(|e|e.to_string());
            drop(gate);
            if result.is_ok() {
                let owner=lib.clone();
                let _=crate::rt::spawn_blocking(move |_| owner.shelf.read().unwrap().queue.wake.notify_one()).await;
                lib.notify(super::LibEvent::Downloads);
            }
            result
        });
        async move {task.await.map_err(|_|"Audiobook queue worker stopped".to_string())?}
    }
    pub(crate) async fn spoken_download_status(self:&Arc<Self>) -> download::Status {
        let owner=self.clone();
        let Ok((namespace,queue))=crate::rt::spawn_blocking(move |_| {
            let slot=owner.shelf.read().unwrap();(slot.namespace.clone(),slot.queue.clone())
        }).await else {return download::Status::default()};
        let counts=self.db_run(move |blocking,db|db.spoken_queue_counts(blocking,&namespace)).await.unwrap_or_default();
        queue.status(counts)
    }
    pub(crate) async fn spoken_download_identity(self:&Arc<Self>) -> Result<(String,Arc<()>),String> {
        let owner=self.clone();
        crate::rt::spawn_blocking(move |_| {
            let slot=owner.shelf.read().unwrap();(slot.namespace.clone(),slot.generation.clone())
        }).await.map_err(|_|"Unable to read audiobook account".into())
    }
    pub(crate) async fn spoken_download_queue(self:&Arc<Self>) -> Result<Vec<SpokenQueueRow>,String> {
        let owner=self.clone();
        let (namespace,generation,queue)=crate::rt::spawn_blocking(move |_| {
            let slot=owner.shelf.read().unwrap();(slot.namespace.clone(),slot.generation.clone(),slot.queue.clone())
        }).await.map_err(|_|"Unable to read audiobook downloads".to_string())?;
        if namespace.is_empty() {return Ok(Vec::new());}
        let account=namespace.clone();
        let rows=self.shelf_db_run(generation.clone(),move |blocking,db|db.spoken_queue_rows(blocking,&account).map_err(fail)).await.map_err(|e|e.to_string())?;
        let active=queue.active_item();let paused=self.downloads.is_paused();
        let owner=self.clone();
        if !crate::rt::spawn_blocking(move |_|owner.shelf_generation_current(&generation)).await.unwrap_or(false) {return Err("Audiobookshelf changed".into());}
        Ok(rows.into_iter().map(|(logical,item,episode,title,failed)| {
            let progress=active.as_ref().filter(|(id,_)|id==&logical).map(|(_,p)|*p);
            SpokenQueueRow {logical,item,episode,title,namespace:namespace.clone(),failed,paused,progress}
        }).collect())
    }
    pub(crate) fn retry_spoken_downloads(self:&Arc<Self>) {
        let lib=self.clone();crate::rt::io().spawn(async move {
            let owner=lib.clone();
            let Ok((namespace,queue))=crate::rt::spawn_blocking(move |_| {
                let slot=owner.shelf.read().unwrap();(slot.namespace.clone(),slot.queue.clone())
            }).await else {return};
            let _=lib.db_run(move |blocking,db|db.retry_spoken_queue(blocking,&namespace)).await;
            queue.wake.notify_one();lib.notify(super::LibEvent::Downloads);
        });
    }
    pub(super) fn start_spoken_downloads(self:&Arc<Self>) { download::start(self); }
    pub(super) fn begin_download_intent(&self,key:String)->Result<Intent,String> {self.shelf.read().unwrap().downloads.begin(key)}
    pub fn begin_remove_spoken_download(&self,logical:&str)->Result<Intent,String> {
        let slot=self.shelf.read().unwrap();let intent=slot.downloads.begin(logical.into())?;
        slot.queue.discard_saved(logical);Ok(intent)
    }
    pub fn shelf_download(&self,item:&str,episode:Option<&str>)->Result<(),String> {
        self.shelf_client().map_err(|error|error.to_string())?;
        self.shelf_download_requested(self.begin_shelf_download(item,episode)?)
    }
    pub fn shelf_download_requested(&self,request:SpokenDownload)->Result<(),String> {
        let lib=self.me.upgrade().ok_or("Library closed")?;
        super::shelf_browse::wait(async move {lib.shelf_download_requested_async(request).await.map_err(fail)})
            .map_err(|error|error.to_string())
    }
    pub async fn shelf_download_requested_async(self:&Arc<Self>,request:SpokenDownload)->Result<(),String> {
        let lib=self.clone();
        // Dropping the UI's wait must not drop owned files while a card write
        // is active. This task retains ownership through publication/cleanup.
        crate::rt::io().spawn(async move {download::run(lib,request).await})
            .await.map_err(|_|"Audiobook download worker stopped".to_string())?
    }
    #[cfg(test)]
    pub(super) fn spoken_cover(&self, id: &str, size: u32) -> Option<Vec<u8>> {
        let weak = self.me.clone();
        let id = id.to_owned();
        super::shelf_browse::wait(async move { Ok(spoken_cover_async(weak, id, size).await) }).ok().flatten()
    }
    pub(super) fn sync_spoken(&self) -> bool {
        let Some(lib) = self.me.upgrade() else { return true };
        super::shelf_browse::wait(async move { Ok(lib.sync_spoken_async().await) }).unwrap_or(true)
    }

    pub(super) async fn sync_spoken_async(self: &Arc<Self>) -> bool {
        let serial = self.shelf.read().unwrap().progress_sync.clone();
        let _serial = serial.lock().await;
        let lib = self.clone();
        let initial = crate::rt::spawn_blocking(move |_| {
            if lib.offline_mode() { return None; }
            let slot = lib.shelf.read().unwrap();
            slot.client.as_ref()?;
            Some((slot.generation.clone(), slot.namespace.clone()))
        }).await;
        let (generation, initial) = match initial {
            Ok(Some(value)) => value,
            Ok(None) => return false,
            Err(_) => return true,
        };
        if self.shelf_db_run(generation, move |blocking, db| Ok(db.pending_spoken(blocking, &initial).map_err(fail)?.is_empty()
            && db.pending_spoken_sessions(blocking, &initial).map_err(fail)?.is_empty()
            && db.pending_spoken_played(blocking, &initial).map_err(fail)?.is_empty()))
            .await.is_ok_and(|empty| empty) { return false; }
        let Ok(client) = self.shelf_client_async().await else { return true; };
        let namespace = match client.namespace_async().await {
            Ok(value) => value,
            Err(_) => return true,
        };
        let Ok(generation) = self.shelf_admission(client.clone()).await else { return true; };
        let account = namespace.clone();
        let Ok(sessions) = self.shelf_db_run(generation.clone(), move |blocking, db| db.pending_spoken_sessions(blocking, &account).map_err(fail)).await else { return true; };
        for upload in sessions {
            let Some(spoken) = &upload.song.spoken else { continue; };
            let SpokenSource::Audiobookshelf { item, episode, .. } = &spoken.source else { continue; };
            let Ok(device_id) = self.shelf_db_run(generation.clone(), |blocking, db| Self::spoken_device_id(blocking, db).map_err(fail)).await else { return true; };
            let payload = json!({"id":upload.session.id,"libraryItemId":item,"episodeId":episode,
                "mediaType":if episode.is_some(){"podcast"}else{"book"},"duration":spoken.duration_ms as f64/1000.0,
                "currentTime":upload.position_ms as f64/1000.0,"startTime":upload.session.start_position_ms as f64/1000.0,
                "timeListening":upload.session.listened_ms as f64/1000.0,"startedAt":upload.session.started_ms,
                "updatedAt":upload.updated_ms,"playMethod":3,"mediaPlayer":"q2nd",
                "deviceInfo":{"deviceId":device_id,"clientName":"Q2 Navidrome"}});
            if client.sync_local_async(&payload).await.is_err() { return true; }
            let saved = upload.clone();
            if self.shelf_db_run(generation.clone(), move |blocking, db| db.acknowledge_spoken_session(blocking, &saved).map_err(fail)).await.is_err() { return true; }
            if self.pull_shelf_position_async(client.clone(), spoken.id.clone(), item, episode.as_deref(), false).await.is_err() { return true; }
        }
        let account = namespace.clone();
        let Ok(pending) = self.shelf_db_run(generation.clone(), move |blocking, db| {
            let mut positions = Vec::new();
            for (song, position) in db.pending_spoken(blocking, &account).map_err(fail)? {
                if let Some(book) = &song.spoken {
                    if !db.has_pending_spoken_session(blocking, &book.id).map_err(fail)? { positions.push((song, position)); }
                }
            }
            Ok(positions)
        }).await else { return true; };
        for (song, position) in pending {
            let Some(spoken) = song.spoken else {
                continue;
            };
            let SpokenSource::Audiobookshelf {
                namespace: account,
                item,
                episode,
                ..
            } = &spoken.source
            else {
                continue;
            };
            if account != &namespace {
                continue;
            }
            // Legacy bookmarks and position-only seeks have no measured listening
            // duration. Reconcile first; never invent sessions to upload them.
            if self.pull_shelf_position_async(client.clone(), spoken.id.clone(), item, episode.as_deref(), true).await.is_err() { return true; }
            let key = spoken.id.clone();
            let Ok(current) = self.shelf_db_run(generation.clone(), move |blocking, db| db.spoken_position(blocking, &key).map_err(fail)).await else { return true; };
            if current.as_ref() != Some(&position) { continue; }
            let payload = json!({"duration":spoken.duration_ms as f64/1000.0,
                "currentTime":position.position_ms as f64/1000.0,
                "progress":position.position_ms as f64/spoken.duration_ms.max(1) as f64});
            if client.update_progress_async(item, episode.as_deref(), &payload).await.is_err() {
                break;
            }
            let logical = spoken.id.clone();
            if self.shelf_db_run(generation.clone(), move |blocking, db| {
                db.acknowledge_spoken(blocking, &logical, position.revision).map_err(fail)
            }).await.is_err() { break; }
            if self.shelf_result(generation.clone(), ()).await.is_err() { break; }
            let _ = self.pull_shelf_position_async(client.clone(), spoken.id, item, episode.as_deref(), false).await;
        }
        let account = namespace.clone();
        let Ok(marks) = self.shelf_db_run(generation.clone(), move |blocking, db| {
            db.pending_spoken_played(blocking, &account).map_err(fail)
        }).await else { return true; };
        for mark in marks {
            if self.shelf_result(generation.clone(), ()).await.is_err() || self.offline_mode() { break; }
            let payload = if mark.played { json!({"isFinished": true}) }
                else { json!({"isFinished": false, "currentTime": 0, "progress": 0, "markAsFinishedTimeRemaining": 0}) };
            if client.update_progress_async(&mark.item, mark.episode.as_deref(), &payload).await.is_err() { break; }
            if self.shelf_db_run(generation.clone(), move |blocking, db| {
                db.acknowledge_spoken_played(blocking, &mark).map_err(fail)
            }).await.is_err() { break; }
        }
        self.shelf_db_run(generation, move |blocking, db| Ok(!db.pending_spoken(blocking, &namespace).map_err(fail)?.is_empty()
            || !db.pending_spoken_sessions(blocking, &namespace).map_err(fail)?.is_empty()
            || !db.pending_spoken_played(blocking, &namespace).map_err(fail)?.is_empty())).await.unwrap_or(true)
    }
}

/// Artwork owns no Library across network waits. The same client identity
/// gates both account verification and publication after the cover transfer.
pub(super) async fn spoken_cover_async(weak: std::sync::Weak<Library>, id: String, size: u32) -> Option<Vec<u8>> {
    enum Input { Local(Option<Vec<u8>>), Remote(Arc<Client>, String, String) }
    let input = super::art::with_lib(&weak, move |lib, blocking| {
        if let Some(relative) = id.strip_prefix("local-book-cover:") {
            return Some(Input::Local(lib.local_book_path(blocking, relative).ok()
                .and_then(|path| super::spoken::read_m4b_info(blocking, &path).ok()).and_then(|info| info.cover)));
        }
        let (namespace, item) = id.strip_prefix("shelf-book-cover:")?.split_once(':')?;
        Some(Input::Remote(lib.configured_shelf_client().ok()?, namespace.to_owned(), item.to_owned()))
    }).await.flatten()?;
    let Input::Remote(client, namespace, item) = input else {
        let Input::Local(bytes) = input else { unreachable!() };
        return bytes;
    };
    let result = client.namespace_async().await;
    let accepted = {
        let lib = weak.upgrade()?;
        lib.finish_shelf_probe_async(client.clone(), result).await.ok()
            .is_some_and(|client| client.cached_namespace() == Some(namespace.as_str()))
    };
    if !accepted { return None; }
    let bytes = client.cover_async(&item, size).await.ok()?;
    super::art::with_lib(&weak, move |lib, _| lib.shelf_current(&client).then_some(bytes)).await.flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    struct BlockRequest {
        path: String,
        without_range: bool,
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    struct ServerState {
        user:String,
        progress: Value,
        other_progress: Value,
        block: Option<BlockRequest>,
        requests: Vec<String>,
        progress_sessions: Vec<String>,
        session_payloads: Vec<Value>,
        played_payloads: Vec<(String, Value)>,
        media_progress: Vec<Value>,
        closed_sessions: usize,
        fail_download: bool,
        resume_drop: bool,
        resume_requests: Vec<(String,String)>,
        cover_response:u16,
        progress_response:u16,
        bad_range: bool,
    }
    struct Fixture {
        lib: Arc<Library>,
        state: Arc<Mutex<ServerState>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        root: std::path::PathBuf,
        proxy: String,
        base: String,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    fn respond(socket: &mut TcpStream, status: &str, headers: &str, body: &[u8]) {
        if let Err(error) = write!(socket,
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len()) {
            assert!(matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset));
            return;
        }
        let _ = socket.write_all(body);
    }
    fn fixture() -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let base = format!("{proxy}/abs");
        let state = Arc::new(Mutex::new(ServerState {
            user:"generated-user".into(),
            progress: json!({"currentTime":12.345,"lastUpdate":100}),
            other_progress: json!({"currentTime":3.0,"lastUpdate":200}),
            block: None,
            requests: Vec::new(),
            progress_sessions: Vec::new(),
            session_payloads: Vec::new(),
            played_payloads: Vec::new(),
            media_progress: Vec::new(),
            closed_sessions: 0,
            fail_download: false,
            resume_drop: false,
            resume_requests: Vec::new(),
            cover_response:200,
            progress_response:200,
            bad_range: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server_state = state.clone();
        let thread = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            while !server_stop.load(Ordering::Relaxed) {
                let (mut socket, _) = match listener.accept() {
                    Ok(socket) => socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                let server_state = server_state.clone();
                handlers.push(std::thread::spawn(move || {
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
                let header = String::from_utf8(header).unwrap();
                assert!(
                    header.contains("Authorization: Bearer generated-token")
                        || header.contains("Authorization: Bearer rotated-generated-token")
                        || header.contains("Authorization: Bearer other-generated-token")
                );
                let line = header.lines().next().unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap();
                let target = parts.next().unwrap();
                let target = if target.starts_with("http://") {
                    target.splitn(4, '/').nth(3).unwrap().to_string()
                } else { target.trim_start_matches('/').to_string() };
                let path = target.strip_prefix("abs").unwrap();
                let len = header
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut bytes = vec![0; len];
                socket.read_exact(&mut bytes).unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let other=header.contains("Authorization: Bearer other-generated-token");
                let mut state = server_state.lock().unwrap();
                state.requests.push(format!("{method} {path}"));
                if method == "POST" && path == "/api/session/local" {
                    state.progress_sessions.push(body["id"].as_str().unwrap().to_owned());
                    state.session_payloads.push(body.clone());
                }
                let normalized=format!("{method} {}",path.split('?').next().unwrap());
                if method=="GET" && path.starts_with("/api/me/progress/") && state.progress_response!=200 {
                    respond(&mut socket,&format!("{} Generated",state.progress_response),"",b"{}");
                    return;
                }
                if !other && state.block.as_ref().is_some_and(|block|block.path==normalized && (!block.without_range || !header.contains("Range: bytes="))) {
                    let block = state.block.take().unwrap();
                    drop(state);
                    if block.entered.send(()).is_err() || block.release.recv_timeout(crate::library::TEST_LIVENESS).is_err() {
                        return;
                    }
                    state = server_state.lock().unwrap();
                }
                let json_response = match (method, path.split('?').next().unwrap()) {
                    ("GET", "/api/me") => Some(json!({"id":if other{"other-user"}else{&state.user},
                        "mediaProgress":if other { &[][..] } else { &state.media_progress[..] }})),
                    ("GET", "/api/libraries") => Some(
                        json!({"libraries":[{"id":"books","mediaType":"book"},{"id":"podcasts","mediaType":"podcast"}]}),
                    ),
                    ("GET", "/api/libraries/books/items") => Some(
                        json!({"results":[{"id":"book","media":{"metadata":{"title":if other{"Other Book"}else{"Generated Book"}}}}]}),
                    ),
                    ("GET", "/api/libraries/podcasts/items") => Some(
                        json!({"results":[{"id":"podcast","media":{"metadata":{"title":"Generated Podcast"}}}]}),
                    ),
                    ("GET", "/api/items/book") => Some(json!({"id":"book","libraryId":"books","mediaType":"book","media":{"duration":20,"metadata":{"title":"Generated Book","authors":[{"name":"Synthetic Author"}],"description":"Generated download summary"}}})),
                    ("GET", "/api/items/podcast") => Some(
                        json!({"id":"podcast","libraryId":"podcasts","mediaType":"podcast","media":{"metadata":{"title":"Generated Podcast"},"episodes":[{"id":"episode","title":if other{"Other Episode"}else{"Generated Episode"},"duration":20,"audioFile":{"ino":"e","duration":20}},{"id":"not-downloaded","title":"Unavailable","audioFile":null}]}}),
                    ),
                    ("POST", "/api/items/book/play")
                    | ("POST", "/api/items/podcast/play/episode") => {
                        assert_eq!(body["forceDirectPlay"], true);
                        Some(
                            json!({"id":"generated-session","displayTitle":"Generated Book","displayAuthor":"Synthetic Author","duration":20,
                        "chapters":[{"start":0,"title":"One"},{"start":15,"title":"Two"}],"audioTracks":[
                        {"contentUrl":"/api/items/book/file/a","startOffset":0,"duration":10,"metadata":{"ext":".wav"}},
                        {"contentUrl":"/api/items/book/file/b","startOffset":10,"duration":10,"metadata":{"ext":".wav"}}]}),
                        )
                    }
                    ("POST", "/api/session/generated-session/close") => {
                        state.closed_sessions += 1;
                        respond(&mut socket, "200 OK", "", b"OK");
                        None
                    }
                    ("GET", "/api/me/progress/book")
                    | ("GET", "/api/me/progress/podcast/episode") => Some(if other{state.other_progress.clone()}else{state.progress.clone()}),
                    ("PATCH", "/api/me/progress/book") | ("PATCH", "/api/me/progress/podcast/episode") => {
                        if body["isFinished"].is_boolean() {
                            state.played_payloads.push((path.into(), body.clone()));
                            if state.progress_response != 200 {
                                respond(&mut socket, "503 Generated", "", b"{}");
                                return;
                            }
                        }
                        let progress=if other{&mut state.other_progress}else{&mut state.progress};
                        *progress=json!({"currentTime":body["currentTime"],"lastUpdate":super::super::spoken::epoch_ms(),
                            "isFinished":body["isFinished"],"progress":body["progress"]});
                        respond(&mut socket, "200 OK", "", b"OK");
                        None
                    }
                    ("POST", "/api/session/local") => {
                        let progress=if other{&mut state.other_progress}else{&mut state.progress};
                        if progress["lastUpdate"].as_u64().unwrap_or(0)<=body["updatedAt"].as_u64().unwrap(){*progress=json!({"currentTime":body["currentTime"],"lastUpdate":body["updatedAt"]});}
                        respond(&mut socket, "200 OK", "", b"OK");
                        None
                    }
                    ("GET","/api/items/book/cover") | ("GET","/api/items/podcast/cover")=>{
                        match state.cover_response {
                            200=>respond(&mut socket,"200 OK","",&crate::gfx::Image::placeholder(8).png().unwrap()),
                            201=>respond(&mut socket,"200 OK","",b"invalid generated image"),
                            404=>respond(&mut socket,"404 Not Found","",b""),
                            401=>respond(&mut socket,"401 Unauthorized","",b"generated denied"),
                            _=>respond(&mut socket,"503 Service Unavailable","",b"generated unavailable"),
                        }None},
                    ("GET", "/api/items/book/file/a") | ("GET", "/api/items/book/file/b") => {
                        let audio = (0..100000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
                        if let Some(range) =
                            header.lines().find_map(|l| l.strip_prefix("Range: bytes="))
                        {
                            let (start, end) = range.split_once('-').unwrap();
                            let start: usize = start.parse().unwrap();
                            let end: usize = if end.is_empty() {audio.len()-1} else {end.parse().unwrap()};
                            if header.lines().any(|line|line.to_ascii_lowercase().starts_with("if-range:")) {
                                let condition=header.lines().find(|line|line.to_ascii_lowercase().starts_with("if-range:")).unwrap().to_owned();
                                state.resume_requests.push((range.to_owned(),condition));
                            }
                            let total = if state.bad_range {
                                audio.len() + 1
                            } else {
                                audio.len()
                            };
                            respond(
                                &mut socket,
                                "206 Partial Content",
                                &format!("Content-Range: bytes {start}-{end}/{total}\r\nETag: \"generated-v1\"\r\n"),
                                &audio[start..=end],
                            );
                        } else if state.resume_drop {
                            state.resume_drop=false;
                            write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"generated-v1\"\r\nConnection: close\r\n\r\n",audio.len()).unwrap();
                            let _=socket.write_all(&audio[..25000]);
                        } else if state.fail_download {
                            write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\nshort",audio.len()).unwrap();
                        } else {
                            respond(&mut socket, "200 OK", "ETag: \"generated-v1\"\r\n", &audio);
                        }
                        None
                    }
                    _ => panic!("unexpected synthetic request {method} {path}"),
                };
                if let Some(value) = json_response {respond(&mut socket,"200 OK","",value.to_string().as_bytes());}
                  }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
          });
        let root = super::super::generated_test_directory("abs-fixture");
        let paths = crate::config::Paths::new(root.clone());
        let mut config = crate::config::Config::default();
        config.storage.reserve_mb = 1;
        let lib = Library::open(paths, &config, Arc::new(|_| {}));
        let client = Arc::new(
            Client::new(&base, "generated-token", Some(&proxy)).unwrap(),
        );
        let key = client.account_cache_key();
        lib.install_shelf(Some(client), key);
        Fixture {
            lib,
            state,
            stop,
            thread: Some(thread),
            root,
            proxy,
            base,
        }
    }
    fn block(
        fixture: &Fixture,
        path: &str,
        without_range: bool,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (entered, waiting) = std::sync::mpsc::sync_channel(1);
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        fixture.state.lock().unwrap().block = Some(BlockRequest {
            path: path.into(),
            without_range,
            entered,
            release: resume,
        });
        (waiting, release)
    }
    fn switch_account(fixture: &Fixture) -> String {
        let client = Arc::new(
            Client::new(
                &fixture.base,
                "other-generated-token",
                Some(&fixture.proxy),
            )
            .unwrap(),
        );
        let key = client.account_cache_key();
        fixture.lib.install_shelf(Some(client), key);
        fixture.lib.shelf_client().unwrap().cached_namespace().unwrap().to_owned()
    }
    fn check_connection(lib: &Arc<Library>, asynchronous: bool) -> Result<(), LibError> {
        if !asynchronous { return lib.check_shelf_connection(); }
        let lib = lib.clone();
        let (send, receive) = std::sync::mpsc::channel();
        crate::rt::io().spawn(async move {
            let result = lib.check_shelf_connection_async().await;
            let _ = send.send(result);
        });
        receive.recv_timeout(crate::library::TEST_LIVENESS).expect("async probe completed")
    }
    #[test]
    fn detected_account_reassignment_invalidates_inflight_ownership_and_requires_reverification() {
        for asynchronous in [false, true] {
            let f=fixture();let old=f.lib.shelf_client().unwrap();let namespace=old.cached_namespace().unwrap().to_owned();let generation=f.lib.shelf_generation();
            f.state.lock().unwrap().user="generated-reassigned-user".into();
            assert!(check_connection(&f.lib, asynchronous).is_err());
            assert!(!f.lib.shelf_current(&old));assert!(!f.lib.shelf_generation_current(&generation));
            assert!(!f.lib.shelf_namespace_current(&namespace));assert!(f.lib.shelf_info_target(None,"book".into(),None).is_err());
            let mut config=crate::config::Config::default();config.audiobookshelf.url=f.base.clone();config.audiobookshelf.token="generated-token".into();
            f.lib.configure(&config);assert!(f.lib.shelf.read().unwrap().namespace.is_empty());
            check_connection(&f.lib, asynchronous).unwrap();assert!(!f.lib.shelf_namespace_current(&namespace));
            assert!(!f.lib.shelf.read().unwrap().namespace.is_empty());
        }
    }
    #[test]
    fn late_changed_identity_probe_cannot_invalidate_a_replacement_account() {
        for asynchronous in [false, true] {
            let f=fixture();f.lib.shelf_client().unwrap();
            let (seen,release)=block(&f,"GET /api/me",false);let lib=f.lib.clone();
            let pending=std::thread::spawn(move||check_connection(&lib, asynchronous));
            seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            if asynchronous {
                let (send, receive) = std::sync::mpsc::channel();
                crate::rt::io().spawn(async move { let _ = send.send(()); });
                receive.recv_timeout(crate::library::TEST_LIVENESS).expect("I/O runtime stays responsive during probe");
            }
            f.state.lock().unwrap().user="generated-reassigned-user".into();
            let replacement=switch_account(&f);let generation=f.lib.shelf_generation();
            release.send(()).unwrap();assert!(pending.join().unwrap().is_err());
            assert!(f.lib.shelf_namespace_current(&replacement));assert!(f.lib.shelf_generation_current(&generation));
            assert!(f.lib.shelf.read().unwrap().client.is_some());
        }
    }
    #[test]
    fn generated_global_route_transitions_replace_transport_but_keep_account_and_unrelated_settings() {
        let fixture=fixture();let lib=&fixture.lib;let mut config=crate::config::Config::default();
        config.audiobookshelf.url=fixture.base.clone();config.audiobookshelf.token="generated-token".into();
        let mut rng=fastrand::Rng::with_seed(0xA85_2026);
        for _ in 0..256 {
            config.tailscale.enabled=rng.bool();lib.set_shelf(&config);
            let slot=lib.shelf.read().unwrap();let client=slot.client.clone().unwrap();let generation=slot.generation.clone();drop(slot);
            assert!(client.configured_as(&fixture.base,"generated-token",config.http_proxy()));
            let account=client.account_cache_key();
            config.display.brightness=rng.u8(1..=100);lib.set_shelf(&config);
            assert!(Arc::ptr_eq(&generation,&lib.shelf_generation()));
            config.tailscale.enabled=!config.tailscale.enabled;lib.set_shelf(&config);
            assert!(!Arc::ptr_eq(&generation,&lib.shelf_generation()));
            let current=lib.shelf.read().unwrap().client.clone().unwrap();
            assert_eq!(current.account_cache_key(),account);
            assert!(current.configured_as(&fixture.base,"generated-token",config.http_proxy()));
        }
    }
    #[test]
    fn blocked_old_account_results_cannot_publish_into_new_account_or_start_playback() {
        crate::audio::callback::test_scope(|| {
        use crate::audio::player::TrackSource;
        for route_change in [false, true] {
            for operation in [
                "items", "episodes", "open", "cover", "range", "progress", "download",
            ] {
                let fixture = fixture();
                let lib = fixture.lib.clone();
                lib.shelf_items(false, Policy::NetworkFirst).unwrap();
                lib.shelf_episodes("podcast", Policy::NetworkFirst).unwrap();
                let songs = lib.shelf_open("book", None).unwrap();
                let old_namespace = lib.shelf.read().unwrap().namespace.clone();
                if operation == "download" {
                    lib.shelf_download("book", None).unwrap();
                }
                if operation == "progress" {
                    lib.db().save_spoken_checkpoint(&crate::rt::Blocking::dedicated_thread(),
                        &songs[0].spoken.as_ref().unwrap().id, 8000, super::super::spoken::epoch_ms(),
                        Some((&songs[0], &listening(1000)))).unwrap();
                }
                let path = match operation {
                    "items" => "GET /api/libraries",
                    "episodes" => "GET /api/items/podcast",
                    "open" => "POST /api/items/book/play",
                    "cover" => "GET /api/items/book/cover",
                    "progress" => "POST /api/session/local",
                    _ => "GET /api/items/book/file/a",
                };
                let (waiting, release) = block(&fixture, path, operation == "download");
                let task = lib.clone();
                let cover = format!("shelf-book-cover:{old_namespace}:book");
                let song = songs[0].clone();
                let old = std::thread::spawn(move || crate::audio::callback::test_scope(|| match operation {
                    "items" => task.shelf_items(false, Policy::NetworkFirst).is_err(),
                    "episodes" => task
                        .shelf_episodes("podcast", Policy::NetworkFirst)
                        .is_err(),
                    "open" => task.shelf_open("book", None).is_err(),
                    "cover" => task.spoken_cover(&cover, 50).is_none(),
                    "range" => {
                        let mut track = super::super::Tracks(task)
                            .open(&crate::rt::Blocking::dedicated_thread(), &song, &Arc::new(crate::audio::interrupt::Interrupt::new(false)))
                            .unwrap();
                        track.source.read(&mut [0; 16]).is_err()
                    }
                    "progress" => {
                        task.sync_spoken();
                        true
                    }
                    _ => task.shelf_download("book", None).is_err(),
                }));
                waiting.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                let old_client = lib.shelf.read().unwrap().client.clone().unwrap();
                let new_namespace = if route_change {
                    let client = Arc::new(Client::new(&fixture.base, "generated-token", None).unwrap());
                    let key = client.account_cache_key();
                    assert_eq!(key, old_client.account_cache_key());
                    lib.install_shelf(Some(client), key);
                    lib.shelf_client().unwrap().cached_namespace().unwrap().to_owned()
                } else {
                    switch_account(&fixture)
                };
                assert!(!lib.shelf_current(&old_client));
                assert_eq!(old_namespace == new_namespace, route_change);
                assert!(
                    lib.shelf_queue_current(&songs) == route_change,
                    "old queue must fail publication gate"
                );
                let new_items = lib.shelf_items(false, Policy::NetworkFirst).unwrap();
                let new_episodes = lib.shelf_episodes("podcast", Policy::NetworkFirst).unwrap();
                assert_eq!(
                    new_items[0].1,
                    if route_change {
                        "Generated Book"
                    } else {
                        "Other Book"
                    }
                );
                assert_eq!(
                    new_episodes[0].1,
                    if route_change {
                        "Generated Episode"
                    } else {
                        "Other Episode"
                    }
                );
                let new_queue = if route_change && operation == "progress" {
                    songs.clone()
                } else {
                    lib.shelf_open("book", None).unwrap()
                };
                assert!(
                    lib.shelf_queue_current(&new_queue),
                    "new authenticated queue must remain playable"
                );
                release.send(()).unwrap();
                assert!(old.join().unwrap(), "{operation} accepted a stale result");
                assert_eq!(
                    lib.shelf_items(false, Policy::CacheOnly).unwrap()[0].1,
                    if route_change {
                        "Generated Book"
                    } else {
                        "Other Book"
                    }
                );
                assert_eq!(
                    lib.shelf_episodes("podcast", Policy::CacheOnly).unwrap()[0].1,
                    if route_change {
                        "Generated Episode"
                    } else {
                        "Other Episode"
                    }
                );
                if operation == "progress" {
                    assert!(
                        lib.spoken_position(&songs[0].spoken.as_ref().unwrap().id)
                            .unwrap()
                            .unwrap()
                            .dirty,
                        "stale ack erased old pending progress"
                    );
                }
                if operation == "download" {
                    assert_eq!(
                        lib.downloaded_spoken().unwrap().len(),
                        1,
                        "route/account changes preserve previous downloads"
                    );
                    assert_eq!(
                        crate::library::stored_files(&fixture.root.join("spoken-downloads")).len(),
                        2,
                        "cancelled download changed old owned files"
                    );
                }
            }
        }
            })
    }
    #[test]
    fn server_second_values_round_trip_millisecond_checkpoints() {
        let mut rng = fastrand::Rng::with_seed(0xA85);
        for _ in 0..10000 {
            let position = rng.u64(0..864000000);
            assert_eq!(ms(&json!(position as f64 / 1000.0)), position);
        }
        for value in [Value::Null, json!(-1), json!("invalid")] {
            assert_eq!(ms(&value), 0);
        }
    }
    #[test]
    fn cancelled_async_open_closes_session_without_queue_publication_and_keeps_pool_live() {
        for path in ["POST /api/items/book/play", "POST /api/session/generated-session/close", "GET /api/items/book/file/a"] {
            let fixture = fixture();
            let lib = &fixture.lib;
            let namespace = lib.shelf_client().unwrap().cached_namespace().unwrap().to_owned();
            let logical = format!("abs:{namespace}:book:");
            let (seen, release) = block(&fixture, path, false);
            let task = lib.clone();
            let pending = crate::rt::io().spawn(async move { task.shelf_open_async("book".into(), None).await });
            seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let task = lib.clone();
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            crate::rt::io().spawn(async move {
                task.db_run(|_blocking, db| db.spoken_response(&crate::rt::Blocking::dedicated_thread(), "device-id").unwrap()).await;
                crate::rt::spawn_blocking(|_| ()).await.unwrap();
                send.send(()).unwrap();
            });
            receive.recv_timeout(crate::library::TEST_LIVENESS).expect("opening must not hold the DB, pool or executor during requests");
            pending.abort();
            super::super::shelf_browse::wait(async move { assert!(pending.await.unwrap_err().is_cancelled()); Ok(()) }).unwrap();
            release.send(()).unwrap();
            let deadline = std::time::Instant::now() + crate::library::TEST_LIVENESS;
            while fixture.state.lock().unwrap().closed_sessions == 0 {
                assert!(std::time::Instant::now() < deadline, "cancelled open leaked its received session");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(lib.spoken_queue(&logical).unwrap().is_empty(), "cancelled request published a queue");
            assert_eq!(fixture.state.lock().unwrap().closed_sessions, 1);
        }
    }

    /// UXI-004 / UX-005: a downloaded item opens from the card with the server
    /// unreachable, and a recent local position is not delayed by a refresh.
    #[test]
    fn downloaded_item_opens_without_the_server() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let logical = songs[0].spoken.as_ref().unwrap().id.clone();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let directory = lib.paths.media_root().join("spoken-downloads");
        std::fs::create_dir_all(&directory).unwrap();
        let files: Vec<_> = songs.iter().enumerate().map(|(i, song)| {
            let path = directory.join(format!("generated-{i}.m4b"));
            std::fs::write(&path, b"generated media").unwrap();
            (song.id.clone(), path, 15)
        }).collect();
        lib.db().put_spoken_downloads(&blocking, &logical, &files).unwrap();
        lib.db().save_spoken_position(&blocking, &logical, 4000, super::super::spoken::epoch_ms()).unwrap();
        let requests = fixture.state.lock().unwrap().requests.len();
        // Every further request would hang: the open must not need any.
        let (_seen, _release) = block(&fixture, "POST /api/items/book/play", false);
        let started = std::time::Instant::now();
        let opened = lib.shelf_open("book", None).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
        assert_eq!(opened.iter().map(|s| &s.id).collect::<Vec<_>>(), songs.iter().map(|s| &s.id).collect::<Vec<_>>());
        assert_eq!(fixture.state.lock().unwrap().requests.len(), requests, "a recent position needs no request");
        assert_eq!(lib.spoken_position(&logical).unwrap().unwrap().position_ms, 4000);
        // A missing file falls back to the server path.
        std::fs::remove_file(&files[0].1).unwrap();
        let task = lib.clone();
        let pending = crate::rt::io().spawn(async move { task.shelf_open_local_async("book", None).await });
        assert!(super::super::shelf_browse::wait(async move { Ok::<_, LibError>(pending.await.unwrap()) }).unwrap().is_none());
    }

    /// UX-007 / PLAYBACK-007: hovering an item stores its information and
    /// remote position, so its info page opens from the card; a second hover
    /// within the window makes no request.
    #[test]
    fn hover_prefetch_stores_info_and_position() {
        let fixture = fixture();
        let lib = &fixture.lib;
        lib.shelf_client().unwrap();
        let target = lib.shelf_info_target(None, "book".into(), None).unwrap();
        assert!(lib.book_info(&target, Policy::CacheOnly).is_err());
        fixture.state.lock().unwrap().progress = json!({"currentTime":42.0,"lastUpdate":500});
        let task = lib.clone(); let hovered = target.clone();
        super::super::shelf_browse::wait(async move { task.prefetch_spoken_async(hovered).await; Ok::<_, LibError>(()) }).unwrap();
        assert!(lib.book_info(&target, Policy::CacheOnly).is_ok(), "info stored on the card");
        let namespace = lib.shelf_client().unwrap().cached_namespace().unwrap().to_owned();
        assert_eq!(lib.spoken_position(&format!("abs:{namespace}:book:")).unwrap().unwrap().position_ms, 42_000);
        let requests = fixture.state.lock().unwrap().requests.len();
        let task = lib.clone();
        super::super::shelf_browse::wait(async move { task.prefetch_spoken_async(target).await; Ok::<_, LibError>(()) }).unwrap();
        assert_eq!(fixture.state.lock().unwrap().requests.len(), requests);
    }

    #[test]
    fn stale_downloaded_position_is_refreshed_before_play() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let logical = songs[0].spoken.as_ref().unwrap().id.clone();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let directory = lib.paths.media_root().join("spoken-downloads");
        std::fs::create_dir_all(&directory).unwrap();
        let files: Vec<_> = songs.iter().enumerate().map(|(i, song)| {
            let path = directory.join(format!("generated-{i}.m4b"));
            std::fs::write(&path, b"generated media").unwrap();
            (song.id.clone(), path, 15)
        }).collect();
        lib.db().put_spoken_downloads(&blocking, &logical, &files).unwrap();
        let old = super::super::spoken::epoch_ms() - 3 * 60 * 60 * 1000;
        lib.db().save_spoken_position(&blocking, &logical, 4000, old).unwrap();
        // The local save is dirty; mark it uploaded so the server's newer answer applies.
        lib.db().accept_spoken_remote(&blocking, &logical, 4000, old).unwrap();
        fixture.state.lock().unwrap().progress = json!({"currentTime":90.0,"lastUpdate":old + 60_000});
        lib.shelf_open("book", None).unwrap();
        assert_eq!(lib.spoken_position(&logical).unwrap().unwrap().position_ms, 90_000);
    }

    #[test]
    fn retained_book_and_episode_reuse_ui_paths_without_reopening_abs() {
        use crate::ui::screens::spoken::Selection;
        for episode in [false, true] {
            let fixture=fixture();
            let (item, ep)=if episode {("podcast",Some("episode"))} else {("book",None)};
            let songs=fixture.lib.shelf_open(item,ep).unwrap();
            let same=Selection::Shelf {item:item.into(),episode:ep.map(str::to_owned)};
            let other=if episode {Selection::Shelf {item:"book".into(),episode:None}}
                else {Selection::Shelf {item:"podcast".into(),episode:Some("episode".into())}};
            let expected=if episode {"POST /api/items/book/play"} else {"POST /api/items/podcast/play/episode"};
            let state=fixture.state.clone();
            crate::ui::settings_tests::exercise_retained_spoken_selection(fixture.lib.clone(),songs,same,other,
                move ||state.lock().unwrap().requests.clone(),expected);
        }
    }

    fn listening(ms: u64) -> crate::audio::listening::Session {
        crate::audio::listening::Session { id: "11111111-1111-4111-8111-111111111111".into(), started_ms: 100, start_position_ms: 0, listened_ms: ms, last_listened_ms: 100 + ms }
    }

    #[test]
    fn account_probe_projects_server_progress_without_opening_each_item() {
        let fixture = fixture();
        fixture.state.lock().unwrap().media_progress = vec![
            json!({"libraryItemId":"book","episodeId":null,"lastUpdate":500,"currentTime":7.0,"progress":0.35}),
            json!({"libraryItemId":"podcast","episodeId":"episode","lastUpdate":500,"isFinished":true}),
        ];
        let lib = &fixture.lib;
        lib.shelf_client().unwrap();
        let ns = lib.shelf_namespace();
        let book = format!("abs:{ns}:book:");
        let ep = format!("abs:{ns}:podcast:episode");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let blocking = crate::rt::Blocking::dedicated_thread();
        loop {
            if lib.db().spoken_percent(&blocking, &ep, 0).unwrap() == 100 { break; }
            assert!(std::time::Instant::now() < deadline, "probe progress was not stored");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lib.db().spoken_percent(&blocking, &book, 0).unwrap(), 35);
        assert_eq!(lib.spoken_position(&book).unwrap().unwrap().position_ms, 7_000);
        assert_eq!(fixture.state.lock().unwrap().requests, ["GET /api/me"]);
    }

    #[test]
    fn explicit_played_marks_survive_offline_and_failed_sync_for_books_and_episodes() {
        use super::super::spoken::PlayedTarget;
        let fixture = fixture();
        let lib = &fixture.lib;
        lib.shelf_client().unwrap();
        let account = lib.shelf_namespace();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let book = PlayedTarget::shelf(account.clone(), "book".into(), None);
        let episode = PlayedTarget::shelf(account.clone(), "podcast".into(), Some("episode".into()));
        lib.set_offline_mode(true);
        for (target, played) in [(book.clone(), true), (episode.clone(), true), (episode.clone(), false)] {
            let lib = lib.clone();
            super::super::shelf_browse::wait(async move { lib.mark_spoken_played(target, played).await.map_err(fail) }).unwrap();
        }
        assert_eq!(lib.db().spoken_percent(&blocking, &book.logical, 20_000).unwrap(), 100);
        assert_eq!(lib.db().spoken_percent(&blocking, &episode.logical, 20_000).unwrap(), 0);
        assert_eq!(lib.spoken_position(&episode.logical).unwrap().unwrap().position_ms, 0);
        assert_eq!(lib.db().pending_spoken_played(&blocking, &account).unwrap().len(), 2);
        assert!(lib.db().pending_spoken_played(&blocking, "different-account").unwrap().is_empty());
        assert!(!lib.sync_spoken());
        assert!(fixture.state.lock().unwrap().played_payloads.is_empty());

        fixture.state.lock().unwrap().progress_response = 503;
        lib.set_offline_mode(false);
        assert!(lib.sync_spoken());
        assert_eq!(lib.db().pending_spoken_played(&blocking, &account).unwrap().len(), 2);
        // Reconnecting also wakes the background sync worker. Record the
        // response boundary so retries during the outage stay in that phase.
        let failed_attempts = {
            let mut state = fixture.state.lock().unwrap();
            assert!(!state.played_payloads.is_empty());
            state.progress_response = 200;
            state.played_payloads.len()
        };
        assert!(!lib.sync_spoken());
        assert!(lib.db().pending_spoken_played(&blocking, &account).unwrap().is_empty());
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.played_payloads[failed_attempts..], [
            ("/api/me/progress/book".into(), json!({"isFinished":true})),
            ("/api/me/progress/podcast/episode".into(),
             json!({"isFinished":false,"currentTime":0,"progress":0,"markAsFinishedTimeRemaining":0})),
        ]);
        assert!(state.session_payloads.is_empty(), "a mark is not a listening session");
    }

    #[test]
    fn only_new_audible_listening_supersedes_an_offline_mark() {
        use super::super::spoken::PlayedTarget;
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let target = PlayedTarget::song(&songs[0]).unwrap();
        let blocking = crate::rt::Blocking::dedicated_thread();
        lib.set_offline_mode(true);
        lib.db().set_spoken_played(&blocking, &target, false, 2_000).unwrap();

        // The durability worker may deliver an older observation after the
        // mark. Preserve its listening time while keeping the explicit cursor.
        lib.db().save_spoken_checkpoint(&blocking, &target.logical, 3_000, 2_100,
            Some((&songs[0], &listening(1_000)))).unwrap();
        assert_eq!(lib.spoken_position(&target.logical).unwrap().unwrap().position_ms, 0);

        // The same session continues after the mark. Its new audio wins.
        let heard = listening(3_000);
        lib.db().save_spoken_checkpoint(&blocking, &target.logical, 5_000, 3_200,
            Some((&songs[0], &heard))).unwrap();
        assert_eq!(lib.spoken_position(&target.logical).unwrap().unwrap().position_ms, 5_000);
        assert_eq!(lib.db().spoken_percent(&blocking, &target.logical, 20_000).unwrap(), 25);

        lib.set_offline_mode(false);
        assert!(!lib.sync_spoken());
        let state = fixture.state.lock().unwrap();
        assert!(state.played_payloads.is_empty(), "the older manual mark must not reset newly heard progress");
        assert_eq!(state.session_payloads.len(), 1);
        assert_eq!(state.session_payloads[0]["currentTime"], 5.0);
    }

    #[test]
    fn downloaded_resume_refreshes_phone_rewind_and_preserves_newer_offline_listening() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let logical = songs[0].spoken.as_ref().unwrap().id.clone();
        let blocking = crate::rt::Blocking::dedicated_thread();
        lib.db().save_spoken_position(&blocking, &logical, 15000, 200).unwrap();
        fixture.state.lock().unwrap().progress = json!({"currentTime":2.0,"lastUpdate":300});
        let task = lib.clone(); let id = logical.clone();
        super::super::shelf_browse::wait(async move { task.spoken_queue_for_playback_async(id).await.map_err(fail) }).unwrap();
        assert_eq!(lib.spoken_position(&logical).unwrap().unwrap().position_ms, 2000);
        assert!(!lib.spoken_position(&logical).unwrap().unwrap().dirty);
        // Fresh offline listening wins over an older server cursor.
        lib.db().save_spoken_position(&blocking, &logical, 6000, 400).unwrap();
        let task = lib.clone(); let id = logical.clone();
        super::super::shelf_browse::wait(async move { task.spoken_queue_for_playback_async(id).await.map_err(fail) }).unwrap();
        assert_eq!(lib.spoken_position(&logical).unwrap().unwrap().position_ms, 6000);
        assert!(lib.spoken_position(&logical).unwrap().unwrap().dirty);
        lib.set_offline_mode(true);
        let requests = fixture.state.lock().unwrap().requests.len();
        let task = lib.clone(); let id = logical.clone();
        super::super::shelf_browse::wait(async move { task.spoken_queue_for_playback_async(id).await.map_err(fail) }).unwrap();
        assert_eq!(fixture.state.lock().unwrap().requests.len(), requests);
    }

    #[test]
    fn cached_resume_survives_temporary_network_failure_but_refuses_authentication_failure() {
        let fixture=fixture(); let lib=&fixture.lib;
        let songs=lib.shelf_open("book",None).unwrap();
        let logical=songs[0].spoken.as_ref().unwrap().id.clone();
        let blocking=crate::rt::Blocking::dedicated_thread();
        lib.db().save_spoken_position(&blocking,&logical,6000,400).unwrap();
        for (status,works) in [(503,true),(401,false),(403,false),(400,false),(200,true)] {
            fixture.state.lock().unwrap().progress_response=status;
            let task=lib.clone(); let id=logical.clone();
            let result=super::super::shelf_browse::wait(async move {task.spoken_queue_for_playback_async(id).await.map_err(fail)});
            assert_eq!(result.is_ok(),works,"HTTP {status}");
            assert_eq!(lib.spoken_position(&logical).unwrap().unwrap().position_ms,6000);
        }
    }

    #[test]
    fn durable_checkpoint_uploads_without_another_play_or_coalescing_delay() {
        let fixture=fixture();let lib=&fixture.lib;
        let songs=lib.shelf_open("book",None).unwrap();
        Library::start_sync(lib);
        let run=lib.clone();let song=songs[0].clone();
        super::super::shelf_browse::wait(async move {
            run.save_spoken_session_async(&song,2000,Some(listening(2000))).await.map_err(fail)
        }).unwrap();
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(2);
        loop {
            if fixture.state.lock().unwrap().session_payloads.len()==1 {break;}
            assert!(std::time::Instant::now()<deadline,"saved session waited for playback or routine three-second batching");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let state=fixture.state.lock().unwrap();
        assert_eq!(state.session_payloads[0]["timeListening"],2.0);
        assert_eq!(state.session_payloads[0]["currentTime"],2.0);
    }

    #[test]
    fn twenty_checkpoints_update_one_cumulative_session_and_paused_retries_are_quiet() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let logical = songs[0].spoken.as_ref().unwrap().id.clone();
        let blocking = crate::rt::Blocking::dedicated_thread();
        for second in 1..=20 {
            lib.db().save_spoken_checkpoint(&blocking, &logical, second*500, 100+second,
                Some((&songs[0], &listening(second*500)))).unwrap();
            assert!(!lib.sync_spoken());
        }
        for second in 21..=40 {
            lib.db().save_spoken_checkpoint(&blocking, &logical, 10000, 100+second,
                Some((&songs[0], &listening(10000)))).unwrap();
            assert!(!lib.sync_spoken());
        }
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.session_payloads.len(), 20);
        assert!(state.session_payloads.iter().all(|p| p["timeListening"].as_f64().unwrap() > 0.0));
        assert_eq!(state.progress_sessions.iter().collect::<std::collections::HashSet<_>>().len(), 1);
        assert_eq!(state.session_payloads.last().unwrap()["timeListening"], 10.0);
        assert_eq!(state.progress["currentTime"], 10.0);
    }

    #[test]
    fn paused_book_does_not_create_twenty_sessions_or_overwrite_phone_progress() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let logical = songs[0].spoken.as_ref().unwrap().id.clone();
        // Twenty ordinary durability saves at the same audible cursor while
        // paused must not manufacture new listening or newer local timestamps.
        let before = lib.spoken_position(&logical).unwrap().unwrap();
        for _ in 0..20 {
            lib.save_spoken(&songs[1], 2345).unwrap();
            assert!(!lib.sync_spoken());
        }
        assert_eq!(fixture.state.lock().unwrap().progress_sessions.len(), 0,
            "durability checkpoints are not listening sessions");
        assert_eq!(lib.spoken_position(&logical).unwrap().unwrap(), before);
        // Another device deliberately rewinds: the freshest timestamp wins,
        // not the furthest position or the paused player's latest heartbeat.
        fixture.state.lock().unwrap().progress = json!({"currentTime":2.0,"lastUpdate":200});
        lib.shelf_open("book", None).unwrap();
        assert_eq!(lib.spoken_position(&logical).unwrap().unwrap().position_ms, 2000);
    }

    #[test]
    fn stalled_async_progress_preserves_new_revision_and_pool_progress() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let logical = songs[0].spoken.as_ref().unwrap().id.clone();
        lib.db().save_spoken_checkpoint(&crate::rt::Blocking::dedicated_thread(), &logical, 1000, 1000, Some((&songs[0], &listening(1000)))).unwrap();
        let (seen, release) = block(&fixture, "POST /api/session/local", false);
        let task = lib.clone();
        let pending = crate::rt::io().spawn(async move { task.sync_spoken_async().await });
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let task = lib.clone();
        let id = logical.clone();
        let song = songs[0].clone();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        crate::rt::io().spawn(async move {
            // Exercise both executor-owned DB work and the pool while HTTP stalls.
            task.db_run(move |_blocking, db| db.save_spoken_checkpoint(&crate::rt::Blocking::dedicated_thread(), &id, 2000, 2000, Some((&song, &listening(2000)))).unwrap()).await;
            crate::rt::spawn_blocking(|_| ()).await.unwrap();
            send.send(()).unwrap();
        });
        receive.recv_timeout(crate::library::TEST_LIVENESS).expect("network wait must not hold DB/pool/executor");
        release.send(()).unwrap();
        assert!(super::super::shelf_browse::wait(async move { Ok(pending.await.unwrap()) }).unwrap());
        let position = lib.spoken_position(&logical).unwrap().unwrap();
        assert_eq!(position.position_ms, 2000);
        assert!(position.dirty, "old acknowledgement must not clear the new revision");
        assert!(!lib.sync_spoken());
        assert!(!lib.spoken_position(&logical).unwrap().unwrap().dirty);
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.progress_sessions.len(), 2);
        assert_eq!(state.progress_sessions[0], state.progress_sessions[1]);
        assert_eq!(state.session_payloads[1]["timeListening"], 2.0);
    }

    #[test]
    fn cancelled_async_progress_remains_dirty_and_retries_same_session() {
        let fixture = fixture();
        let lib = &fixture.lib;
        let songs = lib.shelf_open("book", None).unwrap();
        let logical = songs[0].spoken.as_ref().unwrap().id.clone();
        lib.db().save_spoken_checkpoint(&crate::rt::Blocking::dedicated_thread(), &logical, 3000, 1000, Some((&songs[0], &listening(3000)))).unwrap();
        let (seen, release) = block(&fixture, "POST /api/session/local", false);
        let task = lib.clone();
        let pending = crate::rt::io().spawn(async move { task.sync_spoken_async().await });
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        pending.abort();
        super::super::shelf_browse::wait(async move { assert!(pending.await.unwrap_err().is_cancelled()); Ok(()) }).unwrap();
        assert!(lib.spoken_position(&logical).unwrap().unwrap().dirty);
        release.send(()).unwrap();
        assert!(!lib.sync_spoken());
        assert!(!lib.spoken_position(&logical).unwrap().unwrap().dirty);
        let state = fixture.state.lock().unwrap();
        assert_eq!(state.progress_sessions.len(), 2);
        assert_eq!(state.progress_sessions[0], state.progress_sessions[1]);
    }

    #[test]
    fn books_podcasts_remote_ranges_token_rotation_downloads_and_offline_resume() {
        crate::audio::callback::test_scope(|| {
        use crate::audio::player::TrackSource;
        let fixture = fixture();
        let lib = &fixture.lib;
        assert_eq!(
            lib.shelf_items(false, Policy::NetworkFirst).unwrap(),
            vec![("book".into(), "Generated Book".into())]
        );
        assert_eq!(
            lib.shelf_items(true, Policy::NetworkFirst).unwrap(),
            vec![("podcast".into(), "Generated Podcast".into())]
        );
        assert_eq!(
            lib.shelf_episodes("podcast", Policy::NetworkFirst).unwrap(),
            vec![("episode".into(), "Generated Episode".into())]
        );
        let books = lib.shelf_open("book", None).unwrap();
        assert_eq!(books.len(), 2);
        let source = super::super::Tracks(lib.clone());
        assert_eq!(source.spoken_resume(&books, 0, 0).unwrap(), (1, 2345));
        let mut opened = source
            .open(&crate::rt::Blocking::dedicated_thread(), &books[1], &Arc::new(crate::audio::interrupt::Interrupt::new(false)))
            .unwrap();
        opened.source.seek(SeekFrom::Start(70000)).unwrap();
        let mut bytes = [0; 32];
        opened.source.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, std::array::from_fn(|i| ((70000 + i) % 251) as u8));
        lib.save_spoken(&books[1], 8000).unwrap();
        let logical = books[0].spoken.as_ref().unwrap().id.clone();
        let position = lib.spoken_position(&logical).unwrap().unwrap();
        // A later server rewind must win over older offline replay, not max(time).
        fixture.state.lock().unwrap().progress =
            json!({"currentTime":2.0,"lastUpdate":position.updated_ms+10000});
        lib.sync_spoken();
        assert_eq!(lib.spoken_position(&logical).unwrap().unwrap().position_ms, 2000);
        assert!(!lib.spoken_position(&logical).unwrap().unwrap().dirty);
        lib.shelf.write().unwrap().client = Some(Arc::new(
            Client::new(
                &fixture.base,
                "rotated-generated-token",
                Some(&fixture.proxy),
            )
            .unwrap(),
        ));
        let again = lib.shelf_open("book", None).unwrap();
        assert_eq!(again[0].spoken.as_ref().unwrap().id, logical);
        lib.shelf_download("book", None).unwrap();
        assert_eq!(lib.downloaded_spoken().unwrap().len(), 1);
        assert_eq!(
            crate::library::stored_files(&fixture.root.join("spoken-downloads")).len(),
            2
        );
        let count = fixture.state.lock().unwrap().requests.len();
        lib.set_offline_mode(true);
        let mut offline = source
            .open(&crate::rt::Blocking::dedicated_thread(), &again[1], &Arc::new(crate::audio::interrupt::Interrupt::new(false)))
            .unwrap();
        assert_eq!(offline.origin, crate::audio::player::Origin::Offline);
        offline.source.seek(SeekFrom::Start(70000)).unwrap();
        offline.source.read_exact(&mut bytes).unwrap();
        assert_eq!(fixture.state.lock().unwrap().requests.len(), count);
        // UX-005: a downloaded book opens offline, from the card, with no request.
        assert_eq!(lib.shelf_open("book", None).unwrap().len(), again.len());
        assert_eq!(fixture.state.lock().unwrap().requests.len(), count);
        assert_eq!(lib.shelf_items(true, Policy::CacheOnly).unwrap().len(), 1);
        lib.set_offline_mode(false);
        fixture.state.lock().unwrap().fail_download = true;
        assert!(lib.shelf_download("book", None).is_err());
        assert_eq!(
            crate::library::stored_files(&fixture.root.join("spoken-downloads")).len(),
            2
        );
        assert_eq!(lib.downloaded_spoken().unwrap().len(), 1);
        lib.remove_spoken_download(&logical).unwrap();
        assert!(lib.downloaded_spoken().unwrap().is_empty());
        assert_eq!(
            crate::library::stored_files(&fixture.root.join("spoken-downloads")).len(),
            0
        );
            })
    }
    #[test]
    fn queued_worker_does_not_hold_file_gate_while_shelf_snapshot_is_contended() {
        let f=fixture();f.lib.shelf_client().unwrap();
        let queue=f.lib.shelf.read().unwrap().queue.clone();
        let guard=f.lib.shelf.write().unwrap();
        queue.wake.notify_one();
        // Let the coordinator submit its snapshot, then require both a free
        // file gate and a live executor while the shelf writer remains held.
        let (pulse,seen)=std::sync::mpsc::channel();
        let lib=f.lib.clone();
        crate::rt::io().spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let gate=lib.offline_art_gate.acquire().await;
            let _=pulse.send(());drop(gate);
        });
        let result=seen.recv_timeout(Duration::from_secs(2));
        drop(guard);
        assert!(result.is_ok(),"queue worker blocked the file gate or IO executor behind shelf configuration");
    }
    #[test]
    fn queued_book_resumes_validated_prefix_after_real_disconnect() {
        let f=fixture();f.lib.shelf_client().unwrap();f.state.lock().unwrap().resume_drop=true;
        let request=f.lib.begin_shelf_download("book",None).unwrap();let lib=f.lib.clone();
        super::super::shelf_browse::wait(async move {lib.enqueue_shelf_download_async(request,"Generated book".into()).await.map_err(fail)}).unwrap();
        let deadline=std::time::Instant::now()+Duration::from_secs(30);
        while f.lib.downloaded_spoken().unwrap().is_empty() {
            assert!(std::time::Instant::now()<deadline,"resumed audiobook did not publish");std::thread::sleep(Duration::from_millis(10));
        }
        let requests=f.state.lock().unwrap().resume_requests.clone();
        assert!(!requests.is_empty());assert!(requests[0].0.starts_with("25000-"));assert!(requests[0].1.to_ascii_lowercase().contains("\"generated-v1\""));
        let song=f.lib.downloaded_spoken().unwrap()[0].clone();
        let file=f.lib.db().spoken_download(&crate::rt::Blocking::dedicated_thread(),&song.id).unwrap().unwrap();
        let expected=(0..100000).map(|i|(i%251) as u8).collect::<Vec<_>>();assert_eq!(std::fs::read(file).unwrap(),expected);
    }
    #[test]
    fn durable_spoken_queue_retries_network_and_reports_progress() {
        let f=fixture();f.lib.shelf_client().unwrap();
        f.state.lock().unwrap().fail_download=true;
        let request=f.lib.begin_shelf_download("book",None).unwrap();
        let lib=f.lib.clone();
        super::super::shelf_browse::wait(async move {lib.enqueue_shelf_download_async(request,"Generated book".into()).await.map_err(fail)}).unwrap();
        let deadline=std::time::Instant::now()+crate::library::TEST_LIVENESS;
        while !f.state.lock().unwrap().requests.iter().any(|r|r.contains("/api/items/book/file/a")) {
            assert!(std::time::Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));
        }
        let namespace=f.lib.shelf.read().unwrap().namespace.clone();
        let counts=f.lib.db().spoken_queue_counts(&crate::rt::Blocking::dedicated_thread(),&namespace).unwrap();
        assert_eq!(counts,(1,0),"network error stays queued");
        f.state.lock().unwrap().fail_download=false;
        let (seen,release)=block(&f,"GET /api/items/book/file/a",true);
        f.lib.downloads.interrupt();
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let status=f.lib.shelf.read().unwrap().queue.status((1,0));
        assert_eq!(status.current.as_ref().map(|p|p.0.as_str()),Some("Generated book"));
        release.send(()).unwrap();
        let deadline=std::time::Instant::now()+crate::library::TEST_LIVENESS;
        while f.lib.downloaded_spoken().unwrap().is_empty() {
            assert!(std::time::Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn queued_visibility_is_immediate_and_scoped_to_the_selected_account() {
        let f=fixture();f.lib.shelf_client().unwrap();f.lib.set_offline_mode(true);
        let request=f.lib.begin_shelf_download("book",None).unwrap();let owner=f.lib.clone();
        super::super::shelf_browse::wait(async move {owner.enqueue_shelf_download_async(request,"Generated Book".into()).await.map_err(fail)}).unwrap();
        let owner=f.lib.clone();let rows=super::super::shelf_browse::wait(async move {owner.spoken_download_queue().await.map_err(fail)}).unwrap();
        assert_eq!(rows.len(),1);assert_eq!(rows[0].title,"Generated Book");assert_eq!(rows[0].item,"book");assert!(rows[0].progress.is_none());
        let original=rows[0].namespace.clone();f.lib.set_offline_mode(false);let different=switch_account(&f);assert_ne!(original,different);
        let owner=f.lib.clone();let rows=super::super::shelf_browse::wait(async move {owner.spoken_download_queue().await.map_err(fail)}).unwrap();
        assert!(rows.is_empty());
    }
    #[test]
    fn reconnect_during_download_cleanup_cannot_turn_interruption_into_failure() {
        let f=fixture();f.lib.shelf_client().unwrap();
        let queue=f.lib.shelf.read().unwrap().queue.clone();
        let (cleared,seen_clear)=std::sync::mpsc::channel();let (release_clear,continue_clear)=std::sync::mpsc::channel();
        *queue.after_clear.lock().unwrap()=Some((cleared,continue_clear));
        let (seen,release)=block(&f,"GET /api/items/book/file/a",true);
        let request=f.lib.begin_shelf_download("book",None).unwrap();let lib=f.lib.clone();
        super::super::shelf_browse::wait(async move {lib.enqueue_shelf_download_async(request,"Generated book".into()).await.map_err(fail)}).unwrap();
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        f.lib.set_offline_mode(true);release.send(()).unwrap();
        seen_clear.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        f.lib.set_offline_mode(false);queue.wake.notify_one();release_clear.send(()).unwrap();
        let deadline=std::time::Instant::now()+crate::library::TEST_LIVENESS;
        while f.lib.downloaded_spoken().unwrap().is_empty() {
            assert!(std::time::Instant::now()<deadline,"interrupted download was not retried after reconnect");
            std::thread::sleep(Duration::from_millis(10));
        }
        let namespace=f.lib.shelf.read().unwrap().namespace.clone();
        assert_eq!(f.lib.db().spoken_queue_counts(&crate::rt::Blocking::dedicated_thread(),&namespace).unwrap().1,0);
    }
    #[test]
    fn queued_spoken_transfer_resumes_after_offline_interruption() {
        let f=fixture();f.lib.shelf_client().unwrap();
        let (seen,release)=block(&f,"GET /api/items/book/file/a",true);
        let request=f.lib.begin_shelf_download("book",None).unwrap();let lib=f.lib.clone();
        super::super::shelf_browse::wait(async move {lib.enqueue_shelf_download_async(request,"Generated book".into()).await.map_err(fail)}).unwrap();
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        f.lib.set_offline_mode(true);release.send(()).unwrap();
        let namespace=f.lib.shelf.read().unwrap().namespace.clone();
        assert_eq!(f.lib.db().spoken_queue_counts(&crate::rt::Blocking::dedicated_thread(),&namespace).unwrap(),(1,0));
        f.lib.set_offline_mode(false);f.lib.shelf.read().unwrap().queue.wake.notify_one();
        let deadline=std::time::Instant::now()+crate::library::TEST_LIVENESS;
        while f.lib.downloaded_spoken().unwrap().is_empty() {
            assert!(std::time::Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn spoken_queue_survives_database_reopen() {
        let root=super::super::generated_test_directory("spoken-queue-reopen");
        std::fs::create_dir_all(&root).unwrap();let path=root.join("queue.sqlite");
        let b=&crate::rt::Blocking::dedicated_thread();
        {
            let db=super::super::Db::open(b,&path).unwrap();
            db.enqueue_spoken(b,"account-a","logical","book",Some("episode"),"Book").unwrap();
        }
        let db=super::super::Db::open(b,&path).unwrap();
        let saved=db.next_spoken(b,"account-a").unwrap().unwrap();
        assert_eq!((saved.1.as_str(),saved.2.as_str(),saved.3.as_deref(),saved.4.as_str()),("logical","book",Some("episode"),"Book"));
        assert!(db.next_spoken(b,"account-b").unwrap().is_none());
    }
    #[test]
    fn spoken_queue_replacement_is_revision_bound_and_account_scoped() {
        let f=fixture();let b=&crate::rt::Blocking::dedicated_thread();let db=f.lib.db();
        db.enqueue_spoken(b,"account-a","logical","book",None,"First").unwrap();
        let first=db.next_spoken(b,"account-a").unwrap().unwrap().0;
        db.enqueue_spoken(b,"account-a","logical","book",None,"Second").unwrap();
        db.finish_spoken_queue(b,first,None).unwrap();
        assert!(db.next_spoken(b,"account-b").unwrap().is_none());
        assert_eq!(db.next_spoken(b,"account-a").unwrap().unwrap().4,"Second");
        for n in 0..31 {db.enqueue_spoken(b,"account-a",&format!("other-{n}"),"book",None,"Book").unwrap();}
        assert!(db.enqueue_spoken(b,"account-a","overflow","book",None,"Book").is_err());
        assert_eq!(db.spoken_queue_counts(b,"account-a").unwrap(),(32,0));
    }
    #[test]
    fn spoken_cover_absence_is_distinct_from_auth_network_and_invalid_image() {
        // 404 and an image the player cannot decode (201 here) publish the
        // book without a cover: a retry would get the same image (UX-006
        // fault journey bad-cover). Auth and network failures fail the download.
        for status in [404,401,503,201] {
            let saved=matches!(status,404|201);
            let f=fixture();f.state.lock().unwrap().cover_response=status;
            let result=f.lib.shelf_download("book",None);
            assert_eq!(result.is_ok(),saved,"cover response {status}");
            assert_eq!(f.lib.downloaded_spoken().unwrap().len(),usize::from(saved));
            if !saved {assert!(!f.root.join("spoken-downloads").exists());}
        }
    }
    #[test]
    fn remove_cancels_blocked_transfer_and_completed_newer_actions_stay_authoritative() {
        let f=fixture();f.lib.shelf_download("book",None).unwrap();
        let logical=f.lib.downloaded_spoken().unwrap()[0].spoken.as_ref().unwrap().id.clone();
        let (seen,release)=block(&f,"GET /api/items/book/file/a",true);
        let request=f.lib.begin_shelf_download("book",None).unwrap();let lib=f.lib.clone();
        let pending=std::thread::spawn(move||lib.shelf_download_requested(request));
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        f.lib.remove_spoken_download(&logical).unwrap();release.send(()).unwrap();
        assert!(pending.join().unwrap().is_err());assert!(f.lib.downloaded_spoken().unwrap().is_empty());
        assert_eq!(crate::library::stored_files(&f.root.join("spoken-downloads")).len(),0);
        assert_eq!(crate::library::stored_files(&f.root.join("offline-art")).len(),0);
        // A queued job has not started yet. Removing completes before it is run.
        let old=f.lib.begin_shelf_download("book",None).unwrap();f.lib.remove_spoken_download(&logical).unwrap();
        let requests=f.state.lock().unwrap().requests.len();assert!(f.lib.shelf_download_requested(old).is_err());
        assert_eq!(f.state.lock().unwrap().requests.len(),requests);
        // Conversely, an older queued removal cannot erase a completed requeue.
        let old_remove=f.lib.begin_remove_spoken_download(&logical).unwrap();
        f.lib.shelf_download("book",None).unwrap();
        assert!(f.lib.remove_spoken_download_requested(old_remove).is_err());
        assert_eq!(f.lib.downloaded_spoken().unwrap().len(),1);
    }
    #[test]
    fn async_download_cancels_stalled_headers_on_remove_offline_and_account_change() {
        for mode in 0..3 {
            let f=fixture();f.lib.shelf_client().unwrap();
            let request=f.lib.begin_shelf_download("book",None).unwrap();
            let logical=request.intent.logical.clone();
            let (seen,release)=block(&f,"GET /api/items/book/file/a",true);
            let owner=f.lib.clone();let (done,finished)=std::sync::mpsc::channel();
            crate::rt::io().spawn(async move {done.send(owner.shelf_download_requested_async(request).await).unwrap();});
            seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            match mode {
                0=>f.lib.remove_spoken_download(&logical).unwrap(),
                1=>f.lib.set_offline_mode(true),
                _=>{
                    let client=Arc::new(Client::new(&f.base,"other-generated-token",Some(&f.proxy)).unwrap());
                    let key=client.account_cache_key();f.lib.install_shelf(Some(client),key);
                },
            }
            // Must finish before the fixture permits any response headers.
            assert!(finished.recv_timeout(crate::library::TEST_LIVENESS).expect("stalled request cancellation").is_err());
            assert!(f.lib.downloaded_spoken().unwrap().is_empty());
            assert_eq!(crate::library::stored_files(&f.root.join("spoken-downloads")).len(),0);
            release.send(()).unwrap();
        }
    }

    #[test]
    fn contended_download_configuration_check_keeps_io_executor_responsive() {
        let f=fixture();f.lib.shelf_client().unwrap();
        let request=f.lib.begin_shelf_download("book",None).unwrap();
        let guard=f.lib.shelf.write().unwrap();
        let owner=f.lib.clone();let (done,finished)=std::sync::mpsc::channel();
        crate::rt::io().spawn(async move {done.send(super::download::run(owner,request).await).unwrap();});
        let (pulse,pulsed)=std::sync::mpsc::channel();
        crate::rt::io().spawn(async move {pulse.send(()).unwrap();});
        let responsive=pulsed.recv_timeout(Duration::from_secs(3));
        f.lib.set_offline_mode(true);
        drop(guard);
        assert!(responsive.is_ok(),"configuration lock must not block unrelated tasks");
        assert!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap().is_err());
    }

    #[test]
    fn aborting_async_download_waiter_keeps_file_publication_owned() {
        let f=fixture();f.lib.shelf_client().unwrap();
        let request=f.lib.begin_shelf_download("book",None).unwrap();
        let (seen,release)=block(&f,"GET /api/items/book/file/a",true);
        let owner=f.lib.clone();
        let waiter=crate::rt::io().spawn(async move {owner.shelf_download_requested_async(request).await});
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        waiter.abort();release.send(()).unwrap();
        let end=std::time::Instant::now()+crate::library::TEST_LIVENESS;
        while f.lib.downloaded_spoken().unwrap().is_empty() {
            assert!(std::time::Instant::now()<end,"detached publication must complete");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(crate::library::stored_files(&f.root.join("spoken-downloads")).len(),2);
        let logical=f.lib.downloaded_spoken().unwrap()[0].spoken.as_ref().unwrap().id.clone();
        f.lib.remove_spoken_download(&logical).unwrap();
        assert_eq!(crate::library::stored_files(&f.root.join("spoken-downloads")).len(),0);
    }

    #[test]
    fn downloaded_metadata_and_art_survive_reopen_and_index_failure_preserves_old_files() {
        let f=fixture();f.lib.shelf_download("book",None).unwrap();
        let songs=f.lib.downloaded_spoken().unwrap();let song=&songs[0];let logical=&song.spoken.as_ref().unwrap().id;
        let target=super::super::book_info::BookInfoTarget::from_song(song).unwrap();
        let info=f.lib.book_info(&target,Policy::CacheOnly).unwrap();assert_eq!(info.description,"Generated download summary");
        let original=f.lib.db().spoken_download(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap().unwrap();
        let connection=rusqlite::Connection::open(f.lib.paths.database()).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_art BEFORE INSERT ON offline_art_refs BEGIN SELECT RAISE(ABORT,'generated art fault'); END;").unwrap();
        let (done,received)=std::sync::mpsc::channel();let lib=f.lib.clone();
        let worker=std::thread::spawn(move||{let _=done.send(lib.shelf_download("book",None));});
        assert!(received.recv_timeout(crate::library::TEST_LIVENESS).expect("failed artwork publication must not deadlock").is_err());worker.join().unwrap();
        assert_eq!(f.lib.db().spoken_download(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap().unwrap(),original);assert!(original.exists());
        assert_eq!(crate::library::stored_files(&f.root.join("spoken-downloads")).len(),2);
        connection.execute_batch("DROP TRIGGER fail_art").unwrap();
        std::fs::remove_dir_all(f.lib.paths.art_cache()).unwrap();
        let mut config=crate::config::Config::default();config.audiobookshelf.url=f.base.clone();config.audiobookshelf.token="generated-token".into();
        let reopened=Library::open(f.lib.paths.clone(),&config,Arc::new(|_|{}));reopened.set_offline_mode(true);
        assert_eq!(reopened.book_info(&target,Policy::CacheOnly).unwrap(),info);
        let requests=f.state.lock().unwrap().requests.len();let cover=song.cover_art.as_deref().unwrap();
        let deadline=std::time::Instant::now()+crate::library::TEST_LIVENESS;
        while reopened.art.get(cover,150).is_none() && std::time::Instant::now()<deadline {std::thread::sleep(Duration::from_millis(1));}
        assert!(reopened.art.peek_cover(cover,150).is_some());assert_eq!(f.state.lock().unwrap().requests.len(),requests);
        reopened.remove_spoken_download(logical).unwrap();assert_eq!(crate::library::stored_files(&f.root.join("offline-art")).len(),0);
    }
    #[test]
    fn malformed_range_is_rejected_and_unavailable_tail_does_not_use_direct_transport() {
        crate::audio::callback::test_scope(|| {
        use crate::audio::player::TrackSource;
        let fixture = fixture();
        let books = fixture.lib.shelf_open("book", None).unwrap();
        fixture.state.lock().unwrap().bad_range = true;
        let mut opened = super::super::Tracks(fixture.lib.clone())
            .open(&crate::rt::Blocking::dedicated_thread(), &books[0], &Arc::new(crate::audio::interrupt::Interrupt::new(false)))
            .unwrap();
        assert!(opened.source.read(&mut [0; 16]).is_err());
        fixture.lib.shelf.write().unwrap().client = None;
        assert!(
            super::super::Tracks(fixture.lib.clone())
                .open(&crate::rt::Blocking::dedicated_thread(), &books[0], &Arc::new(crate::audio::interrupt::Interrupt::new(false)))
                .is_err()
        );
            })
    }
}

/// Explicit generated-server benchmark; never loads a saved configuration.
/// The explicit proxy argument is only for this generated fixture.
pub fn validate_spoken_server(
    blocking: &crate::rt::Blocking,
    directory: &std::path::Path,
    url: &str,
    token: &str,
    proxy: &str,
) -> Result<Value, String> {
    use crate::audio::player::TrackSource;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::AtomicBool;
    let paths = crate::config::Paths::new(directory.to_path_buf());
    let mut config = crate::config::Config::default();
    config.storage.reserve_mb = 1;
    let lib = Library::open(paths.clone(), &config, Arc::new(|_| {}));
    lib.shelf.write().unwrap().client = Some(Arc::new(
        Client::new(url, token, Some(proxy)).map_err(|e| e.to_string())?,
    ));
    let books = lib
        .shelf_items(false, Policy::NetworkFirst)
        .map_err(|e| e.to_string())?;
    let podcasts = lib
        .shelf_items(true, Policy::NetworkFirst)
        .map_err(|e| e.to_string())?;
    if books.is_empty() || podcasts.is_empty() {
        return Err("Generated server needs a book and podcast library".into());
    }
    let episodes = lib
        .shelf_episodes(&podcasts[0].0, Policy::NetworkFirst)
        .map_err(|e| e.to_string())?;
    if episodes.is_empty() {
        return Err("Generated server needs a downloaded podcast episode".into());
    }
    // Exercise the same paginated protocol used by ordinary spoken browsing.
    // Legacy shelf_items above omits query pagination and does not exercise
    // the ordinary browser's response shapes and episode projections.
    use super::shelf_browse::{EntryKind, Media, Section};
    let book_libraries = lib.shelf_libraries(Media::Book, Policy::NetworkOnly).map_err(|e| e.to_string())?;
    let podcast_libraries = lib.shelf_libraries(Media::Podcast, Policy::NetworkOnly).map_err(|e| e.to_string())?;
    let book_library = book_libraries.first().ok_or("Generated book library missing")?;
    let podcast_library = podcast_libraries.first().ok_or("Generated podcast library missing")?;
    let mut browser = Vec::new();
    for (library, media, section) in [
        (&book_library.id, Media::Book, Section::Books),
        (&podcast_library.id, Media::Podcast, Section::Books),
        (&podcast_library.id, Media::Podcast, Section::LatestEpisodes),
        (&podcast_library.id, Media::Podcast, Section::Episodes(podcasts[0].0.clone())),
    ] {
        let page = lib.shelf_browse(library, media, &section, 0, 50, Policy::NetworkOnly).map_err(|e| e.to_string())?;
        if page.entries.is_empty() || page.entries.len() > 50 { return Err("Generated browser page missing or unbounded".into()); }
        if !page.entries.iter().all(|entry| matches!((&section, media, &entry.kind),
            (Section::Books, Media::Book, EntryKind::Book { .. }) |
            (Section::Books, Media::Podcast, EntryKind::Podcast { .. }) |
            (Section::LatestEpisodes | Section::Episodes(_), Media::Podcast, EntryKind::Episode { .. }))) {
            return Err("Generated browser media identity mismatch".into());
        }
        browser.push(json!({"section":format!("{section:?}"),"media":media.as_str(),"entries":page.entries.len()}));
    }
    for section in [Section::ContinueListening, Section::RecentlyAdded, Section::NewestEpisodes, Section::ListenAgain] {
        let page = lib.shelf_browse(&podcast_library.id, Media::Podcast, &section, 0, 50, Policy::NetworkOnly).map_err(|e| format!("{section:?}: {e}"))?;
        browser.push(json!({"section":format!("{section:?}"),"media":"podcast","entries":page.entries.len(),"empty_allowed":true}));
    }
    let source = super::Tracks(lib.clone());
    let mut outcomes = Vec::new();
    for (item, episode) in [
        (&books[0].0, None),
        (&podcasts[0].0, Some(episodes[0].0.as_str())),
    ] {
        let info_target = lib
            .shelf_info_target(None, item.to_string(), episode.map(str::to_owned))
            .map_err(|e| e.to_string())?;
        let songs = lib.shelf_open(item, episode)?;
        let logical = songs[0]
            .spoken
            .as_ref()
            .ok_or("Missing spoken identity")?
            .id
            .clone();
        let mut remote_hashes = Vec::new();
        for song in &songs {
            let mut opened = source.open(blocking, song, &Arc::new(crate::audio::interrupt::Interrupt::new(false)))?;
            let mut hash = Md5::new();
            let mut bytes = [0; 32768];
            loop {
                let count = crate::audio::callback::with(blocking, || opened.source.read(&mut bytes)).map_err(|e| e.to_string())?;
                if count == 0 {
                    break;
                }
                hash.update(&bytes[..count]);
            }
            remote_hashes.push(format!("{:x}", hash.finalize()));
        }
        let duration = songs[0].spoken.as_ref().unwrap().duration_ms;
        let target = if songs.len() > 1 {
            let second = songs[1].spoken.as_ref().unwrap().offset_ms;
            second + (duration.saturating_sub(second) / 2).min(12345)
        } else {
            (duration / 2).min(42345)
        };
        let (index, position) = super::spoken::selection(&songs, 0, target);
        lib.save_spoken(&songs[index], position)?;
        if lib.sync_spoken() {
            return Err("Generated progress sync remained pending".into());
        }
        let persisted = lib
            .spoken_position(&logical)?
            .ok_or("Missing saved progress")?;
        if persisted.position_ms != target || persisted.dirty {
            return Err("Server progress differs from saved listening location".into());
        }
        let resume = source.spoken_resume(&songs, 0, 0)?;
        if resume != (index, position) {
            return Err("Resumed physical-file selection differs".into());
        }
        lib.shelf_download(item, episode)?;
        // No prior info read: the explicit download itself must populate it.
        let information=lib.book_info(&info_target,Policy::CacheOnly).map_err(|error|error.to_string())?;
        if lib.book_info(&info_target,Policy::NetworkOnly).map_err(|error|error.to_string())?!=information {return Err("Download metadata differs from server".into());}
        lib.set_offline_mode(true);
        if lib
            .book_info(&info_target, Policy::CacheOnly)
            .map_err(|e| e.to_string())?
            != information
        {
            return Err("Cached book information differs from server metadata".into());
        }
        let mut offline_hashes = Vec::new();
        for song in &songs {
            let mut opened = source.open(blocking, song, &Arc::new(crate::audio::interrupt::Interrupt::new(false)))?;
            if opened.origin != crate::audio::player::Origin::Offline {
                return Err("Download did not open offline".into());
            }
            crate::audio::callback::with(blocking, || opened.source.seek(SeekFrom::Start(0)))
                .map_err(|e| e.to_string())?;
            let mut hash = Md5::new();
            let mut bytes = [0; 32768];
            loop {
                let count = crate::audio::callback::with(blocking, || opened.source.read(&mut bytes)).map_err(|e| e.to_string())?;
                if count == 0 {
                    break;
                }
                hash.update(&bytes[..count]);
            }
            offline_hashes.push(format!("{:x}", hash.finalize()));
        }
        if remote_hashes != offline_hashes {
            return Err("Downloaded bytes differ from streamed bytes".into());
        }
        // A fresh Library/ArtCache and removed disposable cache exercise the
        // normal artwork loader using only durable ownership, with networking off.
        match std::fs::remove_dir_all(paths.art_cache()) {Ok(())=>{},Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},Err(_)=>return Err("Unable to clear generated art cache".into())}
        let mut reopened_config=config.clone();reopened_config.audiobookshelf.url=url.into();reopened_config.audiobookshelf.token=token.into();
        let reopened=Library::open(paths.clone(),&reopened_config,Arc::new(|_|{}));reopened.set_offline_mode(true);
        if reopened.book_info(&info_target,Policy::CacheOnly).map_err(|error|error.to_string())?!=information {return Err("Reopened metadata differs".into());}
        let cover=songs[0].cover_art.as_deref().ok_or("Generated fixture needs a cover identity")?;
        let mut offline_cover_sizes=Vec::new();
        for size in [50,150,320] {
            let deadline=std::time::Instant::now()+std::time::Duration::from_secs(3);
            loop {
                if let Some(image)=reopened.art.get(cover,size) {if image.width!=size || image.height!=size {return Err("Offline cover dimensions differ".into());}break;}
                if std::time::Instant::now()>=deadline {return Err("Generated cover did not survive offline reopen/cache removal".into());}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            offline_cover_sizes.push(size);
        }
        lib.set_offline_mode(false);
        outcomes.push(json!({"podcast":episode.is_some(),"physical_files":songs.len(),"chapters":songs[0].spoken.as_ref().unwrap().chapters.len(),
            "duration_ms":songs[0].spoken.as_ref().unwrap().duration_ms,"position_ms":persisted.position_ms,"file_md5":remote_hashes,"offline_matches":true,
            "book_info_cached_offline":true,"book_info":information,"offline_cover_sizes":offline_cover_sizes,"reopened_offline":true}));
    }
    let mut book_browser = Vec::new();
    for section in [
        Section::ContinueListening,
        Section::ContinueSeries,
        Section::RecentlyAdded,
        Section::RecentSeries,
        Section::Discover,
        Section::ListenAgain,
        Section::Genres,
        Section::Series,
        Section::Collections,
        Section::Playlists,
        Section::Authors,
        Section::Narrators,
    ] {
        let page = lib
            .shelf_browse(
                &book_library.id,
                Media::Book,
                &section,
                0,
                50,
                Policy::NetworkOnly,
            )
            .map_err(|error| format!("Book {section:?}: {error}"))?;
        book_browser.push(json!({"section":format!("{section:?}"),"entries":page.entries.len()}));
    }
    Ok(
        json!({"books":books.len(),"podcasts":podcasts.len(),"episodes":episodes.len(),"browser":browser,"book_browser":book_browser,"outcomes":outcomes}),

    )
}

/// A local position older than this is refreshed from the server before play.
const REMOTE_POSITION_AFTER_MS: u64 = 2 * 60 * 60 * 1000;
/// How long opening a downloaded item waits for that refresh.
const REMOTE_POSITION_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// An item hovered again within this is not prefetched again.
const PREFETCH_AGAIN: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Lock-free memory of recently prefetched items: (key hash, boottime seconds).
/// Returns the age of a recent prefetch of `key`, or records `key` and returns None.
fn recently_prefetched(key: &str) -> Option<u64> {
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    static KEYS: [AtomicU32; 32] = [const { AtomicU32::new(0) }; 32];
    static TIMES: [AtomicU32; 32] = [const { AtomicU32::new(0) }; 32];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let hash = crate::ux::text_hash(key) | 1;
    let now = (crate::playtrace::boottime_us() / 1_000_000) as u32;
    for (k, t) in KEYS.iter().zip(&TIMES) {
        if k.load(Ordering::Relaxed) == hash {
            let age = u64::from(now.wrapping_sub(t.load(Ordering::Relaxed)));
            if age < PREFETCH_AGAIN.as_secs() { return Some(age); }
        }
    }
    let slot = NEXT.fetch_add(1, Ordering::Relaxed) % KEYS.len();
    KEYS[slot].store(hash, Ordering::Relaxed);
    TIMES[slot].store(now, Ordering::Relaxed);
    None
}
