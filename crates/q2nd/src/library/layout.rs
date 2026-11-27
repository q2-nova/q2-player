//! Background packing of cached artwork and sharding of other media.
//! Small batches on the general job pool with a pause after each, held
//! while a playback stream is downloading. Counts go to the log, never names.
use super::Library;
use crate::shard::{self, Progress, Store};
use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub(super) struct Pacing {
    /// Before the first batch, so startup and the first song go first.
    pub start: Duration,
    /// Files per job.
    pub batch: usize,
    /// After each batch.
    pub pause: Duration,
    /// Between checks while a playback stream is downloading.
    pub busy: Duration,
    /// Names held from one directory pass.
    pub pass: usize,
}

impl Pacing {
    pub(super) const DEVICE: Pacing = Pacing {
        start: Duration::from_secs(30), batch: 8, pause: Duration::from_secs(1),
        busy: Duration::from_secs(5), pass: 1024,
    };
}

/// Set when a read-only or I/O error stopped the migration: no retry until
/// the next boot.
static STOPPED: AtomicBool = AtomicBool::new(false);

pub(super) fn start(lib: &Arc<Library>) {
    if lib.paths.ephemeral_media || STOPPED.load(Ordering::Relaxed) { return; }
    crate::rt::io().spawn(run(Arc::downgrade(lib), Pacing::DEVICE));
}

/// Cached artwork followed by the sharded stores. A read-only or I/O error
/// ends sharding for this boot. `None`: the library closed or a worker stopped.
pub(super) async fn run(weak: Weak<Library>, pacing: Pacing) -> Option<Vec<(Store, Progress)>> {
    tokio::time::sleep(pacing.start).await;
    let mut totals = Vec::new();
    for store in Store::ALL {
        if store == Store::Art {
            let progress = super::art_migration::run(&weak, pacing).await?;
            report(store, &progress, true);
            if progress.stopped > 0 {
                log::warn!("media layout {}: artwork packing stopped; it will retry on the next start", store.relative());
            }
            totals.push((store, progress));
            continue;
        }
        let progress = migrate_store(&weak, store, pacing).await?;
        totals.push((store, progress));
        if progress.stopped > 0 {
            STOPPED.store(true, Ordering::Relaxed);
            log::warn!("media layout {}: stopped after a read-only or I/O error; no retry until the next boot", store.relative());
            break;
        }
    }
    Some(totals)
}

async fn migrate_store(weak: &Weak<Library>, store: Store, pacing: Pacing) -> Option<Progress> {
    let paths = weak.upgrade()?.paths.clone();
    let owner = paths.clone();
    if crate::rt::jobs().run_isolated(move |blocking| owner.store_complete(blocking, store)).await.ok()? {
        return Some(Progress::default());
    }
    let mut total = Progress::default();
    // Names that failed once: not tried again in this run.
    let mut skipped = HashSet::new();
    // Shard directories already prepared in this run.
    let mut prepared = HashSet::new();
    loop {
        let root = paths.store_root(store);
        let listed = match crate::rt::jobs().run_isolated(move |blocking| shard::flat_names(blocking, &root, pacing.pass)).await.ok()? {
            Ok(Some(names)) => names,
            // Not created yet: nothing to move, and new files are sharded.
            Ok(None) => Vec::new(),
            Err(error) => {
                log::warn!("media layout {}: unable to read the directory", store.relative());
                if shard::fatal(&error) { total.failed += 1; total.stopped += 1; }
                return Some(total);
            }
        };
        let names: Vec<OsString> = listed.iter().filter(|name| !skipped.contains(*name)).cloned().collect();
        if names.is_empty() {
            if listed.is_empty() && total.conflicts == 0 && total.failed == 0 {
                let owner = paths.clone();
                let marked = crate::rt::jobs().run_isolated(move |blocking| owner.mark_complete(blocking, store)).await.ok()?;
                if let Err(error) = marked {
                    log::warn!("media layout {}: unable to record completion", store.relative());
                    if shard::fatal(&error) { total.failed += 1; total.stopped += 1; }
                }
            }
            report(store, &total, true);
            return Some(total);
        }
        // Shard directories first, paced like the moves: each new exFAT
        // directory writes a whole cluster (256 KiB on a measured card).
        let shards: Vec<String> = shard::shards_of(&names).into_iter().filter(|name| prepared.insert(name.clone())).collect();
        for group in shards.chunks(pacing.batch.max(1)) {
            tokio::time::sleep(pacing.pause).await;
            while weak.upgrade()?.playback_transfer_running() { tokio::time::sleep(pacing.busy).await; }
            let (group, owner) = (group.to_vec(), paths.clone());
            if crate::rt::jobs().run_isolated(move |blocking| shard::prepare_shards(blocking, &owner, store, &group)).await.ok()?.is_err() {
                total.failed += 1;
                total.stopped += 1;
                report(store, &total, true);
                return Some(total);
            }
        }
        let before = total;
        for chunk in names.chunks(pacing.batch.max(1)) {
            tokio::time::sleep(pacing.pause).await;
            while weak.upgrade()?.playback_transfer_running() { tokio::time::sleep(pacing.busy).await; }
            let (chunk, owner) = (chunk.to_vec(), paths.clone());
            let (progress, failed) = crate::rt::jobs().run_isolated(move |blocking| {
                let mut failed = Vec::new();
                (shard::migrate_batch(blocking, &owner, store, &chunk, &mut failed), failed)
            }).await.ok()?;
            total += progress;
            skipped.extend(failed);
            if progress.stopped > 0 {
                report(store, &total, true);
                return Some(total);
            }
        }
        report(store, &total, false);
        // Only conflicts or failures left: stop until the next start.
        if total.moved + total.gone + total.duplicates + total.temporary == before.moved + before.gone + before.duplicates + before.temporary {
            report(store, &total, true);
            return Some(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use crate::library::db::OfflineSet;
    use subsonic::Song;

    fn blocking() -> crate::rt::Blocking { crate::rt::Blocking::dedicated_thread() }
    const FAST: Pacing = Pacing { start: Duration::ZERO, batch: 3, pause: Duration::ZERO, busy: Duration::from_millis(1), pass: 7 };

    fn fixture() -> Arc<Library> { fixture_at(super::super::generated_test_directory("media-layout")) }
    fn fixture_at(root: std::path::PathBuf) -> Arc<Library> {
        let mut config = Config::default();
        config.server.url = "http://127.0.0.1:9".into();
        config.server.user = "generated-layout".into();
        config.server.password = "synthetic-only".into();
        let (lib, _) = Library::create(Paths::new(root), &config, Arc::new(|_| {}));
        lib.set_offline_mode(true);
        lib
    }
    fn song(id: &str) -> Song {
        Song { id: id.into(), suffix: Some("mp3".into()), bit_rate: Some(256), duration: 240, size: Some(16), ..Default::default() }
    }
    /// A file an earlier version left directly in its store's directory.
    fn flat(lib: &Library, store: Store, name: &str, bytes: usize) -> std::path::PathBuf {
        let path = lib.paths.store_root(store).join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![7u8; bytes]).unwrap();
        path
    }
    fn files(lib: &Library, store: Store) -> Vec<std::path::PathBuf> { crate::library::stored_files(&lib.paths.store_root(store)) }
    fn run_now(lib: &Arc<Library>) -> Vec<(Store, Progress)> {
        let weak = Arc::downgrade(lib);
        crate::rt::Ui::new().unwrap().block_on(async move {
            tokio::time::timeout(Duration::from_secs(20), run(weak, FAST)).await.unwrap()
        }).expect("library open")
    }

    #[test]
    fn flat_stores_move_in_batches_while_lookups_accounting_eviction_and_removal_follow() {
        let lib = fixture();
        // Old flat layout: cached songs and downloads indexed by flat paths.
        let cached: Vec<Song> = (0..4).map(|i| song(&format!("generated-cached-{i}"))).collect();
        for item in &cached {
            let path = flat(&lib, Store::Songs, &format!("{}.raw.mp3", item.id), 4096);
            lib.db().put_cached_song(&blocking(), &item.id, "raw", &path, 4096).unwrap();
        }
        let downloaded = song("generated-downloaded");
        let set = OfflineSet { kind: "song".into(), id: downloaded.id.clone(), name: "Generated".into(), artist: String::new(), cover_art: None, song_ids: vec![downloaded.id.clone()] };
        lib.download(set.clone(), std::slice::from_ref(&downloaded)).unwrap();
        let download = flat(&lib, Store::Offline, "generated-downloaded-0000000000000000.mp3", 8192);
        lib.db().put_offline_song(&blocking(), &downloaded, &download, 8192);
        flat(&lib, Store::OfflineArt, "1-generated-0000000000000000.png", 512);
        flat(&lib, Store::Spoken, "generated-book-0000000000000000.m4b", 512);
        let before = lib.storage_allocations().unwrap();
        assert!(before.0 > 0 && before.1 > 0);
        // Part of a batch moved before an interruption: indexes still say flat.
        let name = std::ffi::OsString::from(format!("{}.raw.mp3", cached[0].id));
        assert_eq!(shard::migrate(&blocking(), &lib.paths, Store::Songs, &[name]).moved, 1);
        assert_eq!(lib.storage_allocations().unwrap(), before, "accounting spans both layouts");
        let (found, _, _) = lib.find_local(&cached[0]).expect("moved cached song");
        assert_eq!(found, lib.cache_path(&cached[0], "raw"));

        let totals = run_now(&lib);
        let moved = |store| totals.iter().find(|(s, _)| *s == store).unwrap().1;
        assert_eq!(moved(Store::Songs).moved, 3);
        assert_eq!((moved(Store::Offline).moved, moved(Store::OfflineArt).moved, moved(Store::Spoken).moved), (1, 1, 1));
        for store in Store::ALL.into_iter().filter(|store| *store != Store::Art) {
            assert!(lib.paths.layout().complete(store), "{store:?}");
            assert!(lib.paths.store_root(store).join(shard::MARKER).is_file());
            assert!(shard::flat_names(&blocking(), &lib.paths.store_root(store), 100).unwrap().unwrap().is_empty());
            for file in files(&lib, store) {
                assert_eq!(file, lib.paths.sharded(store, file.file_name().unwrap()), "{store:?} file left flat");
            }
        }
        assert_eq!(lib.storage_allocations().unwrap(), before, "no file lost or counted twice");
        for item in &cached { assert!(lib.find_local(item).is_some()); }
        assert_eq!(lib.find_local(&downloaded).unwrap().0, lib.paths.sharded(Store::Offline, download.file_name().unwrap()));
        // A second start finds every store finished.
        assert!(run_now(&lib).iter().all(|(_, progress)| *progress == Progress::default()));

        // Clearing the cache removes every indexed file, whichever path the index holds.
        lib.clear_song_cache();
        assert!(files(&lib, Store::Songs).is_empty());
        assert_eq!(lib.storage_allocations().unwrap(), (0, before.1));
        lib.remove_download(&set.kind, &set.id).unwrap();
        assert!(files(&lib, Store::Offline).is_empty(), "delete download removes the moved file");
        assert_eq!(lib.storage_allocations().unwrap(), (0, 0));
    }

    #[test]
    fn eviction_removes_cached_songs_in_either_layout() {
        let lib = fixture();
        let mut cached = Vec::new();
        for i in 0..6 {
            let item = song(&format!("generated-evicted-{i}"));
            let path = flat(&lib, Store::Songs, &format!("{}.raw.mp3", item.id), 4096);
            lib.db().put_cached_song(&blocking(), &item.id, "raw", &path, 4096).unwrap();
            if i % 2 == 0 {
                shard::migrate(&blocking(), &lib.paths, Store::Songs, &[path.file_name().unwrap().into()]);
            }
            cached.push(item);
        }
        lib.settings.write().unwrap().cache_bytes = 0;
        let owner = lib.clone();
        crate::rt::Ui::new().unwrap().block_on(async move { owner.evict_cache_owned().await }).unwrap();
        assert!(files(&lib, Store::Songs).is_empty(), "eviction reached flat and sharded files");
        assert_eq!(lib.db().song_cache_size_checked(&blocking()).unwrap(), 0);
        for item in &cached { assert!(lib.find_local(item).is_none()); }
    }

    #[test]
    fn migration_waits_while_a_playback_stream_downloads_and_stops_when_the_library_closes() {
        let lib = fixture();
        let png = crate::gfx::Image::placeholder(50).png().unwrap();
        for i in 0..4 {
            let path = flat(&lib, Store::Art, &format!("{:032x}.png", i), 64);
            std::fs::write(path, &png).unwrap();
        }
        let hold = lib.begin_spoken_playback();
        let weak = Arc::downgrade(&lib);
        let ui = crate::rt::Ui::new().unwrap();
        let stalled = ui.block_on(async { tokio::time::timeout(Duration::from_millis(200), run(weak.clone(), FAST)).await });
        assert!(stalled.is_err(), "no batch runs during playback streaming");
        assert_eq!(files(&lib, Store::Art).iter().filter(|path| path.parent() == Some(lib.paths.art_cache().as_path())).count(), 4);
        drop(hold);
        lib.settings.write().unwrap().reserve_bytes = 0;
        assert_eq!(run_now(&lib)[0].1.moved, 4);
        drop(lib);
        assert!(ui.block_on(run(weak, FAST)).is_none(), "a closed library ends the task");
    }

    #[test]
    fn art_import_resumes_committed_rows_and_retains_conflicts_and_other_owners() {
        let lib = fixture();
        lib.settings.write().unwrap().reserve_bytes = 0;
        let png = crate::gfx::Image::placeholder(50).png().unwrap();
        let other = crate::gfx::Image::placeholder(150).png().unwrap();
        let root = lib.paths.art_cache();
        let name = |i: u32| format!("{i:032x}.png");
        let flat_png = |i, bytes: &[u8]| {
            let path = root.join(name(i));
            std::fs::write(&path, bytes).unwrap();
            path
        };

        // Simulate interruption after a committed row and before loose removal.
        let resumed_key = format!("{:032x}", 0);
        let conflict_key = format!("{:032x}", 1);
        lib.art.pack.import(&blocking(), &[(&resumed_key, &png), (&conflict_key, &other)]).unwrap();
        let resumed = flat_png(0, &png);
        let conflict = flat_png(1, &png);
        let malformed = flat_png(2, b"\x89PNG\r\n\x1a\ntruncated");
        let flat_path = flat_png(3, &png);
        let sharded = lib.paths.sharded(Store::Art, name(4).as_ref());
        std::fs::create_dir_all(sharded.parent().unwrap()).unwrap();
        std::fs::write(&sharded, &png).unwrap();

        let flat_collision = flat_png(6, &png);
        let shard_collision = lib.paths.sharded(Store::Art, name(6).as_ref());
        std::fs::create_dir_all(shard_collision.parent().unwrap()).unwrap();
        std::fs::write(&shard_collision, &other).unwrap();

        let obsolete_marker = root.join(shard::MARKER);
        std::fs::write(&obsolete_marker, b"sharded v1\n").unwrap();
        let unrelated = root.join("other.png");
        std::fs::write(&unrelated, &png).unwrap();
        let owned_name = name(5);
        let offline = lib.paths.sharded(Store::OfflineArt, owned_name.as_ref());
        std::fs::create_dir_all(offline.parent().unwrap()).unwrap();
        std::fs::write(&offline, &png).unwrap();

        let totals = run_now(&lib);
        assert_eq!((totals[0].1.moved, totals[0].1.conflicts), (3, 3));
        for gone in [resumed.as_path(), flat_path.as_path(), sharded.as_path(), sharded.parent().unwrap()] {
            assert!(!gone.exists());
        }
        for keep in [&conflict, &malformed, &unrelated, &obsolete_marker, &offline, &flat_collision, &shard_collision] {
            assert!(keep.is_file());
        }
        assert_eq!(lib.art.pack.get(&blocking(), &conflict_key).unwrap(), Some(other));
        for i in [0, 3, 4] {
            assert_eq!(lib.art.pack.get(&blocking(), &format!("{i:032x}")).unwrap().as_ref(), Some(&png));
        }
        assert_eq!(lib.art.pack.get(&blocking(), &format!("{:032x}", 6)).unwrap(), None);
        let again = run_now(&lib)[0].1;
        assert_eq!((again.moved, again.conflicts, again.failed), (0, 3, 0));
    }

    #[test]
    fn art_import_preserves_loose_files_at_the_free_space_reserve() {
        let lib = fixture();
        let png = crate::gfx::Image::placeholder(50).png().unwrap();
        let key = format!("{:032x}", 0);
        let path = flat(&lib, Store::Art, &format!("{key}.png"), 64);
        std::fs::write(&path, &png).unwrap();
        lib.settings.write().unwrap().reserve_bytes = u64::MAX;

        assert_eq!(run_now(&lib)[0].1.failed, 1);
        assert_eq!(std::fs::read(&path).unwrap(), png);
        assert_eq!(lib.art.pack.get(&blocking(), &key).unwrap(), None);

        lib.art.pack.import(&blocking(), &[(&key, &png)]).unwrap();
        assert_eq!(run_now(&lib)[0].1.moved, 1, "resume the already committed import");
        assert!(!path.exists());
    }

    #[test]
    fn unavailable_art_pack_preserves_loose_art_and_allows_other_layout_work() {
        let lib = fixture();
        lib.settings.write().unwrap().reserve_bytes = 0;
        let png = crate::gfx::Image::placeholder(50).png().unwrap();
        let path = flat(&lib, Store::Art, &format!("{:032x}.png", 0), 64);
        std::fs::write(&path, &png).unwrap();
        let unusable_pack = lib.paths.art_cache().join("artwork.sqlite3");
        std::fs::create_dir(&unusable_pack).unwrap();
        let song = flat(&lib, Store::Songs, "generated-0.raw.mp3", 64);

        let totals = run_now(&lib);
        assert_eq!((totals[0].1.moved, totals[0].1.failed, totals[0].1.stopped), (0, 1, 1));
        assert_eq!(std::fs::read(path).unwrap(), png);
        assert!(unusable_pack.is_dir());
        assert_eq!((totals[1].0, totals[1].1.moved), (Store::Songs, 1));
        assert!(!song.exists());
    }

    fn flat_count(lib: &Library, store: Store) -> usize {
        files(lib, store).iter().filter(|path| path.parent() == Some(lib.paths.store_root(store).as_path())).count()
    }

    #[test]
    fn a_read_only_error_stops_the_whole_migration_at_the_first_failure() {
        let lib = fixture();
        for i in 0..30 { flat(&lib, Store::Songs, &format!("generated-{i}.raw.mp3"), 64); }
        for i in 0..4 { flat(&lib, Store::Offline, &format!("generated-{i}.raw.mp3"), 64); }
        lib.paths.faults().read_only_after = Some(5);
        let totals = run_now(&lib);
        assert_eq!(totals.len(), 2, "no later store is tried");
        let (store, songs) = totals[1];
        assert_eq!(store, Store::Songs);
        assert_eq!((songs.moved, songs.failed, songs.stopped), (5, 1, 1));
        assert_eq!(lib.paths.faults().attempts, 6, "one failed rename, not one per remaining file");
        assert_eq!(flat_count(&lib, Store::Songs), 25);
        assert_eq!(flat_count(&lib, Store::Offline), 4);
        assert!(!lib.paths.layout().complete(Store::Songs) && !lib.paths.store_root(Store::Songs).join(shard::MARKER).exists());
        // Process-wide; tests never clear it, and none rely on start().
        assert!(STOPPED.load(Ordering::Relaxed), "start() does not run again this boot");
    }

    #[test]
    fn shard_directories_exist_before_the_first_move_of_a_pass() {
        let lib = fixture();
        let names: Vec<OsString> = (0..6).map(|i| flat(&lib, Store::Songs, &format!("generated-{i}.raw.mp3"), 64).file_name().unwrap().into()).collect();
        lib.paths.faults().read_only_after = Some(0);
        let totals = run_now(&lib);
        assert_eq!((totals[1].1.moved, totals[1].1.stopped), (0, 1));
        for shard in shard::shards_of(&names) {
            assert!(lib.paths.stream_cache().join(shard).is_dir(), "created before the failed move");
        }
    }

    #[test]
    fn a_failed_file_is_tried_once_per_run_and_the_rest_still_move() {
        let lib = fixture();
        let stuck = flat(&lib, Store::Songs, "generated-0.raw.mp3", 64).file_name().unwrap().to_owned();
        for i in 1..20 { flat(&lib, Store::Songs, &format!("generated-{i}.raw.mp3"), 64); }
        lib.paths.faults().names.push((stuck.clone(), libc::EACCES));
        let totals = run_now(&lib);
        assert_eq!(totals.len(), Store::ALL.len(), "a failure that is not read-only or I/O does not stop other stores");
        let songs = totals[1].1;
        assert_eq!((songs.moved, songs.failed, songs.stopped), (19, 1, 0));
        assert_eq!(lib.paths.faults().attempts, 20, "the failed name was tried once");
        assert_eq!(flat_count(&lib, Store::Songs), 1);
        assert!(!lib.paths.layout().complete(Store::Songs), "a store with a flat file left is not marked complete");
        // The next start (boot) tries it once more.
        let again = run_now(&lib);
        assert_eq!((again[1].1.failed, lib.paths.faults().attempts), (1, 21));
    }

    #[test]
    fn a_move_waits_for_a_writer_holding_the_store_lock() {
        let lib = fixture();
        let names: Vec<OsString> = (0..3).map(|i| flat(&lib, Store::Songs, &format!("generated-{i}.raw.mp3"), 64).file_name().unwrap().into()).collect();
        let writer = lib.paths.lock_store(&blocking(), Store::Songs);
        let paths = lib.paths.clone();
        let mover = std::thread::spawn(move || shard::migrate(&blocking(), &paths, Store::Songs, &names));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(flat_count(&lib, Store::Songs), 3, "no move while a writer holds the lock");
        drop(writer);
        assert_eq!(mover.join().unwrap().moved, 3);
        assert_eq!(flat_count(&lib, Store::Songs), 0);
    }

    /// Host reproduction harness for an exFAT card the kernel set read-only
    /// during the shard migration. Run against a
    /// mounted exFAT image only, never a card:
    /// `Q2_GENERATED_TEST_ROOT=/run/media/USER/IMAGE Q2_STRESS_FILES=7300 cargo test -p q2nd
    /// library::layout::tests::exfat_stress -- --ignored --nocapture`.
    /// Knobs (environment): files, writers, deleters, paced (pause ms), history
    /// (flat-directory churn before the run), sync (one in N writes as the
    /// synced-cover path), hot (writers' new names only in the first N shard
    /// directories). Counts only.
    #[test]
    #[ignore = "needs a mounted exFAT image in Q2_GENERATED_TEST_ROOT"]
    fn exfat_stress() {
        use md5::Digest;
        // Counts of operations: 32 bits are enough, and the Q2 has no 64-bit atomics.
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
        let knob = |name: &str, default: u64| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let (count, writers, deleters, paced, history, sync_every) = (knob("Q2_STRESS_FILES", 7300), knob("Q2_STRESS_WRITERS", 2),
            knob("Q2_STRESS_DELETERS", 1), knob("Q2_STRESS_PACED_MS", 0), knob("Q2_STRESS_HISTORY", 1), knob("Q2_STRESS_SYNC", 5));
        let hot = knob("Q2_STRESS_HOT", 0);
        let kinds = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::<String, u64>::new()));
        // Reuse: an existing library root (a crash copy of an earlier run); nothing is added.
        let reuse = std::env::var_os("Q2_STRESS_REUSE").map(std::path::PathBuf::from);
        let count = if reuse.is_some() { 0 } else { count };
        let lib = reuse.map_or_else(fixture, fixture_at);
        let paths = lib.paths.clone();
        let art = paths.art_cache();
        std::fs::create_dir_all(&art).unwrap();
        let name_of = |seed: &str| std::ffi::OsString::from(format!("{:x}.png", md5::Md5::digest(seed.as_bytes())));
        let bytes = |i: u64| vec![(i % 251) as u8; 10_000 + (i as usize * 7919) % 190_000];
        // Flat store as an earlier version left it; with history, the flat
        // directory also carries deleted entries from eviction and rewrites.
        let started = std::time::Instant::now();
        for i in 0..count {
            std::fs::write(art.join(name_of(&format!("flat-{i}"))), bytes(i)).unwrap();
            if history > 0 && i % 4 == 0 {
                let tmp = art.join(format!("{:x}.tmp", md5::Md5::digest(format!("churn-{i}"))));
                std::fs::write(&tmp, bytes(i)).unwrap();
                std::fs::rename(&tmp, art.join(name_of(&format!("churn-{i}")))).unwrap();
                if i % 8 == 0 { std::fs::remove_file(art.join(name_of(&format!("churn-{i}")))).unwrap(); }
            }
        }
        println!("Q2_STRESS populated {} flat files in {:.1} s", count, started.elapsed().as_secs_f64());
        let stop = Arc::new(AtomicBool::new(false));
        let written = Arc::new(std::sync::Mutex::new(Vec::<std::path::PathBuf>::new()));
        let (creates, removes, errors) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let mut threads = Vec::new();
        for w in 0..writers {
            let (paths, stop, written, creates, errors, kinds) = (paths.clone(), stop.clone(), written.clone(), creates.clone(), errors.clone(), kinds.clone());
            threads.push(std::thread::spawn(move || {
                let b = blocking();
                let mut i = 0u64;
                while !stop.load(Relaxed) {
                    i += 1;
                    // One in four rewrites an existing cover (rename over a file).
                    let name = if i % 4 == 0 && !written.lock().unwrap().is_empty() {
                        let list = written.lock().unwrap(); list[fastrand::usize(..list.len())].file_name().unwrap().to_owned()
                    } else {
                        // With hot shards, new names land in the first few shard directories only.
                        let mut seed = 0u64;
                        loop {
                            let name = name_of(&format!("new-{w}-{i}-{seed}"));
                            if hot == 0 || u64::from_str_radix(&crate::shard::shard(&name), 16).unwrap() < hot { break name; }
                            seed += 1;
                        }
                    };
                    let guard = paths.lock_store(&b, Store::Art);
                    let path = match paths.prepare_sharded(&b, &guard, &name) { Ok(path) => path, Err(_) => { errors.fetch_add(1, Relaxed); std::thread::sleep(Duration::from_millis(10)); continue; } };
                    let result = if sync_every > 0 && i % sync_every == 0 {
                        // The synced-cover path: unique temporary, fsync, rename, directory syncs.
                        let temporary = path.with_extension(format!("sync-{:016x}", fastrand::u64(..)));
                        std::fs::write(&temporary, bytes(i)).and_then(|()| std::fs::File::open(&temporary)?.sync_all())
                            .and_then(|()| crate::shard::rename(&b, &temporary, &path)).and_then(|()| paths.sync_sharded(&b, &path))
                    } else {
                        let tmp = path.with_extension("tmp");
                        std::fs::write(&tmp, bytes(i)).and_then(|()| crate::shard::rename(&b, &tmp, &path))
                    };
                    drop(guard);
                    match result { Ok(()) => { creates.fetch_add(1, Relaxed); written.lock().unwrap().push(path); } Err(error) => { errors.fetch_add(1, Relaxed); *kinds.lock().unwrap().entry(format!("{:?}", error.kind())).or_default() += 1; } }
                    std::thread::sleep(Duration::from_millis(fastrand::u64(0..20)));
                }
            }));
        }
        for _ in 0..deleters {
            let (paths, stop, written, removes, errors) = (paths.clone(), stop.clone(), written.clone(), removes.clone(), errors.clone());
            threads.push(std::thread::spawn(move || {
                let b = blocking();
                while !stop.load(Relaxed) {
                    let victim = { let mut list = written.lock().unwrap(); (list.len() > 50).then(|| { let n = fastrand::usize(..list.len()); list.swap_remove(n) }) };
                    if let Some(path) = victim {
                        match paths.remove_media(&b, &path) { Ok(()) => { removes.fetch_add(1, Relaxed); } Err(_) => { errors.fetch_add(1, Relaxed); } }
                    }
                    std::thread::sleep(Duration::from_millis(fastrand::u64(0..40)));
                }
            }));
        }
        let pacing = Pacing { start: Duration::ZERO, batch: 8, pause: Duration::from_millis(paced), busy: Duration::from_millis(5), pass: 1024 };
        let weak = Arc::downgrade(&lib);
        let migrated = std::time::Instant::now();
        let art_total = crate::rt::Ui::new().unwrap()
            .block_on(async move { migrate_store(&weak, Store::Art, pacing).await }).expect("library open");
        let migration_s = migrated.elapsed().as_secs_f64();
        stop.store(true, Relaxed);
        for thread in threads { thread.join().unwrap(); }
        println!("Q2_STRESS {}", serde_json::json!({
            "files": count, "writers": writers, "deleters": deleters, "paced_ms": paced, "history": history,
            "moved": art_total.moved, "gone": art_total.gone, "duplicates": art_total.duplicates, "temporary": art_total.temporary,
            "conflicts": art_total.conflicts, "failed": art_total.failed, "migration_s": migration_s,
            "writer_creates": creates.load(Relaxed), "deleter_removes": removes.load(Relaxed), "writer_errors": errors.load(Relaxed), "hot": hot, "error_kinds": kinds.lock().unwrap().clone(),
        }));
    }
}

fn report(store: Store, total: &Progress, done: bool) {
    log::info!("media layout {}: {} moved, {} gone, {} stale copies, {} temporary removed, {} conflicts, {} failed{}",
        store.relative(), total.moved, total.gone, total.duplicates, total.temporary, total.conflicts, total.failed,
        if done { "; pass finished" } else { "" });
}
