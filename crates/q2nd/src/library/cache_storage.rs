//! Cache publication owns its file/index permit across awaited SQL. Card jobs
//! never wait for the DB owner, and cancellation still joins final publication.
use super::{Library, LibError, RefillEvent, downloads, transfer_io, xfer};
use crate::audio::source::{CachePublication, Progress};
use std::{path::PathBuf, sync::{Arc, atomic::Ordering}};
use subsonic::Song;

pub(super) struct Completion {
    pub song: Song,
    pub variant: String,
    pub key: (String, String),
    pub part: PathBuf,
    pub dest: PathBuf,
    pub progress: Arc<Progress>,
    pub cancel: Arc<transfer_io::Cancel>,
    pub trace: Arc<crate::playtrace::Trace>,
    pub trace_id: u32,
    pub request: u64,
    pub burst: u64,
    pub generation: u32,
}

impl Library {
    pub(super) async fn complete_cache(self: Arc<Self>, completion: Completion, result: Result<u64, LibError>) {
        let gate = self.offline_art_gate.acquire().await;
        let completion = Arc::new(completion);
        let item = completion.clone(); let lib = self.clone();
        let result = downloads::lane().run_isolated(move |_| {
            let result = if result.is_ok() && lib.client_generation() != item.generation {
                Err(LibError::Server("Server changed during stream".into()))
            } else if result.is_ok() && !item.cancel.begin_publication() {
                Err(LibError::Server("cancelled".into()))
            } else { result };
            item.trace.transfer_end(item.trace_id, if result.is_ok() { 1 } else if item.cancel.is_cancelled() { 2 } else { 3 }, item.progress.snapshot().written);
            result
        }).await.expect("cache completion admission");
        match result {
            Ok(size) => {
                let (item, paths) = (completion.clone(), self.paths.clone());
                let renamed = downloads::lane().run_isolated(move |blocking| rename_part(blocking, &paths, &item.part, &item.dest))
                    .await.expect("cache rename");
                let publication = if !renamed { CachePublication::RenameFailed } else {
                    let item = completion.clone(); let generation = self.client_generation.clone();
                    self.db_run(move |blocking, db| {
                        if generation.load(Ordering::SeqCst) != item.generation || db.put_cached_song(blocking, &item.song.id, &item.variant, &item.dest, size).is_err() {
                            CachePublication::IndexFailed
                        } else {
                            if item.song.spoken.is_none() && item.song.radio_url.is_none() { let _ = db.put_cached_song_meta(blocking, &item.song); }
                            CachePublication::Published
                        }
                    }).await
                };
                let item = completion.clone(); let lib = self.clone();
                downloads::lane().run_isolated(move |_| {
                    item.trace.event(crate::playtrace::Kind::CachePublish, item.trace_id, publication as u32, (size / 1024).min(u64::from(u32::MAX)) as u32);
                    lib.refill.record(RefillEvent::Finished { burst: item.burst, transfer: item.request, duration_ms: u64::from(item.song.duration) * 1000, bytes: size, outcome: publication as u64 });
                    item.progress.finish_cached(publication);
                    lib.background_changed();
                }).await.expect("cache completion report");
                if let Err(error) = self.evict_cache_owned().await { log::warn!("cache eviction: {error}"); }
            }
            Err(error) => {
                let item = completion.clone(); let lib = self.clone();
                downloads::lane().run_isolated(move |blocking| {
                    let outcome = if item.cancel.is_cancelled() { 4 } else { match &error { LibError::Offline => 6, LibError::Auth => 7, _ => 5 } };
                    lib.refill.record(RefillEvent::Finished { burst: item.burst, transfer: item.request, duration_ms: u64::from(item.song.duration) * 1000, bytes: item.progress.snapshot().written, outcome });
                    if item.cancel.is_cancelled() { log::info!("stream {}: stopped, no reader", item.song.title); }
                    else { log::warn!("stream {}: {error}", item.song.title); }
                    item.progress.fail(error.to_string());
                    lib.background_changed();
                    discard_part(blocking, &item.part);
                }).await.expect("cache failure cleanup");
            }
        }
        drop(gate);
        let lib = self.clone();
        // Joining/cancellation keeps the existing inflight critical section;
        // neither it nor its caller holds a DB guard across an async wait.
        downloads::lane().run_isolated(move |_| {
            let mut inflight = lib.inflight.lock().unwrap();
            let slot = inflight.get(&completion.key).map(super::Transfer::slot);
            let own = inflight.get(&completion.key).is_some_and(|t| Arc::ptr_eq(&t.cancel, &completion.cancel));
            if xfer::transition(slot, xfer::Event::Finished { own }).1 == xfer::Effect::Remove { inflight.remove(&completion.key); }
            drop(inflight);
            lib.background_changed();
        }).await.expect("cache transfer retirement");
    }

    /// Caller owns the publication gate; no newer publisher can reuse a victim
    /// between its file removal and index retirement.
    async fn remove_cache_rows(self: &Arc<Self>, rows: Vec<(String, String, PathBuf, u64)>) -> Result<(), String> {
        for (id, variant, path, _) in rows {
            let lib = self.clone();
            downloads::lane().run_isolated(move |blocking| remove_cache_file(blocking, &lib, path)).await.map_err(|_| "Cache storage worker stopped".to_string())??;
            self.db_run(move |blocking, db| db.forget_cached_song_checked(blocking, &id, &variant))
                .await.map_err(|_| "Unable to update song cache index".to_string())?;
        }
        Ok(())
    }

    pub(super) async fn evict_cache_owned(self: &Arc<Self>) -> Result<(), String> {
        let lib = self.clone();
        // Configuration can hold this lock. Take the one policy snapshot on
        // the existing card lane, together with its first free-space query.
        let (settings, mut free) = downloads::lane().run_isolated(move |blocking| {
            let settings = lib.settings.read().unwrap().clone();
            (settings, super::free_space(blocking, lib.paths.media_root()))
        }).await.map_err(|_| "Cache storage worker stopped".to_string())?;
        loop {
            let (mut used, victims) = self.db_run(|blocking, db| Ok::<_, rusqlite::Error>((db.song_cache_size_checked(blocking)?, db.song_cache_lru_checked(blocking, 8)?)))
                .await.map_err(|_| "Unable to read song cache index".to_string())?;
            if victims.is_empty() { return Ok(()); }
            // Least recently used first, one song at a time, stopping at the
            // first removal that brings the cache within its limits.
            for victim in victims {
                if !over_budget(used, free, settings.cache_bytes, settings.reserve_bytes) { return Ok(()); }
                let size = victim.3;
                self.remove_cache_rows(vec![victim]).await?;
                used = used.saturating_sub(size);
                let lib = self.clone();
                free = downloads::lane().run_isolated(move |blocking| super::free_space(blocking, lib.paths.media_root()))
                    .await.map_err(|_| "Cache storage worker stopped".to_string())?;
            }
        }
    }

    pub(crate) fn clear_song_cache_async(self: &Arc<Self>)
        -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
        let lib = self.clone(); let generation = self.client_generation();
        let owned = crate::rt::io().spawn(async move {
            lib.music_binding_ready(generation).await.map_err(|error| error.to_string())?;
            let _gate = lib.offline_art_gate.acquire().await;
            let current = lib.client_generation.clone();
            let rows = lib.db_run(move |blocking, db| {
                if current.load(Ordering::SeqCst) != generation { return Err("Navidrome changed; clear the cache again".to_string()); }
                db.song_cache_lru_checked(blocking, usize::MAX >> 1).map_err(|_| "Unable to read song cache index".to_string())
            }).await?;
            lib.remove_cache_rows(rows).await?;
            lib.notify(super::LibEvent::Downloads);
            Ok::<(), String>(())
        });
        async move { owned.await.map_err(|_| "Cache clearing worker stopped".to_string())? }
    }

    pub(crate) fn delete_cached_song_async(self: &Arc<Self>, account: u32, id: String)
        -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
        let lib = self.clone();
        let generation = lib.client_generation();
        // The operation owns its cleanup even if the listener leaves the page.
        let owned = crate::rt::io().spawn(async move {
            const CHANGED: &str = "Account changed. Choose the song again";
            lib.music_binding_ready(generation).await.map_err(|_| CHANGED.to_string())?;
            let _gate = lib.offline_art_gate.acquire().await;
            if lib.music_account_generation() != account || lib.client_generation() != generation {
                return Err(CHANGED.into());
            }
            let lookup = id.clone();
            let paths = lib.db_run(move |blocking, db| db.cached_song_paths(blocking, &lookup))
                .await.map_err(|_| "Unable to read song cache index")?;
            let owner = lib.paths.clone();
            let staged = downloads::lane().run_isolated(move |blocking| stage_cached_song(blocking, &owner, paths))
                .await.map_err(|_| "Couldn't remove the cached song")??;
            let removed = lib.db_run(move |blocking, db| db.delete_cached_song(blocking, &id)).await;
            let cleanup = downloads::lane().run_isolated(move |blocking| {
                if removed.is_ok() {
                    let mut result = Ok(());
                    for (_, path) in staged {
                        if crate::shard::remove(blocking, &path).is_err() { result = Err("Couldn't remove the cached song"); }
                    }
                    result
                } else {
                    restore_cached_song(blocking, &staged);
                    Err("Unable to update song cache index")
                }
            }).await.map_err(|_| "Couldn't remove the cached song");
            lib.notify(super::LibEvent::Downloads);
            cleanup??;
            Ok(())
        });
        async move { owned.await.map_err(|_| "Couldn't remove the cached song".to_string())? }
    }

    #[cfg(test)]
    pub fn clear_song_cache(&self) {
        let lib = self.me.upgrade().expect("library alive");
        super::shelf_browse::wait(async move { lib.clear_song_cache_async().await.map_err(LibError::Server) }).unwrap();
    }
}

/// From the partial directory into the cache file's shard, under the song
/// store's directory lock.
fn rename_part(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, part: &std::path::Path, dest: &std::path::Path) -> bool {
    let _guard = paths.lock_store(blocking, crate::shard::Store::Songs);
    dest.parent().is_some_and(|shard| crate::shard::create_directory(blocking, shard).is_ok())
        && crate::shard::rename(blocking, part, dest).is_ok()
}
/// Whether the song cache must shrink: over its cap, or the card under its
/// reserve. A free-space query that failed (`None`) is not a full card; only
/// the cap applies then.
fn over_budget(used: u64, free: Option<u64>, cache_bytes: u64, reserve_bytes: u64) -> bool {
    used > cache_bytes || free.is_some_and(|free| free < reserve_bytes)
}
fn discard_part(blocking: &crate::rt::Blocking, part: &std::path::Path) {
    let _ = crate::shard::remove(blocking, part);
}
/// An indexed path may be flat (an earlier version) or sharded; remove both.
fn remove_cache_file(blocking: &crate::rt::Blocking, lib: &Library, path: PathBuf) -> Result<(), String> {
    if !lib.paths.can_modify_media(&path) { return Ok(()); }
    lib.paths.remove_media(blocking, &path).map_err(|_| "Unable to remove a cached song".to_string())
}

// Stage the actual copies by same-filesystem rename so a failed SQL
// transaction can restore the files. The publication gate also stays held
// through SQL. The store lock keeps the flat-to-shard mover out of each batch.
fn stage_cached_song(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, indexed: Vec<PathBuf>)
    -> Result<Vec<(PathBuf, PathBuf)>, &'static str> {
    const FAILED: &str = "Couldn't remove the cached song";
    let _store = paths.lock_store(blocking, crate::shard::Store::Songs);
    let root = paths.stream_cache();
    let mut copies = std::collections::BTreeSet::new();
    for path in indexed {
        copies.extend(paths.song_cache_copies(&path).ok_or(FAILED)?);
    }
    if copies.is_empty() { return Ok(Vec::new()); }

    // Do not follow symlinked directories or stage non-files.
    let partials = paths.stream_partials();
    for parent in [root.parent().ok_or(FAILED)?, &root, &partials] {
        if !std::fs::symlink_metadata(parent).is_ok_and(|meta| meta.is_dir()) { return Err(FAILED); }
    }
    let mut present = Vec::new();
    for copy in copies {
        let parent = copy.parent().ok_or(FAILED)?;
        match std::fs::symlink_metadata(parent) {
            Ok(meta) if meta.is_dir() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            _ => return Err(FAILED),
        }
        match std::fs::symlink_metadata(&copy) {
            Ok(meta) if meta.is_file() => present.push(copy),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(FAILED),
        }
    }
    let mut staged = Vec::new();
    for original in present {
        let temporary = partials.join(format!("delete-{:016x}.part", fastrand::u64(..)));
        if temporary.exists() || crate::shard::rename(blocking, &original, &temporary).is_err() {
            restore_cached_song(blocking, &staged);
            return Err(FAILED);
        }
        staged.push((original, temporary));
    }
    Ok(staged)
}

fn restore_cached_song(blocking: &crate::rt::Blocking, staged: &[(PathBuf, PathBuf)]) {
    for (original, temporary) in staged.iter().rev() {
        if crate::shard::rename(blocking, temporary, original).is_err() {
            log::warn!("Unable to restore a cached song after a failed deletion");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    fn fixture() -> (Arc<Library>, mpsc::Receiver<()>) {
        let (send, receive) = mpsc::channel();
        let root = super::super::generated_test_directory("cache-storage-owner");
        let (lib, _) = Library::create(crate::config::Paths::new(root), &crate::config::Config::default(), Arc::new(move |event| {
            if matches!(event, super::super::LibEvent::Downloads) { let _ = send.send(()); }
        }));
        (lib, receive)
    }

    #[test]
    fn deleting_one_song_removes_all_cache_variants_and_keeps_the_same_song_download() {
        let (lib, completed) = fixture();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let song = Song { id: "selected".into(), ..Default::default() };
        let flat = lib.paths.stream_cache().join("selected.raw");
        let shard = lib.paths.sharded(crate::shard::Store::Songs, flat.file_name().unwrap());
        let other_variant = lib.paths.stream_cache().join("selected.mp3");
        let unrelated = lib.paths.stream_cache().join("unrelated.raw");
        let offline = lib.paths.offline().join("selected.raw");
        std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
        for file in [&flat, &shard, &other_variant, &unrelated, &offline] {
            std::fs::write(file, file.as_os_str().as_encoded_bytes()).unwrap();
        }
        let before = std::fs::read(&offline).unwrap();
        {
            let db = lib.db();
            db.put_cached_song(&blocking, &song.id, "raw", &shard, 10).unwrap();
            db.put_cached_song(&blocking, &song.id, "mp3", &other_variant, 10).unwrap();
            db.put_cached_song(&blocking, "unrelated", "raw", &unrelated, 10).unwrap();
            db.put_cached_song_meta(&blocking, &song).unwrap();
            db.put_offline_song(&blocking, &song, &offline, 10);
        }
        let runtime = crate::rt::Ui::new().unwrap();
        let account = lib.music_account_generation();
        assert!(runtime.block_on(lib.delete_cached_song_async(account.wrapping_add(1), song.id.clone())).is_err());
        assert!(shard.exists());
        assert!(completed.try_recv().is_err());

        runtime.block_on(lib.delete_cached_song_async(account, song.id.clone())).unwrap();
        completed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(!flat.exists() && !shard.exists() && !other_variant.exists());
        assert!(unrelated.exists());
        assert_eq!(std::fs::read(&offline).unwrap(), before);
        let db = lib.db();
        assert_eq!(db.offline_song(&blocking, &song.id), Some(offline));
        assert!(db.cached_song_paths(&blocking, &song.id).unwrap().is_empty());
        assert_eq!(db.cached_song_paths(&blocking, "unrelated").unwrap(), [unrelated]);
        assert!(db.cached_songs(&blocking).unwrap().is_empty());
    }

    #[test]
    fn selective_cache_deletion_keeps_files_on_index_failure_or_unsafe_path() {
        for unsafe_path in [false, true] {
            let (lib, _) = fixture();
            let blocking = crate::rt::Blocking::dedicated_thread();
            let cached = lib.paths.stream_cache().join("selected.raw");
            let offline = lib.paths.offline().join("selected.raw");
            std::fs::write(&cached, b"generated cache").unwrap();
            std::fs::write(&offline, b"generated download").unwrap();
            let db = lib.db();
            db.put_cached_song(&blocking, "selected", "raw", &cached, 15).unwrap();
            if unsafe_path {
                db.put_cached_song(&blocking, "selected", "mp3", &offline, 18).unwrap();
            }
            drop(db);
            if !unsafe_path {
                let connection = rusqlite::Connection::open(lib.paths.database()).unwrap();
                connection.execute_batch("CREATE TRIGGER generated_cache_delete_failure BEFORE DELETE ON song_cache BEGIN SELECT RAISE(ABORT,'generated failure'); END;").unwrap();
            }
            let result = crate::rt::Ui::new().unwrap()
                .block_on(lib.delete_cached_song_async(lib.music_account_generation(), "selected".into()));
            assert!(result.is_err());
            assert_eq!(std::fs::read(&cached).unwrap(), b"generated cache");
            assert_eq!(std::fs::read(&offline).unwrap(), b"generated download");
            assert_eq!(lib.db().cached_song(&blocking, "selected", &["raw"]).unwrap().1, cached);
            assert_eq!(std::fs::read_dir(lib.paths.stream_partials()).unwrap().count(), 0);
        }
    }

    #[test]
    fn an_unreadable_free_space_figure_is_not_a_full_card() {
        const MB: u64 = 1 << 20;
        assert!(!over_budget(100 * MB, None, 4096 * MB, 512 * MB), "a failed statvfs must not empty the cache");
        assert!(over_budget(4097 * MB, None, 4096 * MB, 512 * MB), "the cap still applies");
        assert!(over_budget(100 * MB, Some(511 * MB), 4096 * MB, 512 * MB));
        assert!(!over_budget(4096 * MB, Some(512 * MB), 4096 * MB, 512 * MB), "at both limits is within them");
        assert!(!over_budget(0, Some(u64::MAX), 0, u64::MAX));
    }

    #[test]
    fn eviction_stops_at_the_first_removal_that_fits_the_cap() {
        let (lib, _) = fixture();
        let blocking = crate::rt::Blocking::dedicated_thread();
        for index in 0..6 {
            let path = lib.paths.stream_cache().join(format!("generated-{index}.mp3"));
            std::fs::write(&path, vec![0u8; 4096]).unwrap();
            lib.db().put_cached_song(&blocking, &format!("generated-{index}"), "raw", &path, 4096).unwrap();
        }
        {
            let mut settings = lib.settings.write().unwrap();
            // One song over the cap; the host's free space is not the subject.
            settings.cache_bytes = 5 * 4096;
            settings.reserve_bytes = 0;
        }
        let owner = lib.clone();
        crate::rt::Ui::new().unwrap().block_on(async move { owner.evict_cache_owned().await }).unwrap();
        assert_eq!(lib.db().song_cache_size_checked(&blocking).unwrap(), 5 * 4096, "one song over the cap costs one song");
        let kept = (0..6).filter(|index| lib.paths.stream_cache().join(format!("generated-{index}.mp3")).exists()).count();
        assert_eq!(kept, 5);
    }

    #[test]
    fn held_cache_settings_cannot_block_independent_io_progress() {
        let (lib, _) = fixture();
        let settings = lib.settings.write().unwrap();
        let owner = lib.clone();
        let (entered, begun) = mpsc::channel();
        let (finished, completed) = mpsc::channel();
        crate::rt::io().spawn(async move {
            let mut future = std::pin::pin!(owner.evict_cache_owned());
            let mut entered = Some(entered);
            let result = std::future::poll_fn(|cx| {
                let result = std::future::Future::poll(future.as_mut(), cx);
                // A held settings lock must yield from the actual eviction
                // future's first poll, rather than block the I/O executor.
                if let Some(entered) = entered.take() { let _ = entered.send(()); }
                result
            }).await;
            let _ = finished.send(result);
        });
        let yielded = begun.recv_timeout(Duration::from_secs(2));
        let (pulse, pulsed) = mpsc::channel();
        crate::rt::io().spawn(async move { let _ = pulse.send(()); });
        let progress = pulsed.recv_timeout(Duration::from_secs(1));
        let held = completed.try_recv().is_err();
        // Release and join before asserting either timeout: even the broken
        // implementation must not strand this fixture's lock or task.
        drop(settings);
        let result = completed.recv_timeout(Duration::from_secs(3));
        assert!(held, "eviction cannot read the held policy yet");
        assert!(yielded.is_ok(), "settings contention blocked the first async poll");
        assert!(progress.is_ok(), "settings contention stalled independent I/O");
        result.unwrap().unwrap();
    }

    #[test]
    fn abandoned_cache_clear_finishes_owned_removal_and_preserves_downloads() {
        let (lib, completed) = fixture();
        let cached = lib.paths.stream_cache().join("generated-cached.mp3");
        let downloaded = lib.paths.offline().join("generated-downloaded.mp3");
        std::fs::write(&cached, b"generated cache").unwrap();
        std::fs::write(&downloaded, b"generated download").unwrap();
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), "cached", "raw", &cached, 15).unwrap();
        lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &Song { id: "downloaded".into(), ..Default::default() }, &downloaded, 18);
        let (entered, begun) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv_timeout(Duration::from_secs(5)); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        drop(lib.clear_song_cache_async());
        assert!(cached.exists());
        release.send(()).unwrap();
        completed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(!cached.exists());
        assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), "cached", &["raw"]).is_none());
        assert_eq!(std::fs::read(&downloaded).unwrap(), b"generated download");
        assert_eq!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), "downloaded"), Some(downloaded));
    }

    #[test]
    fn cache_clear_reports_read_and_delete_failures_without_claiming_success() {
        for fail_read in [true, false] {
            let (lib, completed) = fixture();
            let cached = lib.paths.stream_cache().join("generated-failed.mp3");
            std::fs::write(&cached, b"generated cache").unwrap();
            lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), "cached", "raw", &cached, 15).unwrap();
            let connection = rusqlite::Connection::open(lib.paths.database()).unwrap();
            if fail_read { connection.execute_batch("ALTER TABLE song_cache RENAME TO generated_unavailable_cache").unwrap(); }
            else { connection.execute_batch("CREATE TRIGGER generated_cache_delete_failure BEFORE DELETE ON song_cache BEGIN SELECT RAISE(ABORT,'generated cache deletion fault'); END;").unwrap(); }
            let result = crate::rt::Ui::new().unwrap().block_on(lib.clear_song_cache_async());
            assert!(result.is_err());
            assert!(completed.try_recv().is_err(), "failed clear must not notify successful completion");
            if fail_read { assert!(cached.exists()); }
            else { assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), "cached", &["raw"]).is_some(), "failed index retirement remains visible for retry"); }
        }
    }
}
