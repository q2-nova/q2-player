//! `--bench` kinds for the card database, sync and start-up on a generated
//! library: 4,000 albums in the alphabetical catalog (beside the superseded
//! version a refresh leaves until the next one), 250 downloaded albums, 308
//! stored album responses and 500 cached songs. The server's side is
//! generated; nothing reaches a network.
//!
//! - "library-db": the catalog and cache reads and writes of start-up and
//!   browsing, each step timed: open, server binding, the current catalog,
//!   its first page, pages with letter jumps, Cover Flow positions, stored
//!   responses read and written, the song cache, downloaded songs and the
//!   saved queue. The positions change with each iteration.
//! - "library-sync": one generated server delta (albums removed, added,
//!   renamed and recounted; downloaded songs re-encoded or removed) applied
//!   through the database steps of a catalog refresh and Full Sync, with the
//!   server's documents parsed, on a fresh copy of the same database each
//!   iteration.
//! - "app-startup": configuration, library open with its server binding,
//!   the stored catalog's first page, fonts, the App and its first frame, in
//!   the order `main` runs them. The generated server is unreachable and the
//!   library is in Offline Mode.
//!
//! Each reports per-step medians, every iteration's total in microseconds
//! (`iteration_us`; `timed_us` is their median) and a `checksum` that is the
//! same for every build. Setup is outside the timed steps but inside the
//! process's `cpu_us`.
use crate::library::db::{Db, OfflineSet, catalog};
use crate::library::music_reconcile::{MissingSong, media_changed};
use crate::rt::Blocking;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use subsonic::{Album, AlbumWithSongs, Song};

const ALBUMS: u32 = 4000;
/// Albums the delta adds, after the existing IDs.
const ADDED: u32 = 100;
/// Every 16th album is downloaded.
const DOWNLOADED: u32 = 16;
/// Every 13th album has a stored response, and every 8th song of the first
/// 4,000 a cache entry.
const STORED: u32 = 13;
const CACHED: u32 = 8;
const SERVER_URL: &str = "http://127.0.0.1:9";
const SERVER_USER: &str = "generated-library";
const SCAN_BEFORE: &str = "generated-scan-1";
const SCAN_AFTER: &str = "generated-scan-2";

const WORDS: [&str; 32] = ["Night", "Blue", "Ägypten", "Échos", "River", "Static", "Glass", "Über", "Summer", "Ζωή",
    "Orbit", "Paper", "Signal", "Velvet", "Harbor", "Lantern", "Ōkami", "Ember", "North", "Quiet", "Ñandú", "Delta",
    "Mirror", "Zero", "Copper", "Island", "Jasmine", "Kettle", "Meadow", "Yonder", "Willow", "Fable"];
const ARTICLES: [&str; 8] = ["", "", "", "", "", "The ", "A ", "An "];
const GENRES: [&str; 6] = ["Rock", "Jazz", "Electronic", "Classical", "Folk", "Hip-Hop"];

struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self { Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5851_f42d_4c95_7f2d) }
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }
    fn below(&mut self, n: u32) -> u32 { self.next() % n }
    fn pick(&mut self, of: &[&'static str]) -> &'static str { of[self.next() as usize % of.len()] }
}

/// Album `i` as the server lists it before the delta or after it; `None`
/// when the server does not have it.
fn server_album(i: u32, after: bool) -> Option<Album> {
    if (i >= ALBUMS && !after) || (after && i < ALBUMS && i % 37 == 5) { return None; }
    let mut r = Lcg::new(u64::from(i));
    let (article, first, second, prefix) = (r.pick(&ARTICLES), r.pick(&WORDS), r.pick(&WORDS), r.pick(&WORDS));
    let songs = 6 + r.below(14);
    let artist = r.below(600);
    let name = if after && i % 53 == 7 { format!("{prefix} {article}{first} {second}") } else { format!("{article}{first} {second}") };
    let song_count = if after && i % 29 == 3 { songs + 1 } else { songs };
    Some(Album { id: format!("al-{i:05}"), name, artist: format!("Generated Artist {artist:03}"),
        artist_id: Some(format!("ar-{artist:03}")), cover_art: Some(format!("al-{i:05}")), song_count,
        duration: song_count * (150 + r.below(120)), year: Some(1960 + r.below(66) as i32),
        genre: Some(r.pick(&GENRES).into()), created: None, starred: (i % 23 == 0).then(|| "generated".into()),
        user_rating: (i % 31 == 0).then_some(4) })
}

/// The server's alphabetical listing.
fn listing(after: bool) -> Vec<Album> {
    let mut albums: Vec<Album> = (0..ALBUMS + ADDED).filter_map(|i| server_album(i, after)).collect();
    albums.sort_by_cached_key(|a| (a.name.to_lowercase(), a.id.clone()));
    albums
}

/// The songs the server has for album `i`. After the delta, track 2 of every
/// 96th album is gone and every 4th track of every 400th is re-encoded.
fn server_songs(album: &Album, i: u32, after: bool) -> Vec<Song> {
    let mut r = Lcg::new(u64::from(i) ^ 0x51_7cc1_b727_220a);
    (1..=album.song_count).filter_map(|t| {
        let (first, second) = (r.pick(&WORDS), r.pick(&WORDS));
        let (size, seconds, rate) = (18_000_000 + u64::from(r.below(30_000_000)), 120 + r.below(300), 800 + r.below(400));
        if after && i % 96 == 0 && t == 2 { return None; }
        let encoded = after && i % 400 == 0 && t % 4 == 1;
        Some(Song { id: format!("so-{i:05}-{t:02}"), title: format!("{first} {second}"), album: album.name.clone(),
            artist: album.artist.clone(), track: Some(t), year: album.year, genre: album.genre.clone(),
            cover_art: album.cover_art.clone(), size: Some(if encoded { size / 3 } else { size }),
            content_type: Some(if encoded { "audio/mpeg" } else { "audio/flac" }.into()),
            suffix: Some(if encoded { "mp3" } else { "flac" }.into()), duration: seconds,
            bit_rate: Some(if encoded { 320 } else { rate }), sampling_rate: Some(44_100),
            bit_depth: (!encoded).then_some(16), album_id: Some(album.id.clone()), artist_id: album.artist_id.clone(),
            ..Default::default() })
    }).collect()
}

/// A network document as the server sends it.
fn document(field: &str, value: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"subsonic-response": {"status": "ok", "version": "1.16.1", field: value}})).unwrap()
}

fn server_id() -> String { format!("{SERVER_URL}|{SERVER_USER}") }

fn storage(error: rusqlite::Error) -> String { format!("generated library: {error}") }

/// Builds and publishes one catalog version from server pages, as a refresh does.
fn publish(blocking: &Blocking, db: &mut Db, pages: &[Vec<u8>], marker: &str) -> Result<catalog::Catalog, String> {
    let build = db.catalog_begin(blocking).map_err(storage)?;
    let call = subsonic::calls::album_list(subsonic::AlbumListType::AlphabeticalByName, catalog::BATCH_LIMIT as u32, 0);
    let mut offset = 0;
    for page in pages {
        let albums = call.parse_text(page).map_err(|e| e.to_string())?.album;
        let rows: Vec<(Album, String)> = albums.into_iter().map(|a| { let label = crate::ui::list::index_letter(&a.name); (a, label) }).collect();
        offset = db.catalog_append(blocking, &build, offset, &rows).map_err(storage)?;
    }
    db.catalog_publish_marked(blocking, &build, Some(marker)).map_err(storage)
}

/// The listing in server pages of `BATCH_LIMIT`, with the final short page.
fn pages(albums: &[Album]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = albums.chunks(catalog::BATCH_LIMIT).map(|c| document("albumList2", json!({"album": c}))).collect();
    if albums.len() % catalog::BATCH_LIMIT == 0 { out.push(document("albumList2", json!({}))); }
    out
}

fn stored_key(i: u32) -> String { subsonic::calls::album(&format!("al-{i:05}")).request.cache_key() }

fn stored_body(i: u32) -> String {
    let album = server_album(i, false).unwrap();
    let song = server_songs(&album, i, false);
    let mut body = Vec::new();
    subsonic::calls::album(&album.id).write_stored(&AlbumWithSongs { album, song }, &mut body).unwrap();
    String::from_utf8(body).unwrap()
}

fn cached_path(root: &Path, id: &str) -> PathBuf { root.join("cache").join(format!("{id}.flac")) }

/// The library before the delta: the server binding, a superseded and the
/// current catalog, downloaded albums, stored responses and cached songs.
fn generate(blocking: &Blocking, path: &Path, media: &Path) -> Result<(), String> {
    let mut db = Db::open(blocking, path).map_err(storage)?;
    db.bind_music_server(blocking, &server_id()).map_err(storage)?;
    db.observe_scan_marker(blocking, SCAN_BEFORE).map_err(storage)?;
    let before = pages(&listing(false));
    drop(publish(blocking, &mut db, &before, SCAN_BEFORE)?);
    drop(publish(blocking, &mut db, &before, SCAN_BEFORE)?);
    for i in (0..ALBUMS).step_by(DOWNLOADED as usize) {
        let album = server_album(i, false).unwrap();
        let songs = server_songs(&album, i, false);
        let set = OfflineSet { kind: "album".into(), id: album.id.clone(), name: album.name.clone(), artist: album.artist.clone(),
            cover_art: album.cover_art.clone(), song_ids: songs.iter().map(|s| s.id.clone()).collect() };
        let files: Vec<_> = songs.into_iter().map(|s| { let p = media.join("offline").join(format!("{}.flac", s.id)); let n = s.size.unwrap(); (s, p, n) }).collect();
        db.generated_downloaded_album(blocking, &set, &files).map_err(storage)?;
    }
    for i in (0..ALBUMS).step_by(STORED as usize) { db.put_response(blocking, &stored_key(i), &stored_body(i)); }
    for n in (0..ALBUMS).step_by(CACHED as usize) {
        let id = format!("so-{n:05}-01");
        db.put_cached_song(blocking, &id, "raw", &cached_path(media, &id), 1_000_000 + u64::from(n)).map_err(storage)?;
    }
    db.checkpoint(blocking).map_err(storage)
}

/// `SCRATCH/name`, empty. A directory already there is removed only when an
/// earlier run of this kind marked it as generated.
fn scratch(spec: &super::Spec, name: &str) -> Result<PathBuf, String> {
    let root = spec.scratch.join(name);
    let marker = root.join(".q2-generated-library-benchmark");
    if root.exists() {
        if !marker.is_file() { return Err(format!("{} is not a generated {name} scratch", root.display())); }
        std::fs::remove_dir_all(&root).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    std::fs::write(marker, b"generated library only\n").map_err(|e| e.to_string())?;
    Ok(root)
}

/// A fresh copy of a closed database, without a write-ahead log.
fn copy_database(from: &Path, to: &Path) -> Result<(), String> {
    for suffix in ["-wal", "-shm"] { let _ = std::fs::remove_file(format!("{}{suffix}", to.display())); }
    std::fs::copy(from, to).map(drop).map_err(|e| e.to_string())
}

fn median(samples: &[u64]) -> u64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

#[derive(Default)]
struct Steps(BTreeMap<&'static str, Vec<u64>>, Vec<u64>);
impl Steps {
    fn time<T>(&mut self, name: &'static str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let start = Instant::now();
        let value = f()?;
        self.0.entry(name).or_default().push(start.elapsed().as_micros() as u64);
        Ok(value)
    }
    /// Closes an iteration: its total is the sum of its steps.
    fn end_iteration(&mut self) {
        let done = self.1.len();
        self.1.push(self.0.values().map(|v| v.get(done).copied().unwrap_or(0)).sum());
    }
    fn report(&self, total: &str, mut out: serde_json::Map<String, Value>) -> Value {
        for (name, samples) in &self.0 { out.insert(format!("{name}_us"), median(samples).into()); }
        out.insert(total.into(), median(&self.1).into());
        out.insert("iteration_us".into(), json!(self.1));
        out.into()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Queue { songs: Vec<Song>, index: usize, position_ms: u64 }

pub(super) fn db(blocking: &Blocking, spec: &super::Spec) -> Result<Value, String> {
    let root = scratch(spec, "library-db")?;
    let path = root.join("library.db");
    generate(blocking, &path, &root)?;
    let bodies: Vec<(String, String)> = (0..8).map(|k| (stored_key(k * STORED * 7), stored_body(k * STORED * 7))).collect();
    let queue = {
        let songs = (0..40).flat_map(|i| { let a = server_album(i * 3, false).unwrap(); server_songs(&a, i * 3, false) }).take(200).collect();
        serde_json::to_string(&Queue { songs, index: 17, position_ms: 61_000 }).unwrap()
    };
    let album_call = subsonic::calls::album("");
    let (mut steps, mut md5) = (Steps::default(), Md5::new());
    let mut digest = |value: &dyn std::fmt::Debug| md5.update(format!("{value:?}\n").as_bytes());
    for iteration in 0..spec.iterations {
        let mut r = Lcg::new(1000 + iteration as u64);
        let mut db = steps.time("open", || Db::open(blocking, &path).map_err(storage))?;
        let bound = steps.time("bind", || db.bind_music_server(blocking, &server_id()).map_err(storage))?;
        if bound.0 || !bound.1.is_empty() { return Err("generated binding changed".into()); }
        let current = steps.time("catalog", || db.catalog_current(blocking).map_err(storage)?.ok_or_else(|| "no catalog".to_string()))?;
        let first = steps.time("first_page", || db.catalog_page(blocking, &current, 0, 99).map_err(storage))?;
        digest(&(current.len, current.groups, first.iter().map(|a| &a.id).collect::<Vec<_>>()));
        let browse: Vec<(u32, i32)> = (0..10).map(|_| (r.below(current.len), [-3, -1, 1, 3][r.below(4) as usize])).collect();
        let seen = steps.time("browse", || browse.iter().map(|&(at, delta)| {
            let page = db.catalog_page(blocking, &current, at, 100)?;
            let group = db.catalog_jump(blocking, &current, at, delta)?;
            Ok((page.first().map(|a| a.name.clone()), page.len(), group.map(|g| (g.position, g.label))))
        }).collect::<rusqlite::Result<Vec<_>>>().map_err(storage))?;
        digest(&seen);
        let centre = r.below(current.len - 13);
        let positions: Vec<u32> = (0..13).map(|k| centre + k).collect();
        let shown = steps.time("cover_flow", || db.catalog_positions(blocking, &current, &positions).map_err(storage))?;
        digest(&shown.iter().map(|a| &a.cover_art).collect::<Vec<_>>());
        let keys: Vec<String> = (0..40).map(|_| stored_key(r.below(ALBUMS / STORED) * STORED)).collect();
        let stored = steps.time("responses", || keys.iter().map(|key| {
            let (body, _) = db.response(blocking, key).ok_or("stored response missing")?;
            let mut parser = album_call.stored_parser();
            parser.feed(body.as_bytes()).map_err(|e| e.to_string())?;
            let album = parser.finish().map_err(|e| e.to_string())?;
            Ok((album.album.id, album.song.len(), album.song.last().map(|s| s.title.clone())))
        }).collect::<Result<Vec<_>, String>>())?;
        digest(&stored);
        steps.time("response_writes", || { for (key, body) in &bodies { db.put_response(blocking, key, body); } Ok(()) })?;
        let songs: Vec<String> = (0..40).map(|_| format!("so-{:05}-01", r.below(ALBUMS / 4) * 4)).collect();
        let cached = steps.time("song_cache", || {
            let found: Vec<_> = songs.iter().map(|id| db.cached_song(blocking, id, &["raw"]).map(|(v, p)| (v, p.file_name().map(|n| n.to_owned())))).collect();
            for n in 0..8u32 {
                let id = format!("so-{:05}-01", n * CACHED * 5);
                db.put_cached_song(blocking, &id, "raw", &cached_path(&root, &id), 1_000_000 + u64::from(n * CACHED * 5)).map_err(storage)?;
            }
            Ok(found)
        })?;
        digest(&cached);
        let offline: Vec<String> = (0..40).map(|_| format!("so-{:05}-01", r.below(ALBUMS / 8) * 8)).collect();
        let downloads = steps.time("downloads", || {
            let found = offline.iter().filter(|id| db.offline_song(blocking, id).is_some()).count();
            let (mut after, mut total) = (String::new(), 0);
            loop {
                let page = db.download_page(blocking, &after).map_err(storage)?;
                let Some(last) = page.last() else { break };
                after = last.song.id.clone();
                total += page.len();
            }
            Ok((found, total, after))
        })?;
        digest(&downloads);
        let restored = steps.time("queue", || {
            db.set_checked(blocking, "queue", &queue).map_err(storage)?;
            let text = db.get(blocking, "queue").ok_or("saved queue missing")?;
            let saved: Queue = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            Ok((saved.songs.len(), saved.index, saved.position_ms))
        })?;
        digest(&restored);
        steps.end_iteration();
    }
    let checksum = format!("{:x}", md5.finalize());
    let mut out = serde_json::Map::new();
    out.insert("albums".into(), ALBUMS.into());
    out.insert("iterations".into(), spec.iterations.into());
    out.insert("checksum".into(), checksum.into());
    out.insert("scope".into(), "generated library; real Db reads and writes; no UI, no network".into());
    Ok(steps.report("timed_us", out))
}

pub(super) fn sync(blocking: &Blocking, spec: &super::Spec) -> Result<Value, String> {
    let root = scratch(spec, "library-sync")?;
    let (base, work) = (root.join("before.db"), root.join("library.db"));
    generate(blocking, &base, &root)?;
    // The server after the delta, as documents: the alphabetical pages and
    // one getSong response per song it still has.
    let after = listing(true);
    let catalog_pages = pages(&after);
    let song_call = subsonic::calls::song("");
    let server: HashMap<String, Vec<u8>> = (0..ALBUMS + ADDED)
        .filter_map(|i| server_album(i, true).map(|a| (i, a)))
        .flat_map(|(i, a)| server_songs(&a, i, true))
        .map(|s| (s.id.clone(), document("song", serde_json::to_value(&s).unwrap()))).collect();
    let mut steps = Steps::default();
    let mut checksum: Option<String> = None;
    let mut counts = (0, 0, 0);
    for _ in 0..spec.iterations {
        copy_database(&base, &work)?;
        let mut db = Db::open(blocking, &work).map_err(storage)?;
        steps.time("marker", || db.observe_scan_marker(blocking, SCAN_AFTER).map_err(storage))?;
        let published = steps.time("catalog", || publish(blocking, &mut db, &catalog_pages, SCAN_AFTER))?;
        let (missing, upgrades) = steps.time("review", || {
            let (mut after, mut missing, mut upgrades) = (String::new(), Vec::<MissingSong>::new(), 0);
            loop {
                let page = db.download_page(blocking, &after).map_err(storage)?;
                let Some(last) = page.last() else { break };
                after = last.song.id.clone();
                for local in page {
                    let Some(text) = server.get(&local.song.id) else { missing.push(local); continue };
                    let new = song_call.parse_text(text).map_err(|e| e.to_string())?;
                    if media_changed(&local.song, &new) && db.queue_upgrade(blocking, &local, &new).map_err(storage)? { upgrades += 1; }
                }
            }
            Ok((missing, upgrades))
        })?;
        let (removed, files) = steps.time("remove", || db.remove_confirmed_music(blocking, &missing).map_err(storage))?;
        steps.end_iteration();
        let digest = db.generated_sync_digest(blocking, &root).map_err(storage)?;
        if checksum.as_ref().is_some_and(|c| *c != digest) { return Err("generated sync differs between iterations".into()); }
        checksum = Some(digest);
        counts = (published.len, upgrades, removed);
        if files.len() < removed { return Err("removed music without its files".into()); }
    }
    let mut out = serde_json::Map::new();
    out.insert("albums_after".into(), counts.0.into());
    out.insert("upgrades".into(), counts.1.into());
    out.insert("removed".into(), counts.2.into());
    out.insert("iterations".into(), spec.iterations.into());
    out.insert("checksum".into(), checksum.into());
    out.insert("scope".into(), "generated server delta; real Db catalog refresh and Full Sync steps; server documents parsed; no network".into());
    Ok(steps.report("timed_us", out))
}

pub(super) fn startup(blocking: &Blocking, spec: &super::Spec) -> Result<Value, String> {
    use crate::audio::{alsa::{SoftHw, FAKE}, player};
    use crate::config::{Config, Paths};
    let root = scratch(spec, "app-startup")?;
    let paths = Paths::new(root.join("data"));
    paths.create_all(blocking).map_err(|e| e.to_string())?;
    let mut config = Config::default();
    (config.server.url, config.server.user, config.server.password) = (SERVER_URL.into(), SERVER_USER.into(), "generated-only".into());
    config.display.clock_in_title = false;
    config.display.night_warmth = false;
    config.display.backlight_seconds = 0;
    config.display.clicker = false;
    config.power.sleep_minutes = 0;
    config.save(blocking, &paths.config()).map_err(|e| e.to_string())?;
    generate(blocking, &paths.database(), &paths.root)?;
    let ui = crate::rt::Ui::new().map_err(|e| e.to_string())?;
    let mut steps = Steps::default();
    let (mut checksum, mut first_page): (Option<String>, Option<String>) = (None, None);
    // One start-up first: it writes the configuration snapshot and the
    // library's first files, which later start-ups find in place.
    for iteration in 0..spec.iterations + 1 {
        let mut timed = Steps::default();
        let config = timed.time("config", || Ok(paths.load_config(blocking)))?;
        let lib = timed.time("library", || {
            let lib = crate::library::Library::open(paths.clone(), &config, Arc::new(|_| {}));
            lib.set_offline_mode(true);
            Ok(lib)
        })?;
        let albums = timed.time("catalog", || ui.block_on(async {
            let catalog = lib.current_album_catalog().await.map_err(|e| format!("{e:?}"))?.ok_or("no stored catalog")?;
            lib.album_catalog_page(catalog, 0, 99).await.map_err(|e| format!("{e:?}"))
        }))?;
        let fonts = timed.time("fonts", || crate::gfx::font::FontLoad::start(Vec::new()).finish())?;
        let player = player::spawn(Arc::new(crate::library::Tracks(lib.clone())), Box::new(SoftHw(FAKE.into())),
            player::Settings { eq: config.playback.eq.clone(), volume: config.playback.volume, headphone_power: config.playback.headphone_power(),
                filter: config.playback.dac_filter, replay_gain: config.playback.replay_gain, gapless: config.playback.gapless },
            Arc::new(|| {}));
        let mut app = timed.time("app", || {
            let net = crate::net::Net::new(root.join("generated-wifi.sh"), true, Arc::new(|| {}));
            let tailscale = crate::tailscale::Tailscale::new_notifying(root.clone(), root.join("tailscale"), true, false, Arc::new(|| {}));
            let (tx, _) = crate::ui::channel();
            let mut app = crate::ui::App::new(lib.clone(), player.clone(), config, paths.clone(), net, tailscale, fonts, tx);
            ui.block_on(app.restore_queue());
            Ok(app)
        })?;
        let frame = timed.time("first_frame", || {
            let mut md5 = Md5::new();
            for p in &app.render().pixels { md5.update(p.to_le_bytes()); }
            Ok(format!("{:x}", md5.finalize()))
        })?;
        app.presentation_completed();
        player.shutdown();
        drop(app);
        drop(lib);
        let page = format!("{:x}", Md5::digest(albums.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join("\n").as_bytes()));
        if checksum.as_ref().is_some_and(|c| *c != frame) || first_page.as_ref().is_some_and(|p| *p != page) {
            return Err("generated start-up differs between iterations".into());
        }
        (checksum, first_page) = (Some(frame), Some(page));
        if iteration > 0 {
            for (name, samples) in timed.0 { steps.0.entry(name).or_default().extend(samples); }
            steps.end_iteration();
        }
    }
    let mut out = serde_json::Map::new();
    out.insert("iterations".into(), spec.iterations.into());
    out.insert("checksum".into(), checksum.into());
    out.insert("first_page_md5".into(), first_page.into());
    out.insert("scope".into(), "generated configuration and library; real config, Library, catalog, fonts, App and first frame in one process; Offline Mode; no device output".into());
    Ok(steps.report("timed_us", out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_changes_what_it_names() {
        let (before, after) = (listing(false), listing(true));
        assert_eq!(before.len() as u32, ALBUMS);
        let removed = (0..ALBUMS).filter(|i| i % 37 == 5).count();
        assert_eq!(after.len(), ALBUMS as usize - removed + ADDED as usize);
        let renamed = server_album(7, true).unwrap();
        assert_ne!(renamed.name, server_album(7, false).unwrap().name);
        let album = server_album(400, false).unwrap();
        let (old, new) = (server_songs(&album, 400, false), server_songs(&album, 400, true));
        assert!(old.iter().zip(&new).any(|(a, b)| media_changed(a, b)));
        assert!(old.iter().zip(&new).all(|(a, b)| a.id == b.id && a.title == b.title));
    }

    fn spec(kind: &str, scratch: &Path) -> super::super::Spec {
        super::super::Spec { kind: kind.into(), scratch: scratch.to_owned(), iterations: 2, ..Default::default() }
    }

    #[test]
    fn scratch_keeps_a_directory_it_did_not_generate() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/library-bench-foreign");
        let foreign = base.join("library-db");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("kept"), b"not generated").unwrap();
        assert!(scratch(&spec("library-db", &base), "library-db").is_err());
        assert!(foreign.join("kept").is_file());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn library_kinds_repeat_their_checksums() {
        let blocking = Blocking::dedicated_thread();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/library-bench");
        for (kind, run) in [("library-db", db as fn(&Blocking, &super::super::Spec) -> Result<Value, String>), ("library-sync", sync)] {
            let a = run(&blocking, &spec(kind, &base.join(format!("{kind}-a")))).unwrap();
            let b = run(&blocking, &spec(kind, &base.join(format!("{kind}-b")))).unwrap();
            assert_eq!(a["checksum"], b["checksum"], "{kind}");
            assert_eq!(a["iteration_us"].as_array().unwrap().len(), 2);
        }
        let synced = sync(&blocking, &spec("library-sync", &base.join("counts"))).unwrap();
        assert_eq!(synced["albums_after"], ALBUMS - (0..ALBUMS).filter(|i| i % 37 == 5).count() as u32 + ADDED);
        assert!(synced["upgrades"].as_u64().unwrap() > 20 && synced["removed"].as_u64().unwrap() > 40, "{synced}");
        let _ = std::fs::remove_dir_all(base);
    }
}
