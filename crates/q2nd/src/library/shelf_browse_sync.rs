//! Sequential offline metadata/art sync. Each successful page is durable
//! before progress advances. Cancellation retains useful completed work (and,
//! with a kept [`SyncCursor`], the position to resume from), but the
//! complete-catalog marker is written only at the end.
use super::*;
use std::collections::{HashSet, VecDeque};
use md5::Digest;

const SIZE: u32 = 50;
const MAX_PAGES: u32 = 10_000;
const MAX_GROUPS: usize = 100_000;
#[derive(Clone, Default, Debug)]
pub struct SyncProgress {
    pub entries: u64,
    pub covers: u64,
    pub unavailable_covers: u64,
    pub complete: bool,
}
#[derive(Serialize, Deserialize)]
struct Manifest { pages: u32 }
fn manifest_key(namespace: &str, library: &str, media: Media) -> String {
    format!("shelf-sync:{}", serde_json::to_string(&(namespace, library, media)).expect("sync identity"))
}
/// Entries stored between heap trims: a sync of many small sections (one
/// author or one show each) trims once per this many rows, not once per
/// section (the V0.128 device log showed 850 trims a minute).
const TRIM_ENTRIES: u64 = 500;
/// Latest-episode pages the marker round searches for newly updated shows.
const SHOW_DISCOVERY_PAGES: u32 = 4;
/// Reads of a section whose pages show the library changing under them.
const SECTION_ATTEMPTS: u32 = 3;

/// The personalized shelves a library's sync stores.
fn home_sections(media: Media) -> Vec<Section> {
    if media == Media::Book { vec![Section::ContinueListening, Section::ContinueSeries, Section::RecentSeries, Section::Discover] }
    else { vec![Section::ContinueListening, Section::ListenAgain] }
}
/// An entry id, as the duplicate check holds it (8 bytes, not the id).
fn id_digest(id: &str) -> u64 {
    let d = md5::Md5::digest(id.as_bytes());
    u64::from_le_bytes(d[..8].try_into().expect("an md5 digest"))
}

/// Where one library's sync has got to. The automatic sync keeps it across
/// interruptions (playback, a radio doze) and resumes from the first section
/// not yet stored, so an interrupted sync does not fetch the library again
/// from its first page. A section interrupted part-way is fetched again.
#[derive(Default)]
pub(crate) struct SyncCursor {
    started: bool,
    pending: VecDeque<Section>,
    groups: HashSet<String>,
    covers: HashSet<[u8; 16]>,
    /// Covers that could not be stored, by digest: tried again when an
    /// interrupted round resumes (the radio may have gone down with it),
    /// also for a library that had completed.
    missing: std::collections::HashMap<[u8; 16], String>,
    book_pages: u32,
    result: SyncProgress,
    untrimmed: u64,
    /// The personalized document is stored for this round.
    home_fetched: bool,
    /// Digests of the pages this round stored or found current: the others
    /// the library's previous syncs stored are removed at the end.
    kept: HashSet<[u8; 16]>,
    /// Stale pages removed when the library's sync completed.
    pub(crate) pruned: u64,
    /// The library's markers as this round found them.
    markers: Option<Markers>,
    /// The markers had not moved since the last complete sync: only the
    /// personalized shelves were read.
    unchanged: bool,
    /// Podcasts: the shows with episodes newer than the last sync's newest
    /// (`None`: not known, every show is read).
    new_episodes: Option<HashSet<String>>,
    /// Group kinds derived this round, and the groups that were.
    derived_kinds: HashSet<derive::Kind>,
    derived: HashSet<String>,
    /// Covers the server lacked at an earlier sync: asked for again only by a
    /// full resync.
    known_missing: HashSet<String>,
    /// Of those, the ones this round reached and did not ask for.
    skipped_missing: HashSet<String>,
    /// Requests and derived pages, for the tests and the measure log.
    pub(crate) derived_pages: u32,
    /// Sections stored and heap trims requested (the device log's count).
    pub(crate) sections: u32,
    pub(crate) trims: u32,
}

/// The cursors of one sync of every library, by media (books: `true`) and
/// library id. A library's sync takes its cursor and puts it back when it
/// ends or is dropped at an await, so an interruption keeps its place.
#[derive(Clone, Default)]
pub(crate) struct SyncRound {
    cursors: Arc<std::sync::Mutex<std::collections::HashMap<(bool, String), SyncCursor>>>,
    full: bool,
}
impl SyncRound {
    /// A full resync: every list, page and show is read again (with ETags)
    /// whatever the markers say, covers the server lacked are asked for
    /// again, and pages no longer reached are removed. Without it a round
    /// reads the markers and, when they moved, what can have changed.
    pub(crate) fn full() -> Self { Self { full: true, ..Self::default() } }
    pub(crate) fn is_full(&self) -> bool { self.full }
    pub(crate) fn with<R>(&self, key: &(bool, String), f: impl FnOnce(&mut SyncCursor) -> R) -> R {
        f(self.cursors.lock().unwrap().entry(key.clone()).or_default())
    }
}
/// What tells a library changed without reading it: its item count and its
/// newest item (`sort=addedAt`), and for podcasts the newest episode.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Markers {
    total: Option<u64>,
    newest: Option<String>,
    episode: Option<String>,
    /// Older podcast rounds could mark a four-page prefix complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    latest_complete: bool,
}
fn markers_key(scope: &str) -> String { format!("shelf-markers:{scope}") }
fn missing_key(scope: &str) -> String { format!("shelf-missing-covers:{scope}") }
#[path = "shelf_derive.rs"]
mod derive;
struct Held<'a> { round: &'a SyncRound, key: (bool, String), cursor: SyncCursor }
impl Drop for Held<'_> {
    fn drop(&mut self) {
        let cursor = std::mem::take(&mut self.cursor);
        self.round.with(&self.key, |slot| *slot = cursor);
    }
}

impl Library {
    pub async fn sync_shelf_library(self: &Arc<Self>, library: String, media: Media, progress: impl Fn(SyncProgress) + Send) -> Result<SyncProgress, LibError> {
        self.sync_shelf_library_from(library, media, &SyncRound::full(), progress).await
    }

    /// [`Library::sync_shelf_library`] from its cursor in `round`: a library
    /// whose sync completed in this round returns its result without a
    /// request, after trying again any covers it could not store.
    pub(crate) async fn sync_shelf_library_from(self: &Arc<Self>, library: String, media: Media, round: &SyncRound, progress: impl Fn(SyncProgress) + Send) -> Result<SyncProgress, LibError> {
        let key = (media == Media::Book, library.clone());
        let mut held = Held { round, key: key.clone(), cursor: round.with(&key, std::mem::take) };
        let c = &mut held.cursor;
        if c.result.complete && c.missing.is_empty() { return Ok(c.result.clone()); }
        let _sync = self.shelf_auto.sync_lock.lock().await;
        if self.offline_mode() { return Err(LibError::Offline); }
        if self.paths.ephemeral_media { return Err(fail("Insert a writable card to sync the library")); }
        let client = self.shelf_client_async().await?;
        let generation = self.shelf_admission(client.clone()).await?;
        let namespace = client.namespace_async().await.map_err(fail)?;
        let key = manifest_key(&namespace, &library, media);
        // A resumed sync retries the covers its interrupted attempt missed.
        let retry: Vec<([u8; 16], String)> = c.missing.iter().map(|(digest, art)| (*digest, art.clone())).collect();
        for (digest, art) in retry {
            if super::super::art::ArtCache::sync_cover(self.clone(), art, generation.clone()).await? {
                c.missing.remove(&digest);
                c.covers.insert(digest);
                c.result.covers += 1;
            }
        }
        c.result.unavailable_covers = c.missing.len() as u64;
        if c.result.complete { progress(c.result.clone()); return Ok(c.result.clone()); }
        if !c.started {
            // One small request per marker. Unmoved since a complete sync:
            // the round reads only the personalized shelves (listening
            // progress moves without a marker) and ends.
            let markers = self.shelf_markers(&client, &library, media).await?;
            let (previous, complete, known_missing) = {
                let (markers_key, missing_key, manifest) = (markers_key(&key), missing_key(&key), key.clone());
                self.shelf_db_run(generation.clone(), move |blocking, db| {
                    let previous = db.spoken_response(blocking, &markers_key).map_err(fail)?.and_then(|body| serde_json::from_str::<Markers>(&body).ok());
                    let complete = db.spoken_response(blocking, &manifest).map_err(fail)?.and_then(|body| serde_json::from_str::<Option<Manifest>>(&body).ok()).flatten();
                    let missing = db.spoken_response(blocking, &missing_key).map_err(fail)?.and_then(|body| serde_json::from_str::<Vec<String>>(&body).ok()).unwrap_or_default();
                    Ok((previous, complete, missing))
                }).await?
            };
            c.known_missing = known_missing.into_iter().collect();
            if let Some(manifest) = &complete { c.book_pages = manifest.pages; }
            if !round.full && complete.is_some() && previous.as_ref() == Some(&markers) {
                self.sync_home_pages(&library, media, &home_sections(media), &generation).await?;
                c.home_fetched = true;
                c.unchanged = true;
            } else {
                if !round.full && media == Media::Podcast {
                    if let Some(newest) = previous.as_ref().and_then(|p| p.episode.clone()) {
                        c.new_episodes = self.shows_with_new_episodes(&client, &library, &newest).await?;
                    }
                }
                let invalid = key.clone();
                self.shelf_db_run(generation.clone(), move |blocking, db| db.put_spoken_response_durable(blocking, &invalid, "null").map_err(fail)).await?;
                c.pending = VecDeque::from(vec![Section::Books, Section::ContinueListening, Section::RecentlyAdded, Section::Playlists]);
                if media == Media::Book {
                    c.pending.extend([Section::ContinueSeries, Section::RecentSeries, Section::Discover, Section::Genres, Section::Series, Section::Collections, Section::Authors, Section::Narrators]);
                } else { c.pending.extend([Section::ListenAgain, Section::LatestEpisodes]); }
            }
            c.markers = Some(markers);
            c.started = true;
        }
        // The cursor changes only between sections (and for each stored
        // cover), so wherever this is dropped it names the work left.
        while let Some(section) = c.pending.front().cloned() {
            let identity = serde_json::to_string(&section).map_err(fail)?;
            if c.groups.contains(&identity) { c.pending.pop_front(); continue; }
            if c.groups.len() >= MAX_GROUPS { return Err(fail("Library sync exceeds the supported group count")); }
            // Series, author, narrator and genre pages from the stored books.
            if let Some(kind) = derive::Kind::of(&section) {
                if !c.derived_kinds.contains(&kind) {
                    self.derive_groups(&library, media, kind, &namespace, &generation, c).await?;
                    c.derived_kinds.insert(kind);
                }
                if c.derived.contains(&identity) { c.pending.pop_front(); c.groups.insert(identity); c.sections += 1; continue; }
            }
            // A show whose newest episode is not newer than the last sync's
            // keeps its stored episodes until a full resync.
            if let Section::Episodes(show) = &section {
                if !round.full && c.new_episodes.as_ref().is_some_and(|shows| !shows.contains(show))
                    && self.stored_page(&namespace, &library, media, &section, 0, &generation).await?.is_some() {
                    c.pending.pop_front(); c.groups.insert(identity);
                    continue;
                }
            }
            let mut attempt = 0;
            let (discovered, entries, pages) = loop {
                attempt += 1;
                let mut signatures = HashSet::new();
                // A server-paged section's entry ids and total: a total that
                // moves between pages, or an id seen twice, means the library
                // changed under the offsets (an entry was then missed).
                let (mut ids, mut total, mut changed) = (HashSet::new(), None, false);
                let (mut discovered, mut entries, mut pages) = (Vec::new(), 0u64, 0u32);
                for page in 0..MAX_PAGES {
                    self.shelf_result(generation.clone(), ()).await?;
                    let rows = if section.is_home() {
                        // All of the library's shelves come from one document.
                        if !c.home_fetched {
                            self.sync_home_pages(&library, media, &home_sections(media), &generation).await?;
                            c.home_fetched = true;
                        }
                        self.shelf_browse_cached(&library, media, &section, page, SIZE, Policy::CacheOnly, false).await?
                    } else {
                        self.shelf_browse_cached(&library, media, &section, page, SIZE, Policy::NetworkOnly, true).await?
                    };
                    let page_id = page_identity(&library, media, &section, page, SIZE)?;
                    c.kept.insert(md5::Md5::digest(page_id.as_bytes()).into());
                    let signature = md5::Md5::digest(serde_json::to_vec(&rows.entries.iter().map(|entry| &entry.id).collect::<Vec<_>>()).map_err(fail)?);
                    if !rows.entries.is_empty() && !signatures.insert(signature) { return Err(fail("The server repeated a page during library sync")); }
                    if rows.paging == Paging::Server {
                        if page > 0 && total != rows.total { changed = true; }
                        total = rows.total;
                        for entry in &rows.entries { if !ids.insert(id_digest(&entry.id)) { changed = true; } }
                    }
                    pages = page + 1;
                    entries += rows.entries.len() as u64;
                    for entry in rows.entries {
                        match entry.kind {
                            EntryKind::Group {section} => discovered.push(section),
                            EntryKind::Podcast {item} => discovered.push(Section::Episodes(item)),
                            _ => {},
                        }
                        if let Some(art) = entry.art {
                            let digest: [u8; 16] = md5::Md5::digest(art.as_bytes()).into();
                            if c.covers.contains(&digest) || c.missing.contains_key(&digest) { continue; }
                            // The server lacked it before: only a full resync asks again.
                            if !round.full && c.known_missing.contains(&art) { c.skipped_missing.insert(art); continue; }
                            if c.covers.len() + c.missing.len() >= MAX_GROUPS { return Err(fail("Library sync exceeds the supported cover count")); }
                            let stored = super::super::art::ArtCache::sync_cover(self.clone(), art.clone(), generation.clone()).await?;
                            if stored { c.covers.insert(digest); c.result.covers += 1; }
                            else { c.missing.insert(digest, art); c.result.unavailable_covers += 1; }
                        }
                    }
                    let mut snapshot = c.result.clone();
                    if section == Section::Books { snapshot.entries += entries; }
                    progress(snapshot);
                    // Latest Episodes has offset pages: an insertion moves the
                    // tail too, and a full last page may need an empty successor.
                    if !rows.has_more || rows.paging == Paging::Preview { break; }
                    if page + 1 == MAX_PAGES { return Err(fail("Library sync exceeds the supported page count")); }
                }
                if !changed { break (discovered, entries, pages); }
                if attempt == SECTION_ATTEMPTS {
                    // A server whose order is not stable between requests:
                    // keep what it gave, as before this check.
                    log::warn!("library sync: a section changed during each of {SECTION_ATTEMPTS} reads; kept the last");
                    break (discovered, entries, pages);
                }
                log::info!("library sync: the library changed during a section; reading it again");
            };
            // The section's pages and covers are stored: it is done.
            c.pending.pop_front();
            c.groups.insert(identity);
            c.pending.extend(discovered);
            if section == Section::Books { c.book_pages = pages; c.result.entries += entries; }
            c.sections += 1;
            c.untrimmed += entries;
            // Return the stored rows' heap, once per TRIM_ENTRIES rows.
            if c.untrimmed >= TRIM_ENTRIES {
                c.untrimmed = 0;
                c.trims += 1;
                crate::platform::resident::trim_after_sync();
            }
        }
        // Pages a previous sync stored that this one did not reach: a removed
        // show's episodes, a renamed series, pages past a shortened list. Only
        // a full resync reads everything, so only it may remove.
        if round.full {
            let mut kept: Vec<String> = std::mem::take(&mut c.kept).iter().map(|d| d.iter().map(|b| format!("{b:02x}")).collect()).collect();
            kept.sort_unstable();
            let (scope, prefix) = (key.clone(), page_prefix(&namespace));
            let pruned = self.shelf_db_run(generation.clone(), move |blocking, db| {
                db.prune_synced_pages(blocking, &scope, &prefix, &kept).map_err(fail)
            }).await?;
            c.pruned += pruned as u64;
        }
        let body = serde_json::to_string(&Manifest {pages: c.book_pages}).map_err(fail)?;
        let markers = serde_json::to_string(&c.markers).map_err(fail)?;
        let mut missing: Vec<String> = c.missing.values().chain(&c.skipped_missing).cloned().collect();
        missing.sort_unstable();
        missing.dedup();
        let missing = serde_json::to_string(&missing).map_err(fail)?;
        let (markers_key, missing_key, unchanged) = (markers_key(&key), missing_key(&key), c.unchanged);
        self.shelf_db_run(generation.clone(), move |blocking, db| {
            db.put_spoken_response(blocking, &markers_key, &markers).map_err(fail)?;
            db.put_spoken_response(blocking, &missing_key, &missing).map_err(fail)?;
            // Unchanged rows write nothing; the FULL write makes the pages
            // stored before it durable.
            if unchanged { Ok(()) } else { db.put_spoken_response_durable(blocking, &key, &body).map_err(fail) }
        }).await?;
        self.shelf_result(generation, ()).await?;
        c.result.complete = true;
        if std::mem::take(&mut c.untrimmed) > 0 {
            c.trims += 1;
            crate::platform::resident::trim_after_sync();
        }
        progress(c.result.clone());
        Ok(c.result.clone())
    }

    /// The library's markers: its newest item and item count, and for a
    /// podcast library its newest episode. Two requests at most.
    async fn shelf_markers(self: &Arc<Self>, client: &Arc<subsonic::audiobookshelf::Client>, library: &str, media: Media) -> Result<Markers, LibError> {
        let newest = client.newest_items_async(library, media, 1).await.map_err(fail)?;
        let episode = if media == Media::Podcast {
            match client.browse_page_validated_async(library, media, &Section::LatestEpisodes, 0, 1, None).await.map_err(fail)? {
                Validated::Changed(page, _) => page.entries.first().map(|e| e.id.clone()),
                Validated::Unchanged => None,
            }
        } else { None };
        self.shelf_auto.pages_fetched.fetch_add(1 + u32::from(media == Media::Podcast), std::sync::atomic::Ordering::Relaxed);
        Ok(Markers {
            total: newest.total,
            newest: newest.entries.first().map(|e| e.id.clone()),
            episode,
            latest_complete: media == Media::Podcast,
        })
    }

    /// The shows with episodes published since `newest` (the last sync's
    /// newest episode), from the latest episodes, at most four pages; `None`
    /// when `newest` is not among them (every show is read).
    async fn shows_with_new_episodes(self: &Arc<Self>, client: &Arc<subsonic::audiobookshelf::Client>, library: &str, newest: &str) -> Result<Option<HashSet<String>>, LibError> {
        let mut shows = HashSet::new();
        for page in 0..SHOW_DISCOVERY_PAGES {
            let Validated::Changed(rows, _) = client.browse_page_validated_async(library, Media::Podcast, &Section::LatestEpisodes, page, SIZE, None).await.map_err(fail)?
                else { return Ok(None) };
            self.shelf_auto.pages_fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            for entry in &rows.entries {
                if entry.id == newest { return Ok(Some(shows)); }
                if let EntryKind::Episode { item, .. } = &entry.kind { shows.insert(item.clone()); }
            }
            if !rows.has_more { break; }
        }
        Ok(None)
    }

    /// A stored page as the server sent it (art not yet namespaced).
    pub(crate) async fn stored_page(self: &Arc<Self>, namespace: &str, library: &str, media: Media, section: &Section, page: u32, generation: &Arc<()>) -> Result<Option<subsonic::audiobookshelf_browse::Page>, LibError> {
        let key = page_key(namespace, &page_identity(library, media, section, page, SIZE)?);
        let body = self.shelf_db_run(generation.clone(), move |blocking, db| db.spoken_response(blocking, &key).map_err(fail)).await?;
        let Some(body) = body else { return Ok(None) };
        crate::rt::spawn_blocking(move |_| serde_json::from_str(&body).map(Some).map_err(fail)).await.map_err(fail)?
    }

    /// Every stored page of a section, with its rows' facts.
    pub(crate) async fn stored_rows(self: &Arc<Self>, namespace: &str, library: &str, media: Media, section: &Section, generation: &Arc<()>) -> Result<Vec<(Entry, Facts)>, LibError> {
        let mut rows = Vec::new();
        for page in 0..MAX_PAGES {
            let Some(stored) = self.stored_page(namespace, library, media, section, page, generation).await? else { break };
            let mut facts = stored.facts.into_iter();
            rows.extend(stored.entries.into_iter().map(|entry| (entry, facts.next().unwrap_or_default())));
            if !stored.has_more || rows.len() > MAX_GROUPS { break; }
        }
        Ok(rows)
    }

    /// Stores `rows` as the pages of `section`, as the server pages it.
    async fn store_derived(self: &Arc<Self>, namespace: &str, library: &str, media: Media, section: &Section, rows: Vec<(Entry, Facts)>, server_total: bool, generation: &Arc<()>, c: &mut SyncCursor) -> Result<(), LibError> {
        let total = rows.len();
        let chunks: Vec<Vec<(Entry, Facts)>> = if rows.is_empty() { vec![Vec::new()] } else {
            let mut chunks: Vec<_> = rows.chunks(SIZE as usize).map(<[_]>::to_vec).collect();
            // A list without a total pages until a short page, which may be empty.
            if !server_total && total % SIZE as usize == 0 { chunks.push(Vec::new()); }
            chunks
        };
        let mut writes = Vec::new();
        for (page, chunk) in chunks.into_iter().enumerate() {
            let page = page as u32;
            let id = page_identity(library, media, section, page, SIZE)?;
            c.kept.insert(md5::Md5::digest(id.as_bytes()).into());
            let has_more = if server_total { (page as usize + 1) * (SIZE as usize) < total } else { chunk.len() == SIZE as usize };
            let (entries, facts): (Vec<Entry>, Vec<Facts>) = chunk.into_iter().unzip();
            let facts = if facts.iter().all(|f| *f == Facts::default()) { Vec::new() } else { facts };
            let stored = subsonic::audiobookshelf_browse::Page { entries, total: server_total.then_some(total as u64), has_more, paging: Paging::Server, facts };
            writes.push((page_key(namespace, &id), serde_json::to_string(&stored).map_err(fail)?));
            c.derived_pages += 1;
        }
        self.shelf_db_run(generation.clone(), move |blocking, db| {
            writes.iter().try_for_each(|(key, body)| db.put_spoken_page(blocking, key, body, None).map(drop)).map_err(fail)
        }).await
    }

    /// Builds every group page of `kind` from the stored books and the
    /// stored list of `kind`'s groups; the groups it cannot build are left to
    /// be fetched.
    async fn derive_groups(self: &Arc<Self>, library: &str, media: Media, kind: derive::Kind, namespace: &str, generation: &Arc<()>, c: &mut SyncCursor) -> Result<(), LibError> {
        let books = self.stored_rows(namespace, library, media, &Section::Books, generation).await?;
        let (entries, facts): (Vec<Entry>, Vec<Facts>) = self.stored_rows(namespace, library, media, &kind.list(), generation).await?.into_iter().unzip();
        let groups = derive::groups(entries, facts);
        let (derived, fetch) = crate::rt::spawn_blocking(move |_| derive::derive(kind, &books, &groups)).await.map_err(fail)?;
        if !fetch.is_empty() { log::info!("library sync: {} {kind:?} group(s) fetched (their book rows do not name them unambiguously)", fetch.len()); }
        for (section, rows) in derived {
            self.store_derived(namespace, library, media, &section, rows, true, generation, c).await?;
            c.derived.insert(serde_json::to_string(&section).map_err(fail)?);
        }
        Ok(())
    }

    /// The personalized shelves of `sections`, from one document, stored as
    /// each shelf's page 0 with the document's ETag. A 304 (every shelf
    /// stored under the same ETag) writes nothing.
    async fn sync_home_pages(self: &Arc<Self>, library: &str, media: Media, sections: &[Section], generation: &Arc<()>) -> Result<(), LibError> {
        let client = self.shelf_client_async().await?;
        let namespace = client.namespace_async().await.map_err(fail)?;
        let keys = sections.iter().map(|section| page_identity(library, media, section, 0, SIZE).map(|id| page_key(&namespace, &id))).collect::<Result<Vec<_>, _>>()?;
        let lookup = keys.clone();
        let tags = self.shelf_db_run(generation.clone(), move |blocking, db| {
            lookup.iter().map(|key| db.spoken_page_etag(blocking, key).map_err(fail)).collect::<Result<Vec<_>, _>>()
        }).await?;
        let etag = tags.first().cloned().flatten().filter(|tag| tags.iter().all(|t| t.as_deref() == Some(tag.as_str())))
            .filter(|_| !self.shelf_auto.unconditional.load(std::sync::atomic::Ordering::Acquire));
        let validated = client.browse_home_validated_async(library, media, sections, SIZE, etag.as_deref()).await.map_err(fail)?;
        let counter = if matches!(validated, Validated::Unchanged) { &self.shelf_auto.pages_unchanged } else { &self.shelf_auto.pages_fetched };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (pages, tag) = match validated {
            Validated::Unchanged if etag.is_some() => return self.shelf_result(generation.clone(), ()).await,
            Validated::Unchanged => return Err(fail("Unsupported Audiobookshelf response")),
            Validated::Changed(pages, tag) => (pages, tag),
        };
        let bodies = crate::rt::spawn_blocking(move |_| pages.iter().map(|page| serde_json::to_string(page).map_err(fail)).collect::<Result<Vec<_>, _>>())
            .await.map_err(fail)??;
        self.shelf_db_run(generation.clone(), move |blocking, db| {
            keys.iter().zip(&bodies).try_for_each(|(key, body)| db.put_spoken_page(blocking, key, body, tag.as_deref()).map(drop)).map_err(fail)
        }).await?;
        self.shelf_result(generation.clone(), ()).await
    }

    /// The current account's generation and the pages of `library`'s Books
    /// list a completed sync stored; `None` without a completed sync. With
    /// `namespace`, also `None` when the account is another one.
    async fn synced_book_pages(self: &Arc<Self>, library: &str, media: Media, namespace: Option<&str>) -> Result<(Arc<()>, Option<u32>), LibError> {
        let owner = self.clone();
        let library_key = library.to_owned();
        let expected = namespace.map(str::to_owned);
        let (generation, key) = crate::rt::spawn_blocking(move |_| {
            let slot = owner.shelf.read().unwrap();
            let current = expected.is_none_or(|namespace| namespace == slot.namespace);
            (slot.generation.clone(), current.then(|| manifest_key(&slot.namespace, &library_key, media)))
        }).await.map_err(fail)?;
        let Some(key) = key else { return Ok((generation, None)) };
        let manifest = self.shelf_db_run(generation.clone(), move |blocking, db| db.spoken_response(blocking, &key).map_err(fail)).await?;
        let Some(manifest) = manifest else { return Ok((generation, None)); };
        let Some(manifest) = serde_json::from_str::<Option<Manifest>>(&manifest).map_err(fail)? else { return Ok((generation, None)); };
        if manifest.pages > MAX_PAGES { return Err(fail("Invalid local book index")); }
        Ok((generation, Some(manifest.pages)))
    }

    /// The row a completed sync stored for `item` (or its `episode`) of
    /// `library` in account `namespace`: what Book Info can show for an item
    /// whose own page was never fetched (UXI-016). `None` when the sync has
    /// no such row.
    pub(in crate::library) async fn shelf_synced_entry(self: &Arc<Self>, namespace: &str, library: &str, item: &str, episode: Option<&str>) -> Result<Option<Entry>, LibError> {
        let wanted = |entry: &Entry| match (&entry.kind, episode) {
            (EntryKind::Book { item: id } | EntryKind::Podcast { item: id }, None) => id == item,
            (EntryKind::Episode { item: id, episode: ep }, Some(wanted)) => id == item && ep == wanted,
            _ => false,
        };
        if episode.is_some() {
            // Episodes are stored per show, by a completed podcast sync.
            let (generation, pages) = self.synced_book_pages(library, Media::Podcast, Some(namespace)).await?;
            if pages.is_none() { return Ok(None); }
            let section = Section::Episodes(item.to_owned());
            for page in 0..MAX_PAGES {
                // A page the sync did not store ends the search.
                let Ok(cached) = self.shelf_browse_cached(library, Media::Podcast, &section, page, SIZE, Policy::CacheOnly, false).await else { break };
                if let Some(entry) = cached.entries.into_iter().find(wanted) { return self.shelf_result(generation, Some(entry)).await; }
                if !cached.has_more { break; }
            }
            return Ok(None);
        }
        for media in [Media::Book, Media::Podcast] {
            let (generation, pages) = self.synced_book_pages(library, media, Some(namespace)).await?;
            for page in 0..pages.unwrap_or(0) {
                let cached = self.shelf_browse_cached(library, media, &Section::Books, page, SIZE, Policy::CacheOnly, false).await?;
                if let Some(entry) = cached.entries.into_iter().find(wanted) { return self.shelf_result(generation, Some(entry)).await; }
            }
        }
        Ok(None)
    }

    pub(super) async fn shelf_synced_search(self: &Arc<Self>, library: &str, media: Media, term: &str, page: u32, limit: u32) -> Result<Option<BrowsePage>, LibError> {
        if limit == 0 || limit > SIZE || term.trim().is_empty() || term.len() > 4096 { return Err(fail("Unsupported book search")); }
        let (generation, pages) = self.synced_book_pages(library, media, None).await?;
        let Some(pages) = pages else { return Ok(None) };
        let term = term.trim().to_lowercase();
        let start = u64::from(page) * u64::from(limit);
        let mut total = 0u64;
        let mut entries = Vec::new();
        for cached_page in 0..pages {
            let cached = self.shelf_browse_cached(library, media, &Section::Books, cached_page, SIZE, Policy::CacheOnly, false).await?;
            for entry in cached.entries {
                if entry.title.to_lowercase().contains(&term) || entry.detail.to_lowercase().contains(&term) {
                    if total >= start && entries.len() < limit as usize { entries.push(entry); }
                    total += 1;
                }
            }
        }
        self.shelf_result(generation.clone(), ()).await?;
        Ok(Some(BrowsePage {entries, total: Some(total), has_more: start + u64::from(limit) < total, paging: Paging::Snapshot, generation: Some(generation)}))
    }
}

#[cfg(test)]
#[path = "shelf_sync_tests.rs"]
mod model_tests;
