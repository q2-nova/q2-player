//! The album catalog against a model Navidrome: scan markers, requests and
//! rows for an unchanged library, incremental changes, a scan during a build,
//! and generated mutation sequences.
use super::*;
use crate::library::Policy;
use crate::{config::{Config, Paths}, rt::Blocking};
use std::{io::{BufRead, BufReader, Write}, sync::{Mutex, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

type Hook = Box<dyn FnMut(&mut Model, u32) + Send>;

/// A library as Navidrome serves it: albums in name order, a scan marker
/// that moves with every change, and a hook run after each album page.
#[derive(Default)]
pub(crate) struct Model {
    pub albums: Vec<(String, String)>,
    pub scan: u64,
    pub scanning: bool,
    songs: std::collections::HashMap<String, subsonic::Song>,
    song_failure: bool,
    short_audio: bool,
    /// `method` of every request, in order.
    pub requests: Vec<String>,
    pub bytes: u64,
    pub after_page: Option<Hook>,
}
impl Model {
    pub fn with(count: usize) -> Self {
        Self { albums: (0..count).map(|n| (format!("al-{n:06}"), format!("Album {n:06}"))).collect(), scan: 1, ..Self::default() }
    }
    fn sorted(&self) -> Vec<(String, String)> {
        let mut albums = self.albums.clone();
        albums.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
        albums
    }
    pub fn add(&mut self, id: &str, name: &str) { self.albums.push((id.into(), name.into())); self.scan += 1; }
    pub fn remove(&mut self, index: usize) { self.albums.remove(index); self.scan += 1; }
    pub fn rename(&mut self, index: usize, name: &str) { self.albums[index].1 = name.into(); self.scan += 1; }
    /// The catalog the server holds: ids in name order.
    pub fn expected(&self) -> Vec<(String, String)> { self.sorted() }
}

pub(crate) struct ModelServer {
    pub url: String,
    pub model: Arc<Mutex<Model>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for ModelServer {
    fn drop(&mut self) { self.stop.store(true, Ordering::SeqCst); self.thread.take().unwrap().join().unwrap(); }
}
fn query<'a>(target: &'a str, key: &str) -> Option<&'a str> {
    target.split_once('?')?.1.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
}
impl ModelServer {
    pub fn new(model: Model) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let model = Arc::new(Mutex::new(model));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, done) = (model.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(1)); continue; };
                socket.set_nonblocking(false).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() { continue; }
                let target = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                loop { let mut header = String::new(); if reader.read_line(&mut header).is_err() || header == "\r\n" || header.is_empty() { break; } }
                let method = target.trim_start_matches("/rest/").split('?').next().unwrap_or("").to_owned();
                let mut model = shared.lock().unwrap();
                model.requests.push(method.clone());
                let inner = match method.as_str() {
                    "getScanStatus" => serde_json::json!({"status":"ok","scanStatus":{"scanning":model.scanning,
                        "count":model.albums.len() * 10,"folderCount":model.albums.len(),
                        "lastScan":format!("2026-11-22T00:00:00.{:09}Z", model.scan)}}),
                    "getAlbumList2" => {
                        let offset: usize = query(&target, "offset").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let size: usize = query(&target, "size").and_then(|v| v.parse().ok()).unwrap_or(10);
                        let albums: Vec<_> = model.sorted().into_iter().skip(offset).take(size)
                            .map(|(id, name)| serde_json::json!({"id":id,"name":name,"artist":"Generated"})).collect();
                        serde_json::json!({"status":"ok","albumList2":{"album":albums}})
                    }
                    "getSong" if model.song_failure => serde_json::json!({"status":"failed","error":{"code":40,"message":"Unauthorized"}}),
                    "getSong" => match query(&target, "id").and_then(|id|model.songs.get(id)) {
                        Some(song) => serde_json::json!({"status":"ok","song":song}),
                        None => serde_json::json!({"status":"failed","error":{"code":70,"message":"Missing"}}),
                    },
                    "stream" => {
                        let body = if model.short_audio { "short" } else { "new-file" };
                        drop(model);
                        let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        continue;
                    }
                    "getAlbum" => {
                        let id = query(&target, "id").unwrap_or("");
                        let name = model.albums.iter().find(|a| a.0 == id).map(|a| a.1.clone()).unwrap_or_default();
                        serde_json::json!({"status":"ok","album":{"id":id,"name":name,"song":[{"id":format!("{id}-song"),"title":name}]}})
                    }
                    "ping" | "star" | "unstar" | "setRating" | "scrobble" => serde_json::json!({"status":"ok"}),
                    _ => serde_json::json!({"status":"failed","error":{"code":0,"message":"Generated: not modelled"}}),
                };
                let body = serde_json::json!({"subsonic-response":inner}).to_string();
                model.bytes += body.len() as u64;
                if method == "getAlbumList2" {
                    let offset = query(&target, "offset").and_then(|v| v.parse().ok()).unwrap_or(0);
                    if let Some(mut hook) = model.after_page.take() { hook(&mut model, offset); model.after_page = Some(hook); }
                }
                drop(model);
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self { url, model, stop, thread: Some(thread) }
    }
    pub fn requests(&self) -> Vec<String> { std::mem::take(&mut self.model.lock().unwrap().requests) }
}

pub(crate) fn library(server: &ModelServer, label: &str) -> Arc<Library> {
    let mut config = Config::default(); config.server.url = server.url.clone();
    config.server.user = "generated".into(); config.server.password = "synthetic-only".into();
    let (lib, _) = Library::create(Paths::new(crate::library::generated_test_directory(label)), &config, Arc::new(|_| {}));
    lib.db().hold_checkpoints();
    lib
}
fn wait<T>(future: impl Future<Output=T>) -> T {
    crate::rt::Ui::new().unwrap().block_on(async { tokio::time::timeout(crate::library::TEST_LIVENESS, future).await.expect("bounded catalog operation") })
}
/// A new browsing session: the five-minute reuse has lapsed.
fn expire(lib: &Arc<Library>) {
    wait(async { *lib.catalog_refresh.lock().await = Some((lib.client_generation(), Instant::now() - Duration::from_secs(301))); });
}
fn browse(lib: &Arc<Library>) -> Catalog { wait(lib.browse_album_catalog(crate::ui::list::index_letter)).unwrap() }
fn stored(lib: &Arc<Library>, catalog: &Catalog) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < catalog.len() {
        let page = wait(lib.album_catalog_page(catalog.clone(), start, 500)).unwrap();
        start += page.len() as u32;
        out.extend(page.into_iter().map(|a| (a.id, a.name)));
    }
    out
}
fn writes(lib: &Arc<Library>) -> (u64, u64) { let db = lib.db(); (db.total_changes(), db.wal_bytes()) }

#[test]
fn unchanged_library_browse_budget_is_one_marker_request_and_no_rows() {
    let server = ModelServer::new(Model::with(1_200));
    let lib = library(&server, "catalog-budget");
    let first = browse(&lib);
    assert_eq!(stored(&lib, &first), server.model.lock().unwrap().expected());
    assert_eq!(server.requests(), ["getScanStatus", "getAlbumList2", "getAlbumList2", "getAlbumList2", "getScanStatus"]);
    for _ in 0..3 {
        expire(&lib);
        let before = writes(&lib);
        let again = browse(&lib);
        assert_eq!(again.len(), 1_200);
        assert_eq!(server.requests(), ["getScanStatus"], "an unchanged library is one marker request");
        let after = writes(&lib);
        assert_eq!(after.0 - before.0, 0, "no catalog row is written");
        assert_eq!(after.1 - before.1, 0, "no database page is written");
    }
}

#[test]
fn one_album_added_removed_or_renamed_rebuilds_to_the_server_state() {
    let server = ModelServer::new(Model::with(700));
    let lib = library(&server, "catalog-incremental");
    browse(&lib);
    server.requests();
    let changes: [fn(&mut Model); 3] = [
        |m| m.add("al-new", "Aardvark Added"),
        |m| m.remove(3),
        |m| m.rename(10, "Zz Renamed"),
    ];
    for change in changes {
        change(&mut server.model.lock().unwrap());
        expire(&lib);
        let catalog = browse(&lib);
        assert_eq!(stored(&lib, &catalog), server.model.lock().unwrap().expected());
        assert_eq!(server.requests(), ["getScanStatus", "getAlbumList2", "getAlbumList2", "getScanStatus"]);
    }
}

#[test]
fn a_scan_that_removes_an_album_between_pages_never_publishes_a_shifted_catalog() {
    let mut model = Model::with(1_005);
    // After the first page: the first album is removed. Every later album
    // moves back one place, so offset 500 skips the album that was 500th.
    let mut fired = false;
    model.after_page = Some(Box::new(move |m, offset| if offset == 0 && !fired { fired = true; m.remove(0); }));
    let server = ModelServer::new(model);
    let lib = library(&server, "catalog-shift");
    let catalog = browse(&lib);
    assert_eq!(stored(&lib, &catalog), server.model.lock().unwrap().expected(), "the catalog equals the server after the scan");
    assert_eq!(server.requests().iter().filter(|m| *m == "getScanStatus").count(), 3, "before, after, and after the rebuild");
    // The rebuilt catalog carries the marker: the next session fetches nothing more.
    expire(&lib);
    browse(&lib);
    assert_eq!(server.requests(), ["getScanStatus"]);
}

#[test]
fn a_scan_that_moves_an_album_to_the_last_page_retries_a_duplicate_only_page() {
    let mut model = Model::with(1_001);
    // The first page includes album zero. Moving it to the tail puts only
    // that already seen ID at offset 1,000 of the first build.
    let mut fired = false;
    model.after_page = Some(Box::new(move |m, offset| {
        if offset == 0 && !fired {
            fired = true;
            m.rename(0, "Zz Renamed");
        }
    }));
    let server = ModelServer::new(model);
    let lib = library(&server, "catalog-duplicate-tail");
    let catalog = browse(&lib);
    assert_eq!(stored(&lib, &catalog), server.model.lock().unwrap().expected());
    assert_eq!(server.requests(), [
        "getScanStatus", "getAlbumList2", "getAlbumList2", "getAlbumList2",
        "getScanStatus", "getAlbumList2", "getAlbumList2", "getAlbumList2", "getScanStatus",
    ]);
}

#[test]
fn a_duplicate_only_last_page_under_the_same_marker_is_rejected() {
    let mut model = Model::with(500);
    model.add("al-000000", "Zz Duplicate");
    let server = ModelServer::new(model);
    let lib = library(&server, "catalog-stable-duplicate-tail");
    assert!(wait(lib.browse_album_catalog(crate::ui::list::index_letter)).is_err());
    assert!(wait(lib.current_album_catalog()).unwrap().is_none());
    assert_eq!(server.requests(), [
        "getScanStatus", "getAlbumList2", "getAlbumList2", "getScanStatus",
    ]);
}

#[test]
fn a_scan_under_way_publishes_unmarked_so_the_next_browse_builds_again() {
    let mut model = Model::with(40);
    model.scanning = true;
    let server = ModelServer::new(model);
    let lib = library(&server, "catalog-scanning");
    browse(&lib);
    server.requests();
    server.model.lock().unwrap().scanning = false;
    expire(&lib);
    browse(&lib);
    assert_eq!(server.requests(), ["getScanStatus", "getAlbumList2", "getScanStatus"], "no marker was trusted while scanning");
    expire(&lib);
    browse(&lib);
    assert_eq!(server.requests(), ["getScanStatus"]);
}

#[test]
fn a_scan_makes_stored_album_pages_stale_for_prefer_cache() {
    let server = ModelServer::new(Model::with(3));
    let lib = library(&server, "catalog-album-fresh");
    browse(&lib);
    let call = subsonic::calls::album("al-000001");
    let album = || wait(lib.clone().fetch_async(call.clone(), Policy::PreferCache)).unwrap().value;
    // As if the first marker had been seen long ago.
    lib.db().set(&Blocking::dedicated_thread(), "music-stale-before", "1");
    assert_eq!(album().album.name, "Album 000001");
    server.requests();
    assert_eq!(album().album.name, "Album 000001");
    assert!(server.requests().is_empty(), "a fresh stored album opens locally");
    server.model.lock().unwrap().rename(1, "Album Renamed");
    expire(&lib);
    browse(&lib);
    server.requests();
    assert_eq!(album().album.name, "Album Renamed", "after the scan the album is fetched again");
    assert_eq!(server.requests(), ["getAlbum"]);
}

#[test]
fn an_acknowledged_heart_makes_stored_album_pages_stale() {
    let server = ModelServer::new(Model::with(3));
    let lib = library(&server, "catalog-heart-fresh");
    lib.set_online(true);
    lib.db().set(&Blocking::dedicated_thread(), "music-stale-before", "1");
    let call = subsonic::calls::album("al-000002");
    let album = || wait(lib.clone().fetch_async(call.clone(), Policy::PreferCache)).unwrap();
    album();
    album();
    assert_eq!(server.requests(), ["getAlbum"]);
    wait(lib.star(subsonic::ItemKind::Song, "al-000002-song", true)).unwrap();
    wait(lib.sync_round(false));
    assert_eq!(server.requests(), ["star"]);
    album();
    assert_eq!(server.requests(), ["getAlbum"], "the page stored before the heart is fetched again");
}

#[test]
fn the_scan_marker_and_catalog_belong_to_the_account() {
    let server = ModelServer::new(Model::with(5));
    let lib = library(&server, "catalog-account");
    browse(&lib);
    assert!(lib.db().get(&Blocking::dedicated_thread(), "music-scan-marker").is_some());
    let mut config = Config::default(); config.server.url = server.url.clone();
    config.server.user = "generated-other".into(); config.server.password = "synthetic-only".into();
    wait(lib.configure_async(&config)).unwrap();
    // Wait for the new account's binding to clear the old account's data.
    let end = Instant::now() + crate::library::TEST_LIVENESS;
    while lib.db().get(&Blocking::dedicated_thread(), "music-scan-marker").is_some() && Instant::now() < end { std::thread::sleep(Duration::from_millis(5)); }
    assert!(lib.db().get(&Blocking::dedicated_thread(), "music-scan-marker").is_none());
    server.requests();
    let catalog = browse(&lib);
    assert_eq!(catalog.len(), 5);
    assert_eq!(server.requests(), ["getScanStatus", "getAlbumList2", "getScanStatus"], "the old account's marker is not trusted");
}

/// Heap bound for a large library: a catalog build parses each 500-album
/// page as it arrives and stores it before the next, so its Rust heap peak
/// is one page whatever the library holds. 20,000 albums is about 200,000
/// songs at ten a album; the owner's library is 60,130 songs. SQLite's own
/// cache (C allocations, not counted here) is capped at 512 KiB.
#[test]
fn catalog_build_heap_is_one_page_whatever_the_library_size() {
    let b = Blocking::dedicated_thread();
    let call = subsonic::calls::album_list(subsonic::AlbumListType::AlphabeticalByName, 500, 0);
    let page = |offset: u32, count: u32| serde_json::json!({"subsonic-response":{"status":"ok","albumList2":{"album":(offset..offset + count).map(|n| serde_json::json!({
        "id":format!("al-{n:08}"),"name":format!("Generated Album {n:08}"),"artist":format!("Generated Artist {}", n / 7),
        "artistId":format!("ar-{}", n / 7),"coverArt":format!("al-{n:08}"),"songCount":10,"duration":2400,"year":2026,"genre":"Generated"})).collect::<Vec<_>>()}}}).to_string();
    let peak = |albums: u32| {
        let dir = crate::library::generated_test_directory("catalog-heap");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = crate::library::db::Db::open(&b, &dir.join("library.db")).unwrap();
        let ((), heap) = crate::test_alloc::measure(|| {
            let build = db.catalog_begin(&b).unwrap();
            let mut offset = 0;
            loop {
                let count = (albums - offset).min(500);
                let body = page(offset, count);
                let parsed = call.parse_text(body.as_bytes()).unwrap();
                drop(body);
                let rows: Vec<_> = parsed.album.into_iter().map(|a| { let label = crate::ui::list::index_letter(&a.name); (a, label) }).collect();
                offset = db.catalog_append(&b, &build, offset, &rows).unwrap();
                if count < 500 { break; }
            }
            assert_eq!(db.catalog_publish(&b, &build).unwrap().len, albums);
        });
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
        heap.peak
    };
    let small = peak(2_000);
    let large = peak(20_000);
    eprintln!("catalog build heap peak: 2,000 albums {} KiB, 20,000 albums {} KiB", small / 1024, large / 1024);
    assert!(large < 2 << 20, "20,000 albums peaked at {large} bytes");
    assert!(large <= small + small / 4, "the peak grows with the library: {small} -> {large}");
}

mod generated {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum Change { Add(u16), Remove(u16), Rename(u16, u16) }
    fn apply(model: &mut Model, change: &Change, serial: &mut u32) {
        *serial += 1;
        match *change {
            Change::Add(name) => model.add(&format!("gen-{serial}"), &format!("Gen {name:05}")),
            Change::Remove(at) if !model.albums.is_empty() => { let n = model.albums.len(); model.remove(at as usize % n); }
            Change::Rename(at, name) if !model.albums.is_empty() => { let n = model.albums.len(); model.rename(at as usize % n, &format!("Ren {name:05}")); }
            _ => {}
        }
    }
    fn change() -> impl Strategy<Value = Change> {
        prop_oneof![any::<u16>().prop_map(Change::Add), any::<u16>().prop_map(Change::Remove), (any::<u16>(), any::<u16>()).prop_map(|(a, b)| Change::Rename(a, b))]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 12, failure_persistence: None, ..ProptestConfig::default() })]
        /// Each step changes the library, possibly while a build is between
        /// pages (at most once per browse), then browses: the catalog equals
        /// the server's albums after every browse.
        #[test]
        fn generated_mutations_leave_the_catalog_equal_to_the_server(
            start in 0usize..1_100,
            steps in proptest::collection::vec((proptest::collection::vec(change(), 0..4), proptest::option::of(change())), 1..5),
        ) {
            let server = ModelServer::new(Model::with(start));
            let lib = library(&server, "catalog-generated");
            let mut serial = 0u32;
            for (step, (before, during)) in steps.into_iter().enumerate() {
                {
                    let mut model = server.model.lock().unwrap();
                    for change in &before { apply(&mut model, change, &mut serial); }
                    if let Some(change) = during {
                        let mut pending = Some(change);
                        let mut serial = 1_000_000 + step as u32;
                        model.after_page = Some(Box::new(move |m, _| if let Some(change) = pending.take() { apply(m, &change, &mut serial); }));
                    }
                }
                expire(&lib);
                let catalog = browse(&lib);
                let mut model = server.model.lock().unwrap();
                model.after_page = None;
                prop_assert_eq!(stored(&lib, &catalog), model.expected());
            }
        }
    }
}

#[test]
fn full_sync_upgrades_without_losing_old_audio_and_confirms_only_missing_ids() {
    use crate::library::db::OfflineSet;
    let server = ModelServer::new(Model::with(3));
    let lib = library(&server, "full-sync-downloads");
    let b = Blocking::dedicated_thread();
    let ids = ["stable", "removed", "restored"];
    let old: Vec<_> = ids
        .iter()
        .map(|id| subsonic::Song {
            id: (*id).into(),
            title: "Same Title".into(),
            suffix: Some("mp3".into()),
            size: Some(3),
            ..Default::default()
        })
        .collect();
    let new = subsonic::Song {
        suffix: Some("flac".into()),
        size: Some(8),
        ..old[0].clone()
    };
    let path = |id: &str| lib.paths.offline().join(format!("{id}.mp3"));
    {
        let db = lib.db();
        db.put_offline_set(
            &b,
            &OfflineSet {
                kind: "playlist".into(),
                id: "mix".into(),
                name: "Mix".into(),
                artist: String::new(),
                cover_art: None,
                song_ids: ids.iter().map(|id| (*id).into()).collect(),
            },
        );
        for song in &old {
            std::fs::write(path(&song.id), b"old").unwrap();
            db.put_offline_song(&b, song, &path(&song.id), 3);
        }
    }
    server
        .model
        .lock()
        .unwrap()
        .songs
        .insert(new.id.clone(), new.clone());
    let cancelled = AtomicBool::new(false);
    let review = Arc::new(
        wait(lib.review_downloads(crate::ui::list::index_letter, &cancelled, |_| {})).unwrap(),
    );
    assert_eq!(review.upgrades, 1);
    assert_eq!(
        review
            .missing
            .iter()
            .map(|s| s.song.id.as_str())
            .collect::<Vec<_>>(),
        ["removed", "restored"]
    );
    assert_eq!(std::fs::read(path("stable")).unwrap(), b"old");
    let ticket = lib.db().download_ticket(&b, "stable").unwrap().unwrap();
    assert!(lib.db().upgrade_ticket(&b, ticket).unwrap());
    assert!(
        !lib.db()
            .observed_album(&b, std::slice::from_ref(&new))
            .unwrap(),
        "same upgrade in flight"
    );
    let cache = lib.paths.offline().join("cached-version.mp3");
    std::fs::write(&cache, b"old").unwrap();
    lib.db()
        .put_cached_song(&b, "stable", "raw", &cache, 3)
        .unwrap();
    // A self-consistent HTTP response shorter than the expected new original
    // must not evict either usable copy.
    server.model.lock().unwrap().short_audio = true;
    lib.mark_online_for_test(true);
    crate::library::downloads::start(&lib);
    lib.downloads.wake();
    let end = Instant::now() + crate::library::TEST_LIVENESS;
    while lib.db().download_counts(&b) != (0, 1) {
        assert!(Instant::now() < end, "short upgrade was not rejected");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(std::fs::read(path("stable")).unwrap(), b"old");
    assert!(cache.exists());
    server.model.lock().unwrap().short_audio = false;
    lib.db().retry_failed_downloads(&b);
    lib.downloads.wake();
    let end = Instant::now() + crate::library::TEST_LIVENESS;
    while lib.db().download_counts(&b) != (0, 0) {
        assert!(Instant::now() < end, "upgrade worker did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    let replacement = lib.db().offline_song(&b, "stable").unwrap();
    assert_ne!(replacement, path("stable"));
    assert_eq!(std::fs::read(replacement).unwrap(), b"new-file");

    assert!(lib.db().cached_song(&b, "stable", &["raw"]).is_none());
    server.model.lock().unwrap().song_failure = true;
    assert!(wait(lib.confirm_missing(review.clone())).is_err());
    assert!(path("removed").exists());
    server.model.lock().unwrap().song_failure = false;
    server.model.lock().unwrap().scanning = true;
    assert!(wait(lib.confirm_missing(review.clone())).is_err());
    assert!(path("removed").exists());
    server.model.lock().unwrap().scanning = false;
    server
        .model
        .lock()
        .unwrap()
        .songs
        .insert(old[2].id.clone(), old[2].clone());
    assert_eq!(wait(lib.confirm_missing(review.clone())).unwrap(), 1);
    assert!(!path("removed").exists());
    assert!(path("restored").exists());
    assert_eq!(
        lib.db()
            .offline_set(&b, "playlist", "mix")
            .unwrap()
            .song_ids,
        ["stable", "restored"]
    );
    assert_eq!(
        wait(lib.confirm_missing(review.clone())).unwrap(),
        0,
        "idempotent after completion"
    );
    // Reusing the same song ID after an account transition cannot authorize
    // yesterday's reviewed deletion against the newly bound card library.
    let mut other = Config::default();
    other.server.url = server.url.clone();
    other.server.user = "second-generated-account".into();
    other.server.password = "synthetic-only".into();
    wait(lib.configure_async(&other)).unwrap();
    std::fs::write(path("removed"), b"another account").unwrap();
    lib.db().put_offline_song(&b, &old[1], &path("removed"), 15);
    assert!(wait(lib.confirm_missing(review)).is_err());
    assert_eq!(std::fs::read(path("removed")).unwrap(), b"another account");

}
