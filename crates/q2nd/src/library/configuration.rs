//! Configuration admission is memory-only. One detached transition owns its
//! publication permit through DB binding, file cleanup and final client install.
use super::{Library, LibError, settings_for};
use crate::config::Config;
use std::{future::Future, pin::Pin, sync::{Arc, atomic::{AtomicU32, Ordering}}, time::{Duration, Instant}};

#[derive(Clone)]
struct Outcome { revision: u32, result: Option<Result<(), String>> }
pub(super) struct State {
    revision: Arc<AtomicU32>,
    outcome: tokio::sync::watch::Sender<Outcome>,
    music_account: Option<String>,
    music_configured: bool,
    shelf_generation: Option<Arc<()>>,
    shelf_input: Option<(String, String, Option<String>)>,
    shelf_ready: bool,
}
impl Default for State {
    fn default() -> Self {
        Self { revision: Arc::default(), outcome: tokio::sync::watch::channel(Outcome { revision: 0, result: Some(Ok(())) }).0,
            music_account: None, music_configured: false, shelf_generation: None, shelf_input: None, shelf_ready: false }
    }
}
struct Request {
    revision: u32,
    current: Arc<AtomicU32>,
    music: bool,
    music_generation: u32,
    shelf: bool,
    shelf_generation: Arc<()>,
    config: Config,
}
impl Request {
    fn current(&self) -> bool { self.current.load(Ordering::SeqCst) == self.revision }
}

impl Library {
    /// Registers the whole desired configuration before returning. Pure local
    /// settings are immediate; changed accounts cannot admit old responses while
    /// their binding waits for a publication or the database owner.
    pub fn configure_async(self: &Arc<Self>, config: &Config) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>> {
        let mut state = self.configuration.lock().unwrap();
        let fill_changed = {
            let mut current = self.settings.write().unwrap();
            let next = settings_for(config, &self.paths);
            let changed = current.fill_target != next.fill_target;
            *current = next;
            changed
        };
        if fill_changed {
            self.wake_offline_groups();
            self.downloads.interrupt();
            self.downloads.wake();
        }
        self.direct_listens.configure(config);
        *self.spoken_settings.write().unwrap() = config.spoken;
        self.set_shelf_full_interval(config.audiobookshelf.full_sync_interval());
        let configured = config.server.is_configured();
        state.music_configured = configured;
        let credentials = config.server.credentials();
        let mut client = self.client.write().unwrap();
        let music = !configured || !client.as_ref().is_some_and(|client| client.credentials() == &credentials && client.proxy() == config.http_proxy());
        if music {
            if configured {
                let account = format!("{}|{}", credentials.server.trim_end_matches('/'), credentials.user);
                if state.music_account.as_ref() != Some(&account) {
                    self.music_epoch.fetch_add(1, Ordering::SeqCst);
                    state.music_account = Some(account);
                }
            }
            self.replace_client(&mut client, None);
            self.reset_health();
            self.background_changed();
            self.art.set_namespace("");
            self.downloads.interrupt();
        }
        let music_generation = self.client_generation();
        drop(client);
        if music { self.downloads.discard_all_saved(); }
        let shelf_input = (config.audiobookshelf.url.clone(), config.audiobookshelf.token.clone(), config.http_proxy().map(str::to_owned));
        let observed_generation = self.shelf_generation();
        let same_pending = state.shelf_input.as_ref() == Some(&shelf_input)
            && state.shelf_generation.as_ref().is_some_and(|generation| Arc::ptr_eq(generation, &observed_generation));
        let shelf = !(self.shelf_configuration_matches(config) || same_pending && state.shelf_ready);
        let shelf_generation = if shelf && !same_pending { self.invalidate_shelf_configuration() } else { self.shelf_generation() };
        state.shelf_input = Some(shelf_input);
        state.shelf_generation = Some(shelf_generation.clone());
        if shelf { state.shelf_ready = false; }
        // Unchanged clients and completed shelf binding need no owner work.
        // A pending/failed transition has no matching client and takes the
        // ordinary owned path instead; it cannot be accidentally acknowledged.
        if !music && !shelf {
            drop(state);
            self.wake_sync();
            return Box::pin(std::future::ready(Ok(())));
        }
        let revision = state.revision.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        state.outcome.send_replace(Outcome { revision, result: None });
        let request = Request { revision, current: state.revision.clone(), music, music_generation, shelf, shelf_generation, config: config.clone() };
        drop(state);
        let lib = self.clone();
        let worker = crate::rt::io().spawn(async move { apply(lib, request).await });
        let owner = self.clone();
        let completion = crate::rt::io().spawn(async move {
            let result = worker.await.unwrap_or_else(|_| Err("Library configuration worker stopped".into()));
            let state = owner.configuration.lock().unwrap();
            if state.revision.load(Ordering::SeqCst) != revision { return Ok(()); }
            // Preparation/worker failure must also release every ready/barrier
            // observer. A dropped UI future cannot strand this supervisor.
            let pending = state.outcome.borrow().result.is_none();
            if pending {
                state.outcome.send_replace(Outcome { revision, result: Some(result.clone()) });
            }
            result
        });
        Box::pin(async move { completion.await.map_err(|_| "Library configuration completion stopped".to_string())? })
    }

    /// Ordinary startup/generated diagnostic boundary. Runtime/UI uses the
    /// owned future; no worker is allowed to wait for configuration completion.
    pub fn configure(&self, config: &Config) {
        assert!(crate::rt::may_wait_for_tasks(), "await configuration on runtime/pool callers");
        let lib = self.me.upgrade().expect("library alive");
        let work = lib.configure_async(config);
        if let Err(error) = wait(work) { log::error!("{error}"); }
    }
    pub(crate) fn configuration_ready(&self) -> impl Future<Output = Result<(), String>> + Send + 'static + use<> {
        let state = self.configuration.lock().unwrap();
        let mut changed = state.outcome.subscribe();
        let revision = state.revision.load(Ordering::SeqCst);
        async move {
            loop {
                let outcome = changed.borrow_and_update().clone();
                if outcome.revision != revision { return Err("Library settings changed; retry the action".into()); }
                if let Some(result) = outcome.result { return result; }
                changed.changed().await.map_err(|_| "Library configuration owner stopped".to_string())?;
            }
        }
    }
    /// ABS readiness is independent of music storage binding. Admit only the
    /// captured shelf incarnation; never turn a failed music binding into an
    /// ABS outage or admit an old shelf while its replacement is pending.
    pub(crate) async fn shelf_configuration_ready(self: &Arc<Self>, generation: &Arc<()>) -> Result<(), LibError> {
        let mut changed = self.configuration.lock().unwrap().outcome.subscribe();
        loop {
            let outcome = changed.borrow_and_update().clone();
            let owner = self.clone(); let generation = generation.clone();
            let ready = crate::rt::spawn_blocking(move |_| {
                let slot = owner.shelf.read().unwrap();
                if !Arc::ptr_eq(&slot.generation, &generation) {
                    return Err(LibError::Server(subsonic::audiobookshelf::Error::Changed.to_string()));
                }
                Ok(slot.client.is_some())
            }).await.map_err(|_| LibError::Server("Audiobookshelf readiness worker stopped".into()))??;
            if ready { return Ok(()); }
            if outcome.result.is_some() {
                return Err(LibError::Server("Set up Audiobookshelf in Settings → Connections".into()));
            }
            changed.changed().await.map_err(|_| LibError::Offline)?;
        }
    }
    pub(crate) fn music_binding_ready(&self, generation: u32) -> impl Future<Output = Result<(), LibError>> + Send + 'static + use<> {
        let mut changed = self.configuration.lock().unwrap().outcome.subscribe();
        let desired = self.client_generation.clone(); let bound = self.bound_music_generation.clone();
        async move {
            loop {
                let outcome = changed.borrow_and_update().clone();
                if desired.load(Ordering::SeqCst) != generation { return Err(LibError::Offline); }
                if bound.load(Ordering::SeqCst) == generation { return Ok(()); }
                if let Some(Err(error)) = outcome.result { return Err(LibError::Server(error)); }
                changed.changed().await.map_err(|_| LibError::Offline)?;
            }
        }
    }
    pub(super) fn music_download_admission(&self) -> (u32, bool) {
        let state = self.configuration.lock().unwrap();
        (self.client_generation(), state.music_configured)
    }
}
pub(super) fn wait(work: impl Future<Output = Result<(), String>> + Send + 'static) -> Result<(), String> {
    assert!(crate::rt::may_wait_for_tasks(), "await library work on runtime/pool callers");
    assert!(!super::db_worker::guard_held() && !super::db_worker::in_job(), "configuration cannot wait for its DB");
    let (send, receive) = tokio::sync::oneshot::channel();
    crate::rt::io().spawn(async move { let _ = send.send(work.await); });
    receive.blocking_recv().unwrap_or(Err("Library configuration worker stopped".into()))
}

async fn apply(lib: Arc<Library>, request: Request) -> Result<(), String> {
    if !request.current() { return Ok(()); }
    let owner = lib.clone();
    let prepared = crate::rt::spawn_blocking(move |_| {
        let music = if request.music && request.config.server.is_configured() {
            subsonic::Client::with_proxy(request.config.server.credentials(), concat!("q2nd/", env!("CARGO_PKG_VERSION")), request.config.http_proxy())
                .map(Arc::new).map(Some).map_err(|_| "Unable to prepare Navidrome connection".to_string())
        } else { Ok(None) };
        let shelf = request.shelf.then(|| super::shelf::prepare_configuration(&request.config));
        (request, music, shelf)
    }).await.map_err(|_| "Library configuration preparation stopped".to_string());
    let (request, client, shelf) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(error),
    };
    let mut changed = owner.configuration.lock().unwrap().outcome.subscribe();
    let gate = loop {
        if !request.current() { return Ok(()); }
        tokio::select! {
            gate = owner.offline_art_gate.acquire() => break gate,
            result = changed.changed() => {
                result.map_err(|_| "Library configuration owner stopped".to_string())?;
            }
        }
    };
    if !request.current() { return Ok(()); }
    let mut result = client.as_ref().map(|_| ()).map_err(Clone::clone);
    if request.music && result.is_ok() && request.config.server.is_configured() {
        let id = format!("{}|{}", request.config.server.credentials().server.trim_end_matches('/'), request.config.server.user);
        let current = request.current.clone(); let revision = request.revision;
        let bound = owner.db_run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != revision { return Ok(None); }
            db.bind_music_server(blocking, &id).map(|(_, files)| Some(files)).map_err(|_| "Unable to bind music storage to configured server".to_string())
        }).await;
        match bound {
            Ok(Some(files)) => {
                let owner = owner.clone();
                if super::downloads::lane().run_isolated(move |blocking| {
                    for path in files { owner.remove_media_file(blocking, &path); }
                }).await.is_err() { result = Err("Library configuration cleanup stopped".into()); }
            }
            Ok(None) => return Ok(()),
            Err(error) => result = Err(error),
        }
    }
    let mut state = owner.configuration.lock().unwrap();
    if !request.current() { return Ok(()); }
    if let Some(shelf) = shelf {
        owner.publish_shelf_configuration(shelf, &request.shelf_generation);
        state.shelf_ready = true;
    }
    if request.music && result.is_ok() {
        let client = client.expect("successful preparation");
        let namespace = client.as_ref().map(|client| format!("{}|{}", client.credentials().server.trim_end_matches('/'), client.credentials().user)).unwrap_or_default();
        *owner.client.write().unwrap() = client;
        owner.bound_music_generation.store(request.music_generation, Ordering::SeqCst);
        // Disabling during a pending account switch must not label the old
        // storage with the superseded account's epoch: no binding ran here.
        if request.config.server.is_configured() {
            owner.bound_music_epoch.store(owner.music_epoch.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        owner.art.set_namespace(&namespace);
        *owner.last_probe.lock().unwrap() = Instant::now() - Duration::from_secs(3600);
    }
    state.outcome.send_replace(Outcome { revision: request.revision, result: Some(result.clone()) });
    drop(state); drop(gate);
    owner.wake_sync();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::{Paths, Quality}, library::{Policy, db::OfflineSet}, rt};
    use std::{future::Future, sync::mpsc, task::Poll};
    use subsonic::{Song, ItemKind, calls};

    fn fixture() -> (Arc<Library>, Config, Song, std::path::PathBuf) {
        let mut config = Config::default();
        config.server.url = "http://127.0.0.1:9".into(); config.server.user = "generated-a".into();
        config.server.password = "synthetic-only".into();
        config.audiobookshelf.url = "http://127.0.0.1:9".into(); config.audiobookshelf.token = "generated-a".into();
        let (lib, _) = Library::create(Paths::new(super::super::generated_test_directory("configuration-owner")), &config, Arc::new(|_| {}));
        lib.set_offline_mode(true);
        let song = Song { id: "generated-song".into(), ..Default::default() };
        let path = lib.paths.offline().join("generated.mp3"); std::fs::write(&path, b"generated old media").unwrap();
        lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &path, 19);
        let set = OfflineSet { kind: "song".into(), id: song.id.clone(), name: "Generated".into(), artist: String::new(), cover_art: None, song_ids: vec![song.id.clone()] };
        lib.db().enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set, std::slice::from_ref(&song)).unwrap();
        lib.db().put_response(&crate::rt::Blocking::dedicated_thread(), &calls::ping().request.cache_key(), r#"{"generated":"old-account"}"#);
        (lib, config, song, path)
    }
    async fn bounded<T>(work: impl Future<Output = T>) -> T {
        tokio::time::timeout(crate::library::TEST_LIVENESS, work).await.expect("configuration control deadline")
    }

    #[test]
    fn held_binding_keeps_metadata_download_view_clear_and_shutdown_pending_without_occupying_workers() {
        let (lib, mut config, song, old_path) = fixture();
        let old_generation = lib.client_generation();
        let gate = lib.offline_art_gate.lock();
        config.server.user = "generated-b".into();
        let configured = lib.configure_async(&config);
        assert!(!lib.is_configured()); assert_ne!(lib.client_generation(), old_generation);
        let generation = lib.client_generation();
        rt::Ui::new().unwrap().block_on(async {
            bounded(lib.offline_art_gate.waiting_async.notified()).await;
            bounded(lib.shelf_configuration_ready(&lib.shelf_generation())).await.unwrap();
            let metadata = lib.clone().fetch_async(calls::ping(), Policy::CacheOnly);
            let view = lib.download_view_async(generation);
            let clear = lib.clear_song_cache_async();
            let sleep = lib.prepare_sleep_async();
            tokio::pin!(metadata, view, clear, sleep);
            std::future::poll_fn(|cx| {
                assert!(metadata.as_mut().poll(cx).is_pending());
                assert!(view.as_mut().poll(cx).is_pending());
                assert!(clear.as_mut().poll(cx).is_pending());
                assert!(sleep.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            }).await;
            // Eager ordered music writes cannot enter the previous account's
            // still-live database while the new binding is awaiting its gate.
            // Hold the actual owner while observing optimistic feedback: the
            // rejected write may otherwise finish before this assertion runs.
            let held_database = lib.db();
            let heart = lib.star(ItemKind::Song, &song.id, true);
            assert!(lib.loved(&song));
            drop(held_database);
            assert_eq!(bounded(heart).await.unwrap_err(), "Music settings are being applied; retry the action");
            assert!(!lib.loved(&song));
            assert_eq!(bounded(rt::jobs().run_isolated(|_| 23)).await.unwrap(), 23);
            assert_eq!(bounded(super::super::downloads::lane().run_isolated(|_| 29)).await.unwrap(), 29);
            drop(configured); // Accepted binding/cleanup has independent ownership.
            drop(gate);
            bounded(async {
                let (metadata, view, clear, sleep) = tokio::join!(metadata, view, clear, sleep);
                assert!(metadata.is_err(), "old account's cached metadata must not cross the binding");
                let view = view.expect("new binding is readable");
                assert!(view.offline.is_empty() && view.queued.is_empty());
                clear.unwrap(); sleep.unwrap();
            }).await;
        });
        assert!(!old_path.exists()); assert!(lib.is_configured());
        assert_eq!(lib.bound_music_generation.load(Ordering::SeqCst), lib.client_generation());
        assert_eq!(lib.db().get(&crate::rt::Blocking::dedicated_thread(), "server").as_deref(), Some("http://127.0.0.1:9|generated-b"));
    }

    #[test]
    fn superseded_bindings_never_publish_middle_accounts_and_latest_settings_survive_cancelled_observers() {
        let (lib, config, _, old_path) = fixture();
        let first = lib.client_generation(); let shelf = lib.shelf_generation();
        let (entered, begun) = mpsc::channel(); let (release, held) = mpsc::channel();
        lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv(); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let mut middle = config.clone(); middle.server.user = "generated-b".into(); middle.audiobookshelf.token = "generated-b".into();
        let old = lib.configure_async(&middle);
        let middle_generation = lib.client_generation(); let middle_shelf = lib.shelf_generation();
        let mut latest = config.clone(); latest.playback.stream_quality = Quality::Mp3_128; latest.spoken.skip_forward_seconds = 47;
        latest.tailscale.enabled = true;
        let final_observer = lib.configure_async(&latest);
        assert!(first < middle_generation && middle_generation < lib.client_generation());
        assert!(!lib.shelf_generation_current(&shelf) && !lib.shelf_generation_current(&middle_shelf));
        assert_eq!(lib.settings.read().unwrap().stream_quality, Quality::Mp3_128);
        assert_eq!(lib.spoken_settings.read().unwrap().skip_forward_seconds, 47);
        drop((old, final_observer));
        rt::Ui::new().unwrap().block_on(async {
            let ready = lib.configuration_ready(); tokio::pin!(ready);
            std::future::poll_fn(|cx| { assert!(ready.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            assert_eq!(bounded(rt::jobs().run_isolated(|_| 31)).await.unwrap(), 31);
            release.send(()).unwrap();
            bounded(ready).await.unwrap();
            bounded(lib.prepare_sleep_async()).await.unwrap();
        });
        assert!(old_path.exists(), "the superseded middle binding cannot reset the original account");
        let client = lib.client.read().unwrap().clone().unwrap();
        assert_eq!(client.credentials().user, "generated-a"); assert_eq!(client.proxy(), latest.http_proxy());
        assert!(lib.shelf.read().unwrap().client.as_ref().unwrap().configured_as(&latest.audiobookshelf.url, &latest.audiobookshelf.token, latest.http_proxy()));
        assert_eq!(lib.db().get(&crate::rt::Blocking::dedicated_thread(), "server").as_deref(), Some("http://127.0.0.1:9|generated-a"));
        // A transport-only change preserves the admitted music account even
        // while its new client awaits the gate. Rapid durable toggles keep order.
        let epoch = lib.music_account_generation();
        let gate = lib.offline_art_gate.lock();
        latest.server.password = "generated-route-password".into();
        let configuring = lib.configure_async(&latest);
        let song = Song { id: "generated-route-heart".into(), ..Default::default() };
        let first = lib.star(ItemKind::Song, &song.id, true);
        let second = lib.star(ItemKind::Song, &song.id, false);
        rt::Ui::new().unwrap().block_on(async {
            bounded(first).await.unwrap(); bounded(second).await.unwrap();
            assert_eq!(lib.music_account_generation(), epoch);
            drop(gate); bounded(configuring).await.unwrap();
        });
        assert!(!lib.loved(&song));
        assert_eq!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 10).len(), 2);
        // A local-only settings edit does not wait behind unrelated publication.
        let generation = lib.client_generation();
        let gate = lib.offline_art_gate.lock();
        latest.spoken.skip_forward_seconds = 49;
        rt::Ui::new().unwrap().block_on(async { bounded(lib.configure_async(&latest)).await.unwrap(); });
        assert_eq!(lib.client_generation(), generation);
        assert_eq!(lib.spoken_settings.read().unwrap().skip_forward_seconds, 49);
        drop(gate);
    }

    #[test]
    fn failed_binding_refuses_reads_clear_and_sleep_without_unlinking_then_identical_retry_recovers() {
        let (lib, mut config, _, old_path) = fixture();
        let sql = rusqlite::Connection::open(lib.paths.database()).unwrap();
        sql.execute_batch("CREATE TRIGGER generated_reject_binding BEFORE UPDATE ON kv WHEN NEW.key='server' BEGIN SELECT RAISE(ABORT,'generated binding failure'); END;").unwrap();
        config.server.user = "generated-b".into();
        config.audiobookshelf.token = "generated-independent-shelf".into();
        let old_shelf = lib.shelf_generation();
        rt::Ui::new().unwrap().block_on(async {
            assert_eq!(bounded(lib.configure_async(&config)).await.unwrap_err(), "Unable to bind music storage to configured server");
            assert!(!lib.is_configured()); assert!(old_path.exists());
            assert!(bounded(lib.shelf_configuration_ready(&old_shelf)).await.is_err());
            bounded(lib.shelf_configuration_ready(&lib.shelf_generation())).await.unwrap();
            assert!(lib.shelf.read().unwrap().client.as_ref().unwrap().configured_as(&config.audiobookshelf.url, &config.audiobookshelf.token, config.http_proxy()));
            let generation = lib.client_generation();
            assert_ne!(lib.bound_music_generation.load(Ordering::SeqCst), generation);
            assert!(bounded(lib.download_view_async(generation)).await.is_none());
            assert!(bounded(lib.clone().fetch_async(calls::ping(), Policy::CacheOnly)).await.is_err());
            assert!(bounded(lib.clear_song_cache_async()).await.is_err());
            assert!(bounded(lib.prepare_sleep_async()).await.is_err());
        });
        assert!(old_path.exists());
        assert_eq!(lib.db().get(&crate::rt::Blocking::dedicated_thread(), "server").as_deref(), Some("http://127.0.0.1:9|generated-a"));
        sql.execute_batch("DROP TRIGGER generated_reject_binding;").unwrap();
        rt::Ui::new().unwrap().block_on(async {
            bounded(lib.configure_async(&config)).await.unwrap();
            bounded(lib.prepare_sleep_async()).await.unwrap();
        });
        assert!(!old_path.exists()); assert!(lib.is_configured());
    }
}
