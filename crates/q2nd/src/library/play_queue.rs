//! Coalesced player snapshot; only the DB owner performs persistence.
//! The mailbox is never held over SQL, a DB wait, or a network request.
use super::{Db, Library, QueueSave, SavedQueue};
use std::{future::Future, sync::{Arc, Mutex}};
use subsonic::{Song, calls};

type Mailbox = Arc<Mutex<Option<QueueSave>>>;

/// Songs a server queue sync names at most. savePlayQueue is a GET with one
/// `id` per song: 5,000 songs made a target over HTTP's 64 KiB URI limit.
/// 1,000 IDs of up to 40 bytes stay under 48 KiB.
const SYNC_IDS: usize = 1000;

/// The IDs a server queue sync sends: all of them, or for a longer queue the
/// current song and the ones after it, up to SYNC_IDS. The queue on the card
/// keeps every song.
fn sync_ids(songs: &[Song], index: usize) -> Vec<String> {
    let start = if songs.len() <= SYNC_IDS { 0 } else { index.min(songs.len().saturating_sub(SYNC_IDS)) };
    songs.iter().skip(start).take(SYNC_IDS).map(|song| song.id.clone()).collect()
}

fn read(blocking: &crate::rt::Blocking, db: &Db) -> Option<(Vec<Song>, usize, u64)> {
    let text = db.get(blocking, "queue")?;
    let queue: SavedQueue = serde_json::from_str(&text).ok()?;
    Some((queue.songs, queue.index, queue.position_ms))
}

fn flush(blocking: &crate::rt::Blocking, db: &Db, mailbox: &Mailbox) -> Result<Option<SavedQueue>, String> {
    let Some(queue) = mailbox.lock().unwrap().take() else { return Ok(None); };
    let saved = SavedQueue { songs: queue.songs, index: queue.index, position_ms: queue.position_ms };
    let result = serde_json::to_string(&saved).map_err(|_| "Queue serialization failed".to_string())
        .and_then(|text| db.set_checked(blocking, "queue", &text).map_err(|_| "Queue storage failed".to_string()));
    if let Err(error) = result {
        // Another producer may submit while this write is in flight. A failed
        // older snapshot must not replace that newer pending snapshot. Every
        // consumer runs on this one DB owner, so no newer write can overtake us.
        let mut pending = mailbox.lock().unwrap();
        if pending.is_none() {
            *pending = Some(QueueSave { songs: saved.songs, index: saved.index, position_ms: saved.position_ms });
        }
        return Err(error);
    }
    Ok(Some(saved))
}

impl Library {
    #[cfg(test)]
    pub fn saved_queue(&self) -> Option<(Vec<Song>, usize, u64)> {
        self.db_worker.run_blocking(|blocking, db| read(blocking, db))
    }
    pub fn saved_queue_async(&self) -> impl Future<Output = Option<(Vec<Song>, usize, u64)>> + Send + 'static {
        self.db_run(|blocking, db| read(blocking, db))
    }
    fn queue_flush(&self) -> impl Future<Output = Result<Option<SavedQueue>, String>> + Send + 'static {
        let mailbox = self.queue_save.clone();
        self.db_run(move |blocking, db| flush(blocking, db, &mailbox))
    }
    /// Settings/client preparation stays outside DB ownership. Its finite
    /// locks may contend with configuration, so only this stage uses a worker.
    fn queue_sync_request(&self, saved: Option<SavedQueue>) -> Option<subsonic::Request> {
        let saved = saved?;
        if !saved.songs.iter().any(|song| song.spoken.is_some()) && self.settings.read().unwrap().sync_queue && self.is_online() {
            let ids = sync_ids(&saved.songs, saved.index);
            let current = saved.songs.get(saved.index).map(|song| song.id.as_str());
            Some(calls::save_play_queue(&ids, current, saved.position_ms).request)
        } else { None }
    }
    async fn checked_queue_sync_request_async(self: &Arc<Self>) -> Result<Option<subsonic::Request>, String> {
        let saved = self.queue_flush().await?;
        if saved.is_none() { return Ok(None); }
        let lib = self.clone();
        crate::rt::spawn_blocking(move |_| lib.queue_sync_request(saved)).await
            .map_err(|_| "Queue request preparation stopped".into())
    }
    pub(super) async fn flush_queue_async(self: &Arc<Self>) {
        match self.checked_queue_sync_request_async().await {
            Ok(Some(request)) => { let _ = self.fetch_net(&request).await; }
            Ok(None) => {}
            Err(error) => log::warn!("{error}"),
        }
    }
    #[cfg(test)]
    pub(super) fn flush_queue(&self) {
        let owner = self.me.upgrade().expect("library alive");
        super::configuration::wait(async move { owner.flush_queue_async().await; Ok(()) }).unwrap();
    }
    pub async fn save_now_async(self: &Arc<Self>) -> Result<(), String> {
        self.configuration_ready().await?;
        self.db_run(|_, _| ()).await;
        self.check_music_writes()?;
        if let Some(request) = self.checked_queue_sync_request_async().await? {
            let _ = self.fetch_net(&request).await;
        }
        Ok(())
    }
    pub fn save_now(&self) {
        let owner = self.me.upgrade().expect("library alive");
        if let Err(error) = super::configuration::wait(async move { owner.save_now_async().await }) {
            log::warn!("{error}");
        }
    }
    /// Includes all admitted DB writes plus the latest coalesced player snapshot.
    /// No optional network update is part of the sleep/export durability barrier.
    pub async fn prepare_sleep_async(&self) -> Result<(), String> {
        self.configuration_ready().await?;
        self.db_run(|_, _| ()).await;
        self.check_music_writes()?;
        let mailbox = self.queue_save.clone();
        self.db_run(move |blocking, db| {
            flush(blocking, db, &mailbox)?;
            db.checkpoint(blocking).map_err(|_| "Library checkpoint failed; transition cancelled".to_string())
        }).await
    }
    #[cfg(test)]
    pub fn prepare_sleep(&self) -> Result<(), String> {
        let owner = self.me.upgrade().expect("library alive");
        super::configuration::wait(async move { owner.prepare_sleep_async().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use std::time::{Duration, Instant};

    fn fixture() -> Arc<Library> {
        Library::create(Paths::new(super::super::generated_test_directory("queue-owner")), &Config::default(), Arc::new(|_| {})).0
    }

    #[test]
    fn a_long_queue_syncs_the_current_song_and_those_after_it() {
        let songs: Vec<Song> = (0..5000).map(|n| Song { id: format!("generated-song-{n:05}-0123456789abcdef"), ..Default::default() }).collect();
        let short = sync_ids(&songs[..800], 400);
        assert_eq!((short.len(), short[0].as_str()), (800, "generated-song-00000-0123456789abcdef"));
        let ids = sync_ids(&songs, 2500);
        assert_eq!((ids.len(), ids[0].as_str()), (SYNC_IDS, "generated-song-02500-0123456789abcdef"));
        // Near the end the window keeps SYNC_IDS songs, the current among them.
        let end = sync_ids(&songs, 4990);
        assert_eq!((end.len(), end[0].as_str()), (SYNC_IDS, "generated-song-04000-0123456789abcdef"));
        let request = calls::save_play_queue(&ids, Some(&songs[2500].id), 0).request;
        let client = subsonic::Client::new(subsonic::Credentials { server: "http://127.0.0.1:9".into(), user: "generated".into(),
            auth: subsonic::Auth::Password("generated".into()) }, "Q2QueueSyncFixture");
        assert!(client.url(&request).len() < 60 * 1024, "{} bytes", client.url(&request).len());
    }
    fn submit(lib: &Library, position: u64) {
        *lib.queue_save.lock().unwrap() = Some(QueueSave { songs: vec![Song { id: "generated-queue-owner".into(), ..Default::default() }], index: 0, position_ms: position });
    }
    fn wait_taken(lib: &Library) {
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        while lib.queue_save.lock().unwrap().is_some() {
            assert!(Instant::now() < deadline, "queue owner did not begin the write");
            std::thread::yield_now();
        }
    }
    #[test]
    fn failed_inflight_queue_write_preserves_newer_pending_snapshot() {
        let lib = fixture();
        let connection = rusqlite::Connection::open(lib.paths.database()).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_old_queue BEFORE INSERT ON kv WHEN NEW.key='queue' AND instr(NEW.value,'\"position_ms\":700')>0 BEGIN SELECT RAISE(ABORT,'generated older-write failure'); END; BEGIN IMMEDIATE;").unwrap();
        submit(&lib, 700);
        let older = lib.queue_flush();
        wait_taken(&lib);
        submit(&lib, 900);
        connection.execute_batch("ROLLBACK").unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            assert_eq!(tokio::time::timeout(crate::library::TEST_LIVENESS, older).await.unwrap().err().unwrap(), "Queue storage failed");
            assert_eq!(lib.queue_save.lock().unwrap().as_ref().unwrap().position_ms, 900);
            lib.prepare_sleep_async().await.unwrap();
            assert_eq!(lib.saved_queue_async().await.unwrap().2, 900);
        });
        assert!(lib.queue_save.lock().unwrap().is_none());
    }
    #[test]
    fn abandoned_queue_observer_keeps_owned_write_and_later_barrier_ordered() {
        let lib = fixture();
        let connection = rusqlite::Connection::open(lib.paths.database()).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        submit(&lib, 700);
        drop(lib.queue_flush());
        wait_taken(&lib);
        submit(&lib, 900);
        crate::rt::Ui::new().unwrap().block_on(async {
            let barrier = lib.prepare_sleep_async();
            tokio::pin!(barrier);
            tokio::select! {
                result = &mut barrier => panic!("barrier overtook SQLite writer: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            connection.execute_batch("ROLLBACK").unwrap();
            tokio::time::timeout(crate::library::TEST_LIVENESS, barrier).await.unwrap().unwrap();
            assert_eq!(lib.saved_queue_async().await.unwrap().2, 900);
        });
        let reopened = Db::open(&crate::rt::Blocking::dedicated_thread(), &lib.paths.database()).unwrap();
        assert_eq!(read(&crate::rt::Blocking::dedicated_thread(), &reopened).unwrap().2, 900);
    }
}
