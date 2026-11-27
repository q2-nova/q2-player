//! Owned cache snapshots followed by finite filesystem inspection. A publication
//! permit serializes a missing-row decision with rename/index and account reset.
use super::{Library, LibError, InFlight, TransferUse, RefillEvent, acceptable, extension};
use crate::{audio::{player::Origin, source::Progress}, config::Quality};
use std::{path::PathBuf, sync::{Arc, atomic::Ordering}};
use subsonic::Song;

type LocalFile = (PathBuf, Origin, String);
type Fetch = (PathBuf, Arc<Progress>, Option<TransferUse>);

impl Library {
    /// Dedicated player/ordinary fixture boundary; no worker or UI caller may wait.
    pub(super) fn find_local(&self, song: &Song) -> Option<LocalFile> {
        assert!(crate::rt::may_wait_for_tasks(), "await local media lookup on runtime/pool callers");
        let lib = self.me.upgrade()?;
        let quality = self.settings.read().unwrap().stream_quality;
        let songs = Arc::new(vec![song.clone()]);
        super::shelf_browse::wait(async move { lib.local_files_async(songs, quality).await })
            .ok()?.into_iter().next().flatten()
    }
    #[cfg(test)]
    pub fn local_ahead_ms(&self, queue: &[Song], index: usize, position_ms: u64, limit: usize) -> u64 {
        assert!(crate::rt::may_wait_for_tasks(), "await local ahead snapshots on runtime/pool callers");
        let lib = self.me.upgrade().expect("library alive"); let queue = Arc::new(queue.to_vec());
        super::shelf_browse::wait(async move { lib.local_ahead_ms_async(queue, index, position_ms, limit).await }).unwrap_or(0)
    }
    pub(crate) async fn local_ahead_ms_async(self: &Arc<Self>, queue: Arc<Vec<Song>>, index: usize, position_ms: u64, limit: usize) -> Result<u64, LibError> {
        let songs: Vec<Song> = queue.iter().skip(index).take(limit.saturating_add(1))
            .take_while(|song| song.radio_url.is_none() && song.spoken.is_none() && song.duration > 0).cloned().collect();
        if songs.is_empty() { return Ok(0); }
        let songs = Arc::new(songs);
        let quality = self.settings.read().unwrap().stream_quality;
        let local = self.local_files_async(songs.clone(), quality).await?;
        Ok(songs.iter().zip(local).take_while(|(_, local)| local.is_some()).enumerate().map(|(index, (song, _))| {
            let duration = u64::from(song.duration) * 1000;
            if index == 0 { duration.saturating_sub(position_ms) } else { duration }
        }).sum())
    }

    pub(super) async fn local_files_async(self: &Arc<Self>, songs: Arc<Vec<Song>>, quality: Quality) -> Result<Vec<Option<LocalFile>>, LibError> {
        let generation = self.client_generation();
        self.music_binding_ready(generation).await?;
        let gate = self.offline_art_gate.acquire().await;
        let current = self.client_generation.clone(); let tracks = songs.clone();
        let offline = self.db_run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            Ok(tracks.iter().map(|song| db.offline_song(blocking, &song.id)).collect::<Vec<_>>())
        }).await?;
        let paths = self.paths.clone();
        // Indexed paths may be flat (an earlier version) or sharded.
        let local = crate::rt::spawn_blocking(move |blocking| offline.into_iter().map(|path| {
            path.and_then(|path| paths.media_path(&path)).and_then(|path| paths.locate(blocking, &path)).map(|path| {
                let ext = path.extension().map(|ext| ext.to_string_lossy().into_owned()).unwrap_or_default();
                (path, Origin::Offline, ext)
            })
        }).collect::<Vec<_>>()).await.map_err(|_| LibError::Offline)?;
        let current = self.client_generation.clone(); let tracks = songs.clone();
        let (local, cached) = self.db_run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            let cached = tracks.iter().zip(&local).map(|(song, found)| {
                // Downloads retain priority and do not touch cache LRU entries.
                if found.is_some() { return None; }
                let variants = acceptable(song, quality);
                db.cached_song(blocking, &song.id, &variants.iter().map(String::as_str).collect::<Vec<_>>())
            }).collect::<Vec<_>>();
            Ok((local, cached))
        }).await?;
        let paths = self.paths.clone();
        let (local, missing) = crate::rt::spawn_blocking(move |blocking| {
            let mut missing = Vec::new();
            let local = songs.iter().zip(local).zip(cached).map(|((song, found), cached)| {
                if found.is_some() { return found; }
                let (variant, path) = cached?;
                match paths.media_path(&path).and_then(|path| paths.locate(blocking, &path)) {
                    Some(path) => Some((path, Origin::Cached, extension(song, &variant))),
                    None => { missing.push((song.id.clone(), variant)); None }
                }
            }).collect();
            (local, missing)
        }).await.map_err(|_| LibError::Offline)?;
        if !missing.is_empty() {
            let current = self.client_generation.clone();
            self.db_run(move |blocking, db| {
                // A posted job outlives a cancelled observer, so ownership of
                // the permit must travel with this final mutating DB job.
                let _gate = gate;
                if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                // The same gate still excludes account reset and cache
                // publication, including replacement at an identical pathname.
                for (id, variant) in missing { db.forget_cached_song_checked(blocking, &id, &variant).map_err(|_| LibError::Offline)?; }
                Ok(())
            }).await?;
        } else { drop(gate); }
        if self.client_generation() != generation { return Err(LibError::Offline); }
        Ok(local)
    }

    pub(super) fn join_fetch_request(self: &Arc<Self>, inflight: &mut InFlight, key: &(String, String), playback: bool, burst: u64, request: u64, generation: u32) -> Option<Fetch> {
        use q2_machines::transfer as xfer;
        if self.client_generation() != generation { return None; }
        if inflight.get(key).is_some_and(|transfer| transfer.generation != generation) {
            let stale = inflight.remove(key).expect("observed obsolete transfer");
            // Account-local IDs cannot identify another account's partial file.
            // Retire its registry slot before cancellation, independently of
            // its old holds. Cancellation preserves owned file cleanup; a new
            // transfer gets a unique partial and old holds cannot retire it.
            if stale.cancel.cancel() {
                let fill = stale.progress.snapshot();
                self.trace().event(crate::playtrace::Kind::XferCancel, stale.trace_id,
                    super::fill_percent(&fill), (fill.written / 1024) as u32);
            }
            return None;
        }
        let slot = inflight.get(key).map(super::Transfer::slot);
        if let (Some(joined), xfer::Effect::Join { rejoined }) = xfer::transition(slot, xfer::Event::Open { playback }) {
            let transfer = inflight.get_mut(key).expect("joined transfer exists");
            transfer.apply(joined);
            // A fetch-ahead transfer the player now waits for stops yielding.
            if playback { self.priority_changed.notify_waiters(); }
            self.trace().event(crate::playtrace::Kind::XferJoin, transfer.trace_id, transfer.users as u32, rejoined as u32);
            self.refill.record(RefillEvent::Resolved { burst, request, transfer: transfer.journal_id, kind: 2 });
            let hold = TransferUse { lib: self.clone(), key: key.clone(), cancel: transfer.cancel.clone() };
            Some((transfer.part.clone(), transfer.progress.clone(), Some(hold)))
        } else { None }
    }

    pub(super) async fn fetch_in_burst_async(self: &Arc<Self>, song: Song, variant: String, playback: bool, burst: u64) -> Result<Fetch, LibError> {
        let request = self.refill.record(RefillEvent::Request { burst, duration_ms: u64::from(song.duration) * 1000, playback });
        let result = self.prepare_fetch(song, variant, playback, burst, request).await;
        if let Err(error) = &result {
            let kind = match error { LibError::NotConfigured => 4, LibError::Offline => 5, LibError::Auth => 6, LibError::Server(_) => 7 };
            self.refill.record(RefillEvent::Resolved { burst, request, transfer: 0, kind });
        }
        result
    }
    async fn prepare_fetch(self: &Arc<Self>, song: Song, variant: String, playback: bool, burst: u64, request: u64) -> Result<Fetch, LibError> {
        let generation = self.client_generation();
        self.music_binding_ready(generation).await?;
        let key = (song.id.clone(), variant.clone());
        if let Some(joined) = self.join_fetch_request(&mut self.inflight.lock().unwrap(), &key, playback, burst, request, generation) { return Ok(joined); }
        let gate = self.offline_art_gate.acquire().await;
        // Another opener may have installed the slot during the gate wait.
        if let Some(joined) = self.join_fetch_request(&mut self.inflight.lock().unwrap(), &key, playback, burst, request, generation) { return Ok(joined); }
        let current = self.client_generation.clone();
        let cached = self.db_run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
            Ok(db.cached_song(blocking, &key.0, &[&key.1]))
        }).await?;
        let lib = self.clone();
        crate::rt::spawn_blocking(move |blocking| {
            if lib.client_generation() != generation { return Err(LibError::Offline); }
            let cached = cached.and_then(|(_, path)| {
                let path = lib.paths.media_path(&path)?;
                lib.paths.locate_metadata(blocking, &path).ok().flatten().map(|(path, metadata)| (path, metadata.len()))
            });
            // Joins/cancellation/slot installation still share the short
            // inflight lock. No inflight lock survives an await or DB lookup.
            lib.start_fetch_prepared(blocking, &song, &variant, playback, burst, request, cached, generation, gate)
        }).await.map_err(|_| LibError::Server("Stream preparation worker stopped".into()))?
    }
}
