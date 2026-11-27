//! Download commands own their gate, durable DB job and finite card cleanup.
//! A UI reply is only an observer: dropping it cannot strand published files.
use super::{Library, MusicDownloadAction, LibEvent, db::{Db, OfflineSet}, download_intent::Intent};
use std::{future::Future, path::PathBuf, sync::{Arc, atomic::Ordering}};
use subsonic::Song;

enum Mutation {
    Queue { action: MusicDownloadAction, set: OfflineSet, songs: Vec<Song> },
    RemoveMusic { action: MusicDownloadAction, kind: String, id: String },
    RemoveSpoken(Intent),
}
impl Mutation {
    /// Indexed files may be flat (an earlier version) or sharded.
    fn remove_files(&self, blocking: &crate::rt::Blocking, lib: &Library, files: Vec<PathBuf>) -> bool {
        let spoken = matches!(self, Self::RemoveSpoken(_));
        let directory = lib.paths.spoken_downloads();
        let artwork = lib.paths.offline_art();
        let mut failed = false;
        for path in files {
            if (!spoken || path.starts_with(&directory) || path.starts_with(&artwork))
                && lib.paths.can_modify_media(&path) {
                failed |= lib.paths.remove_media(blocking, &path).is_err();
            }
        }
        failed
    }

    fn apply(&self, blocking: &crate::rt::Blocking, db: &mut Db, generation: u32) -> Result<Vec<PathBuf>, String> {
        match self {
            Self::Queue { action, set, songs } => {
                music_admission(action, generation, &set.kind, &set.id)?;
                if !action.configured { return Err("Set up Navidrome before downloading".into()); }
                db.enqueue_download(blocking, set, songs).map_err(|_| "Unable to queue downloads".to_string())?;
                Ok(Vec::new())
            }
            Self::RemoveMusic { action, kind, id } => {
                music_admission(action, generation, kind, id)?;
                db.remove_offline_set_checked(blocking, kind, id).map_err(|_| "Unable to remove downloads".into())
            }
            Self::RemoveSpoken(request) => {
                request.check()?;
                db.remove_spoken_queue(blocking, &request.logical).map_err(|_| "Unable to remove queued audiobook".to_string())?;
                db.remove_spoken_downloads(blocking, &request.logical).map_err(|_| "Unable to update the audiobook download index".into())
            }
        }
    }
}
pub(super) fn music_admission(action: &MusicDownloadAction, generation: u32, kind: &str, id: &str) -> Result<(), String> {
    if action.generation != generation { return Err("Navidrome changed; select the item again".into()); }
    action.intent.check()?;
    if action.intent.logical != format!("music:{kind}:{id}") { return Err("Download selection changed".into()); }
    Ok(())
}

impl Library {
    /// Capture ordering and the exact transport generation at the input event.
    /// Forced Offline still permits durable queueing. No card/DB work here.
    pub fn begin_music_download(&self, kind: &str, id: &str) -> Result<MusicDownloadAction, String> {
        let (generation, configured) = self.music_download_admission();
        Ok(MusicDownloadAction {
            intent: self.begin_download_intent(format!("music:{kind}:{id}"))?,
            generation, configured,
        })
    }
    pub fn download_requested_async(self: &Arc<Self>, action: MusicDownloadAction, set: OfflineSet, songs: Vec<Song>)
        -> impl Future<Output = Result<(), String>> + Send + 'static {
        self.start_download_mutation(Mutation::Queue { action, set, songs })
    }
    pub fn remove_download_requested_async(self: &Arc<Self>, action: MusicDownloadAction, kind: String, id: String)
        -> impl Future<Output = Result<(), String>> + Send + 'static {
        self.start_download_mutation(Mutation::RemoveMusic { action, kind, id })
    }
    pub fn remove_spoken_download_requested_async(self: &Arc<Self>, request: Intent)
        -> impl Future<Output = Result<(), String>> + Send + 'static {
        self.start_download_mutation(Mutation::RemoveSpoken(request))
    }
    fn start_download_mutation(self: &Arc<Self>, mutation: Mutation)
        -> impl Future<Output = Result<(), String>> + Send + 'static {
        let lib = self.clone();
        // Start eagerly: even an unpolled/dropped observer cannot cancel an
        // accepted command between its durable mutation and unlink cleanup.
        let task = crate::rt::io().spawn(async move {
            if let Mutation::Queue { action, .. } | Mutation::RemoveMusic { action, .. } = &mutation {
                lib.music_binding_ready(action.generation).await.map_err(|error| {
                    if lib.client_generation() != action.generation { "Navidrome changed; select the item again".into() }
                    else { error.to_string() }
                })?;
            }
            // Snapshot the queue before acquiring the file gate: configuration
            // may hold the shelf lock while waiting for that gate.
            let spoken_queue=if let Mutation::RemoveSpoken(request)=&mutation {
                let logical=request.logical.clone();let owner=lib.clone();
                let queue=crate::rt::spawn_blocking(move |_|owner.shelf.read().unwrap().queue.clone()).await.map_err(|_|"Download action worker stopped".to_string())?;
                Some((queue,logical))
            } else {None};
            let gate = lib.offline_art_gate.acquire().await;
            if !matches!(&mutation, Mutation::RemoveSpoken(_)) {lib.downloads.invalidate_revision();}
            let current = lib.client_generation.clone();
            let (mutation, files) = lib.db_run(move |blocking, db| {
                // No Library/client lock on the DB owner. Configured account
                // publication shares the gate; generation also rejects disable.
                let result = mutation.apply(blocking, db, current.load(Ordering::SeqCst));
                (mutation, result)
            }).await;
            if !matches!(&mutation, Mutation::RemoveSpoken(_)) {lib.downloads.invalidate_revision();}
            let files = files?;
            if let Some((queue,logical))=spoken_queue {queue.discard_saved(&logical);}
            if let Some(key)=lib.downloads.saved_key() {
                let (id,ticket,generation)=key.clone();
                let live=lib.client_generation()==generation && lib.db_run(move |blocking,db|db.download_request_current(blocking,&id,ticket).unwrap_or(false)).await;
                if !live {lib.downloads.discard_saved(&key);}
            }
            lib.downloads.interrupt();lib.downloads.wake();
            let playlist = match &mutation {
                Mutation::Queue { set, .. } if set.kind == "playlist" => Some(set.id.clone()),
                Mutation::RemoveMusic { kind, id, .. } if kind == "playlist" => Some(id.clone()),
                _ => None,
            };
            let queued = matches!(&mutation, Mutation::Queue { .. });
            if queued {
                drop(mutation); drop(gate);
                lib.downloads.wake();
            } else {
                let owner = lib.clone();
                let cleaned = super::downloads::lane().run_isolated(move |blocking| {
                    let failed = mutation.remove_files(blocking, &owner, files);
                    // Retain intent and publication ownership through all card
                    // work, including when its original UI observer has gone.
                    drop(mutation); drop(gate);
                    failed
                }).await.map_err(|_| "Download cleanup worker stopped".to_string())?;
                if let Some(id) = playlist { lib.notify(LibEvent::PlaylistFollowChanged(id)); }
                lib.notify(LibEvent::Downloads);
                if cleaned { return Err("Download removed from library; some files could not be deleted".into()); }
                return Ok(());
            }
            if let Some(id) = playlist { lib.notify(LibEvent::PlaylistFollowChanged(id)); }
            lib.notify(LibEvent::Downloads);
            Ok(())
        });
        async move { task.await.map_err(|_| "Download action worker stopped".to_string())? }
    }

    // Ordinary-thread compatibility for generated diagnostics and fixtures.
    // These execute the same owner operation, never SQLite on their caller.
    pub fn download_requested(&self, action: MusicDownloadAction, set: OfflineSet, songs: &[Song]) -> Result<(), String> {
        assert_wait();
        wait(self.me.upgrade().ok_or("Library closed")?.download_requested_async(action, set, songs.to_vec()))
    }
    pub fn download(&self, set: OfflineSet, songs: &[Song]) -> Result<(), String> {
        self.download_requested(self.begin_music_download(&set.kind, &set.id)?, set, songs)
    }
    pub fn remove_download_requested(&self, action: MusicDownloadAction, kind: &str, id: &str) -> Result<(), String> {
        assert_wait();
        wait(self.me.upgrade().ok_or("Library closed")?.remove_download_requested_async(action, kind.into(), id.into()))
    }
    pub fn remove_download(&self, kind: &str, id: &str) -> Result<(), String> {
        self.remove_download_requested(self.begin_music_download(kind, id)?, kind, id)
    }
    #[cfg(test)]
    pub fn remove_spoken_download_requested(&self, request: Intent) -> Result<(), String> {
        assert_wait();
        wait(self.me.upgrade().ok_or("Library closed")?.remove_spoken_download_requested_async(request))
    }
    #[cfg(test)]
    pub fn remove_spoken_download(&self, logical: &str) -> Result<(), String> {
        self.remove_spoken_download_requested(self.begin_remove_spoken_download(logical)?)
    }
}
fn assert_wait() {
    assert!(crate::rt::may_wait_for_tasks(), "await download actions on executor/pool");
    assert!(!super::db_worker::guard_held() && !super::db_worker::in_job(), "download action cannot wait for its own DB");
}
fn wait(future: impl Future<Output = Result<(), String>> + Send + 'static) -> Result<(), String> {
    let (send, receive) = tokio::sync::oneshot::channel();
    crate::rt::io().spawn(async move { let _ = send.send(future.await); });
    receive.blocking_recv().unwrap_or(Err("Download action worker stopped".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::{Config, Paths}, rt};
    use std::{sync::mpsc, time::Duration};

    fn fixture() -> (Arc<Library>, tokio::sync::mpsc::UnboundedReceiver<()>, Song, OfflineSet) {
        let mut config = Config::default();
        config.server.url = "http://127.0.0.1:9".into();
        config.server.user = "generated-download-actions".into();
        config.server.password = "synthetic-only".into();
        let (send, events) = tokio::sync::mpsc::unbounded_channel();
        let (lib, _) = Library::create(Paths::new(super::super::generated_test_directory("download-actions")), &config,
            Arc::new(move |event| { if matches!(event, LibEvent::Downloads) { let _ = send.send(()); } }));
        lib.set_offline_mode(true);
        let song = Song { id: "generated-song".into(), ..Default::default() };
        let set = OfflineSet { kind: "song".into(), id: song.id.clone(), name: "Generated".into(), artist: String::new(), cover_art: None, song_ids: vec![song.id.clone()] };
        (lib, events, song, set)
    }
    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(crate::library::TEST_LIVENESS, future).await.expect("download action deadline")
    }

    #[test]
    fn download_admission_awaits_owner_without_parking_workers_and_refuses_newer_intent_or_generation() {
        for replace in [false, true] {
            let (lib, _, song, set) = fixture();
            let (entered, begun) = mpsc::channel(); let (release, held) = mpsc::channel();
            lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv(); });
            begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let gate = lib.offline_art_gate.lock();
            let action = lib.begin_music_download(&set.kind, &set.id).unwrap();
            let queue = lib.download_requested_async(action, set.clone(), vec![song]);
            rt::Ui::new().unwrap().block_on(async {
                bounded(lib.offline_art_gate.waiting_async.notified()).await;
                drop(gate);
                assert_eq!(bounded(super::super::downloads::lane().run_isolated(|_| 17)).await.unwrap(), 17);
                assert_eq!(bounded(rt::jobs().run_isolated(|_| 19)).await.unwrap(), 19);
                let newer = lib.begin_music_download(&set.kind, &set.id).unwrap();
                if replace { lib.client_generation.fetch_add(1, Ordering::SeqCst); }
                release.send(()).unwrap();
                let error = bounded(queue).await.unwrap_err();
                assert_eq!(error, if replace { "Navidrome changed; select the item again" } else { "Download action superseded" });
                if !replace { bounded(lib.remove_download_requested_async(newer, set.kind, set.id)).await.unwrap(); }
            });
            assert!(lib.db().next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_none());
        }
    }

    #[test]
    fn dropped_download_observers_keep_durable_queue_and_music_or_spoken_cleanup_owned() {
        for spoken in [false, true] {
            let (lib, mut events, song, set) = fixture();
            // The observer can disappear before its first poll, while the
            // accepted operation is still waiting for the publication gate.
            let gate = lib.offline_art_gate.lock();
            let queue = lib.download_requested_async(lib.begin_music_download(&set.kind, &set.id).unwrap(), set.clone(), vec![song.clone()]);
            rt::Ui::new().unwrap().block_on(async {
                bounded(lib.offline_art_gate.waiting_async.notified()).await;
                drop(queue); drop(gate);
                bounded(events.recv()).await.unwrap();
            });
            let disk = Db::open(&crate::rt::Blocking::dedicated_thread(), &lib.paths.database()).unwrap();
            assert!(disk.offline_set(&crate::rt::Blocking::dedicated_thread(), &set.kind, &set.id).is_some());
            assert!(disk.next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_some());
            drop(disk);
            let path = if spoken {
                let directory = lib.paths.media_root().join("spoken-downloads");
                std::fs::create_dir_all(&directory).unwrap(); directory.join("generated.m4b")
            } else { lib.paths.offline().join("generated.flac") };
            std::fs::write(&path, b"generated media").unwrap();
            if spoken { lib.db().put_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), "generated-book", &[("generated-part".into(), path.clone(), 15)]).unwrap(); }
            else { lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &path, 15); }
            let gate = lib.offline_art_gate.lock();
            let removal: std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> = if spoken {
                Box::pin(lib.remove_spoken_download_requested_async(lib.begin_remove_spoken_download("generated-book").unwrap()))
            } else {
                Box::pin(lib.remove_download_requested_async(lib.begin_music_download(&set.kind, &set.id).unwrap(), set.kind.clone(), set.id.clone()))
            };
            rt::Ui::new().unwrap().block_on(async {
                bounded(lib.offline_art_gate.waiting_async.notified()).await;
                drop(removal); drop(gate);
                bounded(events.recv()).await.unwrap();
            });
            assert!(!path.exists());
            let disk = rusqlite::Connection::open(lib.paths.database()).unwrap();
            let sql = if spoken { "SELECT count(*) FROM spoken_downloads" } else { "SELECT count(*) FROM offline_songs" };
            assert_eq!(disk.query_row(sql, [], |row| row.get::<_, u32>(0)).unwrap(), 0);
        }
    }

    #[test]
    fn download_mutations_report_sql_and_unlink_failures_without_false_success_or_early_file_loss() {
        let (lib, _, song, set) = fixture();
        let sql = rusqlite::Connection::open(lib.paths.database()).unwrap();
        sql.execute_batch("CREATE TRIGGER generated_reject_queue BEFORE INSERT ON offline_sets BEGIN SELECT RAISE(ABORT,'generated'); END;").unwrap();
        assert_eq!(lib.download(set.clone(), std::slice::from_ref(&song)).unwrap_err(), "Unable to queue downloads");
        assert!(lib.db().next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_none());
        sql.execute_batch("DROP TRIGGER generated_reject_queue;").unwrap();
        lib.download(set.clone(), std::slice::from_ref(&song)).unwrap();
        let path = lib.paths.offline().join("generated.flac");
        std::fs::write(&path, b"generated audio").unwrap();
        lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &path, 15);
        sql.execute_batch("CREATE TRIGGER generated_reject_remove BEFORE DELETE ON offline_sets BEGIN SELECT RAISE(ABORT,'generated'); END;").unwrap();
        assert_eq!(lib.remove_download(&set.kind, &set.id).unwrap_err(), "Unable to remove downloads");
        assert!(path.exists()); assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_some());
        sql.execute_batch("DROP TRIGGER generated_reject_remove;").unwrap();
        // A concrete card failure after successful durable removal remains an
        // error notice; it cannot be mistaken for a rolled-back index update.
        std::fs::remove_file(&path).unwrap(); std::fs::create_dir(&path).unwrap();
        assert_eq!(lib.remove_download(&set.kind, &set.id).unwrap_err(), "Download removed from library; some files could not be deleted");
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        assert!(path.is_dir()); std::fs::remove_dir(path).unwrap();
    }
}
