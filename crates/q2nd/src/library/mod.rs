#![forbid(unsafe_code)]
//! The library: Navidrome through a cache on the card. Every response is
//! kept, so whatever was browsed once can be browsed offline; streamed songs
//! are kept up to a cap; downloads are kept until removed.

pub mod art;
mod art_migration;
pub(crate) mod art_pack;
pub mod db;
pub mod spoken;
pub mod book_info;
mod resumable;
mod shelf;
pub(crate) use shelf::SpokenQueueRow;
pub mod shelf_auto_sync;
pub mod shelf_browse;
pub use shelf::validate_spoken_server;
mod downloads;
pub(crate) mod music_reconcile;
mod cache_storage;
mod storage_downloads;
mod metadata;
mod music_actions;
mod direct_listens;
pub(crate) mod music_catalog;
mod music_remote;
mod play_queue;
pub(crate) mod db_worker;
mod offline_art;
mod download_intent;
mod download_actions;
mod offline_groups;
mod mix_space;
mod refill_log;
mod prefetch;
mod sync_wait;
#[cfg(test)]
mod readiness_tests;
mod cache_lookup;
mod configuration;
mod transfer_io;
mod streaming;
mod layout;
mod health;
#[cfg(test)]
mod health_tests;
pub(crate) use health::with_priority;
pub use q2_machines::server::{Priority, Status as ServerStatus};
pub(crate) use streaming::TransferUse;
use streaming::{Transfer, InFlight, fill_percent};
#[cfg(test)]
use streaming::{retain_obsolete, transfer_complete};
#[cfg(test)]
mod queue_barrier_tests;
use refill_log::Event as RefillEvent;

use q2_machines::transfer as xfer;
use crate::audio::player::{OpenedTrack, Origin, TrackSource};
use crate::audio::source::{CachePublication, Progress, ProgressiveReader};
use crate::config::{Config, Paths, Quality};
use db::{Db, OfflineSet, PendingCall};
use serde::{Deserialize, Serialize};
use subsonic::Streamed;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use subsonic::{Call, Client, ItemKind, Request, Song, StreamOptions, calls};

/// One account-bound stored Downloads page plus current in-memory progress.
pub(crate) struct DownloadInventory {
    pub status: downloads::DownloadStatus,
    pub sets: Vec<(OfflineSet, usize)>,
    pub queued: std::collections::HashSet<String>,
    pub offline: std::collections::HashSet<String>,
    pub failed: std::collections::HashSet<String>,
    pub active: Option<(String,Option<f32>)>,
}

/// Owned availability for a finite row projection; no UI worker reads SQLite.
pub(crate) struct MusicRowSnapshot {
    pub songs: Vec<Song>,
    pub view: DownloadView,
    pub local: std::collections::HashSet<String>,
    pub online: bool,
}

/// Download state for row badges (DOWNLOAD-002): queued songs, the song
/// downloading now, and each downloaded or requested album's songs.
#[derive(Debug, Default, Clone)]
pub struct DownloadView {
    pub queued: std::collections::HashSet<String>,
    pub active: Option<(String, Option<f32>)>,
    pub albums: std::collections::HashMap<String, Vec<String>>,
    /// Songs on the card.
    pub offline: std::collections::HashSet<String>,
}

/// A row's download state: on the card, queued, or downloading (0-1, if known).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DownloadState { None, Done, Queued, Active(Option<f32>) }

impl DownloadView {
    pub fn downloaded(&self, id: &str) -> bool { self.offline.contains(id) }

    pub fn song(&self, id: &str, downloaded: bool) -> DownloadState {
        if let Some((active, f)) = &self.active { if active == id { return DownloadState::Active(*f); } }
        if self.queued.contains(id) { DownloadState::Queued }
        else if downloaded { DownloadState::Done } else { DownloadState::None }
    }
    /// An album: done when none of its songs wait; while they download, the
    /// share of its songs done (with the active song's progress).
    pub fn album(&self, id: &str) -> DownloadState {
        let Some(songs) = self.albums.get(id) else { return DownloadState::None };
        let waiting = songs.iter().filter(|s| self.queued.contains(*s)).count();
        let active = self.active.as_ref().filter(|(s, _)| songs.contains(s));
        if waiting == 0 && active.is_none() {
            // Failed or dropped songs leave the queue without a file: the
            // album is not downloaded until every song is on the card.
            return if songs.iter().all(|s| self.offline.contains(s)) { DownloadState::Done } else { DownloadState::None };
        }
        match active {
            Some((s, f)) if !songs.is_empty() => {
                let done = songs.len() - waiting - usize::from(!self.queued.contains(s));
                DownloadState::Active(Some(((done as f32 + f.unwrap_or(0.0)) / songs.len() as f32).clamp(0.0, 1.0)))
            }
            _ => DownloadState::Queued,
        }
    }
}

/// What the library tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum LibEvent {
    /// A player stream waits for the dozing radio (doze policy: wake it).
    RadioWanted,
    ArtReady,
    RadiosUpdated,
    SpokenChanged,
    SpokenProgress,
    ShelfSync,
    Online(bool),
    Downloads,
    PlaylistFollowChanged(String),
    /// A background failure the listener should hear about, already in plain
    /// words and reported (UX-006).
    Notice(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LibError {
    NotConfigured,
    /// The server can't be reached and nothing is cached.
    Offline,
    /// Wrong user name, password or API key.
    Auth,
    Server(String),
}

/// Why the last Navidrome request that fell back as [`LibError::Offline`]
/// failed: 0 not known, 1 unreachable (refused, DNS, no route), 2 a server
/// error (5xx), 3 timed out. It chooses the words shown for `Offline`, which
/// otherwise says "Not available offline" while the device is online (UX-006).
/// Cleared when the server answers or Offline Mode changes, and valid only
/// for the link it was observed on: a radio doze, its end and each new
/// address start a new link epoch, so an older failure never names the
/// cause of a later Offline (V0.128 read "Can't reach Navidrome" for it).
/// Stored as `cause | epoch << 8`.
#[cfg(not(test))]
static OFFLINE_CAUSE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(not(test))]
static LINK_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(test)]
thread_local! {
    static OFFLINE_CAUSE: std::sync::atomic::AtomicU32 = const { std::sync::atomic::AtomicU32::new(0) };
    static LINK_EPOCH: std::sync::atomic::AtomicU32 = const { std::sync::atomic::AtomicU32::new(0) };
}
fn with_cause<R>(f: impl FnOnce(&std::sync::atomic::AtomicU32, &std::sync::atomic::AtomicU32) -> R) -> R {
    #[cfg(not(test))] { f(&OFFLINE_CAUSE, &LINK_EPOCH) }
    #[cfg(test)] { OFFLINE_CAUSE.with(|cause| LINK_EPOCH.with(|epoch| f(cause, epoch))) }
}
fn offline_cause() -> u8 {
    with_cause(|cause, epoch| {
        let stored = cause.load(Ordering::Relaxed);
        if stored >> 8 == epoch.load(Ordering::Relaxed) & 0x00FF_FFFF { stored as u8 } else { 0 }
    })
}
fn set_offline_cause(value: u8) {
    with_cause(|cause, epoch| cause.store(u32::from(value) | (epoch.load(Ordering::Relaxed) & 0x00FF_FFFF) << 8, Ordering::Relaxed));
}
/// The link changed (a doze began or ended, an address arrived): causes
/// observed before it no longer describe an Offline.
fn link_changed() {
    with_cause(|_, epoch| { epoch.fetch_add(1, Ordering::Relaxed); });
}
fn cause_of(error: &subsonic::Error) -> u8 {
    match error {
        subsonic::Error::Http(status) if *status >= 500 => 2,
        subsonic::Error::Network(message) if message.contains("timed out") || message.contains("timeout") => 3,
        subsonic::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => 3,
        _ => 1,
    }
}

impl std::fmt::Display for LibError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibError::NotConfigured => write!(f, "No server set up"),
            LibError::Offline => f.write_str(match offline_cause() {
                1 => crate::ux::Code::NavidromeUnreachable.message(),
                2 => crate::ux::Code::ServerError.message(),
                3 => crate::ux::Code::Timeout.message(),
                _ => "Not available offline",
            }),
            LibError::Auth => write!(f, "Wrong user name or password"),
            LibError::Server(m) => write!(f, "{m}"),
        }
    }
}

impl From<subsonic::Error> for LibError {
    fn from(e: subsonic::Error) -> Self {
        if e.is_auth() {
            LibError::Auth
        } else if e.is_offline() {
            set_offline_cause(cause_of(&e));
            LibError::Offline
        } else {
            LibError::Server(e.to_string())
        }
    }
}

#[cfg(test)]
mod offline_cause_tests {
    use super::*;

    /// "Not available offline" only when the cause is not known: a server
    /// error, a timeout or an unreachable server say so, with their own code.
    #[test]
    fn offline_fallback_names_what_failed() {
        set_offline_cause(0);
        assert_eq!(LibError::Offline.to_string(), "Not available offline");
        for (error, shown, code) in [
            (subsonic::Error::Http(503), "The server had a problem", crate::ux::Code::ServerError),
            (subsonic::Error::Network("timed out reading response".into()), "The server took too long", crate::ux::Code::Timeout),
            (subsonic::Error::Network("dns error: failed to lookup address information".into()), "Can't reach Navidrome", crate::ux::Code::NavidromeUnreachable),
            (subsonic::Error::Network("Connection refused (os error 111)".into()), "Can't reach Navidrome", crate::ux::Code::NavidromeUnreachable),
        ] {
            let error = LibError::from(error);
            assert_eq!(error, LibError::Offline, "still the cache fallback");
            assert_eq!(error.to_string(), shown);
            assert_eq!(crate::ux::Code::classify(&error.to_string()), Some(code));
        }
        set_offline_cause(0);
        assert_eq!(LibError::Offline.to_string(), "Not available offline");
    }

    /// A player stream's request waits while the radio dozes and until a
    /// new address after it wakes, asks the doze policy to wake it, and
    /// gives up after the reconnect allowance; cancellation ends the wait.
    #[test]
    fn a_player_stream_waits_for_the_dozing_radio_to_link_within_a_bound() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        let (lib, _) = Library::create(Paths::new(generated_test_directory("radio-wait")), &Config::default(),
            Arc::new(move |event| seen.lock().unwrap().push(format!("{event:?}"))));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
        runtime.block_on(async {
            let cancel = transfer_io::Cancel::default();
            assert!(lib.await_radio_for_playback(&cancel).await, "no doze: no wait");
            lib.set_radio_doze(true);
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            {
                let wait = lib.await_radio_for_playback(&cancel);
                tokio::pin!(wait);
                assert!(wait.as_mut().poll(&mut cx).is_pending());
                assert!(lib.playback_wants_radio(), "the doze policy is asked to wake the radio");
                assert!(events.lock().unwrap().iter().any(|event| event == "RadioWanted"));
                lib.set_radio_doze(false);
                tokio::task::yield_now().await;
                assert!(wait.as_mut().poll(&mut cx).is_pending(), "awake but no address yet");
                lib.network_up();
                assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(true));
            }
            assert!(!lib.playback_wants_radio());
            // No link within the allowance: the request goes out and fails.
            lib.set_radio_doze(true);
            let started = tokio::time::Instant::now();
            assert!(!lib.await_radio_for_playback(&cancel).await);
            assert_eq!(started.elapsed(), RADIO_WAKE_WAIT);
            assert!(!lib.playback_wants_radio());
            // A skip cancels the transfer: the wait ends with it.
            let wait = lib.await_radio_for_playback(&cancel);
            tokio::pin!(wait);
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            cancel.cancel();
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(false));
            // Known down (Wi-Fi off) after a wake: no wait for an address.
            lib.set_radio_doze(false);
            lib.set_transport(true);
            lib.set_transport(false);
            assert!(lib.await_radio_for_playback(&transfer_io::Cancel::default()).await);
            assert!(!lib.maintenance_link_ready(), "the foreground timeout does not admit maintenance");
            lib.link_available();
            assert!(lib.maintenance_link_ready(), "Audiobookshelf can use a restored address even while Navidrome's tunnel is down");
            assert!(lib.health.transport_down());
            lib.set_transport(true);
            assert!(!lib.health.transport_down());
        });
    }

    /// A cause belongs to the link it was observed on: after a doze or a
    /// new address an Offline no longer claims the old one.
    #[test]
    fn an_offline_names_only_a_cause_observed_on_the_current_link() {
        let _ = LibError::from(subsonic::Error::Network("Connection refused (os error 111)".into()));
        assert_eq!(LibError::Offline.to_string(), "Can't reach Navidrome");
        link_changed();
        assert_eq!(LibError::Offline.to_string(), "Not available offline", "stale after the link changed");
        let _ = LibError::from(subsonic::Error::Http(503));
        assert_eq!(LibError::Offline.to_string(), "The server had a problem", "a failure on the new link names its cause");
        for _ in 0..(1u32 << 24) / 4096 { with_cause(|_, epoch| { epoch.fetch_add(4096, Ordering::Relaxed); }); }
        assert_eq!(LibError::Offline.to_string(), "The server had a problem", "the 24-bit epoch wraps consistently");
        link_changed();
        assert_eq!(LibError::Offline.to_string(), "Not available offline");
        set_offline_cause(0);
    }
}

/// Where a response may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    CacheOnly,
    /// Fresh cached metadata first; refresh at most once per day.
    PreferCache,
    NetworkOnly,
    /// The network, else the cache.
    NetworkFirst,
}

pub struct Fetched<T> {
    pub value: T,
}

/// A stored copy's parser, type-erased.
trait StoredParse: Send {
    /// False once a piece is refused.
    fn feed(&mut self, piece: &[u8]) -> bool;
    fn finish(self: Box<Self>) -> Result<Parsed, LibError>;
}
struct StoredCopy<T: Streamed> {
    parser: subsonic::document::Parser<T>,
    error: Option<subsonic::Error>,
}
impl<T: Streamed + Send + 'static> StoredParse for StoredCopy<T> {
    fn feed(&mut self, piece: &[u8]) -> bool {
        if self.error.is_some() { return false; }
        self.error = self.parser.feed(piece).err();
        self.error.is_none()
    }
    fn finish(self: Box<Self>) -> Result<Parsed, LibError> {
        let StoredCopy { parser, error } = *self;
        if let Some(error) = error { return Err(error.into()); }
        parser.finish().map(|value| Box::new(value) as Parsed).map_err(LibError::from)
    }
}

/// A type-erased call result, so the cache and network logic is not generic.
type Parsed = Box<dyn std::any::Any + Send>;
type NetFuture = std::pin::Pin<Box<dyn std::future::Future<Output = subsonic::Result<(Parsed, u64)>> + Send>>;
/// Starts a call on the account's client, on the blocking pool: its parsed
/// result and its bytes.
type NetStart = Box<dyn FnOnce(&crate::rt::Blocking, &Arc<Client>) -> NetFuture + Send>;
/// One call's hooks for [`Library::fetch_value`].
struct Hooks {
    network: NetStart,
    /// A parser for a stored copy, fed as it is read.
    read: Arc<dyn Fn() -> Box<dyn StoredParse> + Send + Sync>,
    /// Writes the copy to store.
    write: Arc<dyn Fn(&(dyn std::any::Any + Send), &mut dyn std::io::Write) -> std::io::Result<()> + Send + Sync>,
}

/// Formats the player decodes itself; others are transcoded by the server.
const DECODABLE: [&str; 10] = ["flac", "mp3", "m4a", "mp4", "aac", "alac", "ogg", "oga", "wav", "m4b"];

// Speculative audio only: selected tracks and explicit offline downloads keep
// their normal streaming behavior. These limits use server metadata, not RAM
// size (song streams are written incrementally to the card).
/// Which upcoming songs to fetch now, given which are already on the card.
/// One at a time (`batch` 0): the first `prefetch` not on the card. In
/// bursts: nothing while the next song is on the card; once it is not, the
/// next `batch` songs back to back, so the radio wakes once per burst
/// (fable hunt R1: about 4.9 J of each fetch is size-independent).
/// `prefetch` 0 turns fetching ahead off in either mode.
fn fetch_targets(local: &[bool], prefetch: usize, batch: usize) -> Vec<usize> {
    if prefetch == 0 { return Vec::new(); }
    if batch == 0 {
        return (0..local.len().min(prefetch)).filter(|&i| !local[i]).collect();
    }
    if local.first().copied().unwrap_or(true) { return Vec::new(); }
    (0..local.len().min(batch)).filter(|&i| !local[i]).collect()
}

/// Fetch ahead by duration: nothing while at least `low_ms` of songs at the
/// front of `upcoming` are on the card; below that, every song not on the
/// card that can be fetched ahead (`eligible`), in order, until the cached
/// run covers `fetch_ms`. A song that cannot be fetched ahead (too long, a
/// station) streams when reached and counts as no cached time.
fn fetch_targets_minutes(local: &[bool], eligible: &[bool], durations_ms: &[u64], prefetch: usize, fetch_ms: u64, low_ms: u64) -> Vec<usize> {
    if prefetch == 0 { return Vec::new(); }
    let leading: u64 = local.iter().zip(durations_ms).take_while(|(l, _)| **l).map(|(_, d)| *d).sum();
    if leading >= low_ms { return Vec::new(); }
    let (mut total, mut targets) = (0u64, Vec::new());
    for i in 0..local.len().min(eligible.len()).min(durations_ms.len()) {
        if total >= fetch_ms { break; }
        if local[i] { total += durations_ms[i]; }
        else if eligible[i] { targets.push(i); total += durations_ms[i]; }
    }
    targets
}

const PREFETCH_MAX_SECONDS: u32 = 10 * 60;
/// Longest fetch-ahead waits for a player stream to finish downloading.
#[cfg(not(test))]
const PLAYBACK_PRIORITY_WAIT: Duration = Duration::from_secs(120);
#[cfg(test)]
const PLAYBACK_PRIORITY_WAIT: Duration = Duration::from_millis(600);
/// Longest a player stream waits for a dozing radio to wake and link: the
/// doze policy's reconnect allowance (q2_machines::doze::RECONNECT_MS).
const RADIO_WAKE_WAIT: Duration = Duration::from_millis(q2_machines::doze::RECONNECT_MS);

#[derive(Debug, PartialEq, Eq)]
enum PriorityWait { Clear, Offline, Newer, Timeout }
const PREFETCH_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Fetch-ahead starts this long after the latest request (a track change).
#[cfg(not(test))]
const PREFETCH_SETTLE: Duration = Duration::from_secs(8);
#[cfg(test)]
const PREFETCH_SETTLE: Duration = Duration::from_millis(0);

fn can_prefetch_audio(song: &Song, variant: &str) -> bool { prefetch_bytes(song, variant).is_some() }

/// The fetch-ahead targets the card can keep, in order. Eviction can free at
/// most the cache itself, so the room for new songs is the smaller of the
/// cache's cap and `free + used - reserve`. A song that does not fit in what
/// is left is not fetched: it would be published and evicted at once, or
/// evict the song fetched before it, and stream again when reached.
/// `free` is `None` when the card could not be asked; only the cap applies.
fn space_admitted(targets: &[usize], bytes: &[Option<u64>], cache_bytes: u64, reserve_bytes: u64, used: u64, free: Option<u64>) -> Vec<usize> {
    let card = free.map_or(u64::MAX, |free| free.saturating_add(used).saturating_sub(reserve_bytes));
    let mut room = cache_bytes.min(card);
    targets.iter().copied().filter(|&index| {
        // A song that cannot be fetched ahead is skipped by the caller and takes no room.
        let needed = bytes.get(index).copied().flatten().unwrap_or(0);
        if needed > room { return false; }
        room -= needed;
        true
    }).collect()
}

/// The bytes a fetch-ahead of this song is expected to write, or `None` when
/// it must not be fetched ahead.
fn prefetch_bytes(song: &Song, variant: &str) -> Option<u64> {
    if song.radio_url.is_some() || song.duration == 0 || song.duration > PREFETCH_MAX_SECONDS {
        return None;
    }
    let bytes = if variant == "raw" {
        // Unknown original sizes are not safe to speculate on. Bitrate alone
        // can understate VBR files and does not include container overhead.
        song.size.filter(|n| *n > 0)
    } else {
        // Original size does not describe the selected transcode. Include
        // headroom for MP3 headers/padding; this is an estimate, not a wire cap.
        variant.strip_prefix("mp3-").and_then(|s| s.parse::<u32>().ok())
            .filter(|br| *br > 0)
            .map(|br| u64::from(song.duration) * u64::from(br) * 125 + 64 * 1024)
    };
    bytes.filter(|n| *n <= PREFETCH_MAX_BYTES)
}

/// The cache variant for a song at a quality: `raw` or `mp3-<kbit/s>`.
pub fn variant(song: &Song, quality: Quality) -> String {
    let decodable = song.suffix.as_deref().is_some_and(|s| DECODABLE.contains(&s.to_ascii_lowercase().as_str()));
    match quality.bit_rate() {
        None if decodable => "raw".into(),
        None => "mp3-320".into(),
        Some(br) => {
            // An original the player decodes, at or under the target bitrate
            // (an AAC or MP3 at 256k, a low-rate FLAC), is no bigger than the
            // transcode and loses nothing: stream it as it is.
            if decodable && song.bit_rate.is_some_and(|b| b > 0 && b <= br) {
                "raw".into()
            } else {
                format!("mp3-{br}")
            }
        }
    }
}

/// Variants that will do for a song at a quality, best first.
fn acceptable(song: &Song, quality: Quality) -> Vec<String> {
    let want = variant(song, quality);
    let mut v = vec![want.clone()];
    // A better copy than asked for is fine too.
    for q in Quality::ALL {
        let other = variant(song, q);
        if !v.contains(&other) && rank(&other) >= rank(&want) {
            v.push(other);
        }
    }
    v.sort_by_key(|x| std::cmp::Reverse(rank(x)));
    v
}

fn rank(variant: &str) -> u32 {
    variant.strip_prefix("mp3-").and_then(|b| b.parse().ok()).unwrap_or(10_000)
}

fn stream_options(v: &str) -> StreamOptions {
    match v.strip_prefix("mp3-") {
        Some(br) => StreamOptions { format: Some("mp3".into()), max_bit_rate: br.parse().ok() },
        None => StreamOptions { format: Some("raw".into()), max_bit_rate: None },
    }
}

fn extension(song: &Song, variant: &str) -> String {
    if variant == "raw" { song.suffix.clone().unwrap_or_else(|| "bin".into()) } else { "mp3".into() }
}

/// File-name-safe form of a server id.
fn safe(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

struct QueueSave {
    songs: Vec<Song>,
    index: usize,
    position_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct SavedQueue {
    songs: Vec<Song>,
    index: usize,
    position_ms: u64,
}

#[derive(Clone)]
struct Settings {
    fill_target: Option<u8>,
    stream_quality: Quality,
    download_quality: Quality,
    cache_bytes: u64,
    reserve_bytes: u64,
    prefetch: usize,
    /// Fetch-ahead burst size (0: one at a time).
    batch: usize,
    /// Fetch ahead by duration when non-zero (overrides `batch`).
    fetch_ms: u64,
    fetch_low_ms: u64,
    scrobble: bool,
    sync_queue: bool,
}

impl From<&Config> for Settings {
    fn from(c: &Config) -> Self {
        Settings {
            fill_target: c.storage.fill_my_drive.then_some(c.storage.fill_target_percent.clamp(10, 90)),
            stream_quality: c.playback.stream_quality,
            download_quality: c.playback.download_quality,
            // Saturating: a huge configured size must not wrap to a tiny cap.
            cache_bytes: c.storage.stream_cache_mb.saturating_mul(1024 * 1024),
            reserve_bytes: c.storage.reserve_mb.saturating_mul(1024 * 1024),
            prefetch: c.storage.prefetch_songs as usize,
            batch: c.storage.fetch_batch as usize,
            fetch_ms: u64::from(c.storage.fetch_minutes) * 60_000,
            // Starts with the doze's reconnect allowance, so a wake always
            // finds songs to fetch (q2_machines::doze::RECONNECT_MS).
            fetch_low_ms: u64::from(c.storage.fetch_low_minutes) * 60_000 + q2_machines::doze::RECONNECT_MS,
            scrobble: c.playback.scrobble,
            sync_queue: c.playback.sync_queue,
        }
    }
}

fn settings_for(config: &Config, paths: &Paths) -> Settings {
    let mut settings = Settings::from(config);
    if paths.ephemeral_media {
        settings.cache_bytes = settings.cache_bytes.min(8 * 1024 * 1024);
        settings.reserve_bytes = 2 * 1024 * 1024;
        settings.prefetch = 0;
    }
    settings
}

pub struct MusicDownloadAction {intent:download_intent::Intent,generation:u32,configured:bool}
pub struct Library {
    pub paths: Paths,
    #[cfg(test)]
    db: Arc<Mutex<Db>>,
    /// SQLite for async code (db_worker.rs), over the same `db`.
    db_worker: db_worker::DbWorker,
    /// Bumped whenever `client` is replaced: a response fetched with an
    /// earlier client is not cached (fetch).
    client_generation: Arc<std::sync::atomic::AtomicU32>,
    bound_music_generation: Arc<AtomicU32>,
    bound_music_epoch: Arc<AtomicU32>,
    configuration: Mutex<configuration::State>,
    /// This library, for the synchronous wrappers that start tasks.
    me: std::sync::Weak<Library>,
    offline_art_gate: Arc<offline_art::PublicationGate>,
    client: RwLock<Option<Arc<Client>>>,
    music_feedback: Mutex<music_actions::Feedback>,
    direct_listens: direct_listens::Direct,
    music_epoch: Arc<AtomicU32>,
    music_write_failed: Arc<AtomicBool>,
    settings: RwLock<Settings>,
    spoken_settings: RwLock<crate::config::SpokenSettings>,
    shelf: RwLock<shelf::ShelfSlot>,
    /// Navidrome is not known unreachable (`health`): Online or Checking.
    online: AtomicBool,
    /// The reachability machine and its probe (health.rs).
    health: health::Health,
    /// Offline Mode: the network is never used.
    forced_offline: AtomicBool,
    radio_doze: AtomicBool,
    /// A fetch-ahead burst is running (doze waits for it to end).
    fetching: AtomicBool,
    /// The foreground queue has no missing near-term music.
    prefetch_ready: AtomicBool,
    last_probe: Mutex<Instant>,
    /// The network just came up: probe the server now, without the
    /// coalescing sleep (startup hunt 2026-11-10).
    probe_now: AtomicBool,
    inflight: Mutex<InFlight>,
    /// Connectivity and playback-transfer transitions, not periodic polling.
    priority_changed: tokio::sync::Notify,
    /// Player streams waiting for the radio to wake from a doze and link.
    radio_wanted: std::sync::atomic::AtomicU32,
    /// Playback is still inside its allowance for a dozing radio to wake.
    link_pending: AtomicBool,
    /// The address is still unconfirmed after an automatic radio doze, even
    /// if a foreground stream has exhausted its wait.
    radio_link_unconfirmed: AtomicBool,
    /// Where transfers record their lifecycle (crate::playtrace).
    trace: Mutex<Arc<crate::playtrace::Trace>>,
    refill: refill_log::Journal,
    prefetch_tx: tokio::sync::watch::Sender<Option<Vec<Song>>>,
    metadata: metadata::Warmer,
    /// Per-key request coalescing (PreferCache), held across the request.
    fetch_locks: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    catalog_refresh: tokio::sync::Mutex<Option<(u32, Instant)>>,
    queue_save: Arc<Mutex<Option<QueueSave>>>,
    /// Wakes the sync worker for queued actions and queue saves.
    sync_wake: Arc<tokio::sync::Notify>,
    shelf_auto: shelf_auto_sync::AutoSync,
    offline_groups: offline_groups::Background,
    mix_space: mix_space::Space,
    sync_urgent: Arc<AtomicBool>,
    spoken_playback_requests: AtomicU32,
    pub art: art::ArtCache,
    pub downloads: downloads::Downloads,
    notify: Arc<dyn Fn(LibEvent) + Send + Sync>,
}

impl Library {
    pub(crate) async fn settle_telemetry(&self) -> bool { self.refill.output.settle().await }

    pub fn open(paths: Paths, config: &Config, notify: Arc<dyn Fn(LibEvent) + Send + Sync>) -> Arc<Library> {
        Self::open_with_auto_sync(paths, config, notify, !cfg!(test))
    }

    fn open_with_auto_sync(paths: Paths, config: &Config, notify: Arc<dyn Fn(LibEvent) + Send + Sync>, automatic: bool) -> Arc<Library> {
        let (lib, prefetch_rx) = Self::create(paths, config, notify);
        art::ArtCache::start(&lib);
        downloads::start(&lib);
        lib.start_spoken_downloads();
        Library::start_prefetch(&lib, prefetch_rx);
        Library::start_sync(&lib);
        Library::start_direct_listens(&lib);
        if automatic { shelf_auto_sync::start(&lib); layout::start(&lib); offline_groups::start(&lib); }
        metadata::Warmer::start(&lib);
        crate::record_profile_stage("profile-library-workers-returned");
        lib
    }

    /// Construction separated from worker startup so lifecycle controls can
    /// exercise one real owner without unrelated resident workers retaining it.
    pub(crate) fn create(paths: Paths, config: &Config, notify: Arc<dyn Fn(LibEvent) + Send + Sync>)
        -> (Arc<Library>, tokio::sync::watch::Receiver<Option<Vec<Song>>>) {
        assert!(crate::rt::may_wait_for_tasks(), "library construction belongs to ordinary startup");
        let blocking = &crate::rt::Blocking::dedicated_thread();
        crate::record_profile_stage("profile-library-paths-begin");
        if let Err(e) = paths.create_all(blocking) {
            log::error!("{}: {e}", paths.root.display());
        }
        crate::record_profile_stage("profile-library-paths-returned");
        let db = if paths.ephemeral_media { Db::temporary(blocking) } else { Db::open(blocking, &paths.database()).unwrap_or_else(|e| {
            log::error!("database: {e}; using temporary metadata");
            Db::temporary(blocking)
        }) };
        crate::record_profile_stage("profile-library-database-returned");
        let (prefetch_tx, prefetch_rx) = tokio::sync::watch::channel(None);
        // Construction owns Db exclusively; load the persistent shelf index
        // before handing the connection to its runtime owner.
        let shelf_namespaces = db.shelf_namespaces(blocking).unwrap_or_default();
        #[cfg(test)]
        let db = Arc::new(Mutex::new(db));
        let db_worker = db_worker::DbWorker::start({
            #[cfg(test)]
            { db.clone() }
            #[cfg(not(test))]
            { db }
        });
        let lib = Arc::new_cyclic(|me| Library {
            db_worker,
            client_generation: Arc::default(),
            bound_music_generation: Arc::default(),
            bound_music_epoch: Arc::default(),
            configuration: Mutex::new(configuration::State::default()),
            me: me.clone(),
            offline_art_gate:Arc::new(offline_art::PublicationGate::default()),
            art: art::ArtCache::with_fallback(&paths, paths.legacy_art_cache()),
            downloads: downloads::Downloads::default(),
            settings: RwLock::new(settings_for(config, &paths)),
            spoken_settings: RwLock::new(config.spoken),
            refill: refill_log::Journal::new(paths.database().with_file_name("refill-events.log")),
            shelf: RwLock::new(shelf::ShelfSlot::with_namespaces(shelf_namespaces)),
            paths,
            #[cfg(test)]
            db,
            client: RwLock::new(None),
            music_feedback: Mutex::new(music_actions::Feedback::default()),
            direct_listens: Default::default(),
            music_epoch: Arc::new(AtomicU32::new(0)),
            music_write_failed: Arc::new(AtomicBool::new(false)),
            online: AtomicBool::new(false),
            health: health::Health::default(),
            forced_offline: AtomicBool::new(false),
            radio_doze: AtomicBool::new(false),
            fetching: AtomicBool::new(false),
            prefetch_ready: AtomicBool::new(true),
            last_probe: Mutex::new(Instant::now() - Duration::from_secs(3600)),
            probe_now: AtomicBool::new(false),
            inflight: Mutex::new(HashMap::new()),
            priority_changed: tokio::sync::Notify::new(),
            radio_wanted: std::sync::atomic::AtomicU32::new(0),
            link_pending: AtomicBool::new(false),
            radio_link_unconfirmed: AtomicBool::new(false),
            trace: Mutex::new(crate::playtrace::global()),
            prefetch_tx,
            metadata: metadata::Warmer::default(),
            fetch_locks: Mutex::new(HashMap::new()),
            catalog_refresh: tokio::sync::Mutex::new(None),
            queue_save: Arc::new(Mutex::new(None)),
            sync_wake: Arc::default(),
            shelf_auto: shelf_auto_sync::AutoSync::default(),
            offline_groups: offline_groups::Background::default(),
            mix_space: mix_space::Space::default(),
            sync_urgent: Arc::default(),
            spoken_playback_requests: AtomicU32::new(0),
            notify,
        });
        crate::record_profile_stage("profile-library-clients-begin");
        lib.set_shelf(config);
        crate::record_profile_stage("profile-library-shelf-returned");
        lib.configure(config);
        crate::record_profile_stage("profile-library-server-returned");
        (lib, prefetch_rx)
    }

    pub fn is_configured(&self) -> bool {
        self.client.read().unwrap().is_some()
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed) && !self.forced_offline.load(Ordering::Relaxed)
            && !self.radio_doze.load(Ordering::Relaxed)
    }

    /// The radio is dozing (ui/doze.rs): not the owner's Offline Mode, so
    /// nothing visible changes, but background maintenance waits for the
    /// restored link before flushing queued updates.
    pub fn set_radio_doze(&self, on: bool) {
        if self.radio_doze.swap(on, Ordering::Relaxed) != on {
            // Down, or up but not yet associated: no usable link until an address.
            self.link_pending.store(true, Ordering::Release);
            self.radio_link_unconfirmed.store(true, Ordering::Release);
            link_changed();
            self.background_changed();
            if on { self.direct_listens.cancel_network(); }
            else { subsonic::http::network_changed(); }
        }
    }

    pub fn radio_dozing(&self) -> bool {
        self.radio_doze.load(Ordering::Relaxed)
    }

    fn maintenance_link_ready(&self) -> bool {
        !self.offline_mode() && !self.radio_dozing()
            && !self.radio_link_unconfirmed.load(Ordering::Acquire)
    }

    /// The UI sees an address with no doze in force: a link a stream may
    /// use (also after a failed radio-down was rolled back, with no new
    /// address to report through [`Library::network_up`]).
    pub fn link_available(&self) {
        let foreground_waited = self.link_pending.swap(false, Ordering::AcqRel);
        let address_unconfirmed = self.radio_link_unconfirmed.swap(false, Ordering::AcqRel);
        if foreground_waited || address_unconfirmed {
            self.background_changed();
            self.sync_urgent.store(true, Ordering::Release);
            self.wake_sync();
        }
    }

    /// A player stream waits for the radio: the doze policy wakes it.
    pub fn playback_wants_radio(&self) -> bool { self.radio_wanted.load(Ordering::Acquire) > 0 }

    /// Tests: a player stream starts or stops waiting for the radio.
    #[cfg(test)]
    pub(crate) fn want_radio_for_test(&self, on: bool) {
        if on { self.radio_wanted.fetch_add(1, Ordering::AcqRel); } else { self.radio_wanted.fetch_sub(1, Ordering::AcqRel); }
    }

    /// Before a player stream's request: while the radio dozes, or has just
    /// woken without an address, ask the doze policy to wake it and wait for
    /// the link, at most [`RADIO_WAKE_WAIT`] (a cold reassociation took
    /// 3-18 s on the Q2, 37 s to a settled link). Without it the request
    /// failed at once and the player skipped or stopped. Returns whether the
    /// link is up (the request then goes out either way and reports itself).
    pub(super) async fn await_radio_for_playback(&self, cancel: &transfer_io::Cancel) -> bool {
        let ready = || !self.radio_dozing() && !self.link_pending.load(Ordering::Acquire);
        if ready() { return true; }
        self.radio_wanted.fetch_add(1, Ordering::AcqRel);
        (self.notify)(LibEvent::RadioWanted);
        let until = tokio::time::Instant::now() + RADIO_WAKE_WAIT;
        let up = loop {
            let changed = self.priority_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if ready() { break true; }
            if cancel.is_cancelled() || self.offline_mode() || tokio::time::Instant::now() >= until { break false; }
            tokio::select! {
                biased;
                () = cancel.cancelled() => {},
                () = changed => {},
                () = tokio::time::sleep_until(until) => {},
            }
        };
        self.radio_wanted.fetch_sub(1, Ordering::AcqRel);
        if !up && !cancel.is_cancelled() { log::warn!("stream: no Wi-Fi link within {} s of the request", RADIO_WAKE_WAIT.as_secs()); }
        up
    }

    pub fn set_offline_mode(&self, on: bool) {
        set_offline_cause(0);
        self.forced_offline.store(on, Ordering::Relaxed);
        self.background_changed();
        self.downloads.interrupt();
        if on { self.direct_listens.cancel_network(); }
        else { self.art.retry_missing(); self.wake_sync(); self.wake_offline_groups(); }
        (self.notify)(LibEvent::Online(self.is_online()));
    }

    /// Wi-Fi is usable for Audiobookshelf: not in Offline Mode or a radio doze.
    /// (ABS reachability is independent of Navidrome's.)
    pub fn is_online_for_shelf(&self) -> bool {
        !self.forced_offline.load(Ordering::Relaxed) && !self.radio_doze.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn mark_online_for_test(&self, on: bool) { self.online.store(on, Ordering::Relaxed); }

    pub fn offline_mode(&self) -> bool {
        self.forced_offline.load(Ordering::Relaxed)
    }

    /// Tests: records into a new enabled trace from here on, and returns it.
    #[cfg(test)]
    pub(crate) fn traced_for_test(&self) -> Arc<crate::playtrace::Trace> {
        let trace = Arc::new(crate::playtrace::Trace::new(true));
        *self.trace.lock().unwrap() = trace.clone();
        trace
    }

    pub(crate) fn trace(&self) -> Arc<crate::playtrace::Trace> {
        self.trace.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A fetch-ahead burst is downloading.
    pub fn fetching(&self) -> bool { self.fetching.load(Ordering::Relaxed) }

    /// Asks the prefetch worker for these upcoming songs again, e.g. once the
    /// radio is back after a doze. Same filter as the player's request.
    pub fn prefetch_upcoming(&self, songs: &[Song]) {
        let upcoming: Vec<_> = songs.iter().filter(|s| s.radio_url.is_none() && s.spoken.is_none()).cloned().collect();
        self.prefetch_tx.send_modify(move |queue| {
            self.prefetch_ready.store(upcoming.is_empty(), Ordering::Release);
            *queue = Some(upcoming);
        });
    }

    /// Call after publishing a readiness transition. Priority waiters register
    /// before checking state; the one metadata owner retains a wake permit.
    fn background_changed(&self) {
        self.priority_released();
        self.metadata.wake();
        self.shelf_auto.wake.notify_one();
    }

    /// Work that yields (downloads, fetch-ahead, cover prefetch) looks again.
    fn priority_released(&self) {
        self.priority_changed.notify_waiters();
        self.art.priority_released();
    }

    pub(crate) fn client(&self) -> Result<Arc<Client>, LibError> {
        if self.forced_offline.load(Ordering::Relaxed) {
            return Err(LibError::Offline);
        }
        self.client.read().unwrap().clone().ok_or(LibError::NotConfigured)
    }

    /// Uses the immutable transport selected with a download request. Stale
    /// transports cannot change the replacement account's connection status.
    fn download_response<T>(&self, client: &Arc<Client>, result: subsonic::Result<T>) -> Result<T, LibError> {
        let slot = self.client.read().unwrap();
        if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, client)) {
            self.observe(&result, Priority::Background);
        }
        // Transport and server-provided messages can contain authenticated
        // URLs. Offline-download diagnostics deliberately use fixed messages.
        result.map_err(|error| if error.is_auth() { LibError::Auth } else {
            LibError::Server(match error {
                subsonic::Error::Network(_) | subsonic::Error::Io(_) => "Download connection failed".into(),
                subsonic::Error::Http(status) => format!("Download HTTP {status}"),
                subsonic::Error::Api { code, .. } => format!("Download server error ({code})"),
                subsonic::Error::Json(_) => "Unsupported download response".into(),
            })
        })
    }

    async fn download_response_async<T: Send + 'static>(self: &Arc<Self>, client: Arc<Client>, result: subsonic::Result<T>) -> Result<T, LibError> {
        let lib = self.clone();
        crate::rt::spawn_blocking(move |_| lib.download_response(&client, result)).await
            .map_err(|_| LibError::Server("Download worker stopped".into()))?
    }

    /// Writes into an exclusively created download file, preserving its owner.
    async fn fetch_download_file_async(self: &Arc<Self>, client: &Arc<Client>, req: &Request, output: transfer_io::FileOutput, progress: Arc<Progress>, cancel: Arc<transfer_io::Cancel>, storage: Option<Arc<mix_space::Ticket>>) -> Result<u64, LibError> {
        let response=tokio::select! {
            biased;
            ()=cancel.cancelled()=>Err(LibError::Offline),
            response=client.open_async(req,0)=>self.download_response_async(client.clone(),response).await,
        };
        let response=match response {
            Ok(response)=>response,
            Err(error)=>{crate::rt::spawn_blocking(move |_|drop(output)).await.map_err(|_|LibError::Server("Download worker stopped".into()))?;return Err(error);},
        };
        let total=response.content_length;
        progress.set_total(total);
        if let Some(storage) = &storage {
            storage.ensure(total.unwrap_or(0), 1).await.map_err(downloads::space_error)?;
        }
        let written=transfer_io::copy_file_prioritized(response.response.into_decoded(),output,progress,cancel,None,Some(transfer_io::sync_file),total,transfer_io::playback_priority(self.clone()),storage).await;
        let written=match written {
            Ok(written)=>written,
            Err(transfer_io::Failure::Cancelled)=>return Err(LibError::Offline),
            Err(transfer_io::Failure::Length)=>return Err(LibError::Server("Incomplete downloaded audio".into())),
            Err(transfer_io::Failure::Network(error))=>{
                let lib = self.clone(); let client = client.clone();
                crate::rt::spawn_blocking(move |_| {
                    let slot=lib.client.read().unwrap();
                    if slot.as_ref().is_some_and(|current|Arc::ptr_eq(current,&client)) {lib.observe_failure(&subsonic::Error::Io(error), Priority::Background);}
                }).await.map_err(|_| LibError::Server("Download worker stopped".into()))?;
                return Err(LibError::Server("Download connection interrupted".into()));
            },
            // A full card says so (UX-006: "The card is full").
            Err(transfer_io::Failure::Card(error)) if error.raw_os_error()==Some(libc::ENOSPC)=>return Err(LibError::Server("Card full".into())),
            Err(transfer_io::Failure::Card(_))=>return Err(LibError::Server("Unable to write downloaded audio".into())),
            Err(transfer_io::Failure::Sync(_))=>return Err(LibError::Server("Unable to save downloaded audio".into())),
        };
        if written==0 || total.is_some_and(|total|written!=total) {return Err(LibError::Server("Incomplete downloaded audio".into()));}
        Ok(written)
    }

    /// Connection snapshots and status publication use finite workers. A reply
    /// from a replaced client cannot change the current account's online state.
    pub(crate) async fn fetch_net(&self, request: &subsonic::Request) -> Result<serde_json::Value, LibError> {
        self.fetch_net_generation(request, self.client_generation(), true).await
    }

    async fn fetch_net_generation(&self, request: &subsonic::Request, generation: u32, publish_success: bool) -> Result<serde_json::Value, LibError> {
        /// Started on the blocking pool, where the client is taken.
        fn start_reply(_: &crate::rt::Blocking, client: &Arc<Client>, request: &subsonic::Request) -> NetFuture {
            let request = client.fetch_async(request);
            Box::pin(async move { request.await.map(|value| (Box::new(value) as Parsed, 0)) })
        }
        let request = request.clone();
        let start: NetStart = Box::new(move |blocking, client| start_reply(blocking, client, &request));
        let (value, _) = self.fetch_net_parsed(start, generation, publish_success).await?;
        Ok(*value.downcast::<serde_json::Value>().expect("a JSON reply"))
    }

    /// A typed call from the network, parsed as it arrives, and its
    /// document's bytes.
    pub(super) async fn fetch_net_call<T: Streamed + Send + 'static>(&self, call: &Call<T>, generation: u32, publish_success: bool) -> Result<(T, u64), LibError> {
        let call = call.clone();
        let start: NetStart = Box::new(move |_, client| {
            let request = client.document_async(&call);
            Box::pin(async move { request.await.map(|(value, bytes)| (Box::new(value) as Parsed, bytes)) })
        });
        let (value, bytes) = self.fetch_net_parsed(start, generation, publish_success).await?;
        Ok((*value.downcast::<T>().expect("the call's result type"), bytes))
    }

    /// Not generic: one copy of the client admission and health logic for
    /// every result type.
    async fn fetch_net_parsed(&self, start: NetStart, generation: u32, publish_success: bool) -> Result<(Parsed, u64), LibError> {
        // Local binding failures are retryable admission failures, never a
        // permanent server refusal that would acknowledge a queued action.
        self.music_binding_ready(generation).await.map_err(|_| LibError::Offline)?;
        let owner = self.me.upgrade().ok_or(LibError::Offline)?;
        let priority = health::current_priority();
        let prepare = owner.clone();
        let (client, request) = crate::rt::spawn_blocking(move |blocking| {
            let slot = prepare.client.read().unwrap();
            if prepare.client_generation() != generation || prepare.offline_mode() { return Err(LibError::Offline); }
            let client = slot.clone().ok_or(LibError::NotConfigured)?;
            let request = start(blocking, &client);
            Ok((client, request))
        }).await.map_err(|_| LibError::Offline)??;
        let result = {
            let _foreground = self.foreground_request(priority);
            request.await
        };
        crate::rt::spawn_blocking(move |_| {
            let slot = owner.client.read().unwrap();
            if owner.client_generation() != generation || !slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &client)) {
                return Err(LibError::Offline);
            }
            match &result {
                Ok(_) if publish_success => owner.observe_success(),
                Ok(_) => {}
                Err(error) if error.is_offline() => owner.observe_failure(error, priority),
                Err(_) => {}
            }
            result.map_err(LibError::from)
        }).await.map_err(|_| LibError::Offline)?
    }

    /// A stored response from the current server: evidence that it answers.
    /// Failures are observed where the request failed (`fetch_net_generation`).
    async fn publish_success_generation(&self, generation: u32) -> Result<(), LibError> {
        let owner = self.me.upgrade().ok_or(LibError::Offline)?;
        crate::rt::spawn_blocking(move |_| {
            let _slot = owner.client.read().unwrap();
            if owner.client_generation() != generation { return Err(LibError::Offline); }
            owner.observe_success();
            Ok(())
        }).await.map_err(|_| LibError::Offline)?
    }

    pub(crate) fn notify(&self, e: LibEvent) {
        (self.notify)(e)
    }

    #[cfg(test)]
    pub(crate) fn db(&self) -> db_worker::DbGuard<'_> {
        assert!(!db_worker::in_job(), "Library::db() inside a db closure: the mutex is held");
        db_worker::DbGuard::new(self.db.lock().unwrap())
    }

    /// Runs `job` on the `db` thread (for async code; owned arguments, no
    /// `Library` lock inside).
    pub(crate) fn db_run<R: Send + 'static>(&self, job: impl FnOnce(&crate::rt::Blocking, &mut Db) -> R + Send + 'static) -> impl std::future::Future<Output = R> + Send + 'static {
        self.db_worker.run(job)
    }

    /// Await a captured music binding before posting a read. The future owns
    /// only the admission atoms/notification and DB sender, not Library locks.
    pub(super) fn music_db_run<R: Send + 'static, F>(&self, generation: u32, job: F)
        -> impl std::future::Future<Output = Result<R, LibError>> + Send + 'static + use<R, F>
        where F: FnOnce(&crate::rt::Blocking, &mut Db) -> Result<R, LibError> + Send + 'static {
        let ready = self.music_binding_ready(generation);
        let worker = self.db_worker.clone();
        let current = self.client_generation.clone();
        let bound = self.bound_music_generation.clone();
        async move {
            ready.await?;
            worker.run(move |blocking, db| {
                if current.load(Ordering::SeqCst) != generation || bound.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                let value = job(blocking, db)?;
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                Ok(value)
            }).await
        }
    }

    /// Queues `job` on the `db` thread without waiting.
    pub(crate) fn db_post(&self, job: impl FnOnce(&crate::rt::Blocking, &mut Db) + Send + 'static) {
        self.db_worker.post(job)
    }

    /// The generation of the current client (see `client_generation`).
    pub(crate) fn client_generation(&self) -> u32 {
        self.client_generation.load(Ordering::SeqCst)
    }

    fn replace_client(&self, slot: &mut Option<Arc<Client>>, client: Option<Arc<Client>>) {
        self.client_generation.fetch_add(1, Ordering::SeqCst);
        *slot = client;
    }

    pub fn set_browsing(&self, on: bool) { self.metadata.set_browsing(on); self.art.set_visible(on); }

    pub fn prefetch_albums(&self, ids: impl IntoIterator<Item=String>) {
        self.metadata.enqueue(ids);
    }

    /// Fetches a call's result from the network or the cache.
    /// [`Library::fetch_async`] for threads: runs it on the I/O runtime and
    /// waits. Not on a runtime or pool thread, not inside a `db` closure and
    /// not holding a `db()` guard (the request's cache work needs the mutex).
    pub fn fetch<T: Streamed + Serialize + Send + 'static>(&self, call: &Call<T>, policy: Policy) -> Result<Fetched<T>, LibError> {
        assert!(crate::rt::may_wait_for_tasks(), "a synchronous fetch on a runtime or pool thread");
        assert!(!db_worker::guard_held() && !db_worker::in_job(), "a synchronous fetch holding the database");
        let Some(lib) = self.me.upgrade() else { return Err(LibError::Offline) };
        let call = call.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        crate::rt::io().spawn(async move {
            let _ = tx.send(lib.fetch_async(call, policy).await);
        });
        rx.blocking_recv().unwrap_or(Err(LibError::Offline))
    }

    /// A Subsonic call through the response cache, by `policy`. The cache
    /// work runs on the `db` thread; the request holds no thread. Network
    /// documents and stored copies are parsed as they are read (no complete
    /// text, no JSON tree), and a result is stored streamed.
    pub async fn fetch_async<T: Streamed + serde::Serialize + Send + 'static>(self: Arc<Self>, call: Call<T>, policy: Policy) -> Result<Fetched<T>, LibError> {
        // A thin shell over `fetch_value`, which is not generic: one copy of
        // the cache and network logic for every result type.
        let key = call.request.cache_key();
        let (net, read, write) = (call.clone(), call.clone(), call);
        let hooks = Hooks {
            network: Box::new(move |_, client| {
                let request = client.document_async(&net);
                Box::pin(async move { request.await.map(|(value, bytes)| (Box::new(value) as Parsed, bytes)) })
            }),
            read: Arc::new(move || Box::new(StoredCopy { parser: read.stored_parser(), error: None }) as Box<dyn StoredParse>),
            write: Arc::new(move |value, out| {
                let value = value.downcast_ref::<T>().expect("the call's result type");
                write.write_stored(value, out).map_err(std::io::Error::other)
            }),
        };
        let value = self.fetch_value(key, policy, hooks).await?;
        Ok(Fetched { value: *value.downcast::<T>().expect("the call's result type") })
    }

    /// [`Library::fetch_async`] on type-erased results. A stored copy that
    /// does not parse is not used (or, cache-only, is the error returned); a
    /// fetched document that does not parse is the error, unstored.
    async fn fetch_value(self: Arc<Self>, key: String, policy: Policy, hooks: Hooks) -> Result<Parsed, LibError> {
        let generation = self.client_generation();
        self.music_binding_ready(generation).await?;
        let Hooks { network, read, write } = hooks;
        // Coalesce speculative and foreground requests for the same album.
        // Different albums remain independent; no lock is held on the UI.
        let serial = (policy == Policy::PreferCache).then(|| {
            let mut locks = self.fetch_locks.lock().unwrap();
            if locks.len() > 64 { locks.retain(|_, v| v.strong_count() > 0); }
            let lock = locks.get(&key).and_then(std::sync::Weak::upgrade).unwrap_or_default();
            locks.insert(key.clone(), Arc::downgrade(&lock));
            lock
        });
        let _serial = match serial { Some(lock) => Some(lock.lock_owned().await), None => None };
        // The stored copy is parsed on the `db` thread as it is read.
        let cached = |fresh_only: bool| {
            let key = key.clone();
            let read = read.clone();
            self.music_db_run(generation, move |blocking, db| {
                if fresh_only && !db.response_fresh(blocking, &key, 24 * 3600) { return Ok(None); }
                let mut parser = read();
                Ok(db.response_pieces(blocking, &key, &mut |piece| parser.feed(piece)).map(|_| parser.finish()))
            })
        };
        if policy == Policy::PreferCache {
            if let Some(Ok(v)) = cached(true).await? { return Ok(v); }
        }
        if policy == Policy::CacheOnly {
            let owner = self.clone();
            let configured = crate::rt::spawn_blocking(move |_| owner.is_configured()).await.map_err(|_| LibError::Offline)?;
            let missing = if configured { LibError::Offline } else { LibError::NotConfigured };
            // A cached body that does not parse is returned for the caller to
            // report, as before.
            return cached(false).await?.unwrap_or(Err(missing));
        }
        if self.client_generation() != generation { return Err(LibError::Offline); }
        let response = self.fetch_net_parsed(network, generation, false).await;
        if self.client_generation() != generation { return Err(LibError::Offline); }
        match response {
            Ok((value, bytes)) => {
                let current = self.client_generation.clone();
                let (stored, value, upgrades) = self.db_run(move |blocking, db| {
                    // A server change during the request must not put the old
                    // server's response into the new server's cache.
                    let same = current.load(Ordering::SeqCst) == generation;
                    if same { db.put_response_with(blocking, &key, &|out| write(&*value, out)); }
                    let upgrades = same && value.downcast_ref::<subsonic::AlbumWithSongs>()
                        .is_some_and(|album| db.observed_album(blocking, &album.song).unwrap_or_else(|_| {
                            log::warn!("unable to compare saved album downloads");
                            false
                        }));
                    (same, value, upgrades)
                }).await;
                if !stored { return Err(LibError::Offline); }
                if upgrades { self.downloads.wake(); self.notify(LibEvent::Downloads); }
                self.publish_success_generation(generation).await?;
                crate::platform::resident::trim_after_document(bytes);
                Ok(value)
            }
            Err(e @ (LibError::Offline | LibError::NotConfigured)) if matches!(policy, Policy::NetworkFirst | Policy::PreferCache) => {
                cached(false).await?.unwrap_or(Err(e))
            }
            Err(e) => Err(e),
        }
    }

    // Actions, queued while offline.

    pub async fn add_radio(self: Arc<Self>, name: String, url: String) -> Result<(),LibError> {
        let existing=self.clone().fetch_async(calls::internet_radio_stations(),Policy::NetworkOnly).await?;
        if !existing.value.internet_radio_station.iter().any(|s| s.stream_url == url) {
            self.clone().fetch_async(calls::create_radio_station(&name,&url),Policy::NetworkOnly).await?;
        }
        let _=self.clone().fetch_async(calls::internet_radio_stations(),Policy::NetworkOnly).await;
        self.notify(LibEvent::RadiosUpdated);
        Ok(())
    }

    /// An address was acquired: check the server at once rather than on the
    /// sync thread's 10-20 s cadence.
    pub fn network_up(&self) {
        if !self.radio_dozing() {
            self.link_pending.store(false, Ordering::Release);
            self.radio_link_unconfirmed.store(false, Ordering::Release);
        }
        self.wake_offline_groups();
        link_changed();
        self.background_changed();
        self.probe_now.store(true, Ordering::Relaxed);
        self.shelf_auto.reconnect.store(true, Ordering::Release);
        self.wake_sync();
    }

    fn wake_sync(&self) {
        self.direct_listens.wake();
        // Kept for the next wait when the task is busy.
        self.sync_wake.notify_one();
        self.shelf_auto.wake.notify_one();
    }

    async fn send_pending(&self, call: &PendingCall, generation: u32) -> Result<(), LibError> {
        let c = match call {
            PendingCall::Scrobble { id, time_ms } => calls::scrobble(id, Some(*time_ms), true),
            PendingCall::Star { kind, id, on } => {
                let k = match kind.as_str() {
                    "album" => ItemKind::Album,
                    "artist" => ItemKind::Artist,
                    _ => ItemKind::Song,
                };
                if *on { calls::star(k, id) } else { calls::unstar(k, id) }
            }
            PendingCall::Rating { id, rating } => calls::set_rating(id, *rating),
        };
        match self.fetch_net_generation(&c.request, generation, true).await {
            Ok(_) => Ok(()),
            // The server refused it (a song since deleted): don't retry.
            Err(LibError::Server(m)) => {
                log::warn!("dropping {call:?}: {m}");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    // Downloads.

    /// Capture the account at the UI action; retry on the DB owner without
    /// blocking input. An account replacement while queued refuses the action.
    pub(crate) fn retry_downloads(self: &Arc<Self>) -> impl std::future::Future<Output = ()> + Send + 'static {
        let generation = self.client_generation();
        let lib = self.clone();
        async move {
            if lib.music_binding_ready(generation).await.is_err() { return; }
            let owner = lib.clone();
            let retried = lib.db_run(move |blocking, db| {
                if owner.client_generation() != generation { return false; }
                db.retry_failed_downloads(blocking);
                true
            }).await;
            if retried {
                lib.downloads.wake();
                lib.notify(LibEvent::Downloads);
            }
        }
    }

    #[cfg(test)]
    pub fn offline_set(&self, kind: &str, id: &str) -> Option<OfflineSet> {
        self.db().offline_set(&crate::rt::Blocking::dedicated_thread(), kind, id)
    }

    /// A single stored snapshot; called while the database owner/guard holds
    /// serialization. Never takes another Library lock from a DB job.
    fn stored_download_view(blocking: &crate::rt::Blocking, db: &Db) -> DownloadView {
        let queued = db.queued_download_ids(blocking);
        let albums = db.offline_sets(blocking).into_iter().filter(|s| s.kind == "album").map(|s| (s.id, s.song_ids)).collect();
        let offline = db.offline_ids(blocking);
        DownloadView { queued, active: None, albums, offline }
    }

    /// Fixture comparison against the awaited owner snapshot.
    #[cfg(test)]
    pub fn download_view(&self) -> DownloadView {
        let mut view = Self::stored_download_view(&crate::rt::Blocking::dedicated_thread(), &self.db());
        view.active = self.downloads.active();
        view
    }

    /// Await stored badge state on the DB owner, without occupying a UI/card
    /// worker while SQLite is busy. The caller captures the account when it
    /// requests badges and checks it again before publishing the reply.
    pub(crate) async fn download_view_async(&self, generation: u32) -> Option<DownloadView> {
        self.music_binding_ready(generation).await.ok()?;
        let current = self.client_generation.clone();
        let mut view = self.db_run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return None; }
            let view = Self::stored_download_view(blocking, db);
            (current.load(Ordering::SeqCst) == generation).then_some(view)
        }).await?;
        view.active = self.downloads.active();
        (self.client_generation() == generation).then_some(view)
    }

    pub(crate) fn music_account_generation(&self) -> u32 { self.music_epoch.load(Ordering::SeqCst) }

    /// Popup labels follow account identity, not its current network route.
    pub(crate) fn download_choice(&self, account: u32, kind: Option<String>, id: String)
        -> impl std::future::Future<Output = Result<bool, LibError>> + Send + 'static {
        let lib = self.me.upgrade();
        async move {
            let lib = lib.ok_or(LibError::Offline)?;
            loop {
                if lib.music_account_generation() != account { return Err(LibError::Offline); }
                let generation = lib.client_generation();
                let epoch = lib.music_epoch.clone();
                let kind = kind.clone(); let id = id.clone();
                let found = lib.music_db_run(generation, move |blocking, db| {
                    if epoch.load(Ordering::SeqCst) != account { return Err(LibError::Offline); }
                    let found = match kind {
                        Some(kind) => db.offline_sets_checked(blocking).map(|sets| sets.iter().any(|set| set.kind == kind && set.id == id)),
                        None => db.manually_downloaded_song(blocking, &id),
                    }.map_err(|_| LibError::Server("Unable to read downloads".into()))?;
                    if epoch.load(Ordering::SeqCst) != account { return Err(LibError::Offline); }
                    Ok(found)
                }).await;
                // A route-only transition preserves this popup's account. Wait
                // for its new binding; an A→B→A account epoch can never retry.
                if matches!(found, Err(LibError::Offline)) && lib.client_generation() != generation
                    && lib.music_account_generation() == account { continue; }
                return found;
            }
        }
    }

    pub(crate) fn offline_sets_async(&self) -> impl std::future::Future<Output = Result<Vec<OfflineSet>, LibError>> + Send + 'static {
        let generation = self.client_generation(); let current = self.client_generation.clone();
        self.music_db_run(generation, move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            let sets = db.offline_sets_checked(blocking).map_err(|_| LibError::Server("Unable to read downloads".into()))?;
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            Ok(sets)
        })
    }

    pub(crate) fn offline_collection_async(&self, kind: String, id: String)
        -> impl std::future::Future<Output = Result<Option<(OfflineSet, Vec<Song>)>, LibError>> + Send + 'static {
        let generation = self.client_generation(); let current = self.client_generation.clone();
        self.music_db_run(generation, move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            let set = db.offline_sets_checked(blocking).map_err(|_| LibError::Server("Unable to read downloads".into()))?
                .into_iter().find(|set| set.kind == kind && set.id == id);
            let result = set.map(|set| { let songs = db.offline_songs(blocking, &set.song_ids); (set, songs) });
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            Ok(result)
        })
    }

    pub(crate) fn shuffled_downloaded_albums(&self, mut rng: fastrand::Rng)
        -> impl std::future::Future<Output = Result<Vec<Song>, LibError>> + Send + 'static {
        let generation = self.client_generation(); let current = self.client_generation.clone();
        self.music_db_run(generation, move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            let mut sets: Vec<_> = db.offline_sets_checked(blocking).map_err(|_| LibError::Server("Unable to read downloads".into()))?
                .into_iter().filter(|set| set.kind == "album").collect();
            rng.shuffle(&mut sets);
            let on_card = db.offline_ids_checked(blocking).map_err(|_| LibError::Server("Unable to read downloads".into()))?;
            let mut songs = Vec::new();
            for set in sets {
                if songs.len() >= 150 { break; }
                songs.extend(db.offline_songs(blocking, &set.song_ids).into_iter().filter(|song| on_card.contains(&song.id)));
            }
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            Ok(songs)
        })
    }

    pub(crate) fn music_rows_snapshot(self: &Arc<Self>, songs: Vec<Song>)
        -> impl std::future::Future<Output = Result<MusicRowSnapshot, LibError>> + Send + 'static {
        let generation = self.client_generation(); let current = self.client_generation.clone();
        let online = self.is_online(); let quality = self.settings.read().unwrap().stream_quality;
        let lib = self.clone();
        let stored = self.music_db_run(generation, move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            let view = Self::stored_download_view(blocking, db);
            let mut local = view.offline.clone();
            if !online {
                for song in &songs {
                    if local.contains(&song.id) { continue; }
                    let variants = acceptable(song, quality);
                    let refs: Vec<_> = variants.iter().map(String::as_str).collect();
                    if db.cached_song(blocking, &song.id, &refs).is_some() { local.insert(song.id.clone()); }
                }
            }
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            Ok(MusicRowSnapshot { songs, view, local, online })
        });
        async move {
            let mut snapshot = stored.await?;
            snapshot.view.active = lib.downloads.active();
            if lib.client_generation() != generation { return Err(LibError::Offline); }
            Ok(snapshot)
        }
    }

    /// Stored Downloads rows run in queue order on the DB owner, with no
    /// Library lock in the closure. In-memory progress is sampled afterwards.
    pub(crate) fn download_inventory(self: &Arc<Self>) -> impl std::future::Future<Output = Result<DownloadInventory, LibError>> + Send + 'static {
        let generation = self.client_generation();
        let lib = self.clone();
        async move {
            let current = lib.client_generation.clone();
            let (counts, sets, queued, failed, offline) = lib.music_db_run(generation, move |blocking, db| {
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                let counts = db.download_counts(blocking);
                let offline=db.offline_ids_checked(blocking).map_err(|_|LibError::Server("Unable to read downloaded songs".into()))?;
                let sets = db.offline_sets_checked(blocking).map_err(|_|LibError::Server("Unable to read download collections".into()))?.into_iter().map(|set| {
                    let have = set.song_ids.iter().filter(|id|offline.contains(*id)).count();
                    (set, have)
                }).collect();
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                let queued=db.queued_download_ids_checked(blocking).map_err(|_|LibError::Server("Unable to read download queue".into()))?;
                let failed=db.failed_download_ids(blocking).map_err(|_|LibError::Server("Unable to read download failures".into()))?;
                Ok((counts, sets, queued, failed, offline))
            }).await?;
            let active=lib.downloads.active();
            let status = lib.downloads.status_with_counts(counts);
            if lib.client_generation() != generation { return Err(LibError::Offline); }
            Ok(DownloadInventory { status, sets, queued, failed, offline, active })
        }
    }

    /// Capture Shuffle's account when requested, await the DB owner, then
    /// check the card on the same isolated UI lane used by the old caller.
    /// No worker is occupied while SQLite is busy, and no file I/O runs on io.
    pub(crate) fn downloaded_music_async(self: &Arc<Self>, genre: Option<String>) -> impl std::future::Future<Output = Result<Vec<Song>, LibError>> + Send + 'static {
        let generation = self.client_generation();
        let lib = self.clone();
        async move {
            let current = lib.client_generation.clone();
            let rows = lib.music_db_run(generation, move |blocking, db| {
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                db.downloaded_songs(blocking).map_err(|_| LibError::Server("Unable to read downloaded music".into()))
            }).await?;
            if lib.client_generation() != generation { return Err(LibError::Offline); }
            let current = lib.client_generation.clone();
            let paths = lib.paths.clone();
            crate::rt::jobs().run_isolated(move |blocking| {
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                let songs = Self::present_downloaded_music(blocking, &paths, rows, genre.as_deref());
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                Ok(songs)
            }).await.map_err(|_| LibError::Server("Downloaded music worker stopped".into()))?
        }
    }

    /// Stale catalog rows, cache-only tracks and spoken/radio content are
    /// excluded. This finite card operation never reads the database.
    /// Downloaded and cached music that plays without the server (UX-009),
    /// each song once, downloads first.
    pub(crate) fn local_music_async(self: &Arc<Self>, genre: Option<String>) -> impl std::future::Future<Output = Result<Vec<Song>, LibError>> + Send + 'static {
        let generation = self.client_generation();
        let lib = self.clone();
        async move {
            let current = lib.client_generation.clone();
            let (mut rows, cached) = lib.music_db_run(generation, move |blocking, db| {
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                let downloaded = db.downloaded_songs(blocking).map_err(|_| LibError::Server("Unable to read downloaded music".into()))?;
                Ok((downloaded, db.cached_songs(blocking).unwrap_or_default().into_iter().map(|(song, path, _)| (song, path)).collect::<Vec<_>>()))
            }).await?;
            let mut seen: std::collections::HashSet<String> = rows.iter().map(|(song, _)| song.id.clone()).collect();
            rows.extend(cached.into_iter().filter(|(song, _)| seen.insert(song.id.clone())));
            let current = lib.client_generation.clone();
            let paths = lib.paths.clone();
            crate::rt::jobs().run_isolated(move |blocking| {
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                Ok(Self::present_downloaded_music(blocking, &paths, rows, genre.as_deref()))
            }).await.map_err(|_| LibError::Server("Local music worker stopped".into()))?
        }
    }

    /// Streamed songs with a cached copy on the card and the bytes the cache
    /// registry records for each, largest first: the Song Cache slice of
    /// Storage Usage (UXI-013). The registry is the cache's owner: eviction
    /// and Clear Song Cache remove its rows with the files.
    pub(crate) fn cached_music_async(self: &Arc<Self>) -> impl std::future::Future<Output = Result<Vec<(Song, u64)>, LibError>> + Send + 'static {
        let generation = self.client_generation();
        let lib = self.clone();
        async move {
            let mut songs: Vec<(Song, u64)> = lib.music_db_run(generation, |blocking, db| db.cached_songs(blocking)
                .map_err(|_| LibError::Server("Unable to read song cache index".into()))).await?
                .into_iter().map(|(song, _, size)| (song, size)).collect();
            songs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
            Ok(songs)
        }
    }

    /// One random playable downloaded or cached song of `genre`, checking
    /// files only until one is present. `None` when a small random sample has
    /// none; callers then read the whole local library.
    pub(crate) fn random_local_song_async(self: &Arc<Self>, genre: Option<String>) -> impl std::future::Future<Output = Option<Song>> + Send + 'static {
        let generation = self.client_generation();
        let lib = self.clone();
        async move {
            let current = lib.client_generation.clone();
            let mut rows = lib.music_db_run(generation, move |blocking, db| {
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                db.random_local_songs(blocking, 16).map_err(|_| LibError::Server("Unable to read downloaded music".into()))
            }).await.ok()?;
            fastrand::shuffle(&mut rows);
            let current = lib.client_generation.clone();
            let paths = lib.paths.clone();
            crate::rt::jobs().run_isolated(move |blocking| {
                if current.load(Ordering::SeqCst) != generation { return None; }
                rows.into_iter().find_map(|row| Self::present_downloaded_music(blocking, &paths, vec![row], genre.as_deref()).pop())
            }).await.ok().flatten()
        }
    }

    /// Keeps at least [`SHUFFLE_READY`] songs playable from the card, so
    /// Shuffle Songs can start at once (UX-004/UX-009): when fewer are
    /// downloaded or cached, a few random songs are fetched into the cache.
    pub(crate) async fn fill_shuffle_reserve(self: Arc<Self>) {
        static FILLING: AtomicBool = AtomicBool::new(false);
        if !self.is_online() || FILLING.swap(true, Ordering::AcqRel) { return; }
        struct Done;
        impl Drop for Done { fn drop(&mut self) { FILLING.store(false, Ordering::Release); } }
        let _done = Done;
        let have = self.local_music_async(None).await.map(|songs| songs.len()).unwrap_or(SHUFFLE_READY);
        if have >= SHUFFLE_READY { return; }
        let request = self.clone().fetch_async(subsonic::calls::random_songs(10, None), Policy::NetworkOnly);
        let Ok(found) = health::with_priority(Priority::Background, request).await else { return };
        let owner = self.clone();
        let Ok(quality) = crate::rt::spawn_blocking(move |_| owner.settings.read().unwrap().stream_quality).await else { return };
        let mut fetched = have;
        for song in found.value.song {
            if fetched >= SHUFFLE_READY || !self.is_online() { break; }
            let wanted = variant(&song, quality);
            if !can_prefetch_audio(&song, &wanted) { continue; }
            if let Ok((_, progress, _hold)) = self.fetch_in_burst_async(song, wanted, false, 0).await {
                progress.finished().await;
                fetched += 1;
            }
        }
        log::info!("shuffle reserve: {fetched} songs on the card");
    }

    fn present_downloaded_music(blocking: &crate::rt::Blocking, paths: &Paths, rows: Vec<(Song, PathBuf)>, genre: Option<&str>) -> Vec<Song> {
        rows.into_iter().filter_map(|(song, path)| {
            if song.spoken.is_some() || song.radio_url.is_some()
                || genre.is_some_and(|wanted| !song.genre.as_deref().is_some_and(|value| value.eq_ignore_ascii_case(wanted))) {
                return None;
            }
            let path = paths.media_path(&path)?;
            // Flat (an earlier version) or sharded.
            let (_, metadata) = paths.locate_metadata(blocking, &path).ok().flatten()?;
            (metadata.len() > 0).then_some(song)
        }).collect()
    }

    /// Ordinary generated diagnostics; runtime callers use owned async snapshots.
    pub(crate) fn offline_song_path(&self, id: &str) -> Option<PathBuf> {
        let id = id.to_owned();
        self.db_worker.run_blocking(move |blocking, db| db.offline_song(blocking, &id))
    }

    /// Ids of every downloaded song.
    pub fn offline_ids(&self) -> std::collections::HashSet<String> {
        self.db_worker.run_blocking(|blocking, db| db.offline_ids(blocking))
    }

    /// Bytes used by the song cache and by downloads.
    #[cfg(test)]
    pub fn storage_used(&self) -> (u64, u64) {
        let db = self.db();
        (db.song_cache_size(&crate::rt::Blocking::dedicated_thread()), db.offline_size(&crate::rt::Blocking::dedicated_thread()))
    }

    /// Allocated bytes on the active filesystem, deduplicated by inode across
    /// music/spoken downloads and cache. Stale/missing entries consume no space;
    /// other read errors are surfaced rather than presented as zero usage.
    #[cfg(test)]
    pub(crate) fn storage_allocations(&self) -> Result<(u64, u64), String> {
        let paths = self.db().indexed_media_paths(&crate::rt::Blocking::dedicated_thread()).map_err(|_| "Unable to read storage index")?;
        Self::allocated_media(&crate::rt::Blocking::dedicated_thread(), &self.paths, paths)
    }

    pub(crate) fn storage_allocations_async(self: &Arc<Self>)
        -> impl std::future::Future<Output = Result<(u64, u64), String>> + Send + 'static {
        let generation = self.client_generation(); let current = self.client_generation.clone();
        let paths = self.paths.clone();
        let indexed = self.music_db_run(generation, move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Server("Storage changed while reading. Reopen this page.".into())); }
            db.indexed_media_paths(blocking).map_err(|_| LibError::Server("Unable to read storage index".into()))
        });
        let current = self.client_generation.clone();
        async move {
            let indexed = indexed.await.map_err(|error| error.to_string())?;
            crate::rt::jobs().run_isolated(move |blocking| {
                let result = Self::allocated_media(blocking, &paths, indexed);
                if current.load(Ordering::SeqCst) != generation { return Err("Storage changed while reading. Reopen this page.".into()); }
                result
            }).await.map_err(|_| "Storage worker stopped".to_string())?
        }
    }

    /// Indexed paths may be flat (an earlier version) or sharded.
    fn allocated_media(blocking: &crate::rt::Blocking, roots: &Paths, paths: Vec<(bool, PathBuf)>) -> Result<(u64, u64), String> {
        use std::os::unix::fs::MetadataExt;
        let root = roots.media_root().canonicalize().map_err(|_| "Storage unavailable")?;
        let device = std::fs::metadata(&root).map_err(|_| "Storage unavailable")?.dev();
        let mut seen = std::collections::HashSet::new();
        let (mut cache, mut downloads) = (0u64, 0u64);
        for (download, path) in paths {
            let Some((path, metadata)) = roots.locate_metadata(blocking, &path).map_err(|_| "Unable to read media usage")? else { continue };
            if metadata.dev() != device { continue; }
            let actual = path.canonicalize().map_err(|_| "Unable to read media usage")?;
            if !actual.starts_with(&root) || !seen.insert((metadata.dev(), metadata.ino())) { continue; }
            let allocated = metadata.blocks().checked_mul(512).ok_or("Media usage is too large")?;
            let total = if download { &mut downloads } else { &mut cache };
            *total = total.checked_add(allocated).ok_or("Media usage is too large")?;
        }
        Ok((cache, downloads))
    }

    #[cfg(test)]
    pub(crate) fn reset_ui_fixture(&self) {
        self.music_epoch.fetch_add(1, Ordering::SeqCst);
        self.db().reset(&crate::rt::Blocking::dedicated_thread());
        self.downloads.set_paused(false);
    }

    // Songs: open, stream, cache.

    fn start_prefetch(lib: &Arc<Library>, rx: tokio::sync::watch::Receiver<Option<Vec<Song>>>) {
        crate::rt::io().spawn(prefetch::run(Arc::downgrade(lib), rx));
    }

    /// Sends queued actions and checks for the server while it is away. With
    /// nothing queued and the server reachable it sleeps until woken (or
    /// SYNC_IDLE), so an idle Q2 is not woken every ten seconds.
    fn start_sync(lib: &Arc<Library>) {
        let weak = Arc::downgrade(lib);
        let wake = lib.sync_wake.clone();
        crate::rt::io().spawn(async move {
            let mut wait = SYNC_RETRY;
            loop {
                let kicked = tokio::select! {
                    () = wake.notified() => true,
                    () = tokio::time::sleep(wait) => false,
                };
                let mut urgent = match weak.upgrade() {
                    Some(lib) => (lib.probe_now.swap(false, Ordering::Relaxed) | lib.sync_urgent.swap(false, Ordering::Relaxed)),
                    None => return,
                };
                // Coalesce bursts (skipping through songs) as the fixed
                // ten-second loop did; a network-up probe goes now.
                if kicked && !urgent {
                    let Some(now) = sync_wait::coalesce(&wake, || {
                        weak.upgrade().map(|lib| (lib.probe_now.swap(false, Ordering::Relaxed) | lib.sync_urgent.swap(false, Ordering::Relaxed)))
                    }).await else { return };
                    urgent = now;
                }
                let Some(lib) = weak.upgrade() else { return };
                // Queued plays and queue saves: the listener does not wait on them.
                wait = health::with_priority(Priority::Background, lib.sync_round(urgent)).await;
            }
        });
    }

    /// One pass of the sync worker: Audiobookshelf progress, the server
    /// probe while it is away, queued actions, the queue save. Returns how
    /// long to wait before the next pass without a wake.
    async fn sync_round(self: &Arc<Self>, urgent: bool) -> Duration {
        let mut wait = SYNC_IDLE;
        if !self.maintenance_link_ready() {
            // Once the link is known down, preserve the ordinary local
            // queue checkpoint. The server queue is offered on reachable links.
            if !self.offline_mode() && !self.radio_dozing() && !self.link_pending.load(Ordering::Acquire) {
                self.flush_queue_async().await;
            }
            return wait;
        }
        if self.sync_spoken_async().await { wait = SYNC_RETRY; }
        if !self.maintenance_link_ready() || !self.is_configured() { return wait; }
        if self.health.transport_down() {
            self.flush_queue_async().await;
            return wait;
        }
        if !self.online.load(Ordering::Relaxed) {
            wait = SYNC_RETRY;
            let due = urgent || self.last_probe.lock().unwrap().elapsed() > Duration::from_secs(20);
            if due {
                *self.last_probe.lock().unwrap() = Instant::now();
                self.probe_if_due().await;
            }
        }
        if self.is_online() {
            let generation = self.client_generation();
            let pending = match self.music_db_run(generation, |blocking, db| Ok(db.pending(blocking, 20))).await {
                Ok(pending) => pending,
                Err(_) => return SYNC_RETRY,
            };
            for (id, call) in pending {
                if !self.maintenance_link_ready() || self.health.transport_down() { return SYNC_IDLE; }
                if self.send_pending(&call, generation).await.is_err() { break; }
                // Stored pages show the old heart or rating: after a restart
                // (the in-memory overlay gone) a day-old album page showed it.
                let feedback = !matches!(call, PendingCall::Scrobble { .. });
                // A request and its acknowledgement belong to the same account
                // snapshot, including a replacement while the reply is in flight.
                if self.music_db_run(generation, move |blocking, db| {
                    db.drop_pending(blocking, id);
                    if feedback { db.mark_responses_stale(blocking).map_err(|_| LibError::Offline)?; }
                    Ok(())
                }).await.is_err() { break; }
            }
        }
        if self.db_run(|blocking, db| !db.pending(blocking, 1).is_empty()).await { wait = SYNC_RETRY; }
        if self.maintenance_link_ready() { self.flush_queue_async().await; }
        wait
    }


}

impl Drop for Library {
    fn drop(&mut self) { self.sync_wake.notify_one(); self.shelf_auto.wake.notify_one(); self.direct_listens.wake(); }
}

/// How long a test waits for an event that must come (a reply, a released
/// gate, a lane's worker). Not a timing assertion: the order of events is
/// fixed by channels, and the tests share one process-wide serial download
/// lane and job pool, so a wait's length says nothing about the code. This
/// only ends a run that would otherwise hang.
#[cfg(test)]
pub(crate) const TEST_LIVENESS: Duration = Duration::from_secs(120);

#[cfg(test)]
pub(super) fn generated_test_directory(label: &str) -> PathBuf {
    let root = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"));
    assert!(root.is_absolute(), "generated test root must be absolute");
    root.join(format!("{label}-{:016x}", fastrand::u64(..)))
}

/// A media store's files in both layouts, without directories.
#[cfg(test)]
pub(crate) fn stored_files(root: &Path) -> Vec<PathBuf> {
    crate::shard::walk(&crate::rt::Blocking::dedicated_thread(), root, true, usize::MAX).unwrap().0
        .into_iter().filter(|(_, _, metadata)| !metadata.is_dir()).map(|(_, path, _)| path).collect()
}

pub fn free_space(blocking: &crate::rt::Blocking, path: &Path) -> Option<u64> {
    let st=crate::platform::system::filesystem(blocking,path).ok()?;
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// The sync worker's cadence while offline or with work left, and its
/// longest sleep with nothing queued.
const SYNC_RETRY: Duration = Duration::from_secs(10);
/// A wake (a track start's queue save) waits this long to merge a burst of
/// skips, short enough to share the radio's awake period with the track
/// start's fetch (it stays out of power save ~6 s after traffic).
const SYNC_COALESCE: Duration = Duration::from_secs(3);
const SYNC_IDLE: Duration = Duration::from_secs(300);

/// The player's view of the library.
pub struct Tracks(pub Arc<Library>);

/// The library as the player sees it (the contract is on `TrackSource`).
///
/// `open` returns, in order: a download, a cached copy at the streaming
/// quality or better, or a new stream that plays while it fills. A stream's
/// reader holds its cache transfer (`q2_machines::transfer`): dropping the
/// reader releases the hold, and opening another song stops orphaned
/// transfers that are under 80 % complete.
impl TrackSource for Tracks {
    fn open(&self, blocking: &crate::rt::Blocking, song:&Song, interrupt:&Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack,String> { self.open_track(blocking,song,interrupt,false) }
    fn open_complete(&self, blocking: &crate::rt::Blocking, song:&Song, interrupt:&Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack,String> { self.open_track(blocking,song,interrupt,true) }

    fn upcoming(&self, songs: &[Song]) {
        self.0.prefetch_upcoming(songs);
    }

    fn now_playing(&self, song: &Song) {
        if let Some(request) = self.0.scrobble_now_playing(song) {
            crate::rt::io().spawn(request);
        }
    }

    fn played(&self, song: &Song, started_ms: u64) {
        self.0.scrobble_played(song, started_ms);
    }

    fn direct_listen_generation(&self) -> Option<u64> {
        self.0.direct_listens.admission()
    }

    fn direct_song_played(&self, generation: u64, song: &Song, started_ms: u64) {
        if song.radio_url.is_some() || song.spoken.is_some() { return; }
        for dest in [direct_listens::Destination::ListenBrainz, direct_listens::Destination::LastFm] {
            self.0.submit_direct(generation, dest, direct_listens::Listen {
                artist: song.artist.clone(), title: song.title.clone(), album: Some(song.album.clone()),
                duration_ms: (song.duration != 0).then_some(u64::from(song.duration) * 1000),
                started_at: started_ms / 1000, station: None,
            });
        }
    }

    fn direct_radio_played(&self, generation: u64, station: &Song, artist: &str, title: &str, started_ms: u64, kind: crate::audio::player::RadioListen) {
        let (dest, duration_ms) = match kind {
            crate::audio::player::RadioListen::ListenBrainz => (direct_listens::Destination::ListenBrainz, None),
            crate::audio::player::RadioListen::LastFm { duration_ms } => (direct_listens::Destination::LastFm, duration_ms),
        };
        self.0.submit_direct(generation, dest, direct_listens::Listen {
            artist: artist.into(), title: title.into(), album: None, duration_ms,
            started_at: started_ms / 1000, station: Some(station.id.clone()),
        });
    }

    fn spoken_resume(&self, queue:&[Song],index:usize,fallback_ms:u64)->Result<(usize,u64),String> {
        let Some(spoken)=queue.get(index).and_then(|s|s.spoken.as_ref()) else{return Ok((index,fallback_ms))};
        let position=self.0.spoken_position(&spoken.id)?.map(|p|p.position_ms).unwrap_or(spoken.offset_ms.saturating_add(fallback_ms));
        Ok(spoken::selection(queue,index,position))
    }
    fn spoken_skip(&self)->crate::config::SpokenSettings {self.0.spoken_skip()}
    fn spoken_speed(&self,song:&Song)->Result<Option<u16>,String> {self.0.spoken_speed(song)}
    fn set_spoken_speed(&self,song:&Song,percent:Option<u16>)->Result<(),String> {self.0.set_spoken_speed(song,percent)}
    fn checkpoint(&self,song:&Song,position_ms:u64)->Result<(),String> {self.0.save_spoken(song,position_ms)}
    fn checkpoint_async(self: Arc<Self>, song: Song, position_ms: u64, session: Option<crate::audio::listening::Session>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
        Box::pin(self.0.save_spoken_session_async(&song, position_ms, session))
    }
    fn save_queue(&self, queue: &[Song], index: usize, position_ms: u64) {
        if queue.iter().any(|s| s.radio_url.is_some()) { return; }
        let snapshot = QueueSave { songs: queue.to_vec(), index, position_ms };
        *self.0.queue_save.lock().unwrap() = Some(snapshot);
        self.0.wake_sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_rows_settings_and_fonts_progress_with_general_pool_saturated() {
        crate::rt::test_support::isolated_pool_test(
            "library::tests::local_rows_settings_and_fonts_progress_with_general_pool_saturated",
            "Q2_LOCAL_WORK_POOL_CHILD",
            || {
                use crate::gfx::font::FontLoad;
                use crate::ui::list::{Row, RowAction};
                let blocking = crate::rt::Blocking::dedicated_thread();
                let paths = Paths::new(generated_test_directory("local-work-pool"));
                let mut config = Config::default();
                let (lib, _prefetch) = Library::create(paths.clone(), &config, Arc::new(|_| {}));
                config.playback.volume = 37;
                // An actual generated fallback file, not an absent-path fast path.
                let fallback = paths.root.join("fallback.ttf");
                std::fs::write(&fallback, include_bytes!("../../../../assets/fonts/OpenSans-Regular.ttf")).unwrap();
                let (rows_tx, rows_rx) = mpsc::channel();
                let (fonts_tx, fonts_rx) = mpsc::channel();
                let (rows_task, save, font_thread, rows, saved, font_width) =
                    crate::rt::test_support::with_saturated_general_pool(Duration::from_secs(5), || {
                        let rows_task = crate::rt::io().spawn(async move {
                            let rows = crate::ui::screens::menu::on_pool(lib, |_| {
                                ["Generated album", "Generated song"].into_iter()
                                    .map(|title| Row::new(title, RowAction::None)).collect::<Vec<_>>()
                            }).await;
                            let _ = rows_tx.send(rows.into_iter().map(|row| row.title).collect::<Vec<_>>());
                        });
                        let save = paths.save_config_async(config.clone());
                        let fonts = FontLoad::start(vec![fallback]);
                        // Startup owns this ordinary join; never park a pool worker
                        // waiting for another local job. Retain it on failed admission.
                        let font_thread = std::thread::spawn(move || {
                            let width = fonts.finish().unwrap().measure("Generated", 16, crate::gfx::Weight::Regular);
                            let _ = fonts_tx.send(width);
                        });
                        let saved = crate::rt::Ui::new().unwrap().block_on(async {
                            tokio::time::timeout(Duration::from_secs(3), save.clone().wait()).await
                        });
                        let rows = rows_rx.recv_timeout(Duration::from_secs(3));
                        let font_width = fonts_rx.recv_timeout(Duration::from_secs(3));
                        (rows_task, save, font_thread, rows, saved, font_width)
                    });
                // Release/join the deliberate starvation before asserting outcomes;
                // old routing must fail without leaking accepted writes/font work.
                crate::rt::Ui::new().unwrap().block_on(async {
                    tokio::time::timeout(crate::library::TEST_LIVENESS, async {
                        rows_task.await.unwrap();
                        save.wait().await.unwrap();
                    }).await.unwrap();
                });
                font_thread.join().unwrap();
                assert!(matches!(saved, Ok(Ok(()))), "settings waited behind general-pool work");
                assert_eq!(rows.unwrap(), ["Generated album", "Generated song"]);
                assert!(font_width.unwrap() > 0);
                assert_eq!(Config::load(&blocking, &paths.config()), config);
            },
        );
    }

    #[test]
    fn async_badge_snapshot_orders_database_work_and_rejects_replaced_account() {
        for replace in [false, true] {
            let root = generated_test_directory("badge-db-owner");
            let (lib, _) = Library::create(Paths::new(root.clone()), &Config::default(), Arc::new(|_| {}));
            let song = Song { id: "generated-badge-song".into(), ..Default::default() };
            let set = OfflineSet { kind: "album".into(), id: "generated-badge-album".into(), name: "Generated".into(), artist: String::new(), cover_art: None, song_ids: vec![song.id.clone()] };
            let generation = lib.client_generation();
            let (entered, started) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            lib.db_post(move |_blocking, db| {
                entered.send(()).unwrap();
                wait.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                db.enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set, std::slice::from_ref(&song)).unwrap();
                db.put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &root.join("generated.flac"), 1);
            });
            started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let view = crate::rt::Ui::new().unwrap().block_on(async {
                let snapshot = lib.download_view_async(generation);
                tokio::pin!(snapshot);
                tokio::select! {
                    value = &mut snapshot => panic!("badge query skipped earlier database work: {}", value.is_some()),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
                if replace { lib.client_generation.fetch_add(1, Ordering::SeqCst); }
                release.send(()).unwrap();
                tokio::time::timeout(crate::library::TEST_LIVENESS, snapshot).await.unwrap()
            });
            if replace {
                assert!(view.is_none(), "old-account badges cannot publish");
            } else {
                let view = view.unwrap();
                assert!(view.queued.contains("generated-badge-song"));
                assert!(view.offline.contains("generated-badge-song"));
                assert_eq!(view.albums["generated-badge-album"], ["generated-badge-song"]);
                assert!(view.active.is_none());
                let sync = lib.download_view();
                assert_eq!(view.queued, sync.queued);
                assert_eq!(view.albums, sync.albums);
                assert_eq!(view.offline, sync.offline);
            }
        }
    }

    #[test]
    fn retry_downloads_awaits_database_and_refuses_a_replaced_account() {
        use std::task::{Context, Waker};
        let (lib, config, _root) = library_for("http://127.0.0.1:9", "retry-downloads");
        let song = Song { id: "generated-retry".into(), ..Default::default() };
        let set = OfflineSet { kind: "album".into(), id: "generated".into(), name: "Generated".into(), artist: String::new(), cover_art: None, song_ids: vec![song.id.clone()] };
        {
            let db = lib.db();
            db.enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set, &[song.clone()]).unwrap();
            db.finish_download(&crate::rt::Blocking::dedicated_thread(), &song.id, Some("generated failure"));
        }
        let mut stale = Box::pin(lib.retry_downloads());
        let held = lib.db();
        assert!(stale.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        // Admit real account changes while the DB is held. The middle binding
        // is superseded, but the old action must still refuse its stale epoch.
        let mut middle = config.clone(); middle.server.user = "generated-other".into();
        let superseded = lib.configure_async(&middle);
        let current = lib.configure_async(&config);
        drop(held);
        let ui = crate::rt::Ui::new().unwrap();
        ui.block_on(async {
            tokio::time::timeout(crate::library::TEST_LIVENESS, async {
                superseded.await.unwrap(); current.await.unwrap(); stale.await;
            }).await.expect("account binding or stale retry did not complete");
        });
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 1));
        ui.block_on(async {
            tokio::time::timeout(crate::library::TEST_LIVENESS, lib.retry_downloads()).await
                .expect("retry did not complete after binding");
        });
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0));
    }

    // Check the Python producer's configuration and SQLite format through
    // the player. Generated byte media tests storage, not decoding.
    fn cadence_api_fixture(scene: &str) -> PathBuf {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..").canonicalize().unwrap();
        let base = workspace.join(format!("build/perf/tests/cadence-api-{}-{:016x}",
            std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(&base).unwrap();
        let fixture = base.join("generated.m4b");
        std::fs::write(&fixture, b"generated API fixture; no audio decoding").unwrap();
        let data = base.join("data");
        let status = std::process::Command::new("python3")
            .arg(workspace.join("tests/fixtures/cadence.py"))
            .arg(&data).arg(&fixture).arg(scene).status().unwrap();
        assert!(status.success(), "generated cadence producer failed");
        data
    }

    #[test]
    fn cadence_actual_producer_settings_strictly_deserialize() {
        for scene in ["visible", "dark"] {
            let data = cadence_api_fixture(scene);
            let text = std::fs::read_to_string(data.join("config.toml")).unwrap();
            let strict = toml::from_str::<Config>(&text).expect("cadence producer must never rely on invalid-setting fallback");
            assert_eq!(strict, Config::parse(&text, "generated cadence API fixture"));
            assert!(!strict.display.night_warmth && !strict.display.clock_in_title && !strict.wifi.enabled && !strict.tailscale.enabled);
            assert_eq!(strict.power.sleep_minutes, 0);
            assert_eq!(strict.display.backlight_seconds, if scene == "dark" { 1 } else { 0 });
            std::fs::remove_dir_all(data.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn cadence_actual_producer_queue_survives_library_open_and_server_binding() {
        for scene in ["visible", "dark"] {
            let data = cadence_api_fixture(scene);
            // Exercise binding independently of the strict-settings assertion.
            let config = Config::load(&crate::rt::Blocking::dedicated_thread(), &data.join("config.toml"));
            let lib = Library::open(Paths::new(data.clone()), &config, Arc::new(|_| {}));
            lib.set_offline_mode(true);
            assert!(lib.is_configured());
            let (songs, index, position) = lib.saved_queue().expect("producer queue must survive actual configured-server binding");
            assert_eq!((songs.len(), index, position), (1, 0, 0));
            assert_eq!(songs[0].id, "generated-cadence");
            assert_eq!(lib.db().get(&crate::rt::Blocking::dedicated_thread(), "server").as_deref(), Some("http://127.0.0.1:9|generated"));
            assert_eq!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &songs[0].id), Some(data.join("generated.m4b")));
            assert!(data.join("generated.m4b").is_file());
            drop(lib);
            std::fs::remove_dir_all(data.parent().unwrap()).unwrap();
        }
    }

    /// A db closure that reaches for `Library::db()` would wait on the mutex
    /// its own thread holds: debug builds refuse it; the worker carries on.
    #[test]
    fn a_db_closure_may_not_take_the_library_mutex() {
        let (lib, _) = music_binding_fixture();
        let inner = lib.clone();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        lib.db_post(move |_, _| {
            let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(inner.db()))).is_err();
            tx.send(refused).unwrap();
        });
        assert!(rx.recv_timeout(crate::library::TEST_LIVENESS).unwrap());
        assert_eq!(runtime.block_on(lib.db_run(|_blocking, db| { db.set(&crate::rt::Blocking::dedicated_thread(), "after", "1"); db.get(&crate::rt::Blocking::dedicated_thread(), "after") })).as_deref(), Some("1"));
    }

    /// Replacing the client bumps the generation: before the database binding
    /// of a new server, and on every replacement.
    #[test]
    fn every_client_replacement_moves_the_generation() {
        let (lib, mut config) = music_binding_fixture();
        let first = lib.client_generation();
        config.server.user = "generated-new-user".into();
        lib.configure(&config);
        let second = lib.client_generation();
        assert!(second > first);
        config.server.url = String::new();
        lib.configure(&config);
        assert!(lib.client_generation() > second);
    }

    /// A generated Navidrome answering getAlbum after `delay`, counting
    /// requests; stops when the returned flag is set.
    fn slow_album_server(delay: Duration) -> (String, Arc<AtomicBool>, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (done, requests) = (stop.clone(), count.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let Ok((mut socket, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(5)); continue };
                let requests = requests.clone();
                std::thread::spawn(move || {
                    socket.set_nonblocking(false).unwrap();
                    let mut reader = BufReader::new(socket.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() { return; }
                    let ping = line.contains("/rest/ping");
                    loop { line.clear(); if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() { break; } }
                    if !ping { requests.fetch_add(1, Ordering::SeqCst); std::thread::sleep(delay); }
                    let body = if ping { r#"{"subsonic-response":{"status":"ok"}}"# } else {
                        r#"{"subsonic-response":{"status":"ok","album":{"id":"generated-album","name":"Generated","song":[{"id":"generated-song","title":"Generated"}]}}}"# };
                    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                });
            }
        });
        (url, stop, count)
    }

    fn library_for(url: &str, name: &str) -> (Arc<Library>, Config, PathBuf) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../build/perf/tests/{name}-{:016x}", fastrand::u64(..)));
        let mut config = Config::default();
        config.storage.reserve_mb = 0;
        config.server.url = url.into();
        config.server.user = "generated".into();
        config.server.password = "generated".into();
        (Library::open(Paths::new(root.clone()), &config, Arc::new(|_| {})), config, root)
    }

    #[test]
    fn pending_actions_survive_local_binding_failure_and_cannot_cross_accounts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "generated-first".into();
        let (lib, _) = Library::create(Paths::new(generated_test_directory("pending-binding")), &config, Arc::new(|_| {}));
        let call = PendingCall::Star { kind: "song".into(), id: "generated-favorite".into(), on: true };
        lib.db().push_pending(&crate::rt::Blocking::dedicated_thread(), &call);
        let old_generation = lib.client_generation();
        let sql = rusqlite::Connection::open(lib.paths.database()).unwrap();
        sql.execute_batch("CREATE TRIGGER generated_reject_binding BEFORE UPDATE ON kv WHEN NEW.key='server' BEGIN SELECT RAISE(ABORT,'generated binding failure'); END;").unwrap();
        config.server.user = "generated-second".into();
        crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                assert!(lib.configure_async(&config).await.is_err());
                assert!(matches!(lib.send_pending(&call, lib.client_generation()).await, Err(LibError::Offline)),
                    "local binding failure must not acknowledge a durable action as server-refused");
                assert_eq!(lib.db_run(|blocking, db| db.pending(blocking, 10).len()).await, 1);
                assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
                crate::rt::jobs().run_isolated(move |_| sql.execute_batch("DROP TRIGGER generated_reject_binding;").unwrap()).await.unwrap();
                lib.configure_async(&config).await.unwrap();
                lib.db_run(|blocking, db| db.push_pending(blocking, &PendingCall::Rating { id: "generated-new-account".into(), rating: 4 })).await;
                assert!(matches!(lib.send_pending(&call, old_generation).await, Err(LibError::Offline)),
                    "an already-read old action cannot be sent through the new account");
                assert!(lib.music_db_run(old_generation, |blocking, db| { db.drop_pending(blocking, 1); Ok(()) }).await.is_err());
                assert_eq!(lib.db_run(|blocking, db| db.pending(blocking, 10).len()).await, 1);
                assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
            }).await.expect("binding/action admission did not complete");
        });
    }

    #[test]
    fn network_status_waits_off_executor_and_refuses_a_replaced_client_reply() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "generated-original".into();
        let (lib, _) = Library::create(Paths::new(generated_test_directory("network-owner")), &config, Arc::new(|_| {}));
        let (arrived, receiving) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + crate::library::TEST_LIVENESS;
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing generated request");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            loop { let mut line = String::new(); reader.read_line(&mut line).unwrap(); if line == "\r\n" || line.is_empty() { break; } }
            arrived.send(()).unwrap();
            held.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let body = r#"{"subsonic-response":{"status":"ok"}}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let guard = lib.client.write().unwrap();
        let (begun, started) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let owner = lib.clone();
        crate::rt::io().spawn(async move {
            begun.send(()).unwrap();
            let response = owner.fetch_net(&calls::ping().request).await;
            done.send(response).unwrap();
        });
        started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let (pulse, progress) = mpsc::channel();
        crate::rt::io().spawn(async move { pulse.send(()).unwrap(); });
        let responsive = progress.recv_timeout(Duration::from_millis(250));
        drop(guard); // Release before asserting, including the negative case.
        responsive.expect("client contention blocked the I/O executor");
        receiving.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        config.server.user = "generated-replacement".into();
        lib.configure(&config);
        assert!(!lib.is_online());
        release.send(()).unwrap();
        assert!(matches!(result.recv_timeout(crate::library::TEST_LIVENESS).unwrap(), Err(LibError::Offline)));
        assert!(!lib.is_online(), "old success cannot mark the replacement online");
        server.join().unwrap();
    }

    #[test]
    fn async_sync_round_probes_drains_actions_and_saves_the_queue() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "generated".into();
        config.playback.sync_queue = true;
        let root = generated_test_directory("sync-round");
        let (lib, _) = Library::create(Paths::new(root), &config, Arc::new(|_| {}));
        for call in [PendingCall::Scrobble { id: "generated-song".into(), time_ms: 123 },
            PendingCall::Star { kind: "song".into(), id: "generated-song".into(), on: true },
            PendingCall::Rating { id: "generated-song".into(), rating: 4 }] {
            lib.db().push_pending(&crate::rt::Blocking::dedicated_thread(), &call);
        }
        *lib.queue_save.lock().unwrap() = Some(QueueSave {
            songs: vec![Song { id: "generated-song".into(), ..Default::default() }], index: 0, position_ms: 7654 });
        let task = lib.clone();
        lib.set_radio_doze(true);
        lib.set_radio_doze(false);
        let wait = shelf_browse::wait(async move { Ok(task.sync_round(true).await) }).unwrap();
        assert_eq!(wait, SYNC_IDLE);
        assert!(matches!(listener.accept(), Err(err) if err.kind() == std::io::ErrorKind::WouldBlock));
        assert_eq!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 20).len(), 3);
        assert!(lib.queue_save.lock().unwrap().is_some());
        lib.link_available();

        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + crate::library::TEST_LIVENESS;
            for expected in ["/rest/ping", "/rest/scrobble", "/rest/star", "/rest/setRating", "/rest/savePlayQueue"] {
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(value) => break value,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing sync request {expected}");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();reader.read_line(&mut line).unwrap();
                assert!(line.contains(expected), "unexpected generated request {line}");
                loop { line.clear();reader.read_line(&mut line).unwrap();if line == "\r\n" || line.is_empty() { break; } }
                let body = r#"{"subsonic-response":{"status":"ok"}}"#;
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        let task = lib.clone();
        let wait = shelf_browse::wait(async move { Ok(task.sync_round(true).await) }).unwrap();
        server.join().unwrap();
        assert_eq!(wait, SYNC_RETRY, "first offline probe retains retry cadence");
        assert!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 20).is_empty());
        assert!(lib.queue_save.lock().unwrap().is_none());
        assert_eq!(lib.saved_queue().unwrap().2, 7654);
        assert!(lib.is_online());
    }

    /// A thread and a task asking for the same album at once make one
    /// request; the second is answered from the cache the first filled.
    #[test]
    fn a_thread_and_a_task_share_one_request_per_album() {
        let (url, stop, count) = slow_album_server(Duration::from_millis(300));
        let (lib, _, root) = library_for(&url, "fetch-coalesce");
        let call = calls::album("generated-album");
        let task = crate::rt::io().spawn(lib.clone().fetch_async(call.clone(), Policy::PreferCache));
        std::thread::sleep(Duration::from_millis(50));
        let from_thread = lib.fetch(&call, Policy::PreferCache).unwrap().value;
        let (tx, rx) = std::sync::mpsc::channel();
        crate::rt::io().spawn(async move { let _ = tx.send(task.await.unwrap().map(|f| f.value.song[0].id.clone())); });
        assert_eq!(rx.recv_timeout(crate::library::TEST_LIVENESS).unwrap().unwrap(), "generated-song");
        assert_eq!(from_thread.song[0].id, "generated-song");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A synchronous fetch while holding a `db()` guard would wait for the
    /// `db` thread, which waits for that guard's mutex: refused in every build.
    #[test]
    #[cfg(panic = "unwind")]
    fn a_fetch_holding_the_database_is_refused() {
        let (lib, _, root) = library_for("http://127.0.0.1:9", "fetch-guard");
        let guard = lib.db();
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lib.fetch(&calls::ping(), Policy::NetworkOnly))).is_err();
        drop(guard);
        assert!(refused);
        assert!(lib.fetch(&calls::ping(), Policy::CacheOnly).is_err(), "without the guard it runs (and finds nothing cached)");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn synchronous_adapters_refuse_pool_and_database_owners() {
        let (lib, _, root) = library_for("http://127.0.0.1:9", "wait-owners");
        let refused = || std::panic::catch_unwind(||
            shelf_browse::wait(async { Ok::<_, LibError>(()) })).is_err();
        // A ready future makes even a broken guard terminate: this test never
        // waits for the DB from inside its own job merely to test rejection.
        let held = lib.db();
        assert!(refused());
        drop(held);
        let runtime = crate::rt::Ui::new().unwrap();
        assert!(runtime.block_on(lib.db_run(move |_, _| refused())));
        let owner = lib.clone();
        let pool = runtime.block_on(crate::rt::spawn_blocking(move |_| {
            let fetch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                owner.fetch(&calls::ping(), Policy::CacheOnly))).is_err();
            (fetch, refused())
        })).unwrap();
        assert_eq!(pool, (true, true));
        assert_eq!(shelf_browse::wait(async { Ok::<_, LibError>(7) }).unwrap(), 7);
        assert!(lib.fetch(&calls::ping(), Policy::CacheOnly).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Played songs are queued on the db thread in the order they ended,
    /// with no thread per song.
    #[test]
    fn played_scrobbles_queue_in_order() {
        let root = generated_test_directory("played-order");
        let mut config = Config::default();
        config.server.url = "http://127.0.0.1:9".into();
        config.server.user = "generated".into();
        let (lib, _) = Library::create(Paths::new(root.clone()), &config, Arc::new(|_| {}));
        let tracks = Tracks(lib.clone());
        for i in 0..30u64 {
            tracks.played(&Song { id: format!("generated-{i}"), ..Default::default() }, 1000 + i);
        }
        lib.db_worker.drain();
        let ids: Vec<String> = lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 100).into_iter().filter_map(|(_, c)| match c {
            PendingCall::Scrobble { id, .. } => Some(id), _ => None }).collect();
        assert_eq!(ids, (0..30).map(|i| format!("generated-{i}")).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(root);
    }

    /// The server changes while a request is out: its response is not cached
    /// for the new server, and the caller is told the old server is gone.
    #[test]
    fn a_response_from_a_replaced_server_is_not_cached() {
        let (url, stop, count) = slow_album_server(Duration::from_millis(400));
        let (lib, mut config, root) = library_for(&url, "fetch-server-change");
        let call = calls::album("generated-album");
        let fetching = { let lib = lib.clone(); let call = call.clone(); std::thread::spawn(move || lib.fetch(&call, Policy::NetworkOnly).map(|f| f.value.album.id)) };
        let start = Instant::now();
        while count.load(Ordering::SeqCst) == 0 { assert!(start.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(5)); }
        config.server.user = "generated-other".into();
        lib.configure(&config);
        assert!(matches!(fetching.join().unwrap(), Err(LibError::Offline)));
        assert!(lib.db().response(&crate::rt::Blocking::dedicated_thread(), &call.request.cache_key()).is_none());
        stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(root);
    }

    fn music_binding_fixture() -> (Arc<Library>,Config) {
        let root=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../build/perf/tests/music-binding-{:016x}",fastrand::u64(..)));
        let mut config=Config::default();config.storage.reserve_mb=0;config.server.url="http://127.0.0.1:9".into();config.server.user="generated-old-user".into();
        (Library::open(Paths::new(root),&config,Arc::new(|_|{})),config)
    }
    #[test]
    fn downloaded_inventory_and_shuffle_await_database_order_and_refuse_replaced_account() {
        for replace in [false, true] {
            let root = generated_test_directory("downloaded-db-owner");
            let (lib, _) = Library::create(Paths::new(root), &Config::default(), Arc::new(|_| {}));
            let song = Song { id: "generated-owned-download".into(), ..Default::default() };
            let path = lib.paths.offline().join("generated.flac");
            std::fs::write(&path, b"generated audio").unwrap();
            let set = OfflineSet { kind: "album".into(), id: "generated-album".into(), name: "Generated".into(), artist: String::new(), cover_art: None, song_ids: vec![song.id.clone()] };
            let (entered, started) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            lib.db_post(move |_blocking, db| {
                entered.send(()).unwrap();
                wait.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                db.enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set, std::slice::from_ref(&song)).unwrap();
                db.put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &path, 15);
            });
            started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let inventory = lib.download_inventory();
            let shuffle = lib.downloaded_music_async(None);
            let (inventory, shuffle) = crate::rt::Ui::new().unwrap().block_on(async {
                tokio::pin!(inventory, shuffle);
                tokio::select! {
                    _ = &mut inventory => panic!("Downloads skipped earlier database work"),
                    _ = &mut shuffle => panic!("Shuffle skipped earlier database work"),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
                if replace { lib.client_generation.fetch_add(1, Ordering::SeqCst); }
                release.send(()).unwrap();
                tokio::time::timeout(crate::library::TEST_LIVENESS, async { tokio::join!(inventory, shuffle) }).await.unwrap()
            });
            if replace {
                assert!(matches!(inventory, Err(LibError::Offline)));
                assert!(matches!(shuffle, Err(LibError::Offline)));
            } else {
                let inventory = inventory.unwrap();
                assert_eq!((inventory.status.queued, inventory.status.failed), (1, 0));
                assert!(!inventory.status.paused && inventory.status.current.is_none());
                assert_eq!(inventory.sets.len(), 1);
                assert_eq!(inventory.sets[0].0.id, "generated-album");
                assert_eq!(inventory.sets[0].1, 1);
                let songs = shuffle.unwrap();
                assert_eq!(songs.len(), 1);
                assert_eq!(songs[0].id, "generated-owned-download");
            }
        }
    }

    #[test]
    fn offline_shuffle_inventory_uses_present_downloads_not_cache_or_spoken_media() {
        let (lib, _) = Library::create(Paths::new(generated_test_directory("offline-shuffle-inventory")), &Config::default(), Arc::new(|_| {}));
        let runtime = crate::rt::Ui::new().unwrap();
        lib.set_offline_mode(true);
        let mut expected = Vec::new();
        for n in 0..64 {
            let path = lib.paths.offline().join(format!("generated-{n}.wav"));
            let mut song = Song { id: format!("generated-{n:02}"), genre: Some(if n % 2 == 0 {"Jazz"} else {"Rock"}.into()), ..Default::default() };
            match n % 8 {
                0 => {}, // Missing file.
                1 => { std::fs::write(&path, b"").unwrap(); },
                2 => { std::fs::create_dir(&path).unwrap(); },
                3 => { std::fs::write(&path, b"generated audio").unwrap(); song.spoken = Some(Default::default()); },
                4 => { std::fs::write(&path, b"generated audio").unwrap(); song.radio_url = Some("http://127.0.0.1:9/generated".into()); },
                5 => { std::fs::write(&path, b"generated audio").unwrap(); lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, "raw", &path, 15).unwrap(); continue; },
                _ => { std::fs::write(&path, b"generated audio").unwrap(); expected.push(song.id.clone()); },
            }
            lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &path, 15);
        }
        let all = runtime.block_on(lib.downloaded_music_async(None)).unwrap();
        assert_eq!(all.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), expected);
        let jazz = runtime.block_on(lib.downloaded_music_async(Some("jAzZ".into()))).unwrap();
        assert_eq!(jazz.len(), 8);
        assert!(jazz.iter().all(|s| s.genre.as_deref() == Some("Jazz")));
        assert!(runtime.block_on(lib.downloaded_music_async(Some("not-a-generated-genre".into()))).unwrap().is_empty());
        for song in &all { let path = lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap(); std::fs::remove_file(path).unwrap(); }
        assert!(runtime.block_on(lib.downloaded_music_async(None)).unwrap().is_empty(), "removed card files are not playable downloads");
        assert!(!lib.is_online());
    }

    #[test]
    fn failed_music_server_binding_keeps_old_files_unpublished_to_new_client_and_retries() {
        for fault in ["write","read-type","read-table"] {
            let (lib,mut config)=music_binding_fixture();
            let path=lib.paths.offline().join("generated-old-account.mp3");std::fs::write(&path,b"generated old-account audio").unwrap();
            let song=Song {id:"generated-binding-song".into(),..Default::default()};lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song,&path,27);
            let connection=rusqlite::Connection::open(lib.paths.database()).unwrap();
            match fault {
                "write"=>connection.execute_batch("CREATE TRIGGER reject_music_marker BEFORE UPDATE ON kv WHEN NEW.key='server' BEGIN SELECT RAISE(ABORT,'generated marker write fault'); END;").unwrap(),
                "read-type"=>{connection.execute("UPDATE kv SET value=zeroblob(1) WHERE key='server'",[]).unwrap();},
                _=>connection.execute_batch("ALTER TABLE kv RENAME TO generated_inaccessible_kv;").unwrap(),
            }
            config.server.user="generated-new-user".into();lib.online.store(true,Ordering::Relaxed);lib.configure(&config);
            assert!(!lib.is_configured());assert!(!lib.is_online());assert!(path.exists());
            assert_eq!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id),Some(path.clone()));
            let (table,expected_type)=if fault=="read-table" {("generated_inaccessible_kv","text")} else {("kv",if fault=="read-type" {"blob"} else {"text"})};
            let (marker_type,marker):(String,Vec<u8>)=connection.query_row(&format!("SELECT typeof(value),CAST(value AS BLOB) FROM {table} WHERE key='server'"),[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
            assert_eq!(marker_type,expected_type);
            if fault!="read-type" {assert_eq!(marker,b"http://127.0.0.1:9|generated-old-user");}
            match fault {
                "write"=>connection.execute_batch("DROP TRIGGER reject_music_marker;").unwrap(),
                "read-type"=>{connection.execute("UPDATE kv SET value='http://127.0.0.1:9|generated-old-user' WHERE key='server'",[]).unwrap();},
                _=>connection.execute_batch("ALTER TABLE generated_inaccessible_kv RENAME TO kv;").unwrap(),
            }
            lib.configure(&config);assert!(lib.is_configured());assert!(!path.exists());assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
            assert_eq!(lib.client().unwrap().credentials().user,"generated-new-user");
            assert_eq!(connection.query_row("SELECT value FROM kv WHERE key='server'",[],|row|row.get::<_,String>(0)).unwrap(),"http://127.0.0.1:9|generated-new-user");
        }
    }
    #[test]
    fn account_switch_clears_hearts_without_files_but_same_account_transport_change_preserves_them() {
        let (lib,mut config)=music_binding_fixture();let song=Song {id:"generated-shared-song-id".into(),..Default::default()};
        crate::rt::Ui::new().unwrap().block_on(lib.star(ItemKind::Song, &song.id, true)).unwrap();assert!(lib.loved(&song));
        config.server.password="generated-new-password".into();lib.configure(&config);assert!(lib.loved(&song));
        assert!(lib.db().offline_ids(&crate::rt::Blocking::dedicated_thread()).is_empty());assert_eq!(lib.db().song_cache_size(&crate::rt::Blocking::dedicated_thread()),0);
        config.server.user="generated-other-user".into();lib.configure(&config);assert!(!lib.loved(&song));
    }

    #[test]
    fn music_ui_intents_order_queue_remove_and_account_replacement_before_workers_run() {
        let root=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../build/perf/music-intents-{:016x}",fastrand::u64(..)));
        let mut config=Config::default();config.server.url="http://127.0.0.1:9".into();config.server.user="generated".into();config.server.password="synthetic-only".into();
        let lib=Library::open(Paths::new(root),&config,Arc::new(|_|{}));lib.set_offline_mode(true);
        let song=Song {id:"generated-song".into(),..Default::default()};
        let set=OfflineSet {kind:"song".into(),id:song.id.clone(),name:"Generated".into(),artist:String::new(),cover_art:None,song_ids:vec![song.id.clone()]};
        let old=lib.begin_music_download(&set.kind,&set.id).unwrap();lib.remove_download(&set.kind,&set.id).unwrap();
        assert!(lib.download_requested(old,set.clone(),std::slice::from_ref(&song)).is_err());assert!(lib.db().next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_none());
        let old_remove=lib.begin_music_download(&set.kind,&set.id).unwrap();lib.download(set.clone(),std::slice::from_ref(&song)).unwrap();
        assert!(lib.remove_download_requested(old_remove,&set.kind,&set.id).is_err());assert!(lib.offline_set(&set.kind,&set.id).is_some());
        let old=lib.begin_music_download(&set.kind,&set.id).unwrap();config.server.user="other-generated".into();lib.configure(&config);
        assert!(lib.download_requested(old,set.clone(),std::slice::from_ref(&song)).is_err());assert!(lib.db().next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_none());
        let wrong=lib.begin_music_download("song","other").unwrap();assert!(lib.download_requested(wrong,set,std::slice::from_ref(&song)).is_err());
    }

    #[test]
    fn checked_sleep_flush_preserves_queue_on_sql_failure_and_retries_locally() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../build/perf/tests/queue-sleep-{}", fastrand::u64(..)));
        let lib = Library::open(Paths::new(root), &Config::default(), Arc::new(|_| {}));
        *lib.queue_save.lock().unwrap() = Some(QueueSave {
            songs: vec![Song { id: "generated-sleep".into(), ..Default::default() }], index: 0, position_ms: 8123 });
        lib.db().conn_for_sleep_fault(&crate::rt::Blocking::dedicated_thread());
        assert!(lib.prepare_sleep().is_err());
        assert_eq!(lib.queue_save.lock().unwrap().as_ref().unwrap().position_ms, 8123);
        lib.db().clear_sleep_fault(&crate::rt::Blocking::dedicated_thread());
        lib.prepare_sleep().unwrap();
        assert!(lib.queue_save.lock().unwrap().is_none());
        assert_eq!(lib.saved_queue().unwrap().2, 8123);
    }

    #[test]
    fn final_queue_flush_waits_for_an_inflight_local_write() {
        let (lib, _) = Library::create(Paths::new(generated_test_directory("queue-flush")), &Config::default(), Arc::new(|_| {}));
        let connection = rusqlite::Connection::open(lib.paths.database()).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        *lib.queue_save.lock().unwrap() = Some(QueueSave { songs: vec![Song { id: "shutdown".into(), ..Default::default() }], index: 0, position_ms: 700 });
        let worker_lib = lib.clone();
        let worker = std::thread::spawn(move || worker_lib.flush_queue());
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        while lib.queue_save.lock().unwrap().is_some() {
            assert!(Instant::now() < deadline, "DB owner did not take the pending snapshot");
            std::thread::yield_now();
        }
        // The old write is waiting for the real SQLite writer lock. Submitting
        // a newer player snapshot must not wait for it or be erased by it.
        *lib.queue_save.lock().unwrap() = Some(QueueSave { songs: vec![Song { id: "shutdown".into(), ..Default::default() }], index: 0, position_ms: 900 });
        let (done, completion) = mpsc::channel();
        let final_lib = lib.clone();
        let final_flush = std::thread::spawn(move || { final_lib.save_now(); done.send(()).unwrap(); });
        assert!(matches!(completion.recv_timeout(Duration::from_millis(5)), Err(mpsc::RecvTimeoutError::Timeout)));
        connection.execute_batch("ROLLBACK").unwrap();
        completion.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let saved: SavedQueue = serde_json::from_str(&lib.db().get(&crate::rt::Blocking::dedicated_thread(), "queue").unwrap()).unwrap();
        assert_eq!(saved.position_ms, 900);
        assert_eq!(saved.songs[0].id, "shutdown");
        worker.join().unwrap();
        final_flush.join().unwrap();
    }

    #[test]
    fn copied_card_catalog_and_reused_media_leave_originals_untouched() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../build/perf/catalog-migration-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let old = base.join("card/Q2Navidrome");
        std::fs::create_dir_all(old.join("cache/songs")).unwrap();
        std::fs::create_dir_all(old.join("offline")).unwrap();
        let cached = old.join("cache/songs/generated.mp3");
        let downloaded = old.join("offline/generated.mp3");
        std::fs::write(&cached, b"generated cached audio").unwrap();
        std::fs::write(&downloaded, b"generated offline audio").unwrap();
        let old_db = old.join("library.db");
        let track = Song { id: "generated".into(), suffix: Some("mp3".into()), bit_rate: Some(128), ..Default::default() };
        {
            let db = Db::open(&crate::rt::Blocking::dedicated_thread(), &old_db).unwrap();
            db.put_response(&crate::rt::Blocking::dedicated_thread(), "generated-large-catalog", &"x".repeat(22 * 1024 * 1024));
            db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), &track.id, "raw", &cached, 22).unwrap();
            db.put_offline_song(&crate::rt::Blocking::dedicated_thread(), &track, &downloaded, 23);
            db.put_offline_set(&crate::rt::Blocking::dedicated_thread(), &OfflineSet { kind:"album".into(),id:"fixture".into(),name:"Generated".into(),
                artist:String::new(),cover_art:None,song_ids:vec![track.id.clone()] });
        }
        let before = std::fs::read(&old_db).unwrap();
        assert!(before.len() > 22 * 1024 * 1024);
        let paths = Paths::installed(base.join("internal"), base.join("card/Q2Mainline"), base.join("run/media"), true, Some(old));
        paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
        std::fs::copy(&old_db, paths.database()).unwrap();
        let lib = Library::open(paths, &Config::default(), Arc::new(|_| {}));
        assert!(lib.find_local(&track).is_some());
        assert!(!lib.paths.root.join("library.db").exists(), "no catalog on NAND");
        lib.clear_song_cache();
        lib.remove_download("album", "fixture").unwrap();
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &track.id, "raw", &cached, 22).unwrap();
        lib.db().set(&crate::rt::Blocking::dedicated_thread(), "server", "old-generated-account");
        let mut changed = Config::default();
        changed.server.url = "http://127.0.0.1:9".into();
        changed.server.user = "generated".into();
        changed.server.password = "generated-only".into();
        lib.configure(&changed);
        assert!(cached.exists() && downloaded.exists(), "cleanup/reset cannot unlink reused originals");
        assert_eq!(std::fs::read(&old_db).unwrap(), before, "the original DB remains the pre-migration backup");

        let absent = Paths::installed(base.join("internal-no-card"), base.join("absent-card"), base.join("run/no-card"), false, None);
        let temporary = Library::open(absent, &Config::default(), Arc::new(|_| {}));
        assert!(!temporary.paths.root.join("library.db").exists());
        assert!(!temporary.paths.database().exists(), "no-card metadata stays in memory, not on NAND or the RAM media spool");
    }

    fn song(suffix: &str, br: u32) -> Song {
        Song { id: "1".into(), suffix: Some(suffix.into()), bit_rate: Some(br), ..Default::default() }
    }


    #[test]
    fn variants() {
        assert_eq!(variant(&song("flac", 900), Quality::Original), "raw");
        assert_eq!(variant(&song("opus", 128), Quality::Original), "mp3-320", "opus is transcoded");
        assert_eq!(variant(&song("flac", 900), Quality::Mp3_192), "mp3-192");
        assert_eq!(variant(&song("mp3", 128), Quality::Mp3_192), "raw", "already small enough");
        assert_eq!(variant(&song("m4a", 256), Quality::Mp3_320), "raw", "an AAC under the target is not re-encoded");
        assert_eq!(variant(&song("flac", 300), Quality::Mp3_320), "raw", "a low-rate FLAC is smaller than the transcode");
        assert_eq!(variant(&song("m4a", 900), Quality::Mp3_320), "mp3-320", "ALAC-rate m4a is transcoded");
        assert_eq!(variant(&song("opus", 128), Quality::Mp3_320), "mp3-320", "opus is not decodable here");
        assert_eq!(variant(&Song { bit_rate: None, ..song("mp3", 0) }, Quality::Mp3_320), "mp3-320", "unknown bitrate");
        assert_eq!(acceptable(&song("flac", 900), Quality::Mp3_192), vec!["raw", "mp3-320", "mp3-192"]);
        assert_eq!(acceptable(&song("flac", 900), Quality::Original), vec!["raw"]);
    }

    #[test]
    fn options_for_variants() {
        assert_eq!(stream_options("raw").format.as_deref(), Some("raw"));
        let o = stream_options("mp3-192");
        assert_eq!((o.format.as_deref(), o.max_bit_rate), (Some("mp3"), Some(192)));
        assert_eq!(safe("a/b:c"), "a_b_c");
    }

    #[test]
    fn speculative_audio_requires_bounded_duration_and_size() {
        let mut s = Song { duration: 240, size: Some(8 * 1024 * 1024), ..song("mp3", 256) };
        assert!(can_prefetch_audio(&s, "raw"));
        s.duration = 3600;
        assert!(!can_prefetch_audio(&s, "raw"));
        assert!(!can_prefetch_audio(&s, "mp3-192"));
        s.duration = PREFETCH_MAX_SECONDS;
        s.size = Some(PREFETCH_MAX_BYTES);
        assert!(can_prefetch_audio(&s, "raw"));
        s.size = Some(PREFETCH_MAX_BYTES + 1);
        assert!(!can_prefetch_audio(&s, "raw"));
        assert!(can_prefetch_audio(&s, "mp3-192"), "use the selected transcode size");
        s.size = None;
        assert!(!can_prefetch_audio(&s, "raw"));
        assert!(can_prefetch_audio(&s, "mp3-192"));
        s.size = Some(0);
        assert!(!can_prefetch_audio(&s, "raw"));
        s.duration = 0;
        assert!(!can_prefetch_audio(&s, "mp3-192"));
        s.duration = 240;
        s.radio_url = Some("https://example.invalid/live".into());
        assert!(!can_prefetch_audio(&s, "mp3-192"));
    }

    fn accept_transfer_fixture(listener: std::net::TcpListener) -> std::net::TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    return socket;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "generated transfer never reached its HTTP fixture");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("generated transfer accept failed: {error}"),
            }
        }
    }

    /// One-connection HTTP fixture: answers with `total` bytes, the first
    /// `fast` at once, the rest after `gate` (or trickled 1 KiB per 20 ms
    /// without one). Returns its URL and whether the client hung up early.
    fn transfer_server(total: usize, fast: usize, gate: Option<std::sync::mpsc::Receiver<()>>) -> (String, std::thread::JoinHandle<bool>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut socket = accept_transfer_fixture(listener);
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
            socket.write_all(head.as_bytes()).unwrap();
            if socket.write_all(&vec![7u8; fast]).is_err() { return true; }
            let mut sent = fast;
            if let Some(gate) = gate { let _ = gate.recv(); }
            while sent < total {
                let n = (total - sent).min(1024);
                if socket.write_all(&vec![7u8; n]).and_then(|_| socket.flush()).is_err() { return true; }
                sent += n;
                std::thread::sleep(Duration::from_millis(20));
            }
            false
          });
        (url, handle)
    }

    fn writable_transfer_paths(tag: &str) -> Paths {
        let root = generated_test_directory(&format!("transfer-{tag}"));
        let paths = Paths::new(root);
        // A target must never silently fall back to temporary metadata while
        // its transfer path still names an unwritable build-host directory.
        paths.create_all(&crate::rt::Blocking::dedicated_thread()).expect("generated transfer directories must be writable");
        let probe = paths.stream_cache().join("fixture-writable");
        std::fs::write(&probe, b"generated").expect("generated transfer cache must accept files");
        std::fs::remove_file(probe).unwrap();
        paths
    }

    fn transfer_library(url: &str, tag: &str) -> Arc<Library> {
        let paths = writable_transfer_paths(tag);
        let mut config = Config::default();
        config.server.url = url.into();
        config.server.user = "fixture".into();
        config.playback.stream_quality = Quality::Original;
        config.storage.prefetch_songs = 0;
        // These fixtures test transfer ownership, not free-space eviction.
        // The device's generated RAM filesystem is below the card's normal
        // 512 MiB reserve; leaving that default evicts every completed song.
        config.storage.reserve_mb = 0;
        let lib = Library::open(paths, &config, Arc::new(|_| {}));
        assert!(lib.paths.database().is_file(), "generated transfer database must be persistent");
        lib.online.store(true, Ordering::Relaxed);
        lib
    }

    fn generated_stereo_wave(seconds: u32) -> Vec<u8> {
        let mut wave = b"RIFF".to_vec();
        let data = seconds * 8_000 * 4;
        wave.extend_from_slice(&(36 + data).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16u32.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&2u16.to_le_bytes());
        wave.extend_from_slice(&8_000u32.to_le_bytes());
        wave.extend_from_slice(&32_000u32.to_le_bytes());
        wave.extend_from_slice(&4u16.to_le_bytes());
        wave.extend_from_slice(&16u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&data.to_le_bytes());
        wave.resize(44 + data as usize, 0);
        wave
    }

    #[test]
    fn generated_stream_startup_does_not_wait_for_online_probe() {
        use crate::audio::{alsa::{AudioHw,Sink,FAKE},player::{self,Cmd}};
        use crate::playtrace::{Kind,Trace};
        struct SilentHw;
        impl AudioHw for SilentHw {
            fn device(&mut self)->String {FAKE.into()}
            fn open_sink<'a>(&mut self,b:&'a crate::rt::Blocking,d:&str,r:u32,c:u32,l:u32,f:crate::audio::pcm_format::PcmFormat)->Result<Sink<'a>,String> {Sink::open_format(b,d,r,c,l,f)}
            fn power_on(&mut self,_:&crate::rt::Blocking) {}
            fn power_off(&mut self,_:&crate::rt::Blocking) {}
            fn mute(&mut self,_:&crate::rt::Blocking,_:bool) {}
            fn set_attenuation(&mut self,_:&crate::rt::Blocking,_:u8)->Result<bool,String> {Ok(false)}
            fn set_filter(&mut self,_:&crate::rt::Blocking,_:u8) {}
        }
        let wav = generated_stereo_wave(4);
        let out=generated_test_directory("stream-startup-cost");std::fs::create_dir_all(&out).unwrap();let mut rows=Vec::new();
        for online in [false,true] {
            let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
            let url=format!("http://{}",listener.local_addr().unwrap());
            let stop=Arc::new(AtomicBool::new(false));let stopped=stop.clone();let bytes=Arc::new(wav.clone());
            let (entered,seen)=mpsc::channel();let (release,held)=mpsc::channel();let held=Arc::new(Mutex::new(held));
            let server=std::thread::spawn(move|| {
                let until=Instant::now()+Duration::from_secs(8);let mut workers=Vec::new();
                while !stopped.load(Ordering::Relaxed) && Instant::now()<until {
                    match listener.accept() {
                        Ok((mut socket,_))=> {
                            let (entered,held,bytes)=(entered.clone(),held.clone(),bytes.clone());
                            workers.push(std::thread::spawn(move|| {
                                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                                let mut head=Vec::new();while !head.ends_with(b"\r\n\r\n") && head.len()<8192 {
                                    let mut b=[0];if socket.read_exact(&mut b).is_err(){return;}head.push(b[0]);
                                }
                                let first=String::from_utf8_lossy(&head);let ping=first.starts_with("GET /rest/ping");let stream=first.starts_with("GET /rest/stream");
                                if ping {entered.send(1u8).unwrap();let _=held.lock().unwrap().recv_timeout(Duration::from_secs(3));}
                                if stream {entered.send(2u8).unwrap();}
                                let json=b"{\"subsonic-response\":{\"status\":\"ok\",\"version\":\"1.16.1\"}}";
                                let body:&[u8]=if stream {&bytes}else{json};
                                let _=write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",body.len(),if stream {"audio/wav"}else{"application/json"});let _=socket.write_all(body);
                            }));
                            assert!(workers.len()<=16,"bounded generated request count");
                        },
                        Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>std::thread::sleep(Duration::from_millis(1)),
                        Err(e)=>panic!("generated accept: {e}"),
                    }
                }
                for worker in workers {worker.join().unwrap();}
            });
            let paths=writable_transfer_paths(if online {"startup-online"}else{"startup-probe"});
            let mut config=Config::default();config.server.url=url;config.server.user="fixture".into();
            config.playback.stream_quality=Quality::Original;config.storage.prefetch_songs=0;config.storage.reserve_mb=0;
            let (lib,_)=Library::create(paths,&config,Arc::new(|_|{}));lib.online.store(online,Ordering::Relaxed);
            let trace=Arc::new(Trace::new(true));*lib.trace.lock().unwrap()=trace.clone();
            let settings=player::Settings {eq:Default::default(),volume:100,headphone_power: crate::config::HeadphonePower::Standard,filter:0,replay_gain:crate::config::ReplayGainMode::Off,gapless:true};
            let player=player::spawn_traced(Arc::new(Tracks(lib.clone())),Box::new(SilentHw),settings,Arc::new(||{}),None,trace.clone());
            player.send(Cmd::Play {songs:std::sync::Arc::new(vec![Song {id:"generated-startup".into(),suffix:Some("wav".into()),duration:4,size:Some(wav.len() as u64),..Default::default()}]),index:0,shuffle:false});
            let first=seen.recv_timeout(Duration::from_secs(3));
            // The ping endpoint remains held. A stale online bit must not
            // route the player through it before requesting stream bytes.
            let stream_first=matches!(first,Ok(2));
            if !stream_first {let _=release.send(());} // Clean up a regressed implementation.
            let completed=until(||trace.snapshot().events.iter().any(|e|e.kind==Kind::FirstSubmit));
            player.shutdown();stop.store(true,Ordering::Relaxed);drop(release);server.join().unwrap();
            let snapshot=trace.snapshot();
            std::fs::write(out.join(if online {"online.trace"}else{"probe.trace"}),snapshot.text()).unwrap();
            assert!(stream_first,"song selection must request the stream directly even when online=false");
            assert!(seen.try_iter().all(|request|request!=1),"playback must not issue a preflight ping");
            assert!(completed,"{:?}",player.status().error);
            assert!(lib.is_online(),"successful stream response establishes current-account reachability");
            let event=|kind|snapshot.events.iter().find(|e|e.kind==kind).unwrap();
            let request=event(Kind::Request);let opened=event(Kind::Open);let pcm=event(Kind::FirstPcm);let submit=event(Kind::FirstSubmit);
            assert!(request.t_us<=opened.t_us && opened.t_us<=pcm.t_us && pcm.t_us<=submit.t_us);
            rows.push(serde_json::json!({"online_initial":online,"ping_requested":false,"source_ms":opened.c>>16,"probe_ms":opened.c&65535,"request_to_open_us":opened.t_us-request.t_us,"open_to_first_pcm_us":pcm.t_us-opened.t_us,"first_pcm_to_submit_us":submit.t_us-pcm.t_us,"request_to_submit_us":submit.t_us-request.t_us,"events_lost":snapshot.events_lost,"windows_lost":snapshot.windows_lost}));
        }
        let report=serde_json::json!({"passed":true,"scope":"generated native direct-stream regression; no hardware/network/power latency claim","arms":rows});
        std::fs::write(out.join("result.json"),serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        println!("Q2_STREAM_STARTUP {}",report);
    }

    fn partials(lib: &Library) -> Vec<PathBuf> {
        std::fs::read_dir(lib.paths.stream_partials()).map(|d| d.flatten().map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "part")).collect()).unwrap_or_default()
    }

    #[test]
    fn cached_skip_plays_during_reconnect_and_refill_drains_background_after_confirmation() {
        use crate::audio::{alsa::{FAKE, SoftHw}, player::{self, Cmd, PlayState}};
        use crate::playtrace::Kind;
        use std::io::{BufRead, BufReader};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "generated".into();
        config.server.password = "synthetic-only".into();
        config.playback.stream_quality = Quality::Original;
        config.playback.sync_queue = true;
        config.storage.reserve_mb = 0;
        config.storage.fetch_minutes = 0;
        let paths = writable_transfer_paths("cached-skip-reconnect");

        let mut wave = generated_stereo_wave(16);
        for (number, frame) in wave[44..].chunks_exact_mut(4).enumerate() {
            let sample = if number % 20 < 10 { 800i16 } else { -800i16 };
            frame[..2].copy_from_slice(&sample.to_le_bytes());
            frame[2..].copy_from_slice(&(-sample).to_le_bytes());
        }
        let queue = Arc::new(["generated-start", "generated-local", "generated-refill"].into_iter().map(|id| Song {
            id: id.into(), suffix: Some("wav".into()), duration: 16, size: Some(wave.len() as u64), ..Song::default()
        }).collect::<Vec<_>>());
        let (lib, prefetch) = Library::create(paths.clone(), &config, Arc::new(|_| {}));
        let blocking = crate::rt::Blocking::dedicated_thread();
        let local = shard_ready(lib.cache_path(&queue[1], "raw"));
        std::fs::write(&local, &wave).unwrap();
        lib.db().put_cached_song(&blocking, &queue[1].id, "raw", &local, wave.len() as u64).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + TEST_LIVENESS;
            while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                let socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                if socket.read_line(&mut line).unwrap_or(0) == 0 { continue; }
                let parsed = [
                    ("stream", "id=generated-start", "foreground-stream"),
                    ("stream", "id=generated-refill", "fetch-ahead"),
                    ("scrobble", "submission=false", "now-playing"),
                    ("scrobble", "submission=true", "completed-listen"),
                    ("savePlayQueue", "current=generated-local", "queue-snapshot"),
                ].into_iter().find(|(route, query, _)| line.starts_with(&format!("GET /rest/{route}?")) && line.contains(query));
                let Some((route, _, from)) = parsed else { panic!("unexpected generated request: {line:?}"); };
                loop {
                    line.clear();
                    if socket.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" { break; }
                }
                seen.lock().unwrap().push(from);
                let response = br#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;
                let (body, content): (&[u8], &str) = if route == "stream" { (&wave, "audio/wav") } else { (response, "application/json") };
                let socket = socket.get_mut();
                if write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {content}\r\nConnection: close\r\n\r\n", body.len()).is_ok() {
                    let _ = socket.write_all(body);
                }
            }
        });

        let trace = lib.traced_for_test();
        let player = player::spawn(Arc::new(Tracks(lib.clone())), Box::new(SoftHw(FAKE.into())), player::Settings {
            eq: Default::default(), volume: 20, headphone_power: crate::config::HeadphonePower::Standard,
            filter: 0, replay_gain: crate::config::ReplayGainMode::Off, gapless: true,
        }, Arc::new(|| {}));
        player.send(Cmd::Play { songs: queue.clone(), index: 0, shuffle: false });
        assert!(until(|| player.status().state == PlayState::Playing && player.status().output_owned));
        assert!(until(|| requests.lock().unwrap().len() >= 2));
        assert_eq!(*requests.lock().unwrap(), ["foreground-stream", "now-playing"]);

        lib.set_radio_doze(true);
        lib.set_radio_doze(false);
        let skipped = Instant::now();
        player.send(Cmd::Next);
        assert!(until(|| {
            let status = player.status();
            status.index == 1 && status.state == PlayState::Playing && status.output_owned && status.position_ms > 0
        }));
        let local_skip_progress_ms = skipped.elapsed().as_millis();

        // A durable listen recorded earlier is eligible, but neither the
        // sync round nor a real prefetch plan may use a stale Online status.
        lib.db().push_pending(&blocking, &PendingCall::Scrobble { id: queue[0].id.clone(), time_ms: 123 });
        let owner = lib.clone();
        shelf_browse::wait(async move { assert_eq!(owner.sync_round(true).await, SYNC_IDLE); Ok(()) }).unwrap();
        Library::start_prefetch(&lib, prefetch);
        assert!(until(|| trace.snapshot().events.iter().any(|e| e.kind == Kind::FetchAheadEnd && e.a == 3)));
        assert_eq!(*requests.lock().unwrap(), ["foreground-stream", "now-playing"]);
        assert!(lib.find_local(&queue[2]).is_none());
        assert_eq!(lib.db().pending(&blocking, 1).len(), 1);

        Library::start_sync(&lib);
        lib.link_available();
        // The UI reissues the latest upcoming queue on a confirmed address.
        lib.prefetch_upcoming(&queue[2..]);
        assert!(until(|| lib.find_local(&queue[2]).is_some() && requests.lock().unwrap().len() >= 5));
        assert!(until(|| lib.db().pending(&blocking, 1).is_empty()));
        let mut refill_requests = requests.lock().unwrap()[2..].to_vec();
        refill_requests.sort_unstable();
        assert_eq!(refill_requests, ["completed-listen", "fetch-ahead", "queue-snapshot"]);
        assert_eq!(player.status().index, 1);
        assert!(player.status().error.is_none());
        lib.set_radio_doze(true);
        assert!(!lib.is_online());
        player.shutdown();
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let report = serde_json::json!({
            "passed": true, "scope": "native loopback; fixture link events and software sink",
            "skip_to_first_observed_progress_ms": local_skip_progress_ms,
            "initial": ["foreground-stream", "now-playing"], "while_awaiting_address": [],
            "after_link": refill_requests,
        });
        let evidence = paths.root.join("reconnect-skip-result.json");
        std::fs::OpenOptions::new().write(true).create_new(true).open(evidence).unwrap()
            .write_all(&serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        println!("Q2_REFILL_RECONNECT_SKIP {report}");
    }

    fn until(what: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        while !what() { if Instant::now() > deadline { return false; } std::thread::sleep(Duration::from_millis(10)); }
        true
    }

    /// A sharded file's directory, created as the cache publisher would.
    fn shard_ready(path: PathBuf) -> PathBuf { std::fs::create_dir_all(path.parent().unwrap()).unwrap(); path }

    /// A song already in the stream cache, so opening it needs no network.
    fn cached_song(lib: &Library, id: &str) -> Song {
        let song = Song { id: id.into(), duration: 240, size: Some(16), ..song("mp3", 256) };
        let path = shard_ready(lib.cache_path(&song, "raw"));
        std::fs::write(&path, b"cached fixture..").unwrap();
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, "raw", &path, 16).unwrap();
        assert!(lib.find_local(&song).is_some());
        song
    }

    fn transfer(lib: &Library, id: &str) -> (bool, bool, usize) {
        let inflight = lib.inflight.lock().unwrap();
        let t = inflight.get(&(id.to_string(), "raw".to_string())).expect("transfer in flight");
        (t.orphaned, t.cancel.is_cancelled(), t.users)
    }

    #[test]
    fn card_create_releases_registry_and_rejects_account_change_without_partial_leak() {
        let lib = transfer_library("http://127.0.0.1:9", "unlocked-card-create");
        let song = Song { id: "generated-held-create".into(), ..song("mp3", 256) };
        let generation = lib.client_generation();
        let permit = lib.offline_art_gate.lock();
        let (created, entered) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let owner = lib.clone();
        let worker = std::thread::spawn(move || {
            owner.start_fetch_prepared_using(&crate::rt::Blocking::dedicated_thread(), &song, "raw", true, 0, 0, None, generation, permit, |path| {
                let file = std::fs::File::create_new(path)?;
                created.send(path.to_owned()).unwrap();
                resumed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                Ok(file)
            })
        });
        let path = entered.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let client_unlocked = lib.client.try_write().is_ok();
        // A real I/O-runtime registry reader must finish while card creation
        // remains parked. The independent UI timer makes the old lock scope
        // fail without leaving the global I/O executor permanently blocked.
        let probe = lib.clone();
        let inspected = crate::rt::io().spawn(async move { probe.inflight.lock().unwrap().is_empty() });
        let ui = crate::rt::Ui::new().unwrap();
        let responsive = ui.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), inspected).await
        });
        // Real configuration admission invalidates immediately, but its owned
        // DB/file transition must wait for this preparation's publication gate.
        let changed = lib.configure_async(&Config::default());
        release.send(()).unwrap();
        let prepared = worker.join().unwrap();
        ui.block_on(async {
            tokio::time::timeout(crate::library::TEST_LIVENESS, changed).await.unwrap().unwrap();
        });
        assert!(client_unlocked, "card creation cannot retain the client guard");
        assert!(responsive.is_ok_and(|result| matches!(result, Ok(true))), "card creation blocked the I/O executor's registry read");
        assert!(matches!(prepared, Err(LibError::Offline)), "obsolete preparation cannot install a transfer");
        assert!(!path.exists(), "unused owned partial must be removed");
        assert!(lib.inflight.lock().unwrap().is_empty());
    }

    #[test]
    fn account_switch_cannot_join_a_stalled_same_id_transfer_or_reuse_its_snapshot() {
        let (release_a, gate_a) = mpsc::channel();
        let (url_a, server_a) = transfer_server(8192, 4096, Some(gate_a));
        let (release_b, gate_b) = mpsc::channel();
        let (url_b, server_b) = transfer_server(8192, 4096, Some(gate_b));
        let lib = transfer_library(&url_a, "account-bound-join");
        let song = Song { id: "generated-colliding-id".into(), duration: 240, size: Some(8192), ..song("mp3", 256) };
        let (old_path, old_progress, old_hold) = lib.start_fetch(&song, "raw", true).unwrap();
        assert!(until(|| old_progress.snapshot().written > 0));
        let old_generation = lib.client_generation();
        let mut config = Config::default();
        config.server.url = url_b; config.server.user = "fixture".into();
        config.playback.stream_quality = Quality::Original;
        config.storage.prefetch_songs = 0; config.storage.reserve_mb = 0;
        lib.configure(&config);
        assert_ne!(old_generation, lib.client_generation());
        let (new_path, new_progress, new_hold) = lib.start_fetch(&song, "raw", true).unwrap();
        assert_ne!(old_path, new_path, "another account cannot reuse the old partial");
        assert!(!Arc::ptr_eq(&old_progress, &new_progress));
        assert!(until(|| new_progress.snapshot().written > 0));
        assert!(old_progress.wait_finished_for(Duration::from_secs(3)), "stale transport stops without its server release");
        assert!(old_progress.snapshot().error.is_some());
        drop(old_hold);
        assert_eq!(transfer(&lib, &song.id).2, 1, "an old hold cannot release the replacement slot");
        // A delayed preparation cannot hand back an old cache snapshot either,
        // even when a current transfer with the same key is now registered.
        // Retiring the old transfer may still own publication cleanup. The
        // fixture needs the real permit, not an assumption that it is idle.
        let gate = crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(crate::library::TEST_LIVENESS, lib.offline_art_gate.acquire()).await.unwrap()
        });
        assert!(matches!(lib.start_fetch_prepared(&crate::rt::Blocking::dedicated_thread(), &song, "raw", true, 0, 0,
            Some((old_path.clone(), 4096)), old_generation, gate), Err(LibError::Offline)));
        release_a.send(()).unwrap(); release_b.send(()).unwrap();
        assert!(new_progress.wait_finished_for(Duration::from_secs(5)));
        assert!(new_progress.snapshot().error.is_none());
        assert_eq!(new_progress.snapshot().publication, Some(CachePublication::Published));
        server_a.join().unwrap(); assert!(!server_b.join().unwrap());
        drop(new_hold);
        assert!(until(|| lib.inflight.lock().unwrap().is_empty()));
        assert!(!old_path.exists());
        assert_eq!(std::fs::read(lib.cache_path(&song, "raw")).unwrap(), vec![7; 8192]);
    }

    #[test]
    fn async_cache_lookup_yields_to_workers_and_keeps_download_priority_and_stale_cleanup() {
        use std::{future::Future, task::Poll};
        let lib = transfer_library("http://127.0.0.1:9", "owned-cache-lookup");
        let downloaded = cached_song(&lib, "generated-download");
        let cached = cached_song(&lib, "generated-cache");
        let missing = Song { id: "generated-missing".into(), ..cached.clone() };
        let gone = lib.cache_path(&missing, "raw");
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &missing.id, "raw", &gone, 16).unwrap();
        let offline = lib.paths.offline().join("generated.mp3");
        std::fs::write(&offline, b"generated download").unwrap();
        lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &downloaded, &offline, 18);
        let (entered, begun) = mpsc::channel(); let (release, held) = mpsc::channel();
        lib.db_post(move |_, _| { entered.send(()).unwrap(); held.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let files = crate::rt::Ui::new().unwrap().block_on(async {
            let lookup = lib.local_files_async(Arc::new(vec![downloaded.clone(), cached.clone(), missing.clone()]), Quality::Original);
            tokio::pin!(lookup);
            std::future::poll_fn(|cx| { assert!(lookup.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            assert_eq!(tokio::time::timeout(crate::library::TEST_LIVENESS, crate::rt::spawn_blocking(|_| 37)).await.unwrap().unwrap(), 37);
            assert!(lib.inflight.try_lock().is_ok(), "cache snapshots never retain the transfer registry");
            release.send(()).unwrap();
            tokio::time::timeout(crate::library::TEST_LIVENESS, lookup).await.unwrap().unwrap()
        });
        assert_eq!(files[0].as_ref().unwrap().0, offline);
        assert!(matches!(files[0].as_ref().unwrap().1, Origin::Offline));
        assert!(matches!(files[1].as_ref().unwrap().1, Origin::Cached));
        assert!(files[2].is_none());
        assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), &missing.id, &["raw"]).is_none());
        assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), &downloaded.id, &["raw"]).is_some(), "download priority does not discard a cache copy");
    }

    #[test]
    fn simultaneous_async_openers_join_one_transfer_and_live_join_does_not_wait_for_sql() {
        let (release, gate) = mpsc::channel();
        let (url, server) = transfer_server(8192, 4096, Some(gate));
        let lib = transfer_library(&url, "async-openers");
        let trace = traced(&lib);
        let song = Song { id: "generated-shared-open".into(), duration: 240, size: Some(8192), ..song("mp3", 256) };
        let (first, second) = crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(lib.fetch_in_burst_async(song.clone(), "raw".into(), true, 0),
                    lib.fetch_in_burst_async(song.clone(), "raw".into(), false, 0))
            }).await.unwrap()
        });
        let (path, progress, first) = first.unwrap();
        let (other, joined, second) = second.unwrap();
        assert_eq!(path, other); assert!(Arc::ptr_eq(&progress, &joined));
        let (entered, begun) = mpsc::channel(); let (free, held) = mpsc::channel();
        lib.db_post(move |_, _| { entered.send(()).unwrap(); held.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let (_, joined, third) = crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(crate::library::TEST_LIVENESS, lib.fetch_in_burst_async(song.clone(), "raw".into(), true, 0)).await.unwrap().unwrap()
        });
        assert!(Arc::ptr_eq(&progress, &joined));
        free.send(()).unwrap(); release.send(()).unwrap();
        assert!(progress.wait_finished_for(Duration::from_secs(5)));
        assert!(!server.join().unwrap());
        assert!(until(|| lib.inflight.lock().unwrap().is_empty()));
        assert_eq!(trace.snapshot().events.iter().filter(|event| event.kind == crate::playtrace::Kind::XferStart).count(), 1);
        let (_, hit, hold) = lib.start_fetch(&song, "raw", false).unwrap();
        assert!(hold.is_none()); assert_eq!(hit.snapshot().publication, Some(CachePublication::Hit));
        drop((first, second, third));
    }

    #[test]
    fn a_skip_cancels_the_obsolete_stream_and_removes_its_partial_file() {
        crate::audio::callback::test_scope(|| {
        let (url, server) = transfer_server(4 << 20, 64 << 10, None);
        let lib = transfer_library(&url, "skip");
        let tracks = Tracks(lib.clone());
        let next = cached_song(&lib, "next");
        let song = Song { id: "skipped".into(), duration: 240, size: Some(4 << 20), ..song("mp3", 256) };
        let mut opened = tracks.open(&crate::rt::Blocking::dedicated_thread(), &song, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        let mut head = [0u8; 4096];
        opened.source.read_exact(&mut head).unwrap();
        assert!(lib.playback_transfer_running(), "a player stream downloading holds fetch-ahead back");
        assert_eq!(partials(&lib).len(), 1);
        drop(opened);
        assert_eq!(transfer(&lib, "skipped"), (true, false, 0), "orphaned, still downloading");
        let _playing = tracks.open(&crate::rt::Blocking::dedicated_thread(), &next, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        assert!(until(|| lib.inflight.lock().unwrap().is_empty()), "opening another song ends it");
        assert!(until(|| partials(&lib).is_empty()), "its partial file is removed");
        assert!(lib.find_local(&song).is_none());
        assert!(server.join().unwrap(), "the server saw the download stop early");
        assert!(!lib.playback_transfer_running());
            })
    }

    fn traced(lib: &Library) -> Arc<crate::playtrace::Trace> {
        let trace = Arc::new(crate::playtrace::Trace::new(true));
        *lib.trace.lock().unwrap() = trace.clone();
        trace
    }

    fn xfer_events(trace: &crate::playtrace::Trace) -> Vec<(crate::playtrace::Kind, u32, u32)> {
        use crate::playtrace::Kind::*;
        trace.snapshot().events.iter().filter(|e| matches!(e.kind, XferStart | XferJoin | XferOrphan | XferCancel | XferEnd))
            .map(|e| (e.kind, e.a, e.b)).collect()
    }

    #[test]
    fn transfer_sizes_from_the_server_are_validated_without_overflow() {
        use crate::audio::source::Fill;
        let fill = |written, total| Fill { written, total, ..Fill::default() };
        assert!(retain_obsolete(&fill(80, Some(100))) && !retain_obsolete(&fill(79, Some(100))));
        assert!(!retain_obsolete(&fill(u64::MAX / 2, Some(u64::MAX))), "half of an absurd length is not 80 %");
        assert!(retain_obsolete(&fill(u64::MAX, Some(u64::MAX))));
        assert!(!retain_obsolete(&fill(5, Some(0))) && !retain_obsolete(&fill(5, None)));
        assert_eq!(fill_percent(&fill(u64::MAX, Some(u64::MAX))), 100);
        assert_eq!(fill_percent(&fill(1, Some(0))), 255);
        assert!(transfer_complete("raw", 100, Some(100)) && transfer_complete("raw", 100, None) && transfer_complete("raw", 101, Some(100)));
        assert!(!transfer_complete("raw", 99, Some(100)), "a short original is a dropped connection");
        assert!(transfer_complete("mp3-320", 99, Some(100)), "a transcode's length is an estimate");
        let mut config = Config::default();
        config.storage.stream_cache_mb = u64::MAX;
        config.storage.reserve_mb = u64::MAX / 2;
        let settings = Settings::from(&config);
        assert_eq!((settings.cache_bytes, settings.reserve_bytes), (u64::MAX, u64::MAX));
    }

    /// Many-connection HTTP fixture: every request is answered with `total`
    /// bytes, 2 KiB at once and then 2 KiB every 5 ms, until the client
    /// hangs up. The accept thread lives until the test process ends.
    fn trickle_server(total: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { break };
                std::thread::spawn(move || {
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if socket.read_exact(&mut byte).is_err() { return; }
                        request.push(byte[0]);
                    }
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n");
                    if socket.write_all(head.as_bytes()).is_err() { return; }
                    let mut sent = 0;
                    while sent < total {
                        let n = (total - sent).min(2048);
                        if socket.write_all(&vec![7u8; n]).and_then(|_| socket.flush()).is_err() { return; }
                        sent += n;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                  });
            }
          });
        url
    }

    /// Generated opens, reader drops and orphan sweeps over the real library,
    /// its stream threads and a local server: every slot stays valid
    /// (`q2_machines::transfer`), a slot never counts more holds than there
    /// are readers, and once every reader is gone no transfer or partial
    /// file is left behind.
    #[test]
    fn generated_opens_drops_and_sweeps_keep_slots_valid_and_leak_nothing() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config as PropConfig, RngAlgorithm, TestRng, TestRunner};
        let started = Instant::now();
        let url = trickle_server(96 << 10);
        let config = PropConfig { cases: 8, failure_persistence: None, max_shrink_iters: 30, ..PropConfig::default() };
        let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(RngAlgorithm::ChaCha));
        let result = runner.run(&prop::collection::vec((0u8..5, 0usize..3), 1..16), |ops| {
            let lib = transfer_library(&url, "prop");
            let tracks = Tracks(lib.clone());
            let flag = Arc::new(crate::audio::interrupt::Interrupt::new(false));
            let mut readers: Vec<(usize, OpenedTrack)> = Vec::new();
            for (kind, n) in ops {
                match kind {
                    // Open one of three songs (a skip when another is open).
                    0 | 1 => {
                        let song = Song { id: format!("p{n}"), duration: 240, size: Some(96 << 10), ..song("mp3", 256) };
                        if let Ok(opened) = tracks.open(&crate::rt::Blocking::dedicated_thread(), &song, &flag) { readers.push((n, opened)); }
                    }
                    // A reader goes away.
                    2 => if !readers.is_empty() { readers.remove(n % readers.len()); },
                    // Fetch-ahead moves on, still wanting one song.
                    3 => { lib.cancel_orphans(|k| k.0 == format!("p{n}")); }
                    _ => std::thread::sleep(Duration::from_millis(4 * n as u64)),
                }
                for (key, transfer) in lib.inflight.lock().unwrap().iter() {
                    let slot = transfer.slot();
                    prop_assert!(slot.valid(), "{:?} {:?}", key, slot);
                    let held = readers.iter().filter(|(n, _)| format!("p{n}") == key.0).count();
                    prop_assert!(slot.users <= held, "{} holds on {:?} with {} readers", slot.users, key, held);
                }
            }
            drop(readers);
            lib.cancel_orphans(|_| false);
            prop_assert!(until(|| lib.inflight.lock().unwrap().is_empty()), "a transfer outlived its readers and its sweep");
            prop_assert!(until(|| partials(&lib).is_empty()), "a partial file was left behind");
            Ok(())
        });
        if let Err(error) = result { panic!("{error}"); }
        let base = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"));
        let dir = base.join("property-test-times");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("library-transfers"), format!("{:.2}\n", started.elapsed().as_secs_f64()));
    }

    /// Planner feasibility, not a simulated network or a measured cycle. The
    /// eight generated 150 s tracks need not cover an arm plus a whole extra
    /// fill: the first fill and closing refill can overlap already-local songs.
    #[test]
    fn generated_eight_track_queue_has_a_default_policy_closing_refill() {
        use q2_machines::doze::{self, Effect, Inputs, Thresholds, WakeReason};
        let lib = transfer_library("http://127.0.0.1:9", "refill-plan");
        let config = Config::default();
        assert_eq!((config.storage.fetch_minutes, config.storage.fetch_low_minutes), (10, 3));
        let fill = u64::from(config.storage.fetch_minutes) * 60_000;
        // The planner's low mark, as production derives it: the fetch margin
        // plus the doze reconnect allowance, equal to the doze wake threshold.
        let low = Settings::from(&config).fetch_low_ms;
        let thresholds = Thresholds::after_fetch(config.storage.fetch_low_minutes);
        assert_eq!(low, thresholds.wake_ms);
        let queue: Vec<_> = (0..8).map(|n| Song { id: format!("generated-refill-{n}"),
            duration: 150, size: Some(16), ..song("flac", 256) }).collect();
        let publish = |index: usize| {
            let item = &queue[index];
            let path = shard_ready(lib.cache_path(item, "raw"));
            std::fs::write(&path, b"cached fixture..").unwrap();
            lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &item.id, "raw", &path, 16).unwrap();
        };
        let targets = |index: usize| {
            let upcoming = &queue[index + 1..];
            fetch_targets_minutes(&upcoming.iter().map(|s| lib.find_local(s).is_some()).collect::<Vec<_>>(),
                &upcoming.iter().map(|s| can_prefetch_audio(s, "raw")).collect::<Vec<_>>(),
                &upcoming.iter().map(|s| u64::from(s.duration) * 1000).collect::<Vec<_>>(),
                config.storage.prefetch_songs as usize, fill, low)
                .into_iter().map(|n| index + 1 + n).collect::<Vec<_>>()
        };
        let observe = |seconds: u64, dozing: bool, fetching: bool| {
            let index = (seconds / 150) as usize;
            let ahead = lib.local_ahead_ms(&queue, index, seconds % 150 * 1000, 60);
            (ahead, doze::transition(Inputs { enabled: true, wifi_wanted: true, connected: true,
                dozing, playing: true, idle_ms: seconds * 1000, ahead_ms: ahead, fetching,
                ..Default::default() }, thresholds).1)
        };
        publish(0); // A complete foreground track is an explicit precondition.
        assert_eq!(targets(0), vec![1, 2, 3, 4]);
        for index in targets(0) { publish(index); }
        assert_eq!(observe(30, true, false).0, 720_000);
        assert_eq!(observe(30, false, true).1, Effect::None, "an active fill prevents doze");
        assert_eq!(observe(30, false, false).1, Effect::Doze);
        assert_eq!(observe(510, true, false), (240_000, Effect::None), "wake is strictly below the margin");
        assert_eq!(observe(520, true, false), (230_000, Effect::Wake(WakeReason::Low)));
        assert_eq!(targets(3), vec![5, 6, 7], "cached upcoming track 4 contributes 150 s to the next fill");
        assert_eq!(observe(600, false, false).1, Effect::None, "a wake alone is not a closing refill");
        // Complete the planned refill in this constructed state. Its actual
        // network/reconnect/settle timing remains a physical-run prerequisite.
        for index in targets(3) { publish(index); }
        assert_eq!(observe(660, false, false), (540_000, Effect::Doze));
        assert_eq!(observe(720, false, false), (480_000, Effect::Doze));
        assert_eq!(observe(730, false, false).1, Effect::None, "a late refill can no longer close at the doze threshold");
        // A missing publication must break the consecutive local horizon.
        std::fs::remove_file(lib.cache_path(&queue[6], "raw")).unwrap();
        assert_eq!(observe(660, false, false), (240_000, Effect::None));
    }

    #[test]
    fn generated_queue_depth_can_close_a_second_default_cycle_with_matched_ahead() {
        use q2_machines::doze::{self, Effect, Inputs, Thresholds, WakeReason};
        // Constructed publication times, not a network simulation. The first
        // cycle warms the cache; the second must replace what playback used.
        for count in [8, 11] {
            let lib = transfer_library("http://127.0.0.1:9", &format!("refill-balance-{count}"));
            let config = Config::default();
            assert_eq!((config.storage.fetch_minutes, config.storage.fetch_low_minutes), (10, 3));
            let queue: Vec<_> = (0..count).map(|n| Song { id: format!("generated-balance-{n}"),
                duration: 150, size: Some(16), ..song("flac", 256) }).collect();
            let publish = |index: usize| {
                let item = &queue[index];
                let path = shard_ready(lib.cache_path(item, "raw"));
                std::fs::write(&path, b"cached fixture..").unwrap();
                lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &item.id, "raw", &path, 16).unwrap();
            };
            let targets = |index: usize| {
                let upcoming = &queue[index + 1..];
                fetch_targets_minutes(&upcoming.iter().map(|s| lib.find_local(s).is_some()).collect::<Vec<_>>(),
                    &upcoming.iter().map(|s| can_prefetch_audio(s, "raw")).collect::<Vec<_>>(),
                    &upcoming.iter().map(|s| u64::from(s.duration) * 1000).collect::<Vec<_>>(),
                    config.storage.prefetch_songs as usize,
                    u64::from(config.storage.fetch_minutes) * 60_000,
                    Settings::from(&config).fetch_low_ms)
                    .into_iter().map(|n| index + 1 + n).collect::<Vec<_>>()
            };
            let observe = |seconds: u64, dozing: bool, fetching: bool| {
                let ahead = lib.local_ahead_ms(&queue, (seconds / 150) as usize, seconds % 150 * 1000, 60);
                (ahead, doze::transition(Inputs { enabled: true, wifi_wanted: true, connected: true,
                    dozing, playing: true, idle_ms: seconds * 1000, ahead_ms: ahead, fetching,
                    ..Default::default() }, Thresholds::after_fetch(config.storage.fetch_low_minutes)).1)
            };
            publish(0);
            for index in targets(0) { publish(index); }
            let initial = observe(30, false, false);
            assert_eq!(initial, (720_000, Effect::Doze));
            assert_eq!(observe(520, true, false), (230_000, Effect::Wake(WakeReason::Low)));
            assert_eq!(targets(3), vec![5, 6, 7]);
            for index in targets(3) { publish(index); }
            let first_close = observe(660, false, false);
            assert_eq!(first_close, (540_000, Effect::Doze));
            assert_ne!(first_close.0, initial.0,
                "functional first closure is not equal-buffer energy admission");
            assert_eq!(observe(960, true, false), (240_000, Effect::None));
            assert_eq!(observe(970, true, false), (230_000, Effect::Wake(WakeReason::Low)));
            let second = targets(6);
            if count == 8 {
                assert!(second.is_empty());
                assert_eq!(observe(1110, false, false), (90_000, Effect::None),
                    "eight tracks cannot supply this constructed second refill");
            } else {
                assert_eq!(second, vec![8, 9, 10]);
                assert_eq!(second.iter().map(|&n| u64::from(queue[n].duration)).sum::<u64>(), 1110 - 660);
                for index in second { publish(index); }
                assert_eq!(observe(1110, false, true).1, Effect::None);
                assert_eq!(observe(1110, false, false), first_close,
                    "equal ahead at both constructed boundaries, with450s new content");
                std::fs::remove_file(lib.cache_path(&queue[9], "raw")).unwrap();
                assert_eq!(observe(1110, false, false), (240_000, Effect::None),
                    "a missing middle publication must refuse closure");
            }
        }
    }

    #[test]
    fn persistent_refill_records_actual_bursts_and_offline_attempts_without_tracing() {
        let (url, server) = transfer_server(8192, 8192, None);
        let lib = transfer_library(&url, "refill-bursts");
        *lib.trace.lock().unwrap() = Arc::new(crate::playtrace::Trace::new(false));
        {
            let mut settings = lib.settings.write().unwrap();
            settings.prefetch = 1; settings.batch = 1; settings.fetch_ms = 0;
        }
        let song = Song { id: "private-generated-id".into(), title: "Private Generated Title".into(), duration: 150, size: Some(8192), ..song("mp3", 256) };
        let path = lib.paths.database().with_file_name("refill-events.log");
        let ends = || {
            assert!(lib.refill.output.wait_idle());
            std::fs::read_to_string(&path).unwrap_or_default().lines()
                .filter(|line| line.split_whitespace().nth(5) == Some("5")).count()
        };
        lib.prefetch_upcoming(std::slice::from_ref(&song));
        assert!(until(|| ends() == 1));
        assert!(!server.join().unwrap());
        assert!(lib.find_local(&song).is_some());
        lib.online.store(false, Ordering::Relaxed);
        let absent = Song { id: "generated-offline".into(), ..song.clone() };
        lib.prefetch_upcoming(&[absent]);
        assert!(until(|| ends() == 2));
        assert!(!lib.trace().enabled());
        let rows = refill_rows(&lib);
        let starts: Vec<_> = rows.iter().filter(|r| r[3] == 4).collect();
        assert_eq!(starts.len(), 2);
        let finished: Vec<_> = rows.iter().filter(|r| r[3] == 3).collect();
        assert_eq!(finished.len(), 1);
        assert_eq!((finished[0][4], finished[0][7], finished[0][8], finished[0][9]), (starts[0][1], 150_000, 8192, 1));
        assert_eq!(rows.last().unwrap()[9], 3, "offline burst is a recorded attempt, not a silent success");
        assert_eq!(rows.iter().filter(|r| r[3] == 1).count(), 1);
        if let Some(root) = std::env::var_os("Q2_GENERATED_TEST_ROOT") {
            let mut output = std::fs::File::create_new(PathBuf::from(root).join("refill-fixture.log")).unwrap();
            output.write_all(&std::fs::read(&path).unwrap()).unwrap();
        }
    }

    fn refill_rows(lib: &Library) -> Vec<Vec<u64>> {
        assert!(lib.refill.output.wait_idle());
        let text = std::fs::read_to_string(lib.paths.database().with_file_name("refill-events.log")).unwrap();
        assert!(!text.contains("private") && !text.contains("Private") && !text.contains("http"));
        let rows: Vec<Vec<u64>> = text.lines().map(|line| {
            let mut fields = line.split_whitespace();
            assert_eq!(fields.next(), Some("refill")); assert_eq!(fields.next(), Some("1"));
            let row: Vec<u64> = fields.map(|f| f.parse().unwrap()).collect();
            assert_eq!(row.len(), 10); row
        }).collect();
        assert!(rows.windows(2).all(|p| p[0][0] == p[1][0] && p[0][1] + 1 == p[1][1] && p[0][2] <= p[1][2]));
        rows
    }

    #[test]
    fn cache_publication_checks_rename_and_index_without_breaking_readers() {
        crate::audio::callback::test_scope(|| {
        for expected in [CachePublication::Published, CachePublication::RenameFailed, CachePublication::IndexFailed] {
            let (release, gate) = mpsc::channel();
            let total = 8192;
            let (url, server) = transfer_server(total, total / 2, Some(gate));
            let lib = transfer_library(&url, "publication");
            let trace = traced(&lib);
            let song = Song { id: "generated-publication".into(), duration: 240, size: Some(total as u64), ..song("mp3", 256) };
            let dest = shard_ready(lib.cache_path(&song, "raw"));
            match expected {
                CachePublication::RenameFailed => std::fs::create_dir(&dest).unwrap(),
                CachePublication::IndexFailed => lib.db().reject_cache_publication(&crate::rt::Blocking::dedicated_thread()),
                _ => {}
            }
            let (part, progress, hold) = lib.start_fetch(&song, "raw", true).unwrap();
            let (_, joined, second) = lib.start_fetch(&song, "raw", false).unwrap();
            assert!(Arc::ptr_eq(&progress, &joined), "joining must share the outcome, not count another publication");
            let interrupt = Arc::new(crate::audio::interrupt::Interrupt::new(false));
            let mut reader = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &part, progress.clone(), interrupt.clone()).unwrap();
            assert!(until(|| progress.snapshot().written > 0));
            assert_eq!(progress.snapshot().publication, None, "partial bytes are not a cache success");
            release.send(()).unwrap();
            assert!(progress.wait_finished_for(Duration::from_secs(5)));
            let fill = progress.snapshot();
            assert_eq!(fill.publication, Some(expected));
            assert_eq!(fill.error, None, "cache-only failure must preserve playback");
            progress.wait_complete(&crate::rt::Blocking::dedicated_thread(), &interrupt).unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, vec![7; total]);
            assert!(!server.join().unwrap());
            assert!(until(|| lib.inflight.lock().unwrap().is_empty()));
            assert_eq!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, &["raw"]).is_some(), expected == CachePublication::Published);
            let snapshot = trace.snapshot();
            let publications: Vec<_> = snapshot.events.iter().filter(|e| e.kind == crate::playtrace::Kind::CachePublish).collect();
            assert_eq!(publications.len(), 1);
            assert_eq!((publications[0].a, publications[0].b, publications[0].c), (1, expected as u32, 8));
            let transport = snapshot.events.iter().find(|e| e.kind == crate::playtrace::Kind::XferEnd).unwrap();
            assert_eq!(transport.b, 1, "transport success alone does not establish caching");
            assert!(transport.seq < publications[0].seq);
            if expected == CachePublication::Published {
                let (cached, hit, unused) = lib.start_fetch(&song, "raw", false).unwrap();
                assert_eq!(cached, dest);
                assert!(unused.is_none());
                assert_eq!(hit.snapshot().publication, Some(CachePublication::Hit));
                assert_eq!(trace.snapshot().events.len(), snapshot.events.len(), "a cache hit is not a new transfer");
            }
            drop((hold, second));
            let rows = refill_rows(&lib);
            let finished: Vec<_> = rows.iter().filter(|r| r[3] == 3).collect();
            assert_eq!(finished.len(), 1, "joined requests cannot duplicate completed content");
            assert_eq!((finished[0][7], finished[0][8], finished[0][9]), (240_000, total as u64, expected as u64));
            let resolved: Vec<_> = rows.iter().filter(|r| r[3] == 2).map(|r| r[9]).collect();
            assert_eq!(resolved, if expected == CachePublication::Published { vec![1, 2, 3] } else { vec![1, 2] });
        }
            })
    }

    #[test]
    fn a_raw_transfer_shorter_than_its_declared_length_is_never_cached() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut socket = accept_transfer_fixture(listener);
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            // Declares 8 KiB, sends 1 KiB and closes.
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8192\r\nConnection: close\r\n\r\n");
            let _ = socket.write_all(&[7u8; 1024]);
          });
        let lib = transfer_library(&url, "short");
        let trace = traced(&lib);
        let song = Song { id: "short".into(), duration: 240, size: Some(8192), ..song("mp3", 256) };
        let (_, progress, hold) = lib.start_fetch(&song, "raw", true).unwrap();
        server.join().unwrap();
        assert!(until(|| lib.inflight.lock().unwrap().is_empty()), "the transfer never ended");
        assert!(progress.snapshot().error.is_some());
        assert_eq!(progress.snapshot().publication, None);
        assert!(!trace.snapshot().events.iter().any(|e| e.kind == crate::playtrace::Kind::CachePublish));
        drop(hold);
        assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), "short", &["raw"]).is_none(), "a truncated song was cached as complete");
        assert!(until(|| partials(&lib).is_empty()), "the truncated partial file was left behind");
    }

    #[test]
    fn a_skip_is_traced_as_start_orphan_cancel_and_stop_of_one_numbered_transfer() {
        crate::audio::callback::test_scope(|| {
        use crate::playtrace::Kind::*;
        let (url, server) = transfer_server(4 << 20, 64 << 10, None);
        let lib = transfer_library(&url, "trace-skip");
        let trace = traced(&lib);
        let tracks = Tracks(lib.clone());
        let next = cached_song(&lib, "next");
        let song = Song { id: "private-song-id".into(), title: "Private Title".into(), duration: 240, size: Some(4 << 20), ..song("mp3", 256) };
        let mut opened = tracks.open(&crate::rt::Blocking::dedicated_thread(), &song, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        let mut head = [0u8; 4096];
        opened.source.read_exact(&mut head).unwrap();
        drop(opened);
        let _playing = tracks.open(&crate::rt::Blocking::dedicated_thread(), &next, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        assert!(until(|| xfer_events(&trace).iter().any(|e| e.0 == XferEnd)));
        assert!(server.join().unwrap());
        let snapshot = trace.snapshot();
        let events = xfer_events(&trace);
        // Percent under 80 at the orphan and the cancel; outcome 2 = stopped by the cancel.
        assert_eq!(events.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), [(XferStart, 1), (XferOrphan, 1), (XferCancel, 1), (XferEnd, 1)]);
        assert_eq!((events[0].2, events[3].2), (1, 2), "a player stream, stopped by the cancel");
        assert!(events[1].2 < 80 && events[2].2 < 80);
        let stamped: Vec<_> = snapshot.events.iter().filter(|e| matches!(e.kind, XferStart | XferOrphan | XferCancel | XferEnd)).collect();
        assert!(stamped.windows(2).all(|p| p[0].seq < p[1].seq && p[0].t_us <= p[1].t_us));
        // Download progress is in the windows, in bytes, with the transfer counted active.
        let windows: Vec<_> = snapshot.windows.iter().chain(snapshot.current.iter()).collect();
        let bytes: u64 = windows.iter().map(|w| u64::from(w.xfer_bytes)).sum();
        assert!(bytes >= 4096 && bytes < 4 << 20, "{bytes}");
        // Numeric identities only: neither the dump nor a record carries the song.
        let text = snapshot.text();
        assert!(!text.contains("private") && !text.contains("Private") && !text.contains("mp3") && !text.contains('/'));
        assert!(text.lines().all(|line| line.starts_with("# W ") || line.split_whitespace().skip(1)
            .all(|field| field.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || "-=_".contains(c)))), "{text}");
            })
    }

    #[test]
    fn a_reopen_racing_a_cancel_is_traced_as_a_rejoin_or_a_new_transfer_never_a_join_of_a_cancelled_one() {
        use crate::playtrace::Kind::*;
        // Reopen first, then the cancel: the orphan is rejoined and survives.
        let (url, server) = transfer_server(4 << 20, 64 << 10, None);
        let lib = transfer_library(&url, "trace-rejoin");
        let trace = traced(&lib);
        let song = Song { id: "raced".into(), duration: 240, size: Some(4 << 20), ..song("mp3", 256) };
        let (_, progress, first) = lib.start_fetch(&song, "raw", true).unwrap();
        // The fixture answers one connection: the first transfer must own it
        // before a second transfer exists.
        assert!(until(|| progress.snapshot().written > 0));
        drop(first);
        let (_, _, again) = lib.start_fetch(&song, "raw", true).unwrap();
        assert_eq!(lib.cancel_orphans(|_| false), 0, "held again: not cancelled");
        let kinds = |trace: &crate::playtrace::Trace| xfer_events(trace).iter().map(|e| (e.0, e.1)).collect::<Vec<_>>();
        assert_eq!(kinds(&trace), [(XferStart, 1), (XferOrphan, 1), (XferJoin, 1)]);
        assert_eq!(xfer_events(&trace)[2].2, 1, "one reader after the join");
        assert_eq!(trace.snapshot().events.iter().find(|e| e.kind == XferJoin).unwrap().c, 1, "the join found it orphaned");
        // The cancel first, then the reopen before the transfer thread has
        // seen the flag: a new transfer with its own serial, and the old one
        // still ends as stopped.
        drop(again);
        assert_eq!(lib.cancel_orphans(|_| false), 1);
        let (_, _, restarted) = lib.start_fetch(&song, "raw", true).unwrap();
        assert!(!restarted.as_ref().unwrap().cancel.is_cancelled(), "the new transfer is not born cancelled");
        assert_eq!(kinds(&trace)[3..5], [(XferOrphan, 1), (XferCancel, 1)]);
        assert!(kinds(&trace)[5..].contains(&(XferStart, 2)), "its own serial");
        assert!(until(|| xfer_events(&trace).contains(&(XferEnd, 1, 2))), "the cancelled transfer stops");
        assert!(server.join().unwrap());
        assert!(!kinds(&trace)[5..].iter().any(|e| e.0 == XferJoin), "nothing joins after the cancel");
        drop(restarted);
        lib.cancel_orphans(|_| false);
    }

    #[test]
    fn reopening_the_same_song_rejoins_its_download() {
        crate::audio::callback::test_scope(|| {
        // One connection only: a second download would never be answered.
        let (url, server) = transfer_server(256 << 10, 64 << 10, None);
        let lib = transfer_library(&url, "reopen");
        let tracks = Tracks(lib.clone());
        let song = Song { id: "seeked".into(), duration: 240, size: Some(256 << 10), ..song("mp3", 256) };
        let mut first = tracks.open(&crate::rt::Blocking::dedicated_thread(), &song, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        let mut head = [0u8; 4096];
        first.source.read_exact(&mut head).unwrap();
        let part = partials(&lib);
        drop(first); // a seek, a pause without hardware pause, a failed probe
        let mut again = tracks.open(&crate::rt::Blocking::dedicated_thread(), &song, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        assert_eq!(transfer(&lib, "seeked"), (false, false, 1), "rejoined, not restarted");
        assert_eq!(partials(&lib), part, "the same partial file");
        let mut all = Vec::new();
        again.source.read_to_end(&mut all).unwrap();
        assert_eq!(all.len(), 256 << 10);
        assert!(!server.join().unwrap(), "one download, run to the end");
        assert!(until(|| lib.find_local(&song).is_some()));
            })
    }

    #[test]
    fn a_transfer_past_the_retention_point_finishes_and_is_cached() {
        let (tx, rx) = std::sync::mpsc::channel();
        let total = 100 << 10;
        let (url, server) = transfer_server(total, total * 9 / 10, Some(rx));
        let lib = transfer_library(&url, "retain");
        let tracks = Tracks(lib.clone());
        let next = cached_song(&lib, "next");
        let song = Song { id: "nearly".into(), duration: 240, size: Some(total as u64), ..song("mp3", 256) };
        let opened = tracks.open(&crate::rt::Blocking::dedicated_thread(), &song, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        let key = (song.id.clone(), "raw".to_string());
        let progress = lib.inflight.lock().unwrap().get(&key).unwrap().progress.clone();
        assert!(until(|| progress.snapshot().written as usize >= total * 9 / 10));
        drop(opened);
        let _playing = tracks.open(&crate::rt::Blocking::dedicated_thread(), &next, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        assert_eq!(transfer(&lib, "nearly").1, false, "kept past 80%");
        tx.send(()).unwrap();
        assert!(!server.join().unwrap(), "the download ran to the end");
        assert!(until(|| lib.find_local(&song).is_some()), "and the song is cached");
        assert!(partials(&lib).is_empty());
    }

    #[test]
    fn a_shared_transfer_is_orphaned_only_when_its_last_reader_leaves() {
        let (url, server) = transfer_server(4 << 20, 64 << 10, None);
        let lib = transfer_library(&url, "shared");
        let song = Song { id: "shared".into(), duration: 240, size: Some(4 << 20), ..song("mp3", 256) };
        let (part, progress, first) = lib.start_fetch(&song, "raw", true).unwrap();
        let (joined, _, second) = lib.start_fetch(&song, "raw", false).unwrap();
        assert_eq!(part, joined, "the second reader joins, it does not start another download");
        assert_eq!(transfer(&lib, "shared"), (false, false, 2));
        assert!(until(|| progress.snapshot().written > 0), "this case cancels an established shared stream");
        drop(first);
        assert_eq!(lib.cancel_orphans(|_| false), 0, "one reader left");
        drop(second);
        assert_eq!(transfer(&lib, "shared"), (true, false, 0));
        assert_eq!(lib.cancel_orphans(|k| k.0 == "shared"), 0, "kept when still wanted");
        assert_eq!(lib.cancel_orphans(|_| false), 1);
        assert!(until(|| lib.inflight.lock().unwrap().is_empty()));
        assert!(progress.snapshot().error.is_some());
        assert_eq!(progress.snapshot().publication, None);
        assert!(server.join().unwrap());
        assert!(until(|| partials(&lib).is_empty()));
    }

    #[test]
    fn fetch_ahead_waits_for_a_playing_download_and_gives_up_on_a_newer_request_or_offline() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let (url, server) = transfer_server(4 << 20, 64 << 10, None);
        let lib = transfer_library(&url, "priority");
        let (updates, mut queue) = tokio::sync::watch::channel(None);
        assert_eq!(runtime.block_on(lib.wait_playback_priority(&mut queue)), PriorityWait::Clear);
        let song = Song { id: "playing".into(), duration: 240, size: Some(4 << 20), ..song("mp3", 256) };
        let (_, _, hold) = lib.start_fetch(&song, "raw", true).unwrap();
        let started = Instant::now();
        assert_eq!(runtime.block_on(lib.wait_playback_priority(&mut queue)), PriorityWait::Timeout);
        assert!(started.elapsed() >= PLAYBACK_PRIORITY_WAIT);
        updates.send_replace(Some(Vec::new()));
        assert_eq!(runtime.block_on(lib.wait_playback_priority(&mut queue)), PriorityWait::Newer);
        queue.borrow_and_update();
        lib.online.store(false, Ordering::Relaxed);
        assert_eq!(runtime.block_on(lib.wait_playback_priority(&mut queue)), PriorityWait::Offline);
        lib.online.store(true, Ordering::Relaxed);
        drop(hold);
        assert_eq!(lib.cancel_orphans(|_| false), 1);
        assert!(server.join().unwrap());
        assert_eq!(runtime.block_on(lib.wait_playback_priority(&mut queue)), PriorityWait::Clear);
    }

    #[test]
    fn fetch_ahead_wakes_on_real_transfer_publication() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let (url, server) = transfer_server(3 * 1024, 1024, Some(gate));
        let lib = transfer_library(&url, "priority-completion");
        let song = Song { id: "playing".into(), duration: 240, size: Some(3 * 1024), ..song("mp3", 256) };
        let (_, progress, hold) = lib.start_fetch(&song, "raw", true).unwrap();
        assert!(until(|| progress.snapshot().written > 0));
        runtime.block_on(async {
            use std::future::{Future, poll_fn};
            let (_sender, mut receiver) = tokio::sync::watch::channel(None);
            let wait = lib.wait_playback_priority(&mut receiver);
            tokio::pin!(wait);
            poll_fn(|cx| {
                assert!(wait.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            }).await;
            release.send(()).unwrap();
            assert_eq!(wait.await, PriorityWait::Clear);
        });
        assert!(progress.snapshot().done);
        assert!(!server.join().unwrap());
        drop(hold);
    }

    #[test]
    fn async_prefetch_idle_wait_releases_library_and_retires_on_channel_close() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let (lib, receiver) = Library::create(writable_transfer_paths("prefetch-retirement"), &Config::default(), Arc::new(|_| {}));
        let weak = Arc::downgrade(&lib);
        runtime.block_on(async {
            let worker = tokio::spawn(prefetch::run(weak.clone(), receiver));
            tokio::task::yield_now().await;
            assert!(!worker.is_finished());
            drop(lib);
            tokio::time::timeout(crate::library::TEST_LIVENESS, worker).await.unwrap().unwrap();
            assert!(weak.upgrade().is_none());
        });
    }

    #[test]
    fn startup_sweep_removes_only_partial_files_no_transfer_owns() {
        let (url, server) = transfer_server(4 << 20, 64 << 10, None);
        let lib = transfer_library(&url, "sweep");
        let song = Song { id: "live".into(), duration: 240, size: Some(4 << 20), ..song("mp3", 256) };
        let (live, progress, hold) = lib.start_fetch(&song, "raw", true).unwrap();
        assert!(until(|| progress.snapshot().written > 0), "the fixture must be serving before its stop assertion");
        let stale = lib.paths.stream_cache().join("old.raw.mp3.0badc0de.part");
        let kept = lib.paths.stream_cache().join("cached.raw.mp3");
        std::fs::write(&stale, b"left by a killed run").unwrap();
        std::fs::write(&kept, b"a cached song").unwrap();
        let publication = crate::rt::Ui::new().unwrap().block_on(lib.offline_art_gate.acquire());
        let staged = lib.paths.stream_partials().join("delete-0123456789abcdef.part");
        std::fs::write(&staged, b"awaiting cache index transaction").unwrap();
        assert_eq!(lib.sweep_partials(&crate::rt::Blocking::dedicated_thread()), [staged.clone()]);
        assert!(staged.is_file(), "only the publication owner can finish a staged deletion");
        drop(publication);
        assert!(!stale.exists());
        assert!(live.exists(), "the running transfer's partial file stays");
        assert!(kept.exists());
        drop(hold);
        lib.cancel_orphans(|_| false);
        assert!(server.join().unwrap());
    }

    #[test]
    fn cancelling_before_stream_request_does_not_open_a_connection() {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let lib=transfer_library(&format!("http://{}",listener.local_addr().unwrap()),"pre-request-cancel");
        let client=lib.client().unwrap();
        let request=client.stream_request("generated",&StreamOptions::default());
        // Direct writer fixture, not a registered in-flight cache partial.
        // Keep it outside the startup sweep's legitimately owned namespace.
        let path=lib.paths.database().with_file_name("cancelled-output");
        let output=transfer_io::FileOutput::new(std::fs::File::create(&path).unwrap());
        let cancel=Arc::new(transfer_io::Cancel::default());cancel.cancel();
        let progress=Progress::new();let observed=progress.clone();
        let (done,finished)=mpsc::channel();
        crate::rt::io().spawn(async move {
            done.send(lib.fetch_file_async(client,request,output,progress,cancel,Priority::Foreground,None).await).unwrap();
        });
        assert!(matches!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),Err(LibError::Server(error)) if error=="cancelled"));
        assert_eq!(observed.snapshot().written,0);
        assert!(matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
        assert_eq!(std::fs::metadata(&path).unwrap().len(),0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancelling_a_stream_waiting_for_headers_closes_transport_and_retires_its_partial() {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url=format!("http://{}",listener.local_addr().unwrap());
        let (ready,headers)=mpsc::channel();
        let server=std::thread::spawn(move || {
            let mut socket=accept_transfer_fixture(listener);
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request=Vec::new();
            while !request.ends_with(b"\r\n\r\n") {let mut byte=[0];socket.read_exact(&mut byte).unwrap();request.push(byte[0]);}
            ready.send(()).unwrap();
            // Deliberately never answer: cancellation must not wait for the
            // ordinary media read timeout before closing this connection.
            let mut byte=[0];assert_eq!(socket.read(&mut byte).unwrap(),0);
          });
        let lib=transfer_library(&url,"header-cancel");
        let song=Song {id:"header-cancel".into(),duration:240,size:Some(4<<20),..song("mp3",256)};
        let (path,progress,hold)=lib.start_fetch(&song,"raw",true).unwrap();
        headers.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        drop(hold);assert_eq!(lib.cancel_orphans(|_|false),1);
        assert!(until(|| lib.inflight.lock().unwrap().is_empty()));
        assert!(progress.snapshot().done && progress.snapshot().error.is_some());
        assert!(!path.exists());
        server.join().unwrap();
    }

    #[test]
    fn stream_completion_cannot_publish_into_a_replacement_account() {
        let (release,gate)=mpsc::channel();
        let (url,server)=transfer_server(128<<10,64<<10,Some(gate));
        let lib=transfer_library(&url,"stream-account-replacement");
        let song=Song {id:"generated-old-account".into(),duration:240,size:Some(128<<10),..song("mp3",256)};
        let (path,progress,_hold)=lib.start_fetch(&song,"raw",true).unwrap();
        assert!(until(|| progress.snapshot().written>0));
        let mut config=Config::default();config.server.url="http://127.0.0.1:9".into();config.server.user="replacement".into();
        lib.configure(&config);
        release.send(()).unwrap();
        assert!(until(|| progress.snapshot().done));
        assert!(until(|| lib.inflight.lock().unwrap().is_empty()));
        assert!(progress.snapshot().error.is_some());
        assert_eq!(progress.snapshot().publication,None);
        assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id,&["raw"]).is_none());
        assert!(!path.exists());
        assert!(!lib.online.load(Ordering::Relaxed));
        assert!(!server.join().unwrap());
    }

    #[test]
    fn prefetch_skips_long_track_but_foreground_can_stream_it() {
        crate::audio::callback::test_scope(|| {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let paths = writable_transfer_paths("long-prefetch");
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "fixture".into();
        config.playback.stream_quality = Quality::Original;
        config.storage.reserve_mb = 0; // Generated RAM fixture, not card reserve policy.
        let lib = Library::open(paths, &config, Arc::new(|_| {}));
        lib.online.store(true, Ordering::Relaxed);
        let long = Song { id: "long".into(), duration: 3600, size: Some(100_000_000), ..song("mp3", 256) };
        let short = Song { id: "short".into(), duration: 240, size: Some(8_000_000), ..song("mp3", 256) };
        let tracks = Tracks(lib.clone());
        let server = std::thread::spawn(move || {
            for expected in ["id=short", "id=long"] {
                let deadline = Instant::now() + crate::library::TEST_LIVENESS;
                let mut socket = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("expected {expected}: {e}"),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                assert!(String::from_utf8(request).unwrap().contains(expected));
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture").unwrap();
            }
          });
        tracks.upcoming(&[long.clone(), short.clone()]);
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        while lib.find_local(&short).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lib.find_local(&short).is_some());
        assert!(lib.find_local(&long).is_none());
        let mut opened = tracks.open(&crate::rt::Blocking::dedicated_thread(), &long, &Arc::new(crate::audio::interrupt::Interrupt::new(false))).unwrap();
        let mut data = Vec::new();
        opened.source.read_to_end(&mut data).unwrap();
        assert_eq!(data, b"fixture");
        server.join().unwrap();
            })
    }

    /// A card at its reserve keeps no cached song: every publication is
    /// evicted at once. Fetching ahead then downloads each upcoming song to
    /// delete it, and the song streams again when it is reached.
    #[test]
    fn fetch_ahead_does_not_request_a_song_the_card_cannot_keep() {
        crate::audio::callback::test_scope(|| {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let paths = writable_transfer_paths("full-card-prefetch");
        let mut config = Config::default();
        config.server.url = format!("http://{}", listener.local_addr().unwrap());
        config.server.user = "fixture".into();
        config.playback.stream_quality = Quality::Original;
        // Below its reserve whatever the build host has free.
        config.storage.reserve_mb = u64::MAX;
        let lib = Library::open(paths, &config, Arc::new(|_| {}));
        lib.online.store(true, Ordering::Relaxed);
        let short = Song { id: "short".into(), duration: 240, size: Some(8_000_000), ..song("mp3", 256) };
        let tracks = Tracks(lib.clone());
        tracks.upcoming(&[short.clone()]);
        // The unfixed scheduler connects within a few milliseconds.
        let quiet = Instant::now() + Duration::from_millis(700);
        while Instant::now() < quiet {
            assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
                "fetch-ahead requested a song the card would evict at once");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lib.find_local(&short).is_none());
        // The same queue with room on the card is fetched.
        lib.settings.write().unwrap().reserve_bytes = 0;
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + crate::library::TEST_LIVENESS;
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                    Err(e) => panic!("expected the fetch-ahead request: {e}"),
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            assert!(String::from_utf8(request).unwrap().contains("id=short"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture").unwrap();
        });
        tracks.upcoming(&[short.clone()]);
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        while lib.find_local(&short).is_none() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
        assert!(lib.find_local(&short).is_some(), "with room on the card the song is fetched ahead");
        server.join().unwrap();
            })
    }
    #[test]
    fn hearts_and_ratings_queue_without_contacting_server() {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
        let root=generated_test_directory("preferences");
        let mut config=Config::default();config.server.url=format!("http://{}",listener.local_addr().unwrap());config.server.user="fixture".into();
        let (lib,_)=Library::create(Paths::new(root),&config,Arc::new(|_|{}));
        lib.online.store(true,Ordering::Relaxed);
        let heart=lib.star(ItemKind::Song,"fixture-song",true);let rating=lib.rate("fixture-song",4);
        let song=Song {id:"fixture-song".into(),..Default::default()};
        assert!(lib.loved(&song));
        let runtime=crate::rt::Ui::new().unwrap();
        runtime.block_on(heart).unwrap();runtime.block_on(rating).unwrap();
        assert_eq!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 10).len(),2);
        let heart=lib.star(ItemKind::Song,"fixture-song",false);assert!(!lib.loved(&song));runtime.block_on(heart).unwrap();
        assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn eq_changes_keep_normalized_server_client_and_online_status() {
        let root=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../build/perf/tests/eq-connection-{}-{}",std::process::id(),fastrand::u64(..)));
        let mut config=Config::default();
        config.server.url="http://127.0.0.1:9/navidrome///".into();
        config.server.user="fixture".into();
        let lib=Library::open(Paths::new(root),&config,Arc::new(|_|{}));
        *lib.last_probe.lock().unwrap()=Instant::now();
        lib.online.store(true,Ordering::Relaxed);
        let client=lib.client().unwrap();
        for level in [-6,0,6] {
            config.playback.eq.graphic_enabled=true;
            config.playback.eq.bands[0]=level;
            lib.configure(&config);
            assert!(Arc::ptr_eq(&client,&lib.client().unwrap()),"EQ must not recreate the network client");
            assert!(lib.is_online(),"EQ must not reset the connection indicator");
        }
        config.server.url="http://127.0.0.1:9/different".into();
        lib.configure(&config);
        assert!(!Arc::ptr_eq(&client,&lib.client().unwrap()));
        assert!(!lib.is_online(),"an actual server change still invalidates status");
    }

}

#[cfg(test)]
mod download_view_tests {
    use super::{DownloadState, DownloadView};

    #[test]
    fn songs_and_albums_show_queued_active_and_done_distinctly() {
        let mut v = DownloadView::default();
        v.albums.insert("album".into(), vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        v.queued = ["b", "c", "d"].iter().map(|s| s.to_string()).collect();
        // "b" downloading at 50 %: a done, b half, c and d waiting.
        v.active = Some(("b".into(), Some(0.5)));
        assert_eq!(v.song("a", true), DownloadState::Done);
        assert_eq!(v.song("b", false), DownloadState::Active(Some(0.5)));
        assert_eq!(v.song("c", false), DownloadState::Queued);
        assert_eq!(v.song("x", false), DownloadState::None);
        assert_eq!(v.album("album"), DownloadState::Active(Some(1.5 / 4.0)));
        // Waiting with nothing active: queued, not shown as downloaded.
        v.active = None;
        assert_eq!(v.album("album"), DownloadState::Queued);
        // The active song may already have left the queue table.
        v.queued = ["c", "d"].iter().map(|s| s.to_string()).collect();
        v.active = Some(("b".into(), None));
        assert_eq!(v.album("album"), DownloadState::Active(Some(1.0 / 4.0)));
        v.queued.clear(); v.active = None;
        assert_eq!(v.album("album"), DownloadState::None, "a song failed: not downloaded");
        v.offline = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(v.album("album"), DownloadState::Done);
        assert_eq!(v.album("other"), DownloadState::None);
    }
}

#[cfg(test)]
mod network_up_tests {
    use super::*;

    #[test]
    fn network_up_probes_the_server_at_once() {
        let root = generated_test_directory("network-up");
        let mut config = Config::default();
        config.server.url = "http://127.0.0.1:9".into(); config.server.user = "generated".into(); config.server.password = "synthetic-only".into();
        let lib = Library::open(Paths::new(root.clone()), &config, Arc::new(|_| {}));
        // A probe just ran: the ordinary cadence would wait 20 s.
        let before = Instant::now();
        *lib.last_probe.lock().unwrap() = before;
        lib.network_up();
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        while *lib.last_probe.lock().unwrap() == before && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(20)); }
        assert!(*lib.last_probe.lock().unwrap() > before, "probe ran without waiting for the retry cadence");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod fetch_target_tests {
    use super::{fetch_targets, fetch_targets_minutes, space_admitted};
    use proptest::prelude::*;

    #[test]
    fn fetch_ahead_keeps_within_the_cache_cap_and_the_card_reserve() {
        const MB: u64 = 1 << 20;
        let sizes = [Some(8 * MB), Some(30 * MB), None, Some(8 * MB)];
        let all = [0, 1, 2, 3];
        assert_eq!(space_admitted(&all, &sizes, 4096 * MB, 512 * MB, 0, Some(64_000 * MB)), all, "room for all");
        assert_eq!(space_admitted(&all, &sizes, 4096 * MB, 512 * MB, 0, Some(512 * MB)), vec![2],
            "a card at its reserve with an empty cache keeps no fetched song; the unfetchable one takes no room");
        assert_eq!(space_admitted(&all, &sizes, 4096 * MB, 512 * MB, 100 * MB, Some(430 * MB)), vec![0, 2, 3],
            "18 MB after eviction: the 30 MB song does not fit, the two 8 MB songs do");
        assert_eq!(space_admitted(&all, &sizes, 16 * MB, 0, 0, Some(64_000 * MB)), vec![0, 2, 3], "a cap smaller than the burst");
        assert_eq!(space_admitted(&all, &sizes, 0, 0, 0, Some(64_000 * MB)), vec![2], "no cache, no fetch-ahead");
        assert_eq!(space_admitted(&all, &sizes, 4096 * MB, 512 * MB, 0, None), all, "an unreadable card applies the cap only");
        assert_eq!(space_admitted(&[5], &sizes, 0, 0, 0, None), vec![5], "an index without a size takes no room");
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, failure_persistence: None, ..ProptestConfig::default() })]
        #[test]
        fn admitted_songs_are_an_ordered_subset_that_fits(
            sizes in proptest::collection::vec(proptest::option::of(0u64..=64 << 20), 0..12),
            cache in prop_oneof![Just(0u64), 0u64..=256 << 20, Just(u64::MAX)],
            reserve in prop_oneof![Just(0u64), 0u64..=256 << 20, Just(u64::MAX)],
            used in prop_oneof![Just(0u64), 0u64..=256 << 20, Just(u64::MAX)],
            free in proptest::option::of(prop_oneof![Just(0u64), 0u64..=512 << 20, Just(u64::MAX)]),
        ) {
            let targets: Vec<usize> = (0..sizes.len()).collect();
            let admitted = space_admitted(&targets, &sizes, cache, reserve, used, free);
            // In order, without repeats.
            prop_assert!(admitted.windows(2).all(|pair| pair[0] < pair[1]));
            prop_assert!(admitted.iter().all(|index| targets.contains(index)));
            let room = cache.min(free.map_or(u64::MAX, |free| free.saturating_add(used).saturating_sub(reserve)));
            let total: u128 = admitted.iter().map(|&index| u128::from(sizes[index].unwrap_or(0))).sum();
            prop_assert!(total <= u128::from(room), "{total} admitted into {room}");
            // Greedy in queue order: every refused song did not fit what was left when it was reached.
            let mut left = room;
            for index in targets {
                let needed = sizes[index].unwrap_or(0);
                prop_assert_eq!(admitted.contains(&index), needed <= left);
                if needed <= left { left -= needed; }
            }
        }
    }
    fn fetch_targets_minutes_all(local: &[bool], d: &[u64], prefetch: usize, fill: u64, low: u64) -> Vec<usize> {
        fetch_targets_minutes(local, &vec![true; local.len()], d, prefetch, fill, low)
    }

    #[test]
    fn one_at_a_time_and_bursts() {
        // One at a time: the first `prefetch` songs not on the card.
        assert_eq!(fetch_targets(&[false, false, false], 2, 0), vec![0, 1]);
        assert_eq!(fetch_targets(&[true, false, false], 2, 0), vec![1]);
        // Bursts: nothing while the next song is on the card...
        assert_eq!(fetch_targets(&[true, true, false, false, false], 2, 4), Vec::<usize>::new());
        // ...then the next four back to back.
        assert_eq!(fetch_targets(&[false, false, true, false, false, false], 2, 4), vec![0, 1, 3]);
        assert_eq!(fetch_targets(&[false], 2, 4), vec![0], "a short queue fetches what remains");
        assert_eq!(fetch_targets(&[], 2, 4), Vec::<usize>::new());
        assert_eq!(fetch_targets(&[false, false], 0, 4), Vec::<usize>::new(), "prefetch off stays off with batching");
        // By duration (minutes as ms /60_000 for reading): 3-minute songs,
        // fill to 15, refill below 3.
        let m = 60_000;
        let d = [3 * m; 8];
        assert_eq!(fetch_targets_minutes_all(&[true, false, false, false, false, false, false, false], &d, 2, 15 * m, 3 * m), Vec::<usize>::new(), "3 min still cached");
        assert_eq!(fetch_targets_minutes_all(&[false; 8], &d, 2, 15 * m, 3 * m), vec![0, 1, 2, 3, 4], "fills to 15 min");
        assert_eq!(fetch_targets_minutes_all(&[false, true, false, false, false, false, false, false], &d, 2, 15 * m, 3 * m), vec![0, 2, 3, 4], "cached songs count toward the fill");
        assert_eq!(fetch_targets_minutes_all(&[false, false], &d[..2], 2, 15 * m, 3 * m), vec![0, 1], "a short queue fetches what remains");
        assert_eq!(fetch_targets_minutes_all(&[false; 3], &d[..3], 0, 15 * m, 3 * m), Vec::<usize>::new(), "prefetch off stays off");
        assert_eq!(fetch_targets_minutes(&[false; 4], &[false, true, true, true], &[60 * m, 3 * m, 3 * m, 3 * m], 2, 6 * m, 3 * m), vec![1, 2],
            "a song too long to fetch ahead streams when reached and adds no cached time");
    }
}

/// Songs kept playable from the card for an instant Shuffle Songs start.
pub(crate) const SHUFFLE_READY: usize = 3;
