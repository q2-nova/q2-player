//! Account-scoped ABS browsing. A page is at most50 projected rows; the
//! transport never constructs the entire catalog in memory.
use super::{LibError, Library, Policy};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[path = "shelf_browse_sync.rs"]
mod sync;
pub use sync::SyncProgress;
pub(crate) use sync::SyncRound;
pub use subsonic::audiobookshelf_browse::{
    Entry, EntryKind, Facts, LibraryChoice, Media, Paging, Section,
};
use subsonic::audiobookshelf_browse::Validated;
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrowsePage {
    pub entries: Vec<Entry>,
    pub total: Option<u64>,
    pub has_more: bool,
    pub paging: Paging,
    #[serde(skip)]
    pub generation: Option<Arc<()>>,
}

#[derive(Clone)]
pub struct LibraryChoices {
    pub namespace: String,
    pub media: Media,
    pub libraries: Vec<LibraryChoice>,
}
impl LibraryChoices {
    pub fn preferred(
        &self,
        saved: &crate::config::ShelfLibraryPreference,
    ) -> Result<LibraryChoice, &'static str> {
        if self.namespace.is_empty() || saved.namespace != self.namespace {
            return Err("Saved library belongs to another account");
        }
        self.libraries
            .iter()
            .find(|choice| choice.id == saved.id && choice.media == self.media)
            .cloned()
            .ok_or("Saved library is unavailable for this media type")
    }
    pub fn preference(&self, id: &str) -> Option<crate::config::ShelfLibraryPreference> {
        if self.namespace.is_empty() {
            return None;
        }
        let choice = self
            .libraries
            .iter()
            .find(|choice| choice.id == id && choice.media == self.media)?;
        Some(crate::config::ShelfLibraryPreference {
            namespace: self.namespace.clone(),
            id: choice.id.clone(),
            name: choice.name.clone(),
        })
    }
}
pub(super) fn fail(e: impl std::fmt::Display) -> LibError {
    LibError::Server(e.to_string())
}

/// Seed only generated ABS data through the real client/cache, once per process.
/// Later map resets copy those produced cache records; no menu rows are copied.
#[cfg(test)]
pub(crate) fn seed_hierarchy(
    lib: &Arc<Library>,
    config: &mut crate::config::Config,
) -> Result<(), String> {
    struct Seed {
        account: crate::config::Audiobookshelf,
        cache: Vec<(String, String)>,
    }
    static SEED: std::sync::OnceLock<Result<Seed, String>> = std::sync::OnceLock::new();
    let seed = SEED
        .get_or_init(|| {
            let fixture = tests::fixture();
            fixture.state.lock().unwrap().audit = true;
            for (library, media) in [
                ("a", Media::Book),
                ("b", Media::Book),
                ("p", Media::Podcast),
            ] {
                fixture
                    .lib
                    .shelf_libraries(media, Policy::NetworkOnly)
                    .map_err(|e| e.to_string())?;
                for section in [
                    Section::Books,
                    Section::ContinueListening,
                    Section::ContinueSeries,
                    Section::RecentlyAdded,
                    Section::RecentSeries,
                    Section::Discover,
                    Section::ListenAgain,
                    Section::NewestEpisodes,
                    Section::Genres,
                    Section::Series,
                    Section::Collections,
                    Section::Playlists,
                    Section::Authors,
                    Section::Narrators,
                    Section::ByGenre("Generated Genre".into()),
                    Section::BySeries(format!("{library}-series")),
                    Section::ByAuthor(format!("{library}-author")),
                    Section::ByNarrator("Generated Narrator".into()),
                    Section::Collection(format!("{library}-collection")),
                    Section::Playlist(format!("{library}-playlist")),
                ] {
                    fixture
                        .lib
                        .shelf_browse(library, media, &section, 0, 50, Policy::NetworkOnly)
                        .map_err(|e| format!("{section:?}: {e}"))?;
                }
                if media == Media::Podcast {
                    fixture
                        .lib
                        .shelf_browse(
                            library,
                            media,
                            &Section::LatestEpisodes,
                            0,
                            50,
                            Policy::NetworkOnly,
                        )
                        .map_err(|e| e.to_string())?;
                    fixture
                        .lib
                        .shelf_browse(
                            library,
                            media,
                            &Section::Episodes("p-0".into()),
                            0,
                            50,
                            Policy::NetworkOnly,
                        )
                        .map_err(|e| e.to_string())?;
                }
                let target = fixture
                    .lib
                    .shelf_info_target(Some(library.into()), format!("{library}-0"), None)
                    .map_err(|e| e.to_string())?;
                fixture
                    .lib
                    .book_info(&target, Policy::NetworkOnly)
                    .map_err(|e| e.to_string())?;
            }
            let episode = fixture.lib.shelf_info_target(Some("p".into()), "p-0".into(), Some("generated-episode".into()))
                .map_err(|error| error.to_string())?;
            fixture.lib.book_info(&episode, Policy::NetworkOnly).map_err(|error| error.to_string())?;
            let cache = fixture
                .lib
                .db()
                .audit_spoken_responses(&crate::rt::Blocking::dedicated_thread())
                .map_err(|e| e.to_string())?;
            Ok(Seed {
                account: fixture.config.audiobookshelf.clone(),
                cache,
            })
        })
        .as_ref()
        .map_err(Clone::clone)?;
    for (key, body) in &seed.cache {
        lib.db()
            .put_spoken_response(&crate::rt::Blocking::dedicated_thread(), key, body)
            .map_err(|e| e.to_string())?;
    }
    // The map fixture copies generated persistent records into an already
    // constructed Library; refresh its construction-time namespace snapshot too.
    let namespaces = lib.db().shelf_namespaces(&crate::rt::Blocking::dedicated_thread()).map_err(|e| e.to_string())?;
    lib.shelf.write().unwrap().namespaces = Some(namespaces);
    config.audiobookshelf = seed.account.clone();
    lib.configure(config);
    lib.set_offline_mode(true);
    let namespace = lib.shelf_namespace();
    let blocking = crate::rt::Blocking::dedicated_thread();
    let db = lib.db();
    db.observe_spoken_remote(&blocking, &format!("abs:{namespace}:a-0:"), 84_000, 1, false, Some(35))
        .map_err(|error| error.to_string())?;
    db.observe_spoken_remote(&blocking, &format!("abs:{namespace}:p-0:generated-episode"), 0, 1, false, Some(100))
        .map_err(|error| error.to_string())?;
    Ok(())
}
impl Library {
    pub fn shelf_namespace_current(&self, namespace: &str) -> bool {
        !namespace.is_empty() && self.shelf.read().unwrap().namespace == namespace
    }
    /// Compatibility entry points may wait only outside executor/database jobs.
    pub fn shelf_library_choices(&self, media: Media, policy: Policy) -> Result<LibraryChoices, LibError> {
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        wait(async move { lib.shelf_library_choices_async(media, policy).await })
    }
    pub async fn shelf_library_choices_async(self: &Arc<Self>, media: Media, policy: Policy) -> Result<LibraryChoices, LibError> {
        let (libraries, generation) = self.browse_library_data(media, policy).await?;
        let lib = self.clone();
        crate::rt::spawn_blocking(move |_| {
        let slot = lib.shelf.read().unwrap();
        if !Arc::ptr_eq(&slot.generation, &generation) || slot.namespace.is_empty() {
            return Err(fail("Audiobookshelf changed; choose a library again"));
        }
        if libraries.len() > 1024
            || libraries.iter().any(|choice| {
                choice.media != media
                    || choice.id.is_empty()
                    || choice.id.len() > 4096
                    || choice.name.len() > 4096
            })
        {
            return Err(fail("Unsupported Audiobookshelf library list"));
        }
        let mut ids = std::collections::HashSet::new();
        if libraries.iter().any(|choice| !ids.insert(&choice.id)) {
            return Err(fail("Duplicate Audiobookshelf library identity"));
        }
        Ok(LibraryChoices {
            namespace: slot.namespace.clone(),
            media,
            libraries,
        })
        }).await.map_err(fail)?
    }
    pub fn shelf_browse_current(&self, page: &BrowsePage) -> bool {
        page.generation
            .as_ref()
            .is_some_and(|g| self.shelf_generation_current(g))
    }
    pub fn shelf_libraries(&self, media: Media, policy: Policy) -> Result<Vec<LibraryChoice>, LibError> {
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        wait(async move { lib.shelf_libraries_async(media, policy).await })
    }
    pub async fn shelf_libraries_async(self: &Arc<Self>, media: Media, policy: Policy) -> Result<Vec<LibraryChoice>, LibError> {
        self.browse_library_data(media, policy).await.map(|(data, _)| data)
    }
    async fn browse_library_data(self: &Arc<Self>, media: Media, policy: Policy) -> Result<(Vec<LibraryChoice>, Arc<()>), LibError> {
        self.browse_cached_async(&format!("libraries:{media:?}"), policy, false, |client, _| async move {
            client.browse_libraries_async(media).await.map(|v| Validated::Changed(v, None)).map_err(fail)
        }).await
    }
    pub fn shelf_browse(
        &self, library: &str, media: Media, section: &Section, page: u32, limit: u32, policy: Policy,
    ) -> Result<BrowsePage, LibError> {
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        let library = library.to_owned();
        let section = section.clone();
        wait(async move { lib.shelf_browse_async(&library, media, &section, page, limit, policy).await })
    }
    pub async fn shelf_browse_async(
        self: &Arc<Self>, library: &str, media: Media, section: &Section, page: u32, limit: u32, policy: Policy,
    ) -> Result<BrowsePage, LibError> {
        if policy != Policy::NetworkOnly {
            if let Section::Search(term) = section {
                if let Some(result) = self.shelf_synced_search(library, media, term, page, limit).await? { return Ok(result); }
            }
        }
        self.shelf_browse_cached(library, media, section, page, limit, policy, false).await
    }
    async fn shelf_browse_cached(
        self: &Arc<Self>, library: &str, media: Media, section: &Section, page: u32, limit: u32, policy: Policy, durable: bool,
    ) -> Result<BrowsePage, LibError> {
        if limit == 0 || limit > 50 { return Err(fail("Unsupported browse page size")); }
        let key = page_identity(library, media, section, page, limit)?;
        let (data, generation) = self.browse_cached_async(&key, policy, durable, |client, etag| async move {
            client.browse_page_validated_async(library, media, section, page, limit, etag.as_deref()).await.map_err(fail)
        }).await?;
        let lib = self.clone();
        crate::rt::spawn_blocking(move |_| {
            let slot = lib.shelf.read().unwrap();
            if !Arc::ptr_eq(&slot.generation, &generation) {
                return Err(fail(subsonic::audiobookshelf::Error::Changed));
            }
            let namespace = &slot.namespace;
            let entries = data.entries.into_iter().map(|mut entry| {
                entry.art = entry.art.map(|item| format!("shelf-book-cover:{namespace}:{item}"));
                entry
            }).collect();
            Ok(BrowsePage { entries, total: data.total, has_more: data.has_more,
                paging: data.paging, generation: Some(generation) })
        }).await.map_err(fail)?
    }
    /// Short shelf snapshots and serialization use finite workers; SQLite is
    /// awaited on its owner. Network waits hold neither guard nor worker. Every cache fallback
    /// and publication uses the original generation, including route changes.
    ///
    /// A stored page is revalidated with its ETag: an unchanged page costs a
    /// 304 with no body and is neither parsed from the network nor written.
    /// `strict` (the library sync): a failed write is the result, not a log
    /// line. Pages are written with the connection's NORMAL synchronization;
    /// the sync's FULL completion record makes them durable with it (WAL
    /// commits survive a power cut in order, so a lost page is never behind
    /// a kept completion).
    async fn browse_cached_async<T, F>(
        self: &Arc<Self>, key: &str, policy: Policy, strict: bool,
        fetch: impl Fn(Arc<subsonic::audiobookshelf::Client>, Option<String>) -> F + Send,
    ) -> Result<(T, Arc<()>), LibError>
    where T: Serialize + serde::de::DeserializeOwned + Send + 'static,
          F: std::future::Future<Output = Result<Validated<T>, LibError>> + Send,
    {
        let lib = self.clone();
        let (generation, namespace) = crate::rt::spawn_blocking(move |_| {
            let slot = lib.shelf.read().unwrap();
            (slot.generation.clone(), slot.namespace.clone())
        }).await.map_err(fail)?;
        let suffix = identity_suffix(key);
        let cached = || {
            let lib = self.clone();
            let generation = generation.clone();
            let namespace = namespace.clone();
            let suffix = suffix.clone();
            async move {
                if namespace.is_empty() { return Err(LibError::Offline); }
                let body = lib.shelf_db_run(generation.clone(), move |blocking, db| {
                    db.spoken_response(blocking, &format!("shelf-browse:{namespace}:{suffix}"))
                        .map_err(fail)?.ok_or(LibError::Offline)
                }).await?;
                let data = crate::rt::spawn_blocking(move |_| serde_json::from_str::<T>(&body).map_err(fail))
                    .await.map_err(fail)??;
                lib.shelf_result(generation, data).await
            }
        };
        if self.offline_mode() && matches!(policy, Policy::NetworkOnly) { return Err(LibError::Offline); }
        if matches!(policy, Policy::CacheOnly) || self.offline_mode() {
            return cached().await.map(|data| (data, generation));
        }
        if matches!(policy, Policy::PreferCache) {
            if let Ok(data) = cached().await { return Ok((data, generation)); }
        }
        let fresh = async {
            let client = self.shelf_client_async().await?;
            let account = client.namespace_async().await.map_err(fail)?;
            let page_key = format!("shelf-browse:{account}:{suffix}");
            // Only a copy of this account's page may answer a 304.
            let etag = if account == namespace && !self.shelf_auto.unconditional.load(std::sync::atomic::Ordering::Acquire) {
                let page = page_key.clone();
                self.shelf_db_run(generation.clone(), move |blocking, db| Ok(db.spoken_page_etag(blocking, &page).ok().flatten())).await?
            } else { None };
            let validated = fetch(client.clone(), etag).await?;
            let counter = if matches!(validated, Validated::Unchanged) { &self.shelf_auto.pages_unchanged } else { &self.shelf_auto.pages_fetched };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let validated = match validated {
                // The stored copy is the server's: read it, write nothing.
                Validated::Unchanged if account == namespace => match cached().await {
                    Ok(data) => return Ok(data),
                    Err(_) => fetch(client.clone(), None).await?,
                },
                Validated::Unchanged => fetch(client.clone(), None).await?,
                changed => changed,
            };
            let Validated::Changed(data, tag) = validated else { return Err(fail("Unsupported Audiobookshelf response")) };
            let (data, body) = crate::rt::spawn_blocking(move |_| {
                let body = serde_json::to_string(&data).map_err(fail)?;
                Ok::<_, LibError>((data, body))
            }).await.map_err(fail)??;
            self.shelf_db_run(generation.clone(), move |blocking, db| {
                let written = db.put_spoken_page(blocking, &page_key, &body, tag.as_deref());
                if strict { written.map(drop).map_err(fail) }
                else {
                    if written.is_err() { log::warn!("Audiobookshelf browse metadata could not be cached"); }
                    Ok(())
                }
            }).await?;
            self.shelf_result(generation.clone(), data).await
        }.await;
        let data = if matches!(policy, Policy::NetworkFirst) {
            match fresh { Ok(data) => Ok(data), Err(error) => cached().await.or(Err(error)) }
        } else { fresh }?;
        Ok((data, generation))
    }

}

/// What a browse page's stored copy is keyed by (hashed into the key).
pub(super) fn page_identity(library: &str, media: Media, section: &Section, page: u32, limit: u32) -> Result<String, LibError> {
    serde_json::to_string(&(library, media, section, page, limit)).map_err(fail)
}
fn identity_suffix(identity: &str) -> String {
    use md5::{Digest, Md5};
    format!("{:x}", Md5::digest(identity.as_bytes()))
}
/// The stored page's key for account `namespace`.
pub(super) fn page_key(namespace: &str, identity: &str) -> String { format!("{}{}", page_prefix(namespace), identity_suffix(identity)) }
/// What every stored page key of account `namespace` starts with.
pub(super) fn page_prefix(namespace: &str) -> String { format!("shelf-browse:{namespace}:") }

pub(super) fn wait<T: Send + 'static>(future: impl std::future::Future<Output = Result<T, LibError>> + Send + 'static) -> Result<T, LibError> {
    assert!(crate::rt::may_wait_for_tasks(), "synchronous ABS call on executor or pool");
    assert!(!super::db_worker::guard_held() && !super::db_worker::in_job(), "synchronous ABS call holding database");
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::rt::io().spawn(async move { let _ = tx.send(future.await); });
    rx.blocking_recv().unwrap_or(Err(LibError::Offline))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_library_is_bound_to_account_media_and_current_library_identity() {
        let choices = LibraryChoices {
            namespace: "account-a".into(),
            media: Media::Book,
            libraries: vec![
                LibraryChoice {
                    id: "a".into(),
                    name: "Current Books".into(),
                    media: Media::Book,
                },
                LibraryChoice {
                    id: "p".into(),
                    name: "Podcasts".into(),
                    media: Media::Podcast,
                },
            ],
        };
        let saved = choices.preference("a").unwrap();
        assert_eq!(choices.preferred(&saved).unwrap().name, "Current Books");
        for i in 0..512 {
            let mut stale = saved.clone();
            if i % 2 == 0 {
                stale.namespace = format!("other-account-{i}");
            } else {
                stale.id = format!("deleted-{i}");
            }
            assert!(choices.preferred(&stale).is_err());
        }
        assert!(choices.preference("p").is_none());
        assert!(choices.preference("").is_none());
        let mut wrong_media = saved.clone();
        wrong_media.id = "p".into();
        assert!(choices.preferred(&wrong_media).is_err());
        let mut renamed = saved;
        renamed.name = "Old Name".into();
        assert_eq!(choices.preferred(&renamed).unwrap().name, "Current Books");
    }
    use crate::config::{Config, Paths};
    use crate::library::book_info::BookInfoTarget;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };
    struct Block {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }
    #[derive(Default)]
    pub(super) struct State {
        requests: Vec<String>,
        block: Option<Block>,
        /// Block the request after this many (any path), not the first item page.
        block_at: Option<usize>,
        fail: bool,
        /// Cover requests answer 503.
        fail_covers: bool,
        pub(super) audit: bool,
        sync_covers: bool,
        podcast_episode_count: usize,
    }
    pub(super) struct Fixture {
        pub(super) lib: Arc<Library>,
        pub(super) config: Config,
        pub(super) state: Arc<Mutex<State>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
        root: std::path::PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn response(stream: &mut TcpStream, status: &str, value: serde_json::Value) {
        let body = value.to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    fn audit_episode() -> serde_json::Value {
        serde_json::json!({"id":"generated-episode", "libraryItemId":"p-0", "libraryId":"p",
            "title":"Generated Episode", "episode":"42", "season":"7", "audioFile":{"duration":12}})
    }
    fn audit_item(library: &str) -> serde_json::Value {
        let podcast = library == "p";
        let metadata = if podcast {
            serde_json::json!({"title":"Generated Show","author":"Generated Podcaster","description":"<p>Generated offline summary.</p>"})
        } else {
            serde_json::json!({"title":"Generated Book","authorName":"Generated Author",
                "authors":[{"id":format!("{library}-author"),"name":"Generated Author"}],
                "narrators":["Generated Narrator"],"series":[{"id":format!("{library}-series"),"name":"Generated Series","sequence":"1"}],
                "description":"<p>Generated offline summary.</p>","publishedYear":"2026"})
        };
        let media = if podcast {
            serde_json::json!({"metadata":metadata,"numEpisodes":1,"episodes":[audit_episode()]})
        } else {
            serde_json::json!({"metadata":metadata,"duration":240,"numTracks":1})
        };
        serde_json::json!({"id":format!("{library}-0"),"libraryId":library,"mediaType":if podcast {"podcast"} else {"book"},
            "media":media})
    }
    fn audit_response(path: &str, scope: &str, page: u64, limit: u64) -> serde_json::Value {
        let library = scope.split('-').next().unwrap_or("a");
        let item = audit_item(library);
        if path.starts_with("/api/items/") {
            return item;
        }
        if path.starts_with("/api/collections/") {
            return serde_json::json!({"libraryId":library,"books":[item]});
        }
        if path.starts_with("/api/playlists/") {
            let member = if library == "p" {
                serde_json::json!({"libraryItem":item,"episode":audit_episode()})
            } else {
                serde_json::json!({"libraryItem":item})
            };
            return serde_json::json!({"libraryId":library,"items":[member]});
        }
        if path.ends_with("/filterdata") {
            return serde_json::json!({"genres":["Generated Genre"]});
        }
        if path.ends_with("/narrators") {
            return serde_json::json!({"narrators":[{"id":"generated-narrator","name":"Generated Narrator","numBooks":1}]});
        }
        if path.ends_with("/recent-episodes") {
            let mut episode = audit_episode();
            episode["podcast"] = audit_item("p")["media"].clone();
            return serde_json::json!({"page":page,"limit":limit,"episodes":[episode]});
        }
        if path.ends_with("/personalized") {
            let mut item = item;
            if library == "p" {
                item["recentEpisode"] = audit_episode();
            }
            let mut shelves = Vec::new();
            for id in [
                "continue-listening",
                "discover",
                "listen-again",
                "newest-episodes",
            ] {
                shelves.push(serde_json::json!({"id":id,"total":1,"entities":[item]}));
            }
            shelves.push(serde_json::json!({"id":"continue-series","total":0,"entities":[]}));
            shelves.push(serde_json::json!({"id":"recent-series","total":0,"entities":[]}));
            return serde_json::json!(shelves);
        }
        let group = if path.ends_with("/series") {
            Some(("series", "Generated Series"))
        } else if path.ends_with("/collections") {
            Some(("collection", "Generated Collection"))
        } else if path.ends_with("/playlists") {
            Some(("playlist", "Generated Playlist"))
        } else if path.ends_with("/authors") {
            Some(("author", "Generated Author"))
        } else {
            None
        };
        let row=group.map(|(kind,name)|serde_json::json!({"id":format!("{library}-{kind}"),"name":name,"numBooks":1})).unwrap_or(item);
        serde_json::json!({"page":page,"limit":limit,"total":1,"results":[row]})
    }
    #[test]
    fn generated_hierarchy_cache_uses_real_parser_and_survives_server_shutdown() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../build/perf/hierarchy-cache-{}",
            fastrand::u64(..)
        ));
        let mut config = Config::default();
        let lib = Library::open(Paths::new(root), &config, Arc::new(|_| {}));
        seed_hierarchy(&lib, &mut config).unwrap();
        assert!(lib.offline_mode());
        let choices = lib
            .shelf_library_choices(Media::Book, Policy::CacheOnly)
            .unwrap();
        let preference = choices.preference("a").unwrap();
        assert_eq!(choices.preferred(&preference).unwrap().id, "a");
        let books = lib
            .shelf_browse("a", Media::Book, &Section::Books, 0, 50, Policy::CacheOnly)
            .unwrap();
        assert_eq!(books.entries[0].title, "Generated Book");
        let episodes = lib
            .shelf_browse(
                "p",
                Media::Podcast,
                &Section::Episodes("p-0".into()),
                0,
                50,
                Policy::CacheOnly,
            )
            .unwrap();
        assert!(
            matches!(&episodes.entries[0].kind,EntryKind::Episode {item,episode} if item=="p-0" && episode=="generated-episode")
        );
        let target = lib
            .shelf_info_target(Some("a".into()), "a-0".into(), None)
            .unwrap();
        let info = lib.book_info(&target, Policy::CacheOnly).unwrap();
        assert_eq!(info.title, "Generated Book");
        assert_eq!(info.authors, vec!["Generated Author"]);
    }
    pub(super) fn fixture() -> Fixture { fixture_with_auto(false) }
    fn fixture_with_auto(automatic: bool) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let root = super::super::generated_test_directory("browse-fault");
        let paths = Paths::new(root.clone());
        paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
        let mut config = Config::default();
        // This generated fixture runs on a bounded RAM filesystem, not a card.
        config.storage.reserve_mb = 2;
        config.audiobookshelf.url = base;
        config.audiobookshelf.token = "old".into();
        let state = Arc::new(Mutex::new(State { audit: automatic, sync_covers: automatic, ..State::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, state_worker) = (stop.clone(), state.clone());
        let worker = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let state = state_worker.clone();
                        thread::spawn(move || {
                            socket
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            let mut data = Vec::new();
                            while !data.ends_with(b"\r\n\r\n") {
                                let mut b = [0; 1];
                                if socket.read(&mut b).unwrap_or(0) == 0 {
                                    return;
                                }
                                data.push(b[0]);
                            }
                            let input = String::from_utf8(data).unwrap();
                            let target = input.split_whitespace().nth(1).unwrap().to_owned();
                            let token = input
                                .lines()
                                .find_map(|l| l.strip_prefix("Authorization: Bearer "))
                                .unwrap_or("")
                                .trim();
                            let block = {
                                let mut state = state.lock().unwrap();
                                state.requests.push(format!("{token}:{target}"));
                                if state.block_at.is_some_and(|at| state.requests.len() == at + 1) {
                                    state.block.take()
                                } else if state.block_at.is_none() && token == "old" && target.contains("/items?") {
                                    state.block.take()
                                } else {
                                    None
                                }
                            };
                            if let Some(block) = block {
                                let _ = block.entered.send(());
                                let _ = block.release.recv_timeout(Duration::from_secs(10));
                            }
                            if target == "/api/me" {
                                response(
                                    &mut socket,
                                    "200 OK",
                                    serde_json::json!({"id":format!("user-{token}")}),
                                );
                                return;
                            }
                            if target == "/api/libraries" {
                                response(
                                    &mut socket,
                                    "200 OK",
                                    serde_json::json!({"libraries":[{"id":"a","name":"A","mediaType":"book"},{"id":"b","name":"B","mediaType":"book"},{"id":"p","name":"P","mediaType":"podcast"}]}),
                                );
                                return;
                            }
                            if state.lock().unwrap().fail_covers && target.contains("/cover") {
                                response(&mut socket, "503 Unavailable", serde_json::json!({}));
                                return;
                            }
                            if state.lock().unwrap().sync_covers && target.contains("/cover") {
                                let png = crate::gfx::Image::placeholder(150).png().unwrap();
                                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", png.len());
                                let _ = socket.write_all(&png);
                                return;
                            }
                            if state.lock().unwrap().fail {
                                response(
                                    &mut socket,
                                    "503 Unavailable",
                                    serde_json::json!({"private":"must not enter error"}),
                                );
                                return;
                            }
                            let (path, query_text) =
                                target.split_once('?').unwrap_or((&target, ""));
                            let query = query_text
                                .split('&')
                                .filter_map(|v| v.split_once('='))
                                .collect::<std::collections::HashMap<_, _>>();
                            let page = query
                                .get("page")
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(0);
                            let limit = query
                                .get("limit")
                                .and_then(|v| v.parse::<u64>().ok())
                                .unwrap_or(2);
                            let lib = path.split('/').nth(3).unwrap_or("a");
                            let episode_count=state.lock().unwrap().podcast_episode_count;
                            if episode_count > 0 && path == "/api/items/p-0" {
                                let mut item=audit_item("p");
                                let episodes=(0..episode_count).map(|i|serde_json::json!({
                                    "id":format!("episode-{i:04}"),"title":format!("Generated Episode {i}"),
                                    "audioFile":{"duration":12}
                                })).collect::<Vec<_>>();
                                item["media"]["episodes"]=serde_json::json!(episodes);
                                response(&mut socket,"200 OK",item);
                                return;
                            }
                            if state.lock().unwrap().audit {
                                response(
                                    &mut socket,
                                    "200 OK",
                                    audit_response(path, lib, page, limit),
                                );
                                return;
                            }
                            response(
                                &mut socket,
                                "200 OK",
                                serde_json::json!({"page":page,"limit":limit,"total":100,"results":(0..limit).map(|n|serde_json::json!({"id":format!("{lib}-{}",page*limit+n),"libraryId":lib,"media":{"metadata":{"title":format!("{token} {lib} {}",page*limit+n)}}})).collect::<Vec<_>>()}),
                            );
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let lib = Library::open_with_auto_sync(paths, &config, Arc::new(|_| {}), automatic);
        Fixture {
            lib,
            config,
            state,
            stop,
            worker: Some(worker),
            root,
        }
    }
    fn block(f: &Fixture) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
        let (entered, seen) = mpsc::sync_channel(1);
        let (release, resume) = mpsc::sync_channel(1);
        f.state.lock().unwrap().block = Some(Block {
            entered,
            release: resume,
        });
        (seen, release)
    }
    #[test]
    fn page_cache_separates_libraries_art_is_namespaced_and_network_errors_remain_visible() {
        let f = fixture();
        assert_eq!(
            f.lib
                .shelf_libraries(Media::Book, Policy::NetworkOnly)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            f.lib
                .shelf_libraries(Media::Podcast, Policy::NetworkOnly)
                .unwrap()[0]
                .id,
            "p"
        );
        let a = f
            .lib
            .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly)
            .unwrap();
        let b = f
            .lib
            .shelf_browse("b", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly)
            .unwrap();
        assert_ne!(a.entries[0].id, b.entries[0].id);
        assert!(
            a.entries[0]
                .art
                .as_ref()
                .unwrap()
                .starts_with("shelf-book-cover:")
        );
        assert!(f.lib.shelf_browse_current(&a));
        f.state.lock().unwrap().fail = true;
        assert!(
            f.lib
                .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly)
                .is_err()
        );
        let cached = f
            .lib
            .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::CacheOnly)
            .unwrap();
        assert_eq!(cached.entries, a.entries);
        assert_eq!(f.lib.shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkFirst).unwrap().entries, a.entries);
        let before = f.state.lock().unwrap().requests.len();
        assert_eq!(f.lib.shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::PreferCache).unwrap().entries, a.entries);
        assert_eq!(f.state.lock().unwrap().requests.len(), before);
        assert_eq!(
            f.lib
                .shelf_browse("b", Media::Book, &Section::Books, 0, 2, Policy::CacheOnly)
                .unwrap()
                .entries,
            b.entries
        );
        f.lib.set_offline_mode(true);
        let requests = f.state.lock().unwrap().requests.len();
        assert!(
            f.lib
                .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly)
                .is_err()
        );
        assert!(
            f.lib
                .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::CacheOnly)
                .is_ok()
        );
        assert_eq!(f.state.lock().unwrap().requests.len(), requests);
    }
    #[test]
    fn stalled_async_browse_keeps_executor_and_cached_reads_live_and_can_cancel() {
        let f = fixture();
        let cached = f.lib.shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly).unwrap();
        let (seen, release) = block(&f);
        let lib = f.lib.clone();
        let pending = crate::rt::io().spawn(async move {
            lib.shelf_browse_async("a", Media::Book, &Section::Books, 1, 2, Policy::NetworkOnly).await
        });
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let lib = f.lib.clone();
        let (send, receive) = mpsc::sync_channel(1);
        crate::rt::io().spawn(async move {
            let result = lib.shelf_browse_async("a", Media::Book, &Section::Books, 0, 2, Policy::CacheOnly).await;
            let _ = send.send(result);
        });
        let ready = receive.recv_timeout(crate::library::TEST_LIVENESS).expect("runtime and cache pool stay live").unwrap();
        assert_eq!(ready.entries, cached.entries);
        pending.abort();
        wait(async move { assert!(pending.await.unwrap_err().is_cancelled()); Ok(()) }).unwrap();
        release.send(()).unwrap();
        assert!(f.lib.shelf_browse("a", Media::Book, &Section::Books, 1, 2, Policy::CacheOnly).is_err());
    }

    #[test]
    fn blocked_old_account_cannot_publish_or_replace_new_account_cache() {
        let f = fixture();
        let old = f
            .lib
            .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly)
            .unwrap();
        let generation = f.lib.shelf_generation();
        let (seen, release) = block(&f);
        let lib = f.lib.clone();
        let pending = thread::spawn(move || {
            lib.shelf_browse("a", Media::Book, &Section::Books, 1, 2, Policy::NetworkOnly)
        });
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let mut config = f.config.clone();
        config.audiobookshelf.token = "new".into();
        f.lib.configure(&config);
        assert!(!f.lib.shelf_generation_current(&generation));
        assert!(!f.lib.shelf_browse_current(&old));
        assert!(
            f.lib
                .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::CacheOnly)
                .is_err()
        );
        let new = f
            .lib
            .shelf_browse("a", Media::Book, &Section::Books, 1, 2, Policy::NetworkOnly)
            .unwrap();
        assert!(new.entries[0].title.starts_with("new "));
        release.send(()).unwrap();
        assert!(pending.join().unwrap().is_err());
        assert_eq!(
            f.lib
                .shelf_browse("a", Media::Book, &Section::Books, 1, 2, Policy::CacheOnly)
                .unwrap()
                .entries,
            new.entries
        );
        assert_ne!(old.entries[0].art, new.entries[0].art);
    }
    #[test]
    fn blocked_route_change_preserves_identity_cache_but_cancels_old_operation() {
        let f = fixture();
        let first = f
            .lib
            .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly)
            .unwrap();
        let (seen, release) = block(&f);
        let lib = f.lib.clone();
        let pending = thread::spawn(move || {
            lib.shelf_browse("a", Media::Book, &Section::Books, 1, 2, Policy::NetworkOnly)
        });
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let mut config = f.config.clone();
        config.tailscale.enabled = true;
        f.lib.configure(&config);
        assert!(!f.lib.shelf_browse_current(&first));
        release.send(()).unwrap();
        assert!(pending.join().unwrap().is_err());
        let cached = f
            .lib
            .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::CacheOnly)
            .unwrap();
        assert_eq!(cached.entries, first.entries);
        assert!(f.lib.shelf_browse_current(&cached));
        assert!(
            f.lib
                .shelf_browse("a", Media::Book, &Section::Books, 1, 2, Policy::CacheOnly)
                .is_err()
        );
        let requests = f.state.lock().unwrap().requests.len();
        assert!(
            f.lib
                .shelf_browse("a", Media::Book, &Section::Books, 0, 2, Policy::NetworkOnly)
                .is_err()
        );
        assert_eq!(f.state.lock().unwrap().requests.len(), requests);
    }
    #[test]
    fn explicit_sync_saves_group_pages_and_art_then_searches_all_local_books_without_network() {
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        let lib = fixture.lib.clone();
        let observations = Arc::new(Mutex::new(Vec::new())); let progress = observations.clone();
        let result = wait(async move { lib.sync_shelf_library("a".into(), Media::Book, move |update|progress.lock().unwrap().push(update)).await }).unwrap();
        assert!(result.complete); assert_eq!(result.entries, 1); assert_eq!(result.covers, 1); assert_eq!(result.unavailable_covers, 0);
        let updates = observations.lock().unwrap();
        assert_eq!(updates.iter().filter(|update|update.complete).count(), 1);
        assert!(updates[..updates.len()-1].iter().all(|update|!update.complete)); drop(updates);
        let requests = fixture.state.lock().unwrap().requests.len();
        fixture.lib.set_offline_mode(true);
        for section in [Section::Books, Section::Discover, Section::Collections, Section::Collection("a-collection".into()), Section::Playlist("a-playlist".into()), Section::ByAuthor("a-author".into())] {
            assert_eq!(fixture.lib.shelf_browse("a", Media::Book, &section, 0, 50, Policy::CacheOnly).unwrap().entries.len(), 1, "synced group {section:?}");
        }
        for term in ["generated", "GENERATED AUTHOR"] {
            let result = fixture.lib.shelf_browse("a", Media::Book, &Section::Search(term.into()), 0, 50, Policy::CacheOnly).unwrap();
            assert_eq!(result.entries.len(), 1); assert!(!result.has_more); assert_eq!(result.total, Some(1));
        }
        assert!(fixture.lib.shelf_browse("a", Media::Book, &Section::Search("absent".into()), 0, 50, Policy::CacheOnly).unwrap().entries.is_empty());
        assert_eq!(fixture.state.lock().unwrap().requests.len(), requests, "all browse/search reads are local");
        let page = fixture.lib.shelf_browse("a", Media::Book, &Section::Books, 0, 50, Policy::CacheOnly).unwrap();
        let id = page.entries[0].art.as_deref().unwrap();
        for size in [50, 150] {
            let bytes = fixture.lib.art.reopened_cover(&crate::rt::Blocking::dedicated_thread(), id, size).unwrap();
            let image = image::load_from_memory(&bytes).unwrap();
            assert_eq!((image.width(), image.height()), (size, size));
        }
    }
    #[test]
    fn podcast_server_growth_is_visible_after_stale_snapshot_refresh() {
        let fixture=fixture();
        { let mut state=fixture.state.lock().unwrap(); state.audit=true; state.podcast_episode_count=30; }
        let section=Section::Episodes("p-0".into());
        let first=fixture.lib.shelf_browse("p",Media::Podcast,&section,0,50,Policy::NetworkOnly).unwrap();
        assert_eq!(first.entries.len(),30); assert!(!first.has_more);
        fixture.state.lock().unwrap().podcast_episode_count=151;
        let stale=fixture.lib.shelf_browse("p",Media::Podcast,&section,0,50,Policy::CacheOnly).unwrap();
        assert_eq!(stale.entries.len(),30); assert!(!stale.has_more);
        let refreshed=fixture.lib.shelf_browse("p",Media::Podcast,&section,0,50,Policy::NetworkOnly).unwrap();
        assert_eq!(refreshed.entries.len(),50); assert!(refreshed.has_more);
        let last=fixture.lib.shelf_browse("p",Media::Podcast,&section,3,50,Policy::NetworkOnly).unwrap();
        assert_eq!(last.entries.len(),1); assert_eq!(last.entries[0].id,"episode-0150"); assert!(!last.has_more);
    }
    #[test]
    fn podcast_sync_reaches_and_retains_more_than_two_pages_offline() {
        let fixture = fixture();
        { let mut state=fixture.state.lock().unwrap(); state.audit=true; state.sync_covers=true; state.podcast_episode_count=151; }
        let lib=fixture.lib.clone();
        let result=wait(async move {lib.sync_shelf_library("p".into(),Media::Podcast,|_|{}).await}).unwrap();
        assert!(result.complete);
        let requests=fixture.state.lock().unwrap().requests.len();
        fixture.lib.set_offline_mode(true);
        let section=Section::Episodes("p-0".into());
        for page in [0,1,2,3,2,1,0] {
            let result=fixture.lib.shelf_browse("p",Media::Podcast,&section,page,50,Policy::CacheOnly).unwrap();
            let count=if page==3 {1} else {50};
            assert_eq!(result.entries.len(),count);
            assert_eq!(result.has_more,page<3);
            assert_eq!(result.entries[0].id,format!("episode-{:04}",page*50));
            assert_eq!(result.total,Some(151));
        }
        assert_eq!(fixture.state.lock().unwrap().requests.len(),requests);
    }
    #[test]
    fn book_info_never_fetched_comes_from_the_synced_lists() {
        // UXI-016: an item the listener never opened or hovered online.
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.podcast_episode_count = 60; }
        let target = |library: &str, item: &str, episode: Option<&str>| fixture.lib.shelf_info_target(Some(library.into()), item.into(), episode.map(str::to_owned)).unwrap();
        let synced = |target: BookInfoTarget| { let lib = fixture.lib.clone(); wait(async move { Ok(lib.synced_book_info_async(&target).await) }).unwrap() };
        // A browsed page is not a completed sync.
        fixture.lib.shelf_browse("a", Media::Book, &Section::Books, 0, 50, Policy::NetworkOnly).unwrap();
        assert_eq!(synced(target("a", "a-0", None)), None, "nothing synced yet");
        for (library, media) in [("a", Media::Book), ("p", Media::Podcast)] {
            let lib = fixture.lib.clone();
            assert!(wait(async move { lib.sync_shelf_library(library.into(), media, |_|{}).await }).unwrap().complete);
        }
        let requests = fixture.state.lock().unwrap().requests.len();
        fixture.lib.set_offline_mode(true);
        let book = fixture.lib.shelf_browse("a", Media::Book, &Section::Books, 0, 50, Policy::CacheOnly).unwrap().entries.remove(0);
        let info = synced(target("a", "a-0", None)).expect("a synced book");
        assert_eq!((info.title.as_str(), info.authors.clone()), (book.title.as_str(), vec![book.detail.clone()]));
        let episodes = fixture.lib.shelf_browse("p", Media::Podcast, &Section::Episodes("p-0".into()), 1, 50, Policy::CacheOnly).unwrap();
        let episode = &episodes.entries[5];
        let info = synced(target("p", "p-0", Some(&episode.id))).expect("a synced episode on its second page");
        assert_eq!((info.title.as_str(), info.subtitle.as_str()), (episode.title.as_str(), episode.detail.as_str()));
        assert!(synced(target("p", "p-0", None)).is_some(), "the show itself");
        assert_eq!(synced(target("a", "absent", None)), None);
        assert_eq!(synced(target("p", "p-0", Some("absent"))), None);
        // Another account's identifiers never match this account's rows.
        assert_eq!(synced(BookInfoTarget::Shelf { namespace: "generated-other".into(), library: Some("a".into()), item: "a-0".into(), episode: None }), None);
        assert_eq!(fixture.state.lock().unwrap().requests.len(), requests, "read from the card only");
    }
    #[test]
    fn sync_cache_and_art_write_failures_never_publish_a_complete_catalog() {
        for artwork in [false, true] {
            let fixture = fixture();
            { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
            if artwork {
                let dir = fixture.lib.paths.art_cache(); std::fs::remove_dir_all(&dir).unwrap(); std::fs::write(&dir, b"generated art directory obstruction").unwrap();
            } else {
                let db = rusqlite::Connection::open(fixture.lib.paths.database()).unwrap();
                db.execute_batch("CREATE TRIGGER reject_sync_page BEFORE INSERT ON spoken_responses WHEN NEW.key LIKE 'shelf-browse:%' BEGIN SELECT RAISE(ABORT,'generated cache write failure'); END;").unwrap();
            }
            let lib = fixture.lib.clone();
            let result = wait(async move {lib.sync_shelf_library("a".into(), Media::Book, |_|{}).await});
            assert!(result.is_err(), "failed persistence must stop sync");
            let lib = fixture.lib.clone();
            assert!(wait(async move {lib.shelf_synced_search("a", Media::Book, "generated", 0, 50).await}).unwrap().is_none(), "partial cache cannot admit complete local search");
        }
    }
    #[test]
    fn cancelling_a_real_blocked_sync_keeps_the_completion_marker_incomplete() {
        let fixture = fixture(); fixture.state.lock().unwrap().audit = true;
        let (entered, release) = block(&fixture);
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let lib = fixture.lib.clone();
        crate::rt::io().spawn(async move {
            tokio::select! { biased; _ = cancelled => {}, _ = lib.sync_shelf_library("a".into(), Media::Book, |_|{}) => panic!("blocked request unexpectedly completed"), }
            done.send(()).unwrap();
        });
        entered.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        cancel.send(()).unwrap(); finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); release.send(()).unwrap();
        let lib = fixture.lib.clone();
        assert!(wait(async move {lib.shelf_synced_search("a", Media::Book, "generated", 0, 50).await}).unwrap().is_none());
    }

    #[test]
    fn automatic_sync_populates_generated_book_cache_and_stays_fresh() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let fixture = fixture_with_auto(true);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while fixture.lib.shelf_sync_status() != super::super::shelf_auto_sync::Status::Current {
            assert!(std::time::Instant::now() < deadline, "automatic sync did not complete: {:?}", fixture.lib.shelf_sync_status());
            thread::sleep(Duration::from_millis(10));
        }
        fixture.lib.set_offline_mode(true);
        let page = fixture.lib.shelf_browse("a", Media::Book, &Section::Search("generated".into()), 0, 50, Policy::CacheOnly).unwrap();
        assert_eq!(page.entries.len(), 1);
        let episodes = fixture.lib.shelf_browse("p", Media::Podcast, &Section::LatestEpisodes, 0, 50, Policy::CacheOnly).unwrap();
        assert_eq!(episodes.entries.len(), 1, "podcast metadata remains available without the manual action");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    static AUTO_TEST: Mutex<()> = Mutex::new(());
    fn await_auto_status(lib: &Library, expected: super::super::shelf_auto_sync::Status) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while lib.shelf_sync_status() != expected {
            assert!(std::time::Instant::now() < deadline, "expected {expected:?}, got {:?}", lib.shelf_sync_status());
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn automatic_sync_reconnect_retries_failure_without_busy_polling() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; state.fail = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Unavailable);
        let count = fixture.state.lock().unwrap().requests.len();
        thread::sleep(Duration::from_millis(250));
        assert_eq!(fixture.state.lock().unwrap().requests.len(), count, "failure retry should back off");
        fixture.state.lock().unwrap().fail = false;
        fixture.lib.network_up();
        await_auto_status(&fixture.lib, Status::Current);
        // A reconnect does not refresh a current sync; an expired one is
        // refreshed after a restart, and its failure keeps the cached pages.
        let count = fixture.state.lock().unwrap().requests.len();
        fixture.lib.network_up();
        thread::sleep(Duration::from_millis(250));
        assert_eq!(fixture.state.lock().unwrap().requests.len(), count, "a reconnect must not refresh a current sync");
        fixture.state.lock().unwrap().fail = true;
        restart_with_stamp(&fixture, Some(crate::library::db::now() - 7 * 3600));
        await_auto_status(&fixture.lib, Status::Unavailable);
        fixture.lib.set_offline_mode(true);
        assert_eq!(fixture.lib.shelf_browse("a", Media::Book, &Section::Books, 0, 50, Policy::CacheOnly).unwrap().entries.len(), 1, "partial refresh keeps cached pages usable");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    #[test]
    fn automatic_sync_cancels_old_account_and_runs_for_replacement() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        let (entered, release) = block(&fixture);
        super::super::shelf_auto_sync::start(&fixture.lib);
        entered.recv_timeout(Duration::from_secs(15)).unwrap();
        let old = fixture.lib.shelf_generation();
        let mut config = fixture.config.clone(); config.audiobookshelf.token = "new-generated-token".into();
        let configure = fixture.lib.configure_async(&config);
        wait(async move { configure.await.map_err(fail) }).unwrap();
        assert!(!fixture.lib.shelf_generation_current(&old));
        await_auto_status(&fixture.lib, Status::Current);
        release.send(()).unwrap();
        assert!(fixture.state.lock().unwrap().requests.iter().any(|request| request.starts_with("new-generated-token:") && request.contains("/items?")));
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    #[test]
    fn automatic_sync_yields_to_playback_and_shutdown_cancels_blocked_request() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        let guard = fixture.lib.begin_spoken_playback();
        let (entered, release) = block(&fixture);
        super::super::shelf_auto_sync::start(&fixture.lib);
        assert!(entered.recv_timeout(Duration::from_millis(250)).is_err(), "background sync must wait for playback");
        drop(guard);
        entered.recv_timeout(Duration::from_secs(15)).unwrap();
        let guard = fixture.lib.begin_spoken_playback();
        await_auto_status(&fixture.lib, super::super::shelf_auto_sync::Status::Waiting);
        release.send(()).unwrap();
        drop(guard);
        await_auto_status(&fixture.lib, super::super::shelf_auto_sync::Status::Current);
        // A replacement account syncs at once; shutdown cancels its blocked request.
        let (entered, release) = block(&fixture);
        { let mut state = fixture.state.lock().unwrap(); state.block_at = Some(state.requests.len() + 2); }
        let mut config = fixture.config.clone(); config.audiobookshelf.token = "replacement".into();
        let configure = fixture.lib.configure_async(&config);
        wait(async move { configure.await.map_err(fail) }).unwrap();
        entered.recv_timeout(Duration::from_secs(15)).unwrap();
        let (done, finished) = mpsc::sync_channel(1);
        let lib = fixture.lib.clone();
        crate::rt::io().spawn(async move { lib.stop_shelf_auto_sync().await; let _ = done.send(()); });
        finished.recv_timeout(crate::library::TEST_LIVENESS).expect("shutdown must cancel the blocked request");
        let count = fixture.state.lock().unwrap().requests.len();
        release.send(()).unwrap();
        fixture.lib.network_up();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(fixture.state.lock().unwrap().requests.len(), count, "stopped worker must not restart");
    }

    /// Stops the automatic worker, sets the card's record of the last
    /// complete sync (`None`: none) and starts a new worker, as a restart does.
    fn restart_with_stamp(fixture: &Fixture, at: Option<i64>) {
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
        let namespace = fixture.lib.shelf.read().unwrap().namespace.clone();
        assert!(!namespace.is_empty(), "the sync verified the account");
        let body = serde_json::to_string(&at).unwrap();
        fixture.lib.db().put_spoken_response(&crate::rt::Blocking::dedicated_thread(), &format!("shelf-auto:{namespace}"), &body).unwrap();
        super::super::shelf_auto_sync::restart_for_test(&fixture.lib);
    }

    fn requests(fixture: &Fixture) -> usize { fixture.state.lock().unwrap().requests.len() }

    /// Requests one complete automatic sync of the generated libraries makes.
    fn full_sync_requests() -> usize {
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, super::super::shelf_auto_sync::Status::Current);
        let count = requests(&fixture);
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
        count
    }

    /// Budget (V0.128 device log: hundreds of sync documents a minute while
    /// paused): after a complete sync, a minute of cached playback whose
    /// fetch-ahead bursts wake the radio from a doze, each with a new
    /// address, fetches no Audiobookshelf document.
    #[test]
    fn automatic_sync_budget_no_documents_through_dozes_and_reconnects_once_current() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        let count = requests(&fixture);
        // Twelve bursts: radio on, an address, a burst, radio off again.
        for _ in 0..12 {
            fixture.lib.set_radio_doze(false);
            fixture.lib.network_up();
            thread::sleep(Duration::from_millis(40));
            let burst = fixture.lib.begin_spoken_playback();
            thread::sleep(Duration::from_millis(10));
            drop(burst);
            fixture.lib.set_radio_doze(true);
            thread::sleep(Duration::from_millis(10));
        }
        fixture.lib.set_radio_doze(false);
        thread::sleep(Duration::from_millis(500));
        let fetched = requests(&fixture) - count;
        eprintln!("automatic sync documents through 12 doze cycles after a complete sync: {fetched}");
        assert_eq!(fetched, 0, "documents fetched after a complete sync");
        assert_eq!(fixture.lib.shelf_sync_status(), Status::Current);
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    /// Budget: a restart within six hours of a complete sync fetches no
    /// Audiobookshelf document and reports the sync current; an expired
    /// record syncs again.
    #[test]
    fn automatic_sync_budget_restart_within_freshness_fetches_nothing() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        let full = requests(&fixture);
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
        super::super::shelf_auto_sync::restart_for_test(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        thread::sleep(Duration::from_millis(500));
        let fetched = requests(&fixture) - full;
        eprintln!("automatic sync documents after a restart within freshness: {fetched} (a full sync: {full})");
        assert_eq!(fetched, 0, "documents fetched after a restart within freshness");
        restart_with_stamp(&fixture, Some(crate::library::db::now() - 7 * 3600));
        await_auto_status(&fixture.lib, Status::Current);
        let expired = requests(&fixture) - full;
        eprintln!("automatic sync documents after freshness expired, library unchanged: {expired}");
        assert!(expired > 0 && expired < full / 2, "an expired round reads the markers, not the library: {expired}");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    /// Budget: a new client for the same account (Tailscale switched on or
    /// off changes the proxy, so the client and its generation are replaced)
    /// keeps a complete sync. Before, the worker looked for the record while
    /// the new client's namespace was still unknown and synced everything.
    #[test]
    fn automatic_sync_budget_a_new_client_for_the_same_account_fetches_nothing() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        let full = requests(&fixture);
        for _ in 0..3 {
            let generation = fixture.lib.invalidate_shelf_configuration();
            thread::sleep(Duration::from_millis(50));
            fixture.lib.publish_shelf_configuration(super::super::shelf::prepare_configuration(&fixture.config), &generation);
            fixture.lib.shelf_auto.wake.notify_one();
            thread::sleep(Duration::from_millis(300));
            await_auto_status(&fixture.lib, Status::Current);
        }
        thread::sleep(Duration::from_millis(300));
        let fetched = requests(&fixture) - full;
        eprintln!("automatic sync requests after three client replacements: {fetched} (a full sync: {full})");
        assert!(fetched <= 3, "at most one account check per new client: {fetched}");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    /// Product feedback, 2026-11-22: a full resync runs only when asked for on the
    /// connection page, or in the background while charging, at most once a
    /// day. A marker round (Sync Now, the six-hour freshness) reads a few
    /// documents.
    #[test]
    fn a_full_resync_runs_in_the_background_only_while_charging_and_once_per_interval() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        let first = requests(&fixture);
        let until = |count: usize| {
            let end = std::time::Instant::now() + Duration::from_secs(30);
            while requests(&fixture) < count && std::time::Instant::now() < end { thread::sleep(Duration::from_millis(10)); }
            thread::sleep(Duration::from_millis(100));
            await_auto_status(&fixture.lib, Status::Current);
        };
        thread::sleep(Duration::from_millis(300));
        assert_eq!(requests(&fixture), first, "on battery with the screen on: no full resync");
        fixture.lib.set_background_sync_charging(true);
        until(first + first / 2);
        let after_full = requests(&fixture);
        assert!(after_full - first >= first / 2, "charging: the full resync ran ({} documents)", after_full - first);
        fixture.lib.set_background_sync_charging(false);
        fixture.lib.set_background_sync_charging(true);
        thread::sleep(Duration::from_millis(400));
        assert_eq!(requests(&fixture), after_full, "charging again within the interval: nothing");
        fixture.lib.request_shelf_sync(true);
        until(after_full + first / 2);
        let asked = requests(&fixture);
        assert!(asked - after_full >= first / 2, "Full Resync asked for: it ran");
        fixture.lib.request_shelf_sync(false);
        until(asked + 1);
        let marker_round = requests(&fixture) - asked;
        assert!(marker_round > 0 && marker_round < first / 2, "Sync Now reads the markers: {marker_round}");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    /// The device measurement's `sync-now` file runs one sync despite a
    /// complete one within freshness (`abs-full`: a full resync), and is
    /// removed.
    #[test]
    fn a_measurement_trigger_syncs_once_despite_a_current_record() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        let full = requests(&fixture);
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
        let trigger = fixture.lib.paths.root.join("sync-now");
        std::fs::write(&trigger, "abs-full\n").unwrap();
        super::super::shelf_auto_sync::restart_for_test(&fixture.lib);
        let end = std::time::Instant::now() + Duration::from_secs(30);
        while requests(&fixture) < full + 10 && std::time::Instant::now() < end { thread::sleep(Duration::from_millis(10)); }
        await_auto_status(&fixture.lib, Status::Current);
        assert!(requests(&fixture) - full >= 10, "the trigger synced again");
        assert!(!trigger.exists(), "the trigger is used once");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    /// Budget: the heap is trimmed once per 500 stored rows and at the end,
    /// not after every section (V0.128 trimmed after each of thousands of
    /// one-author sections: 850 trims a minute).
    #[test]
    fn library_sync_trims_by_rows_not_by_section() {
        let fixture = fixture();
        fixture.state.lock().unwrap().audit = true;
        let round = SyncRound::default();
        let (lib, kept) = (fixture.lib.clone(), round.clone());
        assert!(wait(async move { lib.sync_shelf_library_from("a".into(), Media::Book, &kept, |_| {}).await }).unwrap().complete);
        let (sections, trims) = round.with(&(true, "a".into()), |cursor| (cursor.sections, cursor.trims));
        eprintln!("library sync: {sections} sections, {trims} trims");
        assert!(sections >= 10, "the generated library has many sections: {sections}");
        assert_eq!(trims, 1, "one trim for fewer than 500 rows");
    }

    /// Covers that failed in an interrupted attempt (the radio going down
    /// with it) are tried again when the sync resumes; the sync then ends
    /// current, not unavailable.
    #[test]
    fn automatic_sync_resume_retries_covers_the_interrupted_attempt_missed() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let full = full_sync_requests();
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; state.fail_covers = true; }
        let (entered, release) = block(&fixture);
        fixture.state.lock().unwrap().block_at = Some(full * 3 / 4);
        super::super::shelf_auto_sync::start(&fixture.lib);
        entered.recv_timeout(Duration::from_secs(15)).unwrap();
        let guard = fixture.lib.begin_spoken_playback();
        await_auto_status(&fixture.lib, Status::Waiting);
        fixture.state.lock().unwrap().fail_covers = false;
        release.send(()).unwrap();
        drop(guard);
        await_auto_status(&fixture.lib, Status::Current);
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    fn cover_requests(fixture: &Fixture) -> usize {
        fixture.state.lock().unwrap().requests.iter().filter(|request| request.contains("/cover")).count()
    }

    /// Budget: with the screen off on battery the automatic sync makes no
    /// request at all (V0.129 on the Q2: 30-100 KB/s for 15 minutes, paused
    /// and dark, after a restart); the screen or the charger starts it, and
    /// going dark again stops it at its next step, resumable.
    #[test]
    fn automatic_sync_budget_waits_for_the_screen_or_charger() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let full = full_sync_requests();
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        fixture.lib.set_background_sync_allowed(false);
        super::super::shelf_auto_sync::start(&fixture.lib);
        for _ in 0..3 { fixture.lib.network_up(); thread::sleep(Duration::from_millis(150)); }
        eprintln!("automatic sync documents in 450 ms dark on battery, through 3 reconnects: {}", requests(&fixture));
        assert_eq!(requests(&fixture), 0, "no request while dark on battery");
        assert_eq!(fixture.lib.shelf_sync_status(), Status::Waiting);
        // Lit part-way, dark again while a request is in flight.
        let (entered, release) = block(&fixture);
        fixture.state.lock().unwrap().block_at = Some(full * 3 / 4);
        fixture.lib.set_background_sync_allowed(true);
        entered.recv_timeout(Duration::from_secs(15)).unwrap();
        fixture.lib.set_background_sync_allowed(false);
        release.send(()).unwrap();
        await_auto_status(&fixture.lib, Status::Waiting);
        let stopped = requests(&fixture);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(requests(&fixture), stopped, "dark again: no further request");
        fixture.lib.set_background_sync_allowed(true);
        await_auto_status(&fixture.lib, Status::Current);
        let total = requests(&fixture);
        eprintln!("automatic sync documents, darkened at {}: {total} (uninterrupted: {full})", full * 3 / 4);
        assert!(total <= full + 8, "a darkened sync resumes: {total} against {full}");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    /// Budget: covers the server cannot provide do not make every restart,
    /// reconnect and backoff fetch the whole library again. The pages are
    /// complete and stamped; a restart within freshness fetches nothing.
    #[test]
    fn automatic_sync_budget_unavailable_covers_do_not_refetch_the_library() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; state.fail_covers = true; }
        super::super::shelf_auto_sync::start(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        let first = requests(&fixture);
        assert!(cover_requests(&fixture) > 0, "the covers were asked for");
        for _ in 0..3 { fixture.lib.network_up(); thread::sleep(Duration::from_millis(100)); }
        assert_eq!(requests(&fixture), first, "reconnects refetch nothing");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
        super::super::shelf_auto_sync::restart_for_test(&fixture.lib);
        await_auto_status(&fixture.lib, Status::Current);
        thread::sleep(Duration::from_millis(500));
        let fetched = requests(&fixture) - first;
        eprintln!("automatic sync documents after a restart, covers unavailable: {fetched} (first sync: {first})");
        assert_eq!(fetched, 0, "a restart within freshness refetches nothing");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

    /// Budget: a later sync finds the covers already on the card and makes
    /// no cover request, decode or rewrite.
    #[test]
    fn a_second_sync_requests_no_stored_cover_and_leaves_its_files() {
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        let lib = fixture.lib.clone();
        let first = wait(async move { lib.sync_shelf_library("a".into(), Media::Book, |_| {}).await }).unwrap();
        assert!(first.complete && first.covers > 0);
        let covers = cover_requests(&fixture);
        // Readers can update the SQLite WAL index. Its file timestamp cannot
        // distinguish a lookup from a write to the packed artwork.
        let pack = fixture.lib.paths.art_cache().join("artwork.sqlite3");
        let observer = rusqlite::Connection::open_with_flags(pack, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let version = || observer.pragma_query_value::<i64, _>(None, "data_version", |row| row.get(0)).unwrap();
        let before = version();
        let lib = fixture.lib.clone();
        let second = wait(async move { lib.sync_shelf_library("a".into(), Media::Book, |_| {}).await }).unwrap();
        assert!(second.complete);
        assert_eq!(second.covers, first.covers, "stored covers still count");
        assert_eq!(cover_requests(&fixture), covers, "no cover request for stored artwork");
        assert_eq!(version(), before, "a second sync must not commit to the artwork pack");
    }

    /// Budget: a sync interrupted by playback part-way resumes from the
    /// section it stopped in; it does not fetch the libraries again from
    /// their first page.
    #[test]
    fn automatic_sync_budget_interrupted_sync_resumes() {
        let _serial = AUTO_TEST.lock().unwrap_or_else(|e| e.into_inner());
        use super::super::shelf_auto_sync::Status;
        let full = full_sync_requests();
        let fixture = fixture();
        { let mut state = fixture.state.lock().unwrap(); state.audit = true; state.sync_covers = true; }
        let (entered, release) = block(&fixture);
        fixture.state.lock().unwrap().block_at = Some(full * 3 / 4);
        super::super::shelf_auto_sync::start(&fixture.lib);
        entered.recv_timeout(Duration::from_secs(15)).unwrap();
        let guard = fixture.lib.begin_spoken_playback();
        await_auto_status(&fixture.lib, Status::Waiting);
        release.send(()).unwrap();
        drop(guard);
        await_auto_status(&fixture.lib, Status::Current);
        let total = requests(&fixture);
        eprintln!("automatic sync documents, interrupted at {}: {total} (uninterrupted: {full})", full * 3 / 4);
        // The account check, the library lists and the interrupted section again.
        assert!(total <= full + 8, "an interrupted sync fetched {total} documents; uninterrupted {full}");
        let lib = fixture.lib.clone();
        wait(async move { lib.stop_shelf_auto_sync().await; Ok(()) }).unwrap();
    }

}
