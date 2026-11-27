//! Fetch-ahead scheduling on the existing I/O runtime. Database snapshots are
//! awaited; only finite card checks/preparation use workers.
use super::{Library, PriorityWait, RefillEvent, Settings, PREFETCH_SETTLE,
            can_prefetch_audio, fetch_targets, fetch_targets_minutes, prefetch_bytes, space_admitted, variant};
use std::sync::{Arc, Weak, atomic::Ordering};
use subsonic::Song;
use tokio::sync::watch;

// Fetch-ahead only wants the newest queue. A watch retains one queue instead of
// accumulating obsolete copies while a card operation or track open is pending.
async fn latest(rx: &mut watch::Receiver<Option<Vec<Song>>>) -> Option<Vec<Song>> {
    rx.changed().await.ok()?;
    rx.borrow_and_update().clone()
}

async fn plan(lib: &Arc<Library>, songs: Arc<Vec<Song>>) -> Result<(Settings, Vec<usize>, bool), super::LibError> {
    let planner = lib.clone(); let upcoming = songs.clone();
    let settings = crate::rt::spawn_blocking(move |_| {
        let settings = planner.settings.read().unwrap().clone();
        for song in upcoming.iter().take(settings.prefetch) { planner.art.prefetch_song(song); }
        settings
    }).await.map_err(|_| super::LibError::Offline)?;
    let local = lib.local_files_async(songs.clone(), settings.stream_quality).await?;
    let ready = local.iter().take(settings.prefetch.max(1)).all(Option::is_some);
    let policy = settings.clone(); let upcoming = songs.clone();
    let targets = crate::rt::spawn_blocking(move |_| {
        let (settings, songs) = (policy, upcoming);
        let local: Vec<bool> = local.iter().map(Option::is_some).collect();
        if settings.fetch_ms > 0 {
            let durations: Vec<u64> = songs.iter().map(|song| u64::from(song.duration) * 1000).collect();
            let eligible: Vec<bool> = songs.iter().map(|song| can_prefetch_audio(song, &variant(song, settings.stream_quality))).collect();
            fetch_targets_minutes(&local, &eligible, &durations, settings.prefetch, settings.fetch_ms, settings.fetch_low_ms)
        } else { fetch_targets(&local, settings.prefetch, settings.batch) }
    }).await.map_err(|_| super::LibError::Offline)?;
    if targets.is_empty() { return Ok((settings, targets, ready)); }
    // A song the card cannot keep is not fetched ahead (`space_admitted`).
    let used = lib.db_run(|blocking, db| db.song_cache_size_checked(blocking)).await.map_err(|_| super::LibError::Offline)?;
    let (card, policy) = (lib.clone(), settings.clone());
    let targets = crate::rt::spawn_blocking(move |blocking| {
        let free = super::free_space(blocking, card.paths.media_root());
        let bytes: Vec<Option<u64>> = songs.iter().map(|song| prefetch_bytes(song, &variant(song, policy.stream_quality))).collect();
        let admitted = space_admitted(&targets, &bytes, policy.cache_bytes, policy.reserve_bytes, used, free);
        if admitted.len() < targets.len() {
            log::info!("fetch-ahead: card room for {} of {} songs", admitted.len(), targets.len());
        }
        admitted
    }).await.map_err(|_| super::LibError::Offline)?;
    Ok((settings, targets, ready))
}

pub(super) async fn run(weak: Weak<Library>, mut rx: watch::Receiver<Option<Vec<Song>>>) {
    if let Some(lib) = weak.upgrade() {
        let owner = lib.clone();
        let deletions = crate::rt::spawn_blocking(move |blocking| owner.sweep_partials(blocking)).await.expect("prefetch startup");
        // A staged cache deletion owns these names through its index transaction.
        // An ordinary startup with no tombstones needs no publication permit.
        if !deletions.is_empty() {
            let gate = lib.offline_art_gate.acquire().await;
            crate::rt::spawn_blocking(move |blocking| {
                let _gate = gate;
                for path in deletions { let _ = crate::shard::remove(blocking, &path); }
            }).await.expect("cache deletion cleanup");
        }
    }
    let mut next = None;
    loop {
        let mut songs = match next.take() {
            Some(songs) => songs,
            None => match latest(&mut rx).await { Some(songs) => songs, None => return },
        };
        // Preserve the fixed track-open settling interval and coalesce skips.
        let settle = tokio::time::sleep(PREFETCH_SETTLE);
        tokio::pin!(settle);
        loop {
            tokio::select! {
                request = latest(&mut rx) => match request { Some(newer) => songs = newer, None => return },
                () = &mut settle => break,
            }
        }
        let Some(lib) = weak.upgrade() else { return };
        let songs = Arc::new(songs);
        let Ok((settings, targets, ready)) = plan(&lib, songs.clone()).await else { continue };
        lib.fetching.store(!targets.is_empty(), Ordering::Relaxed);
        if !targets.is_empty() { log::info!("fetch-ahead {} songs t={}", targets.len(), crate::metrics::uptime()); }
        let burst = !targets.is_empty();
        let burst_id = if burst { lib.refill.record(RefillEvent::BurstBegin { targets: targets.len() }) } else { 0 };
        if burst {
            lib.trace().event(crate::playtrace::Kind::FetchAhead, targets.len() as u32, 0, 0);
            crate::ux::record(crate::ux::Mark::Fetch, crate::ux::tagged(0, targets.len() as u64));
        } else { crate::ux::record(crate::ux::Mark::Fetch, crate::ux::tagged(3, 0)); }
        let mut burst_end = 1;
        for index in targets {
            let song = &songs[index];
            if !lib.is_online() { burst_end = 3; break; }
            let quality = variant(song, settings.stream_quality);
            if !can_prefetch_audio(song, &quality) { continue; }
            match lib.wait_playback_priority(&mut rx).await {
                PriorityWait::Clear | PriorityWait::Timeout => {},
                PriorityWait::Offline => { burst_end = 3; break; },
                PriorityWait::Newer => { next = rx.borrow_and_update().clone(); burst_end = 2; break; },
            }
            let started = std::time::Instant::now();
            let transfer = lib.fetch_in_burst_async(song.clone(), quality, false, burst_id).await;
            if let Ok((_, progress, hold)) = transfer {
                // One transfer at a time. Completion and a newer queue wake
                // this task directly; neither requires a polling worker.
                tokio::select! {
                    biased;
                    () = progress.finished() => {},
                    request = latest(&mut rx) => {
                        drop(hold);
                        next = request;
                        let wanted: std::collections::HashSet<String> = next.iter().flatten().map(|s| s.id.clone()).collect();
                        lib.cancel_orphans(|key| wanted.contains(&key.0));
                        burst_end = 2;
                        break;
                    },
                }
                log::info!("fetch-ahead song done t={}", crate::metrics::uptime());
                crate::ux::record(crate::ux::Mark::Fetch, crate::ux::tagged(1, started.elapsed().as_secs()));
            }
        }
        lib.fetching.store(false, Ordering::Relaxed);
        if burst {
            lib.trace().event(crate::playtrace::Kind::FetchAheadEnd, burst_end, 0, 0);
            crate::ux::record(crate::ux::Mark::Fetch, crate::ux::tagged(2, u64::from(burst_end)));
        }
        if burst { lib.refill.record(RefillEvent::BurstEnd { burst: burst_id, reason: u64::from(burst_end) }); }
        // Enable overlapping offline reads/writes only after the immediate
        // upcoming tracks are actually present. A newer queue resets this
        // flag inside the same watch lock, including during the settle delay.
        if next.is_none() && !songs.is_empty() && matches!(rx.has_changed(), Ok(false)) {
            let ready = if burst && !ready {
                let near = songs.iter().take(settings.prefetch.max(1)).cloned().collect();
                lib.local_files_async(Arc::new(near), settings.stream_quality).await
                    .is_ok_and(|files| files.iter().all(Option::is_some))
            } else { ready };
            let queue = rx.borrow();
            if !queue.has_changed() { lib.prefetch_ready.store(ready, Ordering::Release); }
        }
        if rx.has_changed().is_err() { return; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_queue_is_retained_during_busy_work_and_receiver_retirement_is_observed() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let (sender, mut receiver) = watch::channel(None);
            for index in 0..100 {
                sender.send_replace(Some(vec![Song { id: index.to_string(), ..Song::default() }]));
            }
            let songs = latest(&mut receiver).await.unwrap();
            assert_eq!(songs[0].id, "99");
            assert!(!receiver.has_changed().unwrap());
            // An empty queue is a real update, distinct from sender shutdown.
            sender.send_replace(Some(Vec::new()));
            assert!(latest(&mut receiver).await.unwrap().is_empty());
            drop(sender);
            assert!(latest(&mut receiver).await.is_none());
        });
    }
}
