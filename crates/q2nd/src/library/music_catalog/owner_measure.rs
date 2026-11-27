//! Host measurement of both library syncs through `tests/perf/sync_host_measure.py`,
//! a local proxy that holds the real server's credentials and counts requests
//! and bytes. Ignored: the tool runs it. Prints numbers only (no names, no
//! ids); the card database it fills is deleted at the end.
use crate::config::{Config, Paths};
use crate::library::{Library, Policy, shelf_browse::{Media, SyncRound}};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn phase(base: &str, name: &str) {
    let address = base.trim_start_matches("http://");
    let mut socket = std::net::TcpStream::connect(address).expect("the measurement proxy");
    let _ = write!(socket, "GET /__phase/{name} HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\n\r\n");
    let mut sink = Vec::new();
    let _ = socket.read_to_end(&mut sink);
}
fn wait<T>(future: impl std::future::Future<Output = T>) -> T {
    crate::rt::Ui::new().unwrap().block_on(async { tokio::time::timeout(Duration::from_secs(3600), future).await.expect("bounded sync") })
}
/// Bytes written to the card's database files (the WAL never shrinks while
/// checkpoints are held; the main file grows only at checkpoints).
fn written(lib: &Library) -> (u64, u64) {
    let db = lib.db();
    (db.total_changes(), db.wal_bytes())
}
fn report(name: &str, started: Instant, before: (u64, u64), lib: &Library, extra: &str) {
    let after = written(lib);
    println!("sync-measure {name} ms={} rows={} wal_bytes={}{extra}", started.elapsed().as_millis(), after.0 - before.0, after.1 - before.1);
}

#[test]
#[ignore = "run by tests/perf/sync_host_measure.py"]
fn measure_sync_through_proxy() {
    let Ok(base) = std::env::var("Q2ND_SYNC_PROXY") else { return };
    let abs = std::env::var("Q2ND_SYNC_ABS").is_ok_and(|v| v == "1");
    let root = crate::library::generated_test_directory("sync-measure");
    let mut config = Config::default();
    config.server.url = base.clone();
    config.server.user = "proxy".into();
    config.server.password = "proxy".into();
    if abs {
        config.audiobookshelf.url = base.clone();
        config.audiobookshelf.token = "proxy".into();
    }
    config.storage.reserve_mb = 256;
    let lib = Library::open_with_auto_sync(Paths::new(root.clone()), &config, Arc::new(|_| {}), false);
    lib.set_online(true);
    lib.db().hold_checkpoints();
    let browse = |lib: &Arc<Library>| wait(lib.browse_album_catalog(crate::ui::list::index_letter)).map(|c| c.len());
    for (name, expire) in [("catalog-full", false), ("catalog-unchanged", true)] {
        if expire { wait(async { *lib.catalog_refresh.lock().await = None; }); }
        phase(&base, name);
        let (started, before) = (Instant::now(), written(&lib));
        let albums = browse(&lib).expect("album catalog");
        report(name, started, before, &lib, &format!(" albums={albums}"));
    }
    if abs {
        for name in ["abs-full", "abs-unchanged"] {
            phase(&base, name);
            let (started, before) = (Instant::now(), written(&lib));
            let round = if name == "abs-full" { SyncRound::full() } else { SyncRound::default() };
            let (mut libraries, mut entries, mut covers, mut missing, mut failed) = (0, 0, 0, 0, 0);
            for media in [Media::Book, Media::Podcast] {
                let Ok(choices) = wait(lib.shelf_library_choices_async(media, Policy::NetworkOnly)) else { failed += 1; continue };
                for choice in choices.libraries {
                    libraries += 1;
                    match wait(lib.sync_shelf_library_from(choice.id, media, &round, |_| {})) {
                        Ok(progress) => { entries += progress.entries; covers += progress.covers; missing += progress.unavailable_covers; }
                        Err(_) => failed += 1,
                    }
                }
            }
            report(name, started, before, &lib, &format!(" libraries={libraries} entries={entries} covers={covers} covers_unavailable={missing} failed={failed}"));
        }
    }
    if abs { verify_derived(&base, &lib); }
    phase(&base, "end");
    drop(lib);
    let _ = std::fs::remove_dir_all(root);
}

/// Derived pages against the server's: up to 25 groups of each kind per
/// book library (page 0) and the first three latest-episode pages per
/// podcast library. Counts only.
fn verify_derived(base: &str, lib: &Arc<Library>) {
    use crate::library::shelf_browse::{EntryKind, Section};
    use subsonic::audiobookshelf_browse::Validated;
    phase(base, "abs-verify");
    let client = wait(lib.shelf_client_async()).expect("the account");
    let (namespace, generation) = (lib.shelf_namespace(), lib.shelf_generation());
    let mut counts: std::collections::BTreeMap<String, [u32; 5]> = Default::default();
    let mut compare = |kind: &str, library: &str, media: Media, section: &Section, page: u32| {
        let row = counts.entry(kind.to_owned()).or_default();
        let stored = wait(lib.stored_page(&namespace, library, media, section, page, &generation)).ok().flatten();
        let fetched = match wait(client.browse_page_validated_async(library, media, section, page, 50, None)) {
            Ok(Validated::Changed(page, _)) => page,
            _ => { row[4] += 1; return false; }
        };
        let Some(stored) = stored else { row[3] += 1; return false };
        // [equal, same entries in another order, other entries, not stored, fetch failed]
        if stored == fetched || (stored.entries == fetched.entries && stored.total == fetched.total && stored.has_more == fetched.has_more) { row[0] += 1; }
        else {
            let mut a: Vec<_> = stored.entries.iter().map(|e| &e.id).collect();
            let mut b: Vec<_> = fetched.entries.iter().map(|e| &e.id).collect();
            a.sort(); b.sort();
            if a == b { row[1] += 1 } else { row[2] += 1 }
        }
        stored.has_more
    };
    for media in [Media::Book, Media::Podcast] {
        let Ok(choices) = wait(lib.shelf_library_choices_async(media, Policy::CacheOnly)) else { continue };
        for choice in choices.libraries {
            if media == Media::Podcast {
                for page in 0..3 { if !compare("latest", &choice.id, media, &Section::LatestEpisodes, page) { break; } }
                continue;
            }
            for (kind, list) in [("series", Section::Series), ("author", Section::Authors), ("narrator", Section::Narrators), ("genre", Section::Genres)] {
                let groups = wait(lib.stored_rows(&namespace, &choice.id, media, &list, &generation)).unwrap_or_default();
                for (entry, _) in groups.into_iter().take(25) {
                    if let EntryKind::Group { section } = entry.kind { compare(kind, &choice.id, media, &section, 0); }
                }
            }
        }
    }
    for (kind, [equal, reordered, other, absent, failed]) in counts {
        println!("sync-measure abs-verify-{kind} equal={equal} reordered={reordered} other={other} not_stored={absent} fetch_failed={failed}");
    }
}
