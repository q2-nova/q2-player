//! Cover art: fetched at the size it is shown (the server scales it), kept on
//! the card, decoded into a small memory cache.

use super::{art_pack::ContentId, LibEvent, Library};
use crate::gfx::Image;
use std::collections::HashMap;
use md5::{Digest, Md5};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
enum Source {
    Cover(String),
    Artist { id: String, cover: Option<String>, url: Option<String> },
}

// Server/user namespace, source, requested square size.
type Key = (String, Source, u32);

/// Decoded artwork kept in memory. Cover Flow's drawn and warmed window is
/// about 25 covers of 150 px (88 KiB each, 2.2 MiB); Home's slides are 320 px
/// (400 KiB). Everything else reloads from the card's PNG cache. It was
/// 8 MiB: on the Q2 (118 MiB, 5 MB free and 20 MB of file cache after Cover
/// Flow, V0.113) that was 40 % of the page cache tailscaled and the player
/// page their code into (docs/server-status.md, memory).
const MEMORY_BYTES: usize = 3 * 1024 * 1024;
/// What a screen that filled the cache (Cover Flow) leaves when it closes:
/// Now Playing's cover and a list's thumbnails.
pub const MEMORY_AFTER_BROWSING: usize = 1024 * 1024;
/// What is kept while the screen is off: about the most recent Now Playing
/// cover and its thumbnail (88 and 10 KiB). The rest reloads from the card's
/// PNG cache when the screen comes back.
pub const MEMORY_SCREEN_OFF: usize = 128 * 1024;
/// A cover from the server, all of it: a slow one gives way to the next.
/// Artwork failures never count against the server (library/health.rs).
#[cfg(not(test))]
const NETWORK_LIMIT: Duration = Duration::from_secs(10);
#[cfg(test)]
const NETWORK_LIMIT: Duration = Duration::from_secs(2);

/// Decoded artwork held in memory, for memory telemetry
/// (`platform/resident.rs`): KiB and images. 32-bit: the Q2 has no 64-bit atomics.
pub(crate) static DECODED_KIB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub(crate) static DECODED_IMAGES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub const NOW_PLAYING_SIZE: u32 = 150;
pub const THUMBNAIL_SIZE: u32 = 50;

// Many track keys may share a tiny set of images. Bound the metadata even
// when the pixel budget alone would keep all those aliases indefinitely.
const MEMORY_ENTRIES: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SharedImage(ContentId, u32);

struct Loaded {
    image: Arc<Image>,
    shared: Option<SharedImage>,
}

impl From<Image> for Loaded {
    fn from(image: Image) -> Self { Self { image: Arc::new(image), shared: None } }
}

#[derive(Default)]
struct Memory {
    aliases: HashMap<Key, (SharedImage, u64)>,
    local: HashMap<Key, (Arc<Image>, u64)>,
    shared: HashMap<SharedImage, (Arc<Image>, u64)>,
    bytes: usize,
    clock: u64,
}

impl Memory {
    fn get(&mut self, k: &Key) -> Option<Arc<Image>> {
        self.clock += 1;
        let entry = if let Some((shared, used)) = self.aliases.get_mut(k) {
            *used = self.clock;
            self.shared.get_mut(shared)
        } else {
            self.local.get_mut(k)
        };
        Self::touch(entry, self.clock)
    }

    fn touch(entry: Option<&mut (Arc<Image>, u64)>, clock: u64) -> Option<Arc<Image>> {
        entry.map(|(img, used)| {
            *used = clock;
            img.clone()
        })
    }

    fn get_shared(&mut self, shared: SharedImage) -> Option<Arc<Image>> {
        self.clock += 1;
        Self::touch(self.shared.get_mut(&shared), self.clock)
    }

    fn put(&mut self, k: Key, load: Loaded) {
        if let Some(shared) = load.shared {
            if let Some((old, _)) = self.local.remove(&k) {
                self.bytes -= bytes(&old);
            }
            self.remember_shared(shared, load.image);
            if self.shared.contains_key(&shared) {
                self.aliases.insert(k, (shared, self.clock));
                if self.aliases.len() > MEMORY_ENTRIES {
                    let oldest = self.aliases.iter().min_by_key(|(_, (_, used))| *used)
                        .map(|(key, _)| key.clone()).unwrap();
                    self.aliases.remove(&oldest);
                }
            }
            self.published();
        } else {
            self.clock += 1;
            self.aliases.remove(&k);
            self.bytes += bytes(&load.image);
            if let Some((old, _)) = self.local.insert(k, (load.image, self.clock)) {
                self.bytes -= bytes(&old);
            }
            self.shrink(MEMORY_BYTES);
        }
    }

    // Register a packed decode before releasing the worker gate, so two workers
    // loading different aliases can't decode the same blob concurrently.
    fn remember_shared(&mut self, shared: SharedImage, image: Arc<Image>) -> Arc<Image> {
        if let Some(existing) = self.get_shared(shared) { return existing; }
        self.bytes += bytes(&image);
        self.shared.insert(shared, (image.clone(), self.clock));
        self.shrink(MEMORY_BYTES);
        image
    }

    /// Evicts the least recently used images until at most `budget` remain.
    fn shrink(&mut self, budget: usize) {
        while self.bytes > budget || self.local.len() + self.shared.len() > MEMORY_ENTRIES {
            let local = self.local.iter().min_by_key(|(_, (_, used))| *used);
            let shared = self.shared.iter().min_by_key(|(_, (_, used))| *used);
            if let Some((key, _)) = local.filter(|(_, (_, used))| shared.is_none_or(|(_, (_, shared_used))| used < shared_used)) {
                let key = key.clone();
                self.bytes -= bytes(&self.local.remove(&key).unwrap().0);
            } else if let Some((&key, _)) = shared {
                self.bytes -= bytes(&self.shared.remove(&key).unwrap().0);
                self.aliases.retain(|_, (target, _)| *target != key);
            }
        }
        self.published();
    }

    fn published(&self) {
        DECODED_KIB.store((self.bytes / 1024) as u32, Ordering::Relaxed);
        DECODED_IMAGES.store((self.local.len() + self.shared.len()) as u32, Ordering::Relaxed);
    }
}

/// The pixel buffer's real size (`u32` per pixel, its allocation).
fn bytes(image: &Image) -> usize { image.pixels.capacity() * 4 }

#[derive(Default)]
struct ActiveCover {
    prefetched: bool,
    downloaded: bool,
}

#[derive(Default)]
struct Work {
    queue: Vec<Key>,
    active: HashMap<Key, ActiveCover>,
    window: std::collections::HashSet<Key>,
    retry: HashMap<Key, Instant>,
    /// Queued keys only prefetched (Cover Flow, upcoming songs), not yet
    /// asked for by a drawn view. They yield to waiting requests.
    prefetched: std::collections::HashSet<Key>,
}

impl Work {
    fn prefetch(&mut self, key: Key) {
        // Visible requests stay at the end (highest priority). Background
        // artwork cannot displace them or duplicate work already in flight.
        if !self.active.contains_key(&key) && !self.queue.contains(&key) && self.queue.len() < 64 {
            self.prefetched.insert(key.clone());
            self.queue.insert(0, key);
        }
    }

    fn enqueue(&mut self, key: Key) {
        if let Some(active) = self.active.get_mut(&key) { active.prefetched = false; return; }
        self.prefetched.remove(&key);
        if let Some(i) = self.queue.iter().position(|k| k == &key) { self.queue.remove(i); }
        self.queue.push(key);
        if self.queue.len() > 64 {
            let dropped = self.queue.remove(0);
            self.retry.remove(&dropped);
            self.prefetched.remove(&dropped);
        }
    }

    /// The next key to load; prefetched keys only when `prefetch` allows.
    fn ready(&mut self, now: Instant, retry: bool, prefetch: bool) -> Option<Key> {
        self.take_ready(now, retry, prefetch, false)
    }

    fn take_ready(&mut self, now: Instant, retry: bool, prefetch: bool, window_only: bool) -> Option<Key> {
        let i = self.queue.iter().rposition(|k| (!window_only || self.window.contains(k)) && self.retry.get(k).is_none_or(|at| retry && *at <= now)
            && (prefetch || !self.prefetched.contains(k)))?;
        let key = self.queue.remove(i);
        self.retry.remove(&key);
        let prefetched = self.prefetched.remove(&key);
        self.active.insert(key.clone(), ActiveCover { prefetched, downloaded: false });
        Some(key)
    }

    fn finish(&mut self, key: &Key, failed: bool, now: Instant) {
        let ActiveCover { downloaded, prefetched } = self.active.remove(key).unwrap_or_default();
        if failed {
            // A committed cover may have arrived after this load passed its
            // disk lookup. Try the normal loader once more before backing off.
            let deadline = if downloaded { now } else { now + Duration::from_secs(30) };
            self.retry.insert(key.clone(), deadline);
            if prefetched { self.prefetch(key.clone()); } else { self.enqueue(key.clone()); }
        }
    }

    fn downloaded_cover(&mut self, id: &str, now: Instant) -> bool {
        let matches = |key: &Key| matches!(&key.1, Source::Cover(cover) if cover == id);
        let mut changed = false;
        for (key, active) in &mut self.active {
            if matches(key) { active.downloaded = true; changed = true; }
        }
        for (key, deadline) in &mut self.retry {
            if matches(key) {
                // Keep it a retry, so presentation work still waits in the dark.
                *deadline = now;
                changed = true;
            }
        }
        changed
    }

    fn delay(&self, now: Instant) -> Duration {
        self.retry.values().filter(|at| **at > now).map(|at| at.saturating_duration_since(now)).min().unwrap_or(Duration::from_secs(60))
    }
}

pub struct ArtCache {
    pub(super) pack: super::art_pack::ArtPack,
    dir: PathBuf,
    /// Sharded and flat cache paths (crate::shard).
    paths: crate::config::Paths,
    fallback: Option<PathBuf>,
    namespace: Mutex<String>,
    memory: Mutex<Memory>,
    // Two workers may discover aliases simultaneously. Stripe by payload so
    // they share a cold decode without serializing every unrelated cover.
    packed_decode: [Mutex<()>; 16],
    work: Mutex<Work>,
    wake: Arc<Notify>,
    window_wake: Arc<Notify>,
    visible: AtomicBool,
}

impl ArtCache {
    #[cfg(test)]
    pub fn new(dir: PathBuf) -> ArtCache { Self::with_fallback(&crate::config::Paths::new(dir), None) }

    pub fn with_fallback(paths: &crate::config::Paths, fallback: Option<PathBuf>) -> ArtCache {
        ArtCache {
            pack: super::art_pack::ArtPack::new(paths.art_cache()),
            dir: paths.art_cache(),
            paths: paths.clone(),
            fallback,
            namespace: Mutex::default(),
            memory: Mutex::default(),
            packed_decode: Default::default(),
            work: Mutex::default(),
            wake: Arc::default(),
            window_wake: Arc::default(),
            visible: AtomicBool::new(true),
        }
    }

    /// The decoded images; every caller holds it only for a lookup or an update.
    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> { self.memory.lock().unwrap() }

    pub fn set_visible(&self, on: bool) {
        if self.visible.swap(on, Ordering::Relaxed) != on { self.wake.notify_one(); }
    }

    /// The image for `cover_id` at `size` pixels square, if loaded; else
    /// asks for it, and [`LibEvent::ArtReady`] follows.
    pub fn get(&self, cover_id: &str, size: u32) -> Option<Arc<Image>> {
        if cover_id.trim().is_empty() { return None; }
        self.request(Source::Cover(cover_id.to_string()), size)
    }

    pub fn get_artist(&self, artist: &subsonic::Artist, size: u32) -> Option<Arc<Image>> {
        self.request(Source::Artist { id: artist.id.clone(), cover: artist.cover_art.clone(), url: artist.artist_image_url.clone() }, size)
    }

    /// Warm both views used by upcoming songs, through the same worker and
    /// disk cache as visible artwork. Also useful for already-cached music.
    pub fn prefetch_song(&self, song: &subsonic::Song) {
        let Some(id) = song.cover_art.as_deref().filter(|id| !id.trim().is_empty()) else { return };
        let namespace = self.namespace.lock().unwrap().clone();
        for size in [NOW_PLAYING_SIZE, THUMBNAIL_SIZE] {
            let key = (namespace.clone(), Source::Cover(id.into()), size);
            if self.memory().get(&key).is_none() {
                self.work.lock().unwrap().prefetch(key);
            }
        }
        self.wake.notify_one();
    }

    /// Memory only, with no network/disk work on the drawing thread.
    pub fn peek_cover(&self, id: &str, size: u32) -> Option<Arc<Image>> {
        let key = (self.namespace.lock().unwrap().clone(), Source::Cover(id.into()), size);
        self.memory().get(&key)
    }

    pub fn prefetch_cover(&self, id: &str, size: u32) {
        if id.is_empty() || size == 0 || size > 1024 { return; }
        let key = (self.namespace.lock().unwrap().clone(), Source::Cover(id.into()), size);
        if self.memory().get(&key).is_none() {
            self.work.lock().unwrap().prefetch(key);
            self.wake.notify_one();
        }
    }

    /// Submit a complete visible frame atomically. The painter orders covers
    /// farthest first, so the selected cover ends up next on the LIFO queue.
    /// The single worker cannot start a far cover halfway through the batch.
    pub(crate) fn frame_covers<const N: usize>(&self, ids: [Option<&str>; N], size: u32) -> [Option<Arc<Image>>; N] {
        let namespace = self.namespace.lock().unwrap();
        let mut memory = self.memory();
        let mut work = self.work.lock().unwrap();
        let mut missing = false;
        let images = ids.map(|id| {
            let key = (namespace.clone(), Source::Cover(id.filter(|id| !id.trim().is_empty())?.into()), size);
            if let Some(img) = memory.get(&key) { return Some(img); }
            work.enqueue(key);
            missing = true;
            None
        });
        if missing { self.wake.notify_one(); }
        images
    }

    /// Replace the priority of the speculative window after a wheel turn.
    /// Present requests and retry deadlines keep precedence. Nearest first.
    pub(crate) fn warm_window<'a>(&self, ids: impl Iterator<Item=&'a str>, size: u32) {
        let namespace = self.namespace.lock().unwrap();
        let mut memory = self.memory();
        let mut work = self.work.lock().unwrap();
        let mut insert = work.queue.iter().position(|k| !work.prefetched.contains(k)).unwrap_or(work.queue.len());
        work.window.clear();
        for id in ids.filter(|id| !id.trim().is_empty()) {
            let key = (namespace.clone(), Source::Cover(id.into()), size);
            work.window.insert(key.clone());
            if memory.get(&key).is_some() || work.active.contains_key(&key) { continue; }
            if let Some(old) = work.queue.iter().position(|k| k == &key) {
                if !work.prefetched.contains(&key) || old >= insert { continue; }
                work.queue.remove(old);
                insert -= 1;
            } else if work.queue.len() == 64 {
                if insert == 0 { continue; }
                let dropped = work.queue.remove(0);
                work.prefetched.remove(&dropped);
                work.retry.remove(&dropped);
                insert -= 1;
            }
            work.prefetched.insert(key.clone());
            work.queue.insert(insert, key);
        }
        self.wake.notify_one();
        self.window_wake.notify_one();
    }

    pub(crate) fn close_window(&self) {
        let mut work = self.work.lock().unwrap();
        for key in std::mem::take(&mut work.window) {
            if work.prefetched.remove(&key) {
                work.queue.retain(|queued| queued != &key);
                work.retry.remove(&key);
            }
        }
        self.window_wake.notify_one();
    }

    fn request(&self, source: Source, size: u32) -> Option<Arc<Image>> {
        if size == 0 || size > 1024 { return None; }
        let key = (self.namespace.lock().unwrap().clone(), source, size);
        if let Some(img) = self.memory().get(&key) {
            return Some(img);
        }
        self.work.lock().unwrap().enqueue(key);
        self.wake.notify_one();
        None
    }

    fn digest(key: &Key) -> String {
        // Preserve the opaque identity of both generations of loose artwork.
        format!("{:x}", Md5::digest(serde_json::to_vec(key).expect("art key")))
    }

    #[cfg(test)]
    pub(super) fn reopened_cover(&self, blocking: &crate::rt::Blocking, id: &str, size: u32) -> Option<Vec<u8>> {
        let key = (self.namespace.lock().unwrap().clone(), Source::Cover(id.to_owned()), size);
        super::art_pack::ArtPack::new(self.dir.clone()).get(blocking, &Self::digest(&key)).unwrap()
    }

    #[cfg(test)]
    fn path(&self, key: &Key) -> PathBuf {
        self.paths.sharded(crate::shard::Store::Art, format!("{}.png", Self::digest(key)).as_ref())
    }

    fn loose_image(_blocking: &crate::rt::Blocking, path: &std::path::Path, size: u32) -> Option<Image> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
        let metadata = file.metadata().ok()?;
        const LIMIT: u64 = 5 * 1024 * 1024;
        if !metadata.is_file() || metadata.len() > LIMIT { return None; }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(LIMIT + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > LIMIT { return None; }
        Image::decode_square(&bytes, size)
    }

    fn packed_image(&self, blocking: &crate::rt::Blocking, key: &Key, digest: &str, size: u32, purpose: Purpose) -> Option<Loaded> {
        let mut cached = None;
        let (id, encoded) = self.pack.get_shared(blocking, digest, |id| {
            cached = self.memory().get_shared(SharedImage(id, size));
            cached.is_some()
        }).ok().flatten()?;
        let shared = id.map(|id| SharedImage(id, size));
        if let Some(image) = cached {
            return Some(Loaded { image, shared });
        }
        let _decode = id.map(|id| self.packed_decode[id.decode_slot(self.packed_decode.len())].lock().unwrap());
        if let Some(image) = shared.and_then(|key| self.memory().get_shared(key)) {
            return Some(Loaded { image, shared });
        }
        let image = Arc::new(Image::decode_square(&encoded?, size)?);
        Some(self.remember_packed(key, image, shared, purpose))
    }

    fn remember_packed(&self, key: &Key, image: Arc<Image>, shared: Option<SharedImage>, purpose: Purpose) -> Loaded {
        let namespace = self.namespace.lock().unwrap();
        let image = match shared {
            Some(shared) if *namespace == key.0 && purpose == Purpose::Display => {
                self.memory().remember_shared(shared, image)
            }
            _ => image,
        };
        Loaded { image, shared }
    }

    fn cached_image(&self, blocking: &crate::rt::Blocking, key: &Key, size: u32, purpose: Purpose) -> Option<Loaded> {
        let digest = Self::digest(key);
        let packed = || self.packed_image(blocking, key, &digest, size, purpose);
        if let Some(image) = packed() { return Some(image); }
        let name = format!("{digest}.png");
        let shard = self.paths.sharded(crate::shard::Store::Art, name.as_ref());
        let flat = self.dir.join(&name);
        for path in [shard, flat] {
            if let Some(image) = Self::loose_image(blocking, &path, size) {
                return Some(image.into());
            }
        }
        // A migration batch can commit and unlink between the first lookup and open.
        packed().or_else(|| {
            Self::loose_image(blocking, &self.fallback.as_ref()?.join(name), size).map(Loaded::from)
        })
    }

    pub fn set_namespace(&self, namespace: &str) {
        let mut current = self.namespace.lock().unwrap();
        if *current == namespace { return; }
        *current = namespace.to_owned();
        *self.memory() = Memory::default();
        Memory::default().published();
        let mut work = self.work.lock().unwrap();
        work.queue.clear();
        work.retry.clear();
        work.prefetched.clear();
        work.window.clear();
        self.window_wake.notify_one();
        self.wake.notify_one();
    }

    pub fn start(lib: &Arc<Library>) {
        crate::rt::io().spawn(run(Arc::downgrade(lib), lib.art.wake.clone(), false));
        // One extra request only for the live wheel window, so a slow HTTP
        // response need not delay the next cover by its entire round trip.
        crate::rt::io().spawn(run(Arc::downgrade(lib), lib.art.window_wake.clone(), true));
    }

    /// Reuse the art loader, then acknowledge a durable pair of exact-size rows.
    /// Ordinary presentation caches intentionally do not promise durability.
    pub(super) async fn sync_cover(lib: Arc<Library>, id: String, generation: Arc<()>) -> Result<bool, super::LibError> {
        let owner = lib.clone();
        let (key, stored) = crate::rt::spawn_blocking(move |blocking| {
            let key = (owner.art.namespace.lock().unwrap().clone(), Source::Cover(id), NOW_PLAYING_SIZE);
            let stored = owner.art.stored(blocking, &key);
            (key, stored)
        }).await.map_err(super::shelf_browse::fail)?;
        // Both sizes already on the card: no request, decode or rewrite.
        if stored { return lib.shelf_result(generation, true).await; }
        let Some(image) = load_async(&Arc::downgrade(&lib), key.clone(), Purpose::Sync).await else { return Ok(false); };
        let active = lib.clone();
        let expected = generation.clone();
        let saved = crate::rt::spawn_blocking(move |blocking| {
            let changed = || super::shelf_browse::fail("Audiobookshelf changed during library sync");
            let slot = lib.shelf.read().unwrap();
            if !Arc::ptr_eq(&slot.generation, &expected) { return Err(changed()); }
            drop(slot);
            let namespace = lib.art.namespace.lock().unwrap();
            if *namespace != key.0 { return Err(changed()); }
            drop(namespace);
            if lib.paths.ephemeral_media || !lib.paths.can_modify_media(&lib.art.dir) {
                return Err(super::shelf_browse::fail("Insert a writable card to sync artwork"));
            }
            let large = image.image.png().ok_or_else(||super::shelf_browse::fail("Unable to encode synced artwork"))?;
            let thumbnail = image.image.thumbnail(THUMBNAIL_SIZE).and_then(|image|image.png())
                .ok_or_else(||super::shelf_browse::fail("Unable to encode synced thumbnail"))?;
            let reserve = lib.settings.read().unwrap().reserve_bytes;
            // Allow space for the DB and journal pages plus exFAT cluster rounding.
            let transaction_space = 1024 * 1024 + 2 * (large.len() + thumbnail.len()) as u64;
            if super::free_space(blocking, lib.paths.media_root()).is_none_or(|free|free < reserve.saturating_add(transaction_space)) {
                return Err(super::shelf_browse::fail("Not enough card space to sync artwork"));
            }
            let large_key = Self::digest(&key);
            let small_key = Self::digest(&(key.0, key.1, THUMBNAIL_SIZE));
            lib.art.pack.put(blocking, &[(&large_key, &large), (&small_key, &thumbnail)])
                .map_err(super::shelf_browse::fail)?;
            Ok(true)
        }).await.map_err(super::shelf_browse::fail)??;
        active.shelf_result(generation, saved).await
    }

    /// Both presentation sizes of this cover have durable packed rows.
    fn stored(&self, blocking: &crate::rt::Blocking, key: &Key) -> bool {
        [NOW_PLAYING_SIZE, THUMBNAIL_SIZE].into_iter().all(|size| self.pack.contains(blocking, &Self::digest(&(key.0.clone(), key.1.clone(), size))).unwrap_or(false))
    }

    /// Drops decoded artwork down to `budget` bytes, least recent first:
    /// a screen that filled the cache has closed (Cover Flow).
    pub fn shrink_memory(&self, budget: usize) {
        self.memory().shrink(budget);
    }

    /// A request the prefetch yielded to has ended: look again.
    pub(super) fn priority_released(&self) { self.wake.notify_one(); self.window_wake.notify_one(); }

    /// Retry immediately after connectivity returns.
    pub fn retry_missing(&self) {
        self.work.lock().unwrap().retry.clear();
        self.wake.notify_one();
    }
    /// A committed offline cover can satisfy an existing visible request now.
    /// Do not start unrelated retries or prepare presentation sizes in the dark.
    pub fn downloaded_cover(&self,id:&str) {
        if self.work.lock().unwrap().downloaded_cover(id, Instant::now()) {self.wake.notify_one();}
    }
}

impl Drop for ArtCache {
    fn drop(&mut self) { self.wake.notify_one(); self.window_wake.notify_one(); }
}

/// No library ownership survives a network or idle wait. Drop wakes the task;
/// namespace replacement cancels obsolete network work before the next request.
async fn run(weak: Weak<Library>, wake: Arc<Notify>, window_only: bool) {
    loop {
        let notified = wake.notified();
        let Some((key, delay)) = with_lib(&weak, move |lib, _| {
            // Prefetch yields to a listener waiting on the server, the
            // health probe and a song still downloading for the player.
            let prefetch = lib.maintenance_link_ready() && !lib.background_should_yield() && !lib.playback_transfer_running();
            let mut work = lib.art.work.lock().unwrap();
            let visible = lib.art.visible.load(Ordering::Relaxed);
            let now = Instant::now();
            let key = if window_only {
                (visible && prefetch).then(|| work.take_ready(now, true, true, true)).flatten()
            } else { work.ready(now, visible, prefetch) };
            let delay = (!window_only && visible && !work.retry.is_empty()).then(|| work.delay(now));
            (key, delay)
        }).await else { return };
        let Some(key) = key else {
            if let Some(delay) = delay {
                tokio::select! { () = notified => {}, () = tokio::time::sleep(delay) => {} }
            } else { notified.await; }
            continue;
        };
        let image = {
            let pending = load_async(&weak, key.clone(), Purpose::Display);
            tokio::pin!(pending);
            loop {
                tokio::select! {
                    image = &mut pending => break image,
                    () = wake.notified() => {
                        let k = key.clone();
                        if with_lib(&weak, move |lib, _| lib.art.current(&k)).await != Some(true) {
                            break None;
                        }
                    }
                }
            }
        };
        with_lib(&weak, move |lib, _| {
            // Keep identity stable through memory publication. Namespace changes
            // take this same gate before clearing memory and queued work.
            let namespace = lib.art.namespace.lock().unwrap();
            let current = *namespace == key.0;
            lib.art.work.lock().unwrap().finish(&key, current && image.is_none(), Instant::now());
            if current {
                if let Some(image) = image {
                    lib.art.memory().put(key, image);
                    drop(namespace);
                    lib.notify(LibEvent::ArtReady);
                }
            }
        }).await;
    }
}

pub(super) async fn with_lib<T: Send + 'static>(weak: &Weak<Library>,
    work: impl FnOnce(&Library, &crate::rt::Blocking) -> T + Send + 'static) -> Option<T> {
    let weak = weak.clone();
    crate::rt::spawn_blocking(move |blocking| {
        let lib = weak.upgrade()?;
        Some(work(&lib, blocking))
    }).await.ok().flatten()
}

impl ArtCache {
    fn current(&self, key: &Key) -> bool { *self.namespace.lock().unwrap() == key.0 }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose { Display, Sync }

enum Load {
    Ready(Option<Loaded>),
    Spoken(String),
    Network(Arc<subsonic::Client>),
}

fn prepare(lib: &Library, key: &Key, indexed: Option<super::offline_art::IndexedCover>, purpose: Purpose, blocking: &crate::rt::Blocking) -> Load {
    if !lib.art.current(key) { return Load::Ready(None); }
    if let Some(image) = indexed.and_then(|cover| cover.load(blocking, lib, key.2)) {
        return Load::Ready(Some(image.into()));
    }
    if let Some(image) = lib.art.cached_image(blocking, key, key.2, purpose) { return Load::Ready(Some(image)); }
    if let Source::Cover(id) = &key.1 {
        if id.starts_with("local-book-cover:") || id.starts_with("shelf-book-cover:") {
            return Load::Spoken(id.clone());
        }
    }
    if !lib.is_online() {
        return Load::Ready(if purpose == Purpose::Display { cached_other_size(blocking, lib, key) } else { None });
    }
    let Ok(client) = lib.client() else { return Load::Ready(None) };
    let creds = client.credentials();
    if format!("{}|{}", creds.server.trim_end_matches('/'), creds.user) != key.0 {
        return Load::Ready(None);
    }
    Load::Network(client)
}

async fn load_async(weak: &Weak<Library>, key: Key, purpose: Purpose) -> Option<Loaded> {
    let indexed = if let Source::Cover(id) = &key.1 {
        let indexed = { let lib = weak.upgrade()?; super::offline_art::indexed_cover(&lib, &key.0, id) };
        Some(indexed.await)
    } else { None };
    let k = key.clone();
    let image = match with_lib(weak, move |lib, blocking| prepare(lib, &k, indexed, purpose, blocking)).await? {
        Load::Ready(image) => return image,
        Load::Spoken(id) => {
            let bytes = super::shelf::spoken_cover_async(weak.clone(), id, key.2).await;
            decode(bytes, key.2).await
        }
        // Artwork failures must not mark the music server offline. One
        // immutable client owns all artist fallbacks for this request.
        Load::Network(client) => fetch(client, key.1.clone(), key.2).await,
    };
    // Explicit sync owns admission and commits both sizes in one transaction.
    if purpose == Purpose::Sync { return image.map(Loaded::from); }
    with_lib(weak, move |lib, blocking| publish_cache(blocking, lib, &key, image)).await.flatten()
}

fn publish_cache(blocking: &crate::rt::Blocking, lib: &Library, key: &Key, image: Option<Image>) -> Option<Loaded> {
    if !lib.art.current(key) { return None; }
    let Some(img) = image else { return cached_other_size(blocking, lib, key) };
    let encoded = img.png();
    if !lib.art.current(key) { return None; }
    let shared = encoded.and_then(|png| lib.art.pack.put_presentation(blocking, &ArtCache::digest(key), &png).ok().flatten())
        .map(|id| SharedImage(id, key.2));
    Some(lib.art.remember_packed(key, Arc::new(img), shared, Purpose::Display))
}

#[cfg(test)]
fn load(lib: &Library, key: &Key) -> Option<Arc<Image>> {
    let weak = lib.me.clone();
    let key = key.clone();
    super::shelf_browse::wait(async move { Ok(load_async(&weak, key, Purpose::Display).await) }).ok().flatten().map(|load| load.image)
}

// Existing 150/50px art is preferable to a placeholder when offline or a
// provider fails. Resize on the art worker, never during a UI frame. Do not
// persist this lower-resolution fallback over the desired server-size cache.
fn cached_other_size(blocking: &crate::rt::Blocking, lib: &Library, key: &Key) -> Option<Loaded> {
    for size in [320, NOW_PLAYING_SIZE, THUMBNAIL_SIZE] {
        if size == key.2 { continue; }
        let alternate = (key.0.clone(), key.1.clone(), size);
        if let Some(image) = lib.art.cached_image(blocking, &alternate, key.2, Purpose::Display) { return Some(image); }
    }
    None
}

async fn decode(bytes: Option<Vec<u8>>, size: u32) -> Option<Image> {
    let bytes = bytes?;
    crate::rt::spawn_blocking(move |_| Image::decode_square(&bytes, size)).await.ok().flatten()
}

async fn cover(client: &subsonic::Client, id: &str, size: u32) -> Option<Image> {
    let request = async { client.fetch_bytes_async(&client.cover_art_request(id, Some(size)), 4*1024*1024).await };
    let bytes = tokio::time::timeout(NETWORK_LIMIT, request).await.ok().and_then(Result::ok);
    decode(bytes, size).await
}

async fn url_image(client: &subsonic::Client, url: &str, size: u32) -> Option<Image> {
    let request = async { client.fetch_image_url_async(url, 4*1024*1024).await };
    decode(tokio::time::timeout(NETWORK_LIMIT, request).await.ok().and_then(Result::ok), size).await
}

async fn fetch(client: Arc<subsonic::Client>, source: Source, size: u32) -> Option<Image> {
    match source {
        Source::Cover(id) => cover(&client, &id, size).await,
        Source::Artist { id, cover: art, url } => {
            if let Some(id) = art.filter(|s| !s.is_empty()) {
                if let Some(image) = cover(&client, &id, size).await { return Some(image); }
            }
            if let Some(url) = url.filter(|s| !s.is_empty()) {
                if let Some(image) = url_image(&client, &url, size).await { return Some(image); }
            }
            let call = subsonic::calls::artist_info(&id);
            let info = client.get_async(&call).await.ok()?;
            for url in info.image_urls() {
                if let Some(image) = url_image(&client, url, size).await { return Some(image); }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_worker_only_takes_current_window_and_never_duplicates_a_load() {
        let key = |id: &str| (String::new(), Source::Cover(id.into()), 150);
        let mut work = Work::default();
        let now = Instant::now();
        work.prefetch(key("near"));
        work.window.insert(key("near"));
        work.enqueue(key("visible elsewhere"));
        assert_eq!(work.take_ready(now, true, true, true), Some(key("near")));
        work.enqueue(key("near"));
        assert!(work.take_ready(now, true, true, true).is_none());
        assert_eq!(work.ready(now, true, true), Some(key("visible elsewhere")));
        assert_eq!(work.active.len(), 2);
    }

    #[test]
    fn legacy_art_is_read_only_even_when_invalid() {
        let base = super::super::generated_test_directory("reused-art");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let old = base.join("card/Q2Navidrome");
        std::fs::create_dir_all(old.join("cache/art")).unwrap();
        let paths = crate::config::Paths::installed(base.join("internal"), base.join("card/Q2Mainline"), base.join("run/media"), true, Some(old.clone()));
        let lib = Library::open(paths, &crate::config::Config::default(), Arc::new(|_| {}));
        let key = (String::new(), Source::Cover("generated".into()), 50);
        let primary = lib.art.path(&key);
        let legacy = old.join("cache/art").join(primary.file_name().unwrap());
        let png = Image::placeholder(50).png().unwrap();
        std::fs::write(&legacy, &png).unwrap();
        assert!(load(&lib, &key).is_some());
        assert!(!primary.exists());
        assert_eq!(std::fs::read(&legacy).unwrap(), png);
        std::fs::write(&legacy, b"invalid generated image").unwrap();
        assert!(load(&lib, &key).is_none());
        assert_eq!(std::fs::read(&legacy).unwrap(), b"invalid generated image");
    }

    #[test]
    fn flat_art_from_an_earlier_version_loads_until_moved_and_new_art_is_packed() {
        let root = super::super::generated_test_directory("art-layout");
        let lib = Library::open(crate::config::Paths::new(root.clone()), &crate::config::Config::default(), Arc::new(|_|{}));
        let old = (String::new(), Source::Cover("generated-old".into()), THUMBNAIL_SIZE);
        let sharded = lib.art.path(&old);
        assert_eq!(sharded.parent().unwrap().parent().unwrap(), lib.paths.art_cache());
        let flat = lib.paths.art_cache().join(sharded.file_name().unwrap());
        std::fs::write(&flat, Image::placeholder(THUMBNAIL_SIZE).png().unwrap()).unwrap();
        assert!(load(&lib, &old).is_some(), "flat file read in place");
        crate::shard::migrate(&crate::rt::Blocking::dedicated_thread(), &lib.paths, crate::shard::Store::Art, &[sharded.file_name().unwrap().into()]);
        assert!(sharded.is_file() && !flat.exists());
        *lib.art.memory.lock().unwrap() = Memory::default();
        assert!(load(&lib, &old).is_some(), "moved file read from its shard");
        let new = (String::new(), Source::Cover("generated-new".into()), THUMBNAIL_SIZE);
        assert!(publish_cache(&crate::rt::Blocking::dedicated_thread(), &lib, &new, Some(Image::placeholder(THUMBNAIL_SIZE))).is_some());
        let written = lib.art.path(&new);
        let reopened = super::super::art_pack::ArtPack::new(lib.paths.art_cache());
        let png = reopened.get(&crate::rt::Blocking::dedicated_thread(), &ArtCache::digest(&new)).unwrap().unwrap();
        assert_eq!(image::load_from_memory(&png).unwrap().width(), THUMBNAIL_SIZE);
        assert!(!written.exists());
        assert!(!lib.paths.art_cache().join(written.file_name().unwrap()).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_slideshow_uses_cached_smaller_cover() {
        let root = super::super::generated_test_directory("art-fallback");
        let lib = Library::open(crate::config::Paths::new(root.clone()), &crate::config::Config::default(), Arc::new(|_|{}));
        let small = (String::new(), Source::Cover("fixture".into()), NOW_PLAYING_SIZE);
        put_cached(&lib, &small, Image::placeholder(NOW_PLAYING_SIZE).png().unwrap()).unwrap();
        let large = (String::new(), Source::Cover("fixture".into()), 320);
        let image = load(&lib, &large).expect("offline thumbnail fallback");
        assert_eq!((image.width,image.height),(320,320));
        let blocking = crate::rt::Blocking::dedicated_thread();
        publish_cache(&blocking, &lib, &small, Some(Image::placeholder(NOW_PLAYING_SIZE)));
        std::fs::remove_file(lib.art.path(&small)).unwrap();
        assert!(load(&lib, &large).is_some(), "packed-only offline fallback");
        assert!(lib.art.pack.get(&blocking, &ArtCache::digest(&large)).unwrap().is_none());
        assert!(!lib.art.path(&large).exists(), "do not persist a low-resolution substitute over full-size art");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn screen_off_defers_retries_but_allows_fresh_prefetch() {
        let mut work = Work::default();
        let now = Instant::now();
        let old = (String::new(), Source::Cover("old".into()), 50);
        let next = (String::new(), Source::Cover("next".into()), 50);
        work.retry.insert(old.clone(), now);
        work.enqueue(old);
        work.prefetch(next.clone());
        assert!(work.ready(now, false, true) == Some(next));
        assert!(work.ready(now, false, true).is_none());
        assert!(work.ready(now, true, true).is_some());
    }

    #[test]
    fn artist_falls_back_from_broken_cover_and_url_to_info_webp() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = base.clone();
        let worker = std::thread::spawn(move || {
            let mut webp = std::io::Cursor::new(Vec::new());
            ::image::RgbImage::from_pixel(80, 40, ::image::Rgb([10, 220, 30]))
                .write_to(&mut webp, ::image::ImageFormat::WebP).unwrap();
            for route in ["/rest/getCoverArt?", "/broken", "/rest/getArtistInfo2?", "/small.webp"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(request.starts_with(&format!("GET {route}")), "{request}");
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() { break; }
                }
                let (status, typ, body) = if route.contains("getArtistInfo2") {
                    ("200 OK", "application/json", serde_json::to_vec(&serde_json::json!({
                        "subsonic-response": { "status":"ok", "artistInfo2": { "smallImageUrl": format!("{server}/small.webp") } }
                    })).unwrap())
                } else if route == "/small.webp" {
                    ("200 OK", "image/webp", webp.get_ref().clone())
                } else { ("404 Not Found", "text/plain", b"missing".to_vec()) };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: {typ}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        let client = subsonic::Client::new(subsonic::Credentials {
            server: base.clone(), user: "test".into(), auth: subsonic::Auth::Password("test".into()),
        }, "test");
        let image = super::super::shelf_browse::wait(async move { Ok(fetch(Arc::new(client), Source::Artist {
            id: "artist".into(), cover: Some("bad-cover".into()), url: Some(format!("{base}/broken")),
        }, 50).await) }).unwrap().unwrap();
        assert_eq!(image.width, 50);
        assert!(image.pixels.iter().all(|p| (p >> 8) & 255 > 200));
        worker.join().unwrap();
    }

    fn fixture() -> (Arc<Library>, PathBuf, std::sync::mpsc::Receiver<()>) {
        let root = super::super::generated_test_directory("art-owner");
        let (send, events) = std::sync::mpsc::channel();
        let (lib, _) = Library::create(crate::config::Paths::new(root.clone()),
            &crate::config::Config::default(), Arc::new(move |event| {
                if matches!(event, LibEvent::ArtReady) { let _ = send.send(()); }
            }));
        (lib, root, events)
    }

    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        while !condition() {
            assert!(Instant::now() < deadline, "art owner failed to reach the expected state");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn packed_track_aliases_share_worker_pixels_by_requested_size() {
        let (lib, root, events) = fixture();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let small = THUMBNAIL_SIZE;
        let large = NOW_PLAYING_SIZE;
        let first = |id: &str, size| ArtCache::digest(&(String::new(), Source::Cover(id.into()), size));
        let png = Image::placeholder(small).png().unwrap();
        let distinct = Image { width: small, height: small, pixels: vec![0x443322; (small * small) as usize] }.png().unwrap();
        lib.art.pack.put(&blocking, &[
            (&first("track-a", small), &png),
            (&first("track-b", small), &png),
            (&first("track-b", large), &png),
            (&first("track-c", small), &distinct),
        ]).unwrap();

        ArtCache::start(&lib);
        // Enqueue a window and a frame together to exercise both worker paths.
        lib.art.warm_window(["track-a", "track-b"].into_iter(), small);
        let _ = lib.art.frame_covers([Some("track-a"), Some("track-b"), Some("track-c")], small);
        lib.art.get("track-b", large);
        for _ in 0..4 {
            events.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        }
        let a = lib.art.peek_cover("track-a", small).unwrap();
        let b = lib.art.peek_cover("track-b", small).unwrap();
        let c = lib.art.peek_cover("track-c", small).unwrap();
        let larger = lib.art.peek_cover("track-b", large).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        assert!(c.pixels.iter().all(|&pixel| pixel == 0x443322));
        assert_eq!((larger.width, larger.height), (large, large));
        assert_eq!(lib.art.memory().bytes, bytes(&a) + bytes(&c) + bytes(&larger));

        lib.art.shrink_memory(0);
        assert!(lib.art.peek_cover("track-a", small).is_none());
        assert!(lib.art.peek_cover("track-b", small).is_none());
        lib.art.get("track-b", small);
        events.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let reloaded = lib.art.peek_cover("track-b", small).unwrap();
        assert!(!Arc::ptr_eq(&a, &reloaded));
        assert_eq!(a.pixels, reloaded.pixels);

        lib.art.set_namespace("replacement account");
        assert!(lib.art.peek_cover("track-b", small).is_none());
        assert!(lib.art.memory().shared.is_empty());
        drop(lib);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_publish_shares_pixels_and_bounds_track_aliases() {
        let (lib, root, _) = fixture();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let track = |id: usize| (String::new(), Source::Cover(format!("track-{id}")), THUMBNAIL_SIZE);
        let load = |id| publish_cache(&blocking, &lib, &track(id), Some(Image::placeholder(THUMBNAIL_SIZE))).unwrap();
        let first = load(0);
        let next = load(1);
        assert!(Arc::ptr_eq(&first.image, &next.image));
        let expected = first.image;
        let mut memory = lib.art.memory();
        for id in 0..=MEMORY_ENTRIES {
            memory.put(track(id), Loaded { image: expected.clone(), shared: first.shared });
        }
        assert_eq!(memory.aliases.len(), MEMORY_ENTRIES);
        assert!(memory.get(&track(0)).is_none());
        assert!(Arc::ptr_eq(&memory.get(&track(MEMORY_ENTRIES)).unwrap(), &expected));
        assert_eq!(memory.bytes, bytes(&expected));
        assert_eq!(memory.shared.len(), 1);
        drop(memory);
        drop(lib);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn async_owner_releases_idle_library_and_exits_on_drop_without_periodic_polling() {
        let (lib, root, _) = fixture();
        let weak = Arc::downgrade(&lib);
        let wake = Arc::downgrade(&lib.art.wake);
        ArtCache::start(&lib);
        // A real queued cache miss proves the owner has run before idle/drop.
        lib.art.get("missing", 50);
        until(|| !lib.art.work.lock().unwrap().retry.is_empty());
        lib.art.set_visible(false);
        until(|| Arc::strong_count(&lib) == 1);
        drop(lib);
        until(|| weak.upgrade().is_none() && wake.upgrade().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn async_owner_cancels_obsolete_body_and_publishes_only_replacement_cache() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (entered, seen) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + crate::library::TEST_LIVENESS;
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "generated art request never arrived");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("generated listener failed: {error}"),
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            loop { let mut line = String::new(); reader.read_line(&mut line).unwrap(); if line == "\r\n" { break; } }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx").unwrap();
            entered.send(()).unwrap();
            match socket.read(&mut [0]) {
                Ok(0) => {},
                Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => {},
                result => panic!("obsolete art retained its socket: {result:?}"),
            }
        });
        let (lib, root, events) = fixture();
        *lib.client.write().unwrap() = Some(Arc::new(subsonic::Client::new(subsonic::Credentials {
            server: base.clone(), user: "generated".into(), auth: subsonic::Auth::Password("generated".into()),
        }, "generated-test")));
        lib.online.store(true, Ordering::Relaxed);
        lib.art.set_namespace(&format!("{base}|generated"));
        ArtCache::start(&lib);
        lib.art.get("old", 50);
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        // Neither the network stall nor its decoder occupies the DB/file pool.
        let (send, received) = std::sync::mpsc::channel();
        crate::rt::spawn_blocking(move |_| { send.send(()).unwrap(); });
        received.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let replacement = ("replacement".into(), Source::Cover("new".into()), 50);
        put_cached(&lib, &replacement, Image::placeholder(50).png().unwrap()).unwrap();
        lib.art.set_namespace("replacement");
        lib.art.get("new", 50);
        events.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(lib.art.peek_cover("new", 50).is_some());
        assert!(lib.art.peek_cover("old", 50).is_none());
        server.join().unwrap();
        let weak = Arc::downgrade(&lib);
        drop(lib);
        until(|| weak.upgrade().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn async_owner_keeps_download_retry_dark_until_wake_and_loads_fresh_prefetch() {
        let (lib, root, events) = fixture();
        ArtCache::start(&lib);
        lib.art.get("committed", 50);
        until(|| !lib.art.work.lock().unwrap().retry.is_empty());
        lib.art.set_visible(false);
        for id in ["committed", "fresh"] {
            let key = (String::new(), Source::Cover(id.into()), 50);
            put_cached(&lib, &key, Image::placeholder(50).png().unwrap()).unwrap();
        }
        lib.art.downloaded_cover("committed");
        lib.art.prefetch_cover("fresh", 50);
        events.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(lib.art.peek_cover("fresh", 50).is_some());
        assert!(lib.art.peek_cover("committed", 50).is_none());
        assert!(lib.art.work.lock().unwrap().retry.keys().any(|key| key.1 == Source::Cover("committed".into())));
        lib.art.set_visible(true);
        events.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(lib.art.peek_cover("committed", 50).is_some());
        let weak = Arc::downgrade(&lib);
        drop(lib);
        until(|| weak.upgrade().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn put_cached(lib: &Library, key: &Key, bytes: Vec<u8>) -> std::io::Result<()> {
        let path = lib.art.path(key);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, bytes)
    }
    fn key(id: &str) -> Key { ("server".into(), Source::Cover(id.into()), 1) }

    #[test]
    fn memory_evicts_least_recent() {
        let mut m = Memory::default();
        let big = |n: u32| Image { width: n, height: n, pixels: vec![0; (n * n) as usize] }.into();
        // Each is 1 MiB: three fit, a fourth pushes the oldest out.
        m.put(key("a"), big(512));
        m.put(key("b"), big(512));
        m.put(key("c"), big(512));
        assert!(m.get(&key("a")).is_some());
        m.put(key("d"), big(512));
        assert!(m.get(&key("b")).is_none(), "b was least recently used");
        assert!(m.get(&key("a")).is_some());
        assert!(m.bytes <= MEMORY_BYTES);
        // Closing Cover Flow keeps the most recent megabyte.
        m.shrink(MEMORY_AFTER_BROWSING);
        assert_eq!((m.local.len(), m.bytes), (1, 1024 * 1024));
        assert!(m.get(&key("a")).is_some(), "the most recently used stays");
    }

    #[test]
    fn prefetch_yields_while_drawn_covers_do_not() {
        let mut work = Work::default();
        let now = Instant::now();
        work.prefetch(key("ahead"));
        work.enqueue(key("drawn"));
        assert!(work.ready(now, true, false) == Some(key("drawn")), "a drawn cover never waits");
        work.finish(&key("drawn"), false, now);
        assert!(work.ready(now, true, false).is_none(), "prefetch waits while the server is busy for the listener");
        assert!(work.ready(now, true, true) == Some(key("ahead")));
        // A failed prefetch stays a prefetch.
        work.finish(&key("ahead"), true, now);
        assert!(work.ready(now + Duration::from_secs(31), true, false).is_none());
        // Drawing it makes it a drawn request.
        work.enqueue(key("ahead"));
        assert!(work.ready(now + Duration::from_secs(31), true, false) == Some(key("ahead")));
    }

    #[test]
    fn retries_without_duplicate_requests_or_busy_loop() {
        let mut work = Work::default();
        let now = Instant::now();
        work.enqueue(key("a"));
        assert!(work.ready(now, true, true) == Some(key("a")));
        work.enqueue(key("a"));
        assert!(work.queue.is_empty(), "in-flight request is not duplicated");
        work.active.clear();
        work.retry.insert(key("a"), now + Duration::from_secs(30));
        work.enqueue(key("a"));
        work.enqueue(key("a"));
        assert_eq!(work.queue.len(), 1);
        assert!(work.ready(now, true, true).is_none());
        assert_eq!(work.delay(now), Duration::from_secs(30));
        assert!(work.ready(now + Duration::from_secs(30), true, true) == Some(key("a")));
    }

    #[test]
    fn committed_cover_retries_an_active_failure_once_without_an_input_redraw() {
        let mut work = Work::default();
        let now = Instant::now();
        let cover = key("downloaded");
        work.enqueue(cover.clone());
        assert!(work.ready(now, true, true) == Some(cover.clone()));
        assert!(work.downloaded_cover("downloaded", now));
        assert!(work.queue.is_empty(), "do not duplicate the active load");
        work.finish(&cover, true, now);
        assert!(work.ready(now, true, true) == Some(cover.clone()));
        work.finish(&cover, true, now);
        assert!(work.ready(now, true, true).is_none(), "one commit cannot create a busy retry loop");
        assert_eq!(work.delay(now), Duration::from_secs(30));
        assert!(work.ready(now + Duration::from_secs(30), true, true) == Some(cover.clone()));
        work.finish(&cover, false, now + Duration::from_secs(30));
        assert!(work.queue.is_empty());
    }

    #[test]
    fn committed_cover_does_not_retry_unrelated_or_successful_active_loads() {
        let now = Instant::now();
        for source in [Source::Cover("other".into()), Source::Artist { id: "downloaded".into(), cover: None, url: None }] {
            let mut work = Work::default();
            let unrelated = ("server".into(), source, 1);
            work.enqueue(unrelated.clone());
            assert!(work.ready(now, true, true) == Some(unrelated.clone()));
            assert!(!work.downloaded_cover("downloaded", now));
            work.finish(&unrelated, true, now);
            assert!(work.ready(now, true, true).is_none());
            assert_eq!(work.delay(now), Duration::from_secs(30));
        }
        let mut work = Work::default();
        let cover = key("downloaded");
        work.enqueue(cover.clone());
        assert!(work.ready(now, true, true) == Some(cover.clone()));
        assert!(work.downloaded_cover("downloaded", now));
        work.finish(&cover, false, now);
        assert!(work.ready(now, true, true).is_none(), "a successful active load needs no repeat");
        assert!(work.retry.is_empty());
    }

    #[test]
    fn committed_cover_preserves_dark_retry_gating_and_unrelated_deadlines() {
        let now = Instant::now();
        for active in [false, true] {
            let mut work = Work::default();
            let cover = key("downloaded");
            let unrelated = key("other");
            work.retry.insert(unrelated.clone(), now + Duration::from_secs(30));
            work.enqueue(unrelated.clone());
            work.enqueue(cover.clone());
            if active {
                assert!(work.ready(now, true, true) == Some(cover.clone()));
            } else {
                work.retry.insert(cover.clone(), now + Duration::from_secs(30));
            }
            assert!(work.downloaded_cover("downloaded", now));
            if active { work.finish(&cover, true, now); }
            assert!(work.ready(now, false, true).is_none(), "committed art must not start dark presentation work");
            assert_eq!(work.retry.get(&unrelated), Some(&(now + Duration::from_secs(30))));
            let fresh = key("fresh");
            work.prefetch(fresh.clone());
            assert!(work.ready(now, false, true) == Some(fresh.clone()), "normal fresh prefetch still works");
            work.finish(&fresh, false, now);
            assert!(work.ready(now, true, true) == Some(cover.clone()), "screen wake retries the committed cover immediately");
            work.finish(&cover, false, now);
            assert!(work.ready(now, true, true).is_none(), "unrelated art keeps its original backoff");
            assert!(work.ready(now + Duration::from_secs(30), true, true) == Some(unrelated));
        }
    }

    #[test]
    fn prefetch_warms_both_sizes_and_yields_to_visible_art() {
        let cache = ArtCache::new(PathBuf::from("unused"));
        cache.set_namespace("server");
        let song = subsonic::Song { cover_art: Some("upcoming".into()), ..Default::default() };
        cache.prefetch_song(&song);
        cache.prefetch_song(&song);
        let mut work = cache.work.lock().unwrap();
        assert_eq!(work.queue.len(), 2);
        assert!(work.queue.iter().any(|k| k.2 == NOW_PLAYING_SIZE));
        assert!(work.queue.iter().any(|k| k.2 == THUMBNAIL_SIZE));
        work.enqueue(key("visible"));
        assert!(work.ready(Instant::now(), true, true) == Some(key("visible")));
    }

    #[test]
    fn cache_keys_separate_servers_and_do_not_expose_urls() {
        let cache = ArtCache::new(PathBuf::from("cache"));
        assert_ne!(cache.path(&key("a:b")), cache.path(&key("a/b")));
        let mut other = key("a:b"); other.0 = "other-server".into();
        assert_ne!(cache.path(&key("a:b")), cache.path(&other));
        let source = Source::Artist { id: "artist".into(), cover: None, url: Some("https://image.invalid/?secret=example".into()) };
        let path = cache.path(&("server".into(), source, 52));
        assert_eq!(path.file_name().unwrap().len(), 36);
        assert!(!path.to_string_lossy().contains("secret"));
    }
}
