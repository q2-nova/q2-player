//! One background owner for account-scoped spoken metadata and artwork.
use super::{Library, LibEvent, Policy};
use super::shelf_browse::{Media, SyncRound};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::time::Duration;

const FRESHNESS: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status { Waiting, Syncing, Current, Unavailable }
impl Status {
    const ALL: [Status; 4] = [Status::Waiting, Status::Syncing, Status::Current, Status::Unavailable];
    /// The stored code: 0 before the first status, then 1 + the variant.
    fn code(self) -> u8 { 1 + Self::ALL.iter().position(|s| *s == self).expect("a status") as u8 }
}
pub(super) struct AutoSync {
    pub wake: Arc<tokio::sync::Notify>,
    pub reconnect: AtomicBool,
    /// The screen is on or the battery is charging: the sync may use the
    /// radio. With the screen off on battery it waits, so its pages and
    /// covers never hold the radio out of power save (V0.129: 30-100 KB/s
    /// for 15 minutes, paused and dark, after a restart).
    allowed: AtomicBool,
    /// [`Status::code`]; 0 before the worker publishes one.
    status: AtomicU8,
    started: AtomicBool,
    stopping: AtomicBool,
    stopped: tokio::sync::watch::Sender<bool>,
    /// Counts for the device measurement log (`library sync measure`):
    /// Audiobookshelf pages fetched whole and answered 304, and album
    /// catalog requests (scan markers and pages).
    pub(super) pages_fetched: AtomicU32,
    pub(super) pages_unchanged: AtomicU32,
    pub(super) catalog_requests: AtomicU32,
    /// `abs-full`: the next round sends no ETags (a full sync's requests).
    pub(super) unconditional: AtomicBool,
    /// The battery is charging: a background full resync may run.
    charging: AtomicBool,
    /// Asked from the connection page: 1 sync now, 2 full resync.
    request: AtomicU8,
    /// Hours between background full resyncs (0: none).
    full_hours: AtomicU32,
    /// The running round is a full resync.
    full_running: AtomicBool,
    /// Entries stored by the running round, for the connection page.
    pub(super) progress: AtomicU32,
    /// One library sync at a time (the automatic worker, an explicit sync).
    pub(super) sync_lock: tokio::sync::Mutex<()>,
}
impl Default for AutoSync {
    fn default() -> Self {
        let (stopped, _) = tokio::sync::watch::channel(false);
        Self { wake: Arc::default(), reconnect: AtomicBool::new(false), allowed: AtomicBool::new(true), status: AtomicU8::new(0), started: AtomicBool::new(false), stopping: AtomicBool::new(false), stopped,
            pages_fetched: AtomicU32::new(0), pages_unchanged: AtomicU32::new(0), catalog_requests: AtomicU32::new(0), unconditional: AtomicBool::new(false),
            charging: AtomicBool::new(false), request: AtomicU8::new(0), full_hours: AtomicU32::new(24), full_running: AtomicBool::new(false), progress: AtomicU32::new(0), sync_lock: tokio::sync::Mutex::const_new(()) }
    }
}
impl Library {
    /// Stop and join the automatic worker before network and card shutdown.
    pub async fn stop_shelf_auto_sync(&self) {
        self.shelf_auto.stopping.store(true, Ordering::Release);
        self.shelf_auto.wake.notify_one();
        if !self.shelf_auto.started.load(Ordering::Acquire) { return; }
        let mut stopped = self.shelf_auto.stopped.subscribe();
        while !*stopped.borrow_and_update() {
            if stopped.changed().await.is_err() { break; }
        }
    }
    /// The UI: the screen is on or the battery charges. An automatic sync
    /// runs only then; one under way stops at its next step and resumes.
    pub fn set_background_sync_allowed(&self, on: bool) {
        if self.shelf_auto.allowed.swap(on, Ordering::AcqRel) != on { self.shelf_auto.wake.notify_one(); }
    }
    fn background_sync_allowed(&self) -> bool { self.shelf_auto.allowed.load(Ordering::Acquire) }
    /// The UI: the battery charges. A background full resync runs only then.
    pub fn set_background_sync_charging(&self, on: bool) {
        if self.shelf_auto.charging.swap(on, Ordering::AcqRel) != on { self.shelf_auto.wake.notify_one(); }
    }
    /// The connection page: sync now (the markers, and what moved), or a
    /// full resync of every library.
    pub fn request_shelf_sync(&self, full: bool) {
        self.shelf_auto.request.fetch_max(if full { 2 } else { 1 }, Ordering::AcqRel);
        self.shelf_auto.wake.notify_one();
    }
    /// Whether the running round is a full resync, and the entries it has
    /// stored so far.
    pub fn shelf_sync_progress(&self) -> (bool, u32) {
        (self.shelf_auto.full_running.load(Ordering::Acquire), self.shelf_auto.progress.load(Ordering::Relaxed))
    }
    pub(super) fn set_shelf_full_interval(&self, interval: Option<Duration>) {
        let hours = interval.map_or(0, |i| u32::try_from(i.as_secs() / 3600).unwrap_or(u32::MAX).max(1));
        self.shelf_auto.full_hours.store(hours, Ordering::Release);
    }
    pub fn shelf_sync_status(&self) -> Status {
        match self.shelf_auto.status.load(Ordering::Acquire) { 0 => Status::Waiting, code => Status::ALL[usize::from(code - 1)] }
    }
    fn shelf_sync_set_status(&self, status: Status) {
        if self.shelf_auto.status.swap(status.code(), Ordering::AcqRel) != status.code() {
            (self.notify)(LibEvent::ShelfSync);
        }
    }
}

struct Finished(tokio::sync::watch::Sender<bool>);
impl Drop for Finished {
    fn drop(&mut self) { self.0.send_replace(true); }
}

/// The card's record of the account's last complete sync: `shelf-auto:`
/// and its namespace, holding the Unix second it completed (`null` while a
/// sync is under way). A restart within [`FRESHNESS`] does not sync again.
fn stamp_key(namespace: &str) -> String { format!("shelf-auto:{namespace}") }

/// Seconds since the account's last complete sync, if one is recorded.
async fn synced_age(lib: &Arc<Library>, generation: &Arc<()>) -> Option<Duration> {
    let namespace = lib.shelf_namespace();
    if namespace.is_empty() { return None; }
    let key = stamp_key(&namespace);
    let body = lib.shelf_db_run(generation.clone(), move |blocking, db| db.spoken_response(blocking, &key).map_err(|e| super::LibError::Server(e.to_string()))).await.ok()??;
    let at = serde_json::from_str::<Option<i64>>(&body).ok()??;
    let age = super::db::now().checked_sub(at).filter(|age| *age >= 0)?;
    Some(Duration::from_secs(age as u64))
}

/// The account's last complete full resync (`shelf-full:`), as an age.
async fn full_age(lib: &Arc<Library>, generation: &Arc<()>) -> Option<Duration> {
    let namespace = lib.shelf_namespace();
    if namespace.is_empty() { return None; }
    let key = format!("shelf-full:{namespace}");
    let body = lib.shelf_db_run(generation.clone(), move |blocking, db| db.spoken_response(blocking, &key).map_err(|e| super::LibError::Server(e.to_string()))).await.ok()??;
    let at = serde_json::from_str::<i64>(&body).ok()?;
    let age = super::db::now().checked_sub(at).filter(|age| *age >= 0)?;
    Some(Duration::from_secs(age as u64))
}
async fn record_full(lib: &Arc<Library>, generation: &Arc<()>) {
    let namespace = lib.shelf_namespace();
    if namespace.is_empty() { return; }
    let (key, body) = (format!("shelf-full:{namespace}"), super::db::now().to_string());
    let _ = lib.shelf_db_run(generation.clone(), move |blocking, db| db.put_spoken_response(blocking, &key, &body).map_err(|e| super::LibError::Server(e.to_string()))).await;
}

/// Whether the record was written (an account not yet verified has none).
async fn record_stamp(lib: &Arc<Library>, generation: &Arc<()>, at: Option<i64>) -> bool {
    let namespace = lib.shelf_namespace();
    if namespace.is_empty() { return true; }
    let (key, body) = (stamp_key(&namespace), serde_json::to_string(&at).expect("a stamp"));
    lib.shelf_db_run(generation.clone(), move |blocking, db| db.put_spoken_response(blocking, &key, &body).map_err(|e| super::LibError::Server(e.to_string()))).await.is_ok()
}

/// The wait after `failures` consecutive incomplete syncs: [`RETRY`],
/// doubling, at most [`FRESHNESS`].
fn backoff(failures: u32) -> Duration {
    RETRY.saturating_mul(1 << failures.min(8)).min(FRESHNESS)
}

/// Device measurement (`tests/perf/sync_device_measure.py`): a file
/// `sync-now` in the data folder, read and removed at start. A line `abs`
/// syncs Audiobookshelf now, ignoring a complete sync within freshness; a
/// line `abs-full` does so without ETags (every page fetched whole); a
/// line `catalog` opens the album catalog as Albums does (the scan-marker
/// check); `catalog-full` builds it again. Each logs one numeric
/// `library sync measure` line. Returns whether `abs` was asked for.
async fn measurement_trigger(lib: &Arc<Library>) -> bool {
    let path = lib.paths.root.join("sync-now");
    let text = crate::rt::spawn_blocking(move |_| {
        let text = std::fs::read_to_string(&path).ok();
        if text.is_some() { let _ = std::fs::remove_file(&path); }
        text
    }).await.ok().flatten().unwrap_or_default();
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let catalog = lines.iter().find_map(|l| match *l { "catalog" => Some(false), "catalog-full" => Some(true), _ => None });
    if let Some(full) = catalog {
        let lib = lib.clone();
        crate::rt::io().spawn(async move {
            // After the configuration is applied and the server answers (the
            // radio associates after start), at most two minutes.
            let _ = lib.configuration_ready().await;
            for _ in 0..120 { if lib.is_online() { break; } tokio::time::sleep(Duration::from_secs(1)).await; }
            let (started, before) = (std::time::Instant::now(), lib.shelf_auto.catalog_requests.load(Ordering::Relaxed));
            let result = if full { lib.refresh_album_catalog(crate::ui::list::index_letter).await } else { lib.browse_album_catalog(crate::ui::list::index_letter).await };
            log::info!("library sync measure: catalog mode={} ms={} requests={} albums={} ok={}", if full { "full" } else { "browse" },
                started.elapsed().as_millis(), lib.shelf_auto.catalog_requests.load(Ordering::Relaxed).wrapping_sub(before),
                result.as_ref().map_or(0, |c| c.len()), result.is_ok());
        });
    }
    if lines.contains(&"abs-full") { lib.shelf_auto.unconditional.store(true, Ordering::Release); }
    lines.iter().any(|l| *l == "abs" || *l == "abs-full")
}

pub(super) fn start(lib: &Arc<Library>) {
    if lib.shelf_auto.started.swap(true, Ordering::AcqRel) { return; }
    spawn_worker(lib);
}

/// Tests: a new worker on the same library and card, as after a restart.
#[cfg(test)]
pub(super) fn restart_for_test(lib: &Arc<Library>) {
    lib.shelf_auto.stopping.store(false, Ordering::Release);
    lib.shelf_auto.stopped.send_replace(false);
    lib.shelf_auto.status.store(0, Ordering::Release);
    lib.shelf_auto.started.store(true, Ordering::Release);
    spawn_worker(lib);
}

fn spawn_worker(lib: &Arc<Library>) {
    let stopped = lib.shelf_auto.stopped.clone();
    let weak = Arc::downgrade(lib);
    let wake = lib.shelf_auto.wake.clone();
    crate::rt::io().spawn(async move {
        let _finished = Finished(stopped);
        let mut wait = Duration::ZERO;
        let mut due = tokio::time::Instant::now();
        let mut last_generation: Option<Arc<()>> = None;
        let mut last_result = Status::Waiting;
        let mut failures = 0u32;
        // Kept while a sync is interrupted; a new sync starts without it.
        // With when it started: a round older than FRESHNESS starts again, so
        // a sync reported current never holds data older than that.
        let mut round: Option<(SyncRound, tokio::time::Instant)> = None;
        // The account's completion record has been looked for. A new client
        // (a proxy change replaces it) has no namespace until verified, and
        // the record is kept by namespace.
        let mut stamp_checked = false;
        // A measurement asked for a sync now (see [`measurement_trigger`]).
        let mut forced = match weak.upgrade() { Some(lib) => measurement_trigger(&lib).await, None => false };
        // A full resync asked for from the connection page, until it runs.
        let mut asked_full = match weak.upgrade() { Some(lib) => lib.shelf_auto.unconditional.load(Ordering::Acquire), None => false };
        loop {
            tokio::select! { _ = wake.notified() => {}, _ = tokio::time::sleep(wait) => {} }
            let Some(lib) = weak.upgrade() else { break };
            if lib.shelf_auto.stopping.load(Ordering::Acquire) { break; }
            let asked = lib.shelf_auto.request.swap(0, Ordering::AcqRel);
            if asked == 2 { asked_full = true; }
            let generation = lib.shelf_generation();
            if last_generation.as_ref().is_none_or(|old| !Arc::ptr_eq(old, &generation)) {
                due = tokio::time::Instant::now();
                last_result = Status::Waiting;
                last_generation = Some(generation.clone());
                failures = 0;
                round = None;
                stamp_checked = !lib.shelf_namespace().is_empty() || forced;
                // A complete sync of this account within FRESHNESS (before a
                // restart) is current: the next one is due when it expires.
                if std::mem::take(&mut forced) {} else if let Some(age) = synced_age(&lib, &generation).await.filter(|age| *age < FRESHNESS) {
                    last_result = Status::Current;
                    due += FRESHNESS - age;
                }
            }
            // A reconnect retries a sync that failed or was interrupted at
            // once; a current sync stays current (a radio doze ends with a
            // reconnect every few minutes during cached playback).
            if lib.shelf_auto.reconnect.swap(false, Ordering::AcqRel) && last_result != Status::Current { due = tokio::time::Instant::now(); }
            if asked != 0 { due = tokio::time::Instant::now(); }
            if !lib.maintenance_link_ready() || lib.shelf.read().unwrap().client.is_none() {
                lib.shelf_sync_set_status(if last_result == Status::Current { Status::Current } else { Status::Waiting });
                wait = FRESHNESS;
                continue;
            }
            if lib.playback_transfer_running() || lib.spoken_playback_active() {
                lib.shelf_sync_set_status(if last_result == Status::Current { Status::Current } else { Status::Waiting });
                wait = RETRY;
                continue;
            }
            if !lib.background_sync_allowed() {
                // The screen or the charger wakes this owner.
                lib.shelf_sync_set_status(if last_result == Status::Current { Status::Current } else { Status::Waiting });
                wait = FRESHNESS;
                continue;
            }
            if !stamp_checked && round.is_none() {
                // Verify the account (one small request, which the sync makes
                // anyway), then trust a complete sync of it as at a restart.
                if lib.shelf_namespace().is_empty() { let _ = lib.shelf_client_async().await; }
                if !lib.shelf_namespace().is_empty() && lib.shelf_generation_current(&generation) {
                    stamp_checked = true;
                    if let Some(age) = synced_age(&lib, &generation).await.filter(|age| *age < FRESHNESS) {
                        last_result = Status::Current;
                        due = tokio::time::Instant::now() + (FRESHNESS - age);
                    }
                }
            }
            // A full resync: asked for, or in the background while charging
            // once its interval has passed (and only then).
            let hours = lib.shelf_auto.full_hours.load(Ordering::Acquire);
            let full_due = asked_full || (hours > 0 && lib.shelf_auto.charging.load(Ordering::Acquire)
                && full_age(&lib, &generation).await.is_none_or(|age| age >= Duration::from_secs(u64::from(hours) * 3600)));
            if full_due && round.as_ref().is_none_or(|(r, _)| !r.is_full()) {
                round = None;
                due = tokio::time::Instant::now();
            }
            if tokio::time::Instant::now() < due {
                lib.shelf_sync_set_status(last_result);
                wait = due.saturating_duration_since(tokio::time::Instant::now());
                continue;
            }
            lib.shelf_sync_set_status(Status::Syncing);
            let measured = (std::time::Instant::now(), lib.shelf_auto.pages_fetched.load(Ordering::Relaxed), lib.shelf_auto.pages_unchanged.load(Ordering::Relaxed));
            if round.as_ref().is_some_and(|(_, started)| started.elapsed() >= FRESHNESS) { round = None; }
            let progress = match &round {
                Some((round, _)) => round.clone(),
                None => {
                    // The record says no complete sync until this one ends;
                    // without it a restart could trust half-stored data.
                    if !record_stamp(&lib, &generation, None).await {
                        lib.shelf_sync_set_status(Status::Unavailable);
                        wait = backoff(failures); failures = failures.saturating_add(1);
                        last_result = Status::Unavailable;
                        due = tokio::time::Instant::now() + wait;
                        continue;
                    }
                    let kind = if full_due { SyncRound::full() } else { SyncRound::default() };
                    round.insert((kind, tokio::time::Instant::now())).0.clone()
                }
            };
            let full = progress.is_full();
            lib.shelf_auto.full_running.store(full, Ordering::Release);
            lib.shelf_auto.progress.store(0, Ordering::Relaxed);
            (lib.notify)(LibEvent::ShelfSync);
            let owner = lib.clone();
            let work = async move {
                let (mut incomplete, mut covers_missing) = (false, 0u64);
                for media in [Media::Book, Media::Podcast] {
                    let choices = match owner.shelf_library_choices_async(media, Policy::NetworkOnly).await {
                        Ok(choices) => choices,
                        Err(e) => { log::warn!("library sync: {media:?} libraries: {e}"); incomplete = true; continue; }
                    };
                    for choice in choices.libraries {
                        let reporter = owner.clone();
                        let base = reporter.shelf_auto.progress.load(Ordering::Relaxed);
                        let report = move |p: super::shelf_browse::SyncProgress| {
                            let now = base.saturating_add(u32::try_from(p.entries).unwrap_or(u32::MAX));
                            // The connection page shows the count: tell it every 200 entries.
                            if reporter.shelf_auto.progress.swap(now, Ordering::Relaxed) / 200 != now / 200 { (reporter.notify)(LibEvent::ShelfSync); }
                        };
                        match owner.sync_shelf_library_from(choice.id, media, &progress, report).await {
                            Ok(progress) => covers_missing += progress.unavailable_covers,
                            Err(e) => { log::warn!("library sync: a {media:?} library: {e}"); incomplete = true; }
                        }
                    }
                }
                // Ok(n): every page is stored; n covers could not be.
                if incomplete { Err(super::LibError::Server("Some library data is unavailable".into())) } else { Ok(covers_missing) }
            };
            tokio::pin!(work);
            let result = loop {
                tokio::select! {
                    biased;
                    _ = wake.notified() => {
                        if lib.shelf_auto.stopping.load(Ordering::Acquire) || !lib.maintenance_link_ready() || !lib.shelf_generation_current(&generation)
                            || lib.playback_transfer_running() || lib.spoken_playback_active() || !lib.background_sync_allowed() { break None; }
                    }
                    result = &mut work => break Some(result),
                }
            };
            if lib.shelf_auto.stopping.load(Ordering::Acquire) { break; }
            log::info!("library sync measure: abs ms={} pages_fetched={} pages_unchanged={} result={}", measured.0.elapsed().as_millis(),
                lib.shelf_auto.pages_fetched.load(Ordering::Relaxed).wrapping_sub(measured.1), lib.shelf_auto.pages_unchanged.load(Ordering::Relaxed).wrapping_sub(measured.2),
                match &result { Some(Ok(0)) => "complete", Some(Ok(_)) => "covers_missing", Some(Err(_)) => "failed", None => "interrupted" });
            if result.is_some() { lib.shelf_auto.unconditional.store(false, Ordering::Release); }
            lib.shelf_auto.full_running.store(false, Ordering::Release);
            if full && matches!(result, Some(Ok(_))) && lib.shelf_generation_current(&generation) {
                record_full(&lib, &generation).await;
                asked_full = false;
            }
            match result {
                Some(Ok(0)) if lib.shelf_generation_current(&generation) => {
                    record_stamp(&lib, &generation, Some(super::db::now())).await;
                    lib.shelf_sync_set_status(Status::Current); wait = FRESHNESS; failures = 0; round = None;
                }
                // Every page is stored; only covers are missing. The data is
                // complete (a restart within FRESHNESS does not sync again),
                // and the kept round retries just those covers, backing off,
                // instead of refetching every page each time.
                Some(Ok(missing)) if lib.shelf_generation_current(&generation) => {
                    log::info!("library sync complete; {missing} cover(s) unavailable, retried later");
                    record_stamp(&lib, &generation, Some(super::db::now())).await;
                    lib.shelf_sync_set_status(Status::Current); wait = backoff(failures); failures = failures.saturating_add(1);
                }
                Some(Err(_)) if lib.shelf_generation_current(&generation) => {
                    lib.shelf_sync_set_status(Status::Unavailable); wait = backoff(failures); failures = failures.saturating_add(1); round = None;
                }
                // Interrupted: resume from `round` once allowed.
                _ => { lib.shelf_sync_set_status(Status::Waiting); wait = Duration::ZERO; }
            }
            last_result = lib.shelf_sync_status();
            due = tokio::time::Instant::now() + wait;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_syncs_back_off_to_freshness() {
        let minutes: Vec<u64> = (0..8).map(|n| backoff(n).as_secs() / 60).collect();
        assert_eq!(minutes, [5, 10, 20, 40, 80, 160, 320, 360]);
        assert_eq!(backoff(u32::MAX), FRESHNESS);
    }
}
