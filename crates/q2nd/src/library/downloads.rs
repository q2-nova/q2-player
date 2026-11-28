//! Downloads for offline listening: one song at a time, in queue order, kept
//! in the database so a restart carries on.

use super::{LibError, LibEvent, Library, acceptable, stream_options, variant};
use crate::audio::source::Progress;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
pub struct Downloads {
    active_transfers: AtomicUsize,
    wake: Arc<Notify>,
    interrupt: Notify,
    /// The song downloading now: its ID, title and progress.
    current: Mutex<Option<(String, String, Arc<Progress>)>>,
    saved: Mutex<Option<SavedMusic>>,
    #[cfg(test)]
    reclaim_retire_hold: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    revision: Mutex<Arc<()>>,
    paused: AtomicBool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DownloadStatus {
    pub queued: usize,
    pub failed: usize,
    /// The song downloading now and how far along it is (0-1, if known).
    pub current: Option<(String, Option<f32>)>,
    pub paused: bool,
}

/// Held across preparation, transfer and publication for either media service.
pub(crate) struct ActiveTransfer<'a>(&'a AtomicUsize);

impl Drop for ActiveTransfer<'_> {
    fn drop(&mut self) { self.0.fetch_sub(1, Ordering::AcqRel); }
}

impl Downloads {
    pub(crate) fn start_transfer(&self) -> ActiveTransfer<'_> {
        self.active_transfers.fetch_add(1, Ordering::AcqRel);
        ActiveTransfer(&self.active_transfers)
    }

    pub(crate) fn transferring(&self) -> bool {
        self.active_transfers.load(Ordering::Acquire) != 0
    }

    pub fn wake(&self) {
        self.wake.notify_one();
    }
    pub(super) fn invalidate_revision(&self) {*self.revision.lock().unwrap()=Arc::new(()); }
    pub(super) fn has_saved(&self)->bool {self.saved.lock().unwrap().is_some()}
    pub(super) fn discard_all_saved(&self) {let saved=self.saved.lock().unwrap().take();drop(saved);}
    pub(super) async fn discard_saved_automatic(&self) -> std::io::Result<()> {
        let saved = {
            let mut slot = self.saved.lock().unwrap();
            if slot.as_ref().is_some_and(|saved| saved.automatic) { slot.take() } else { None }
        };
        if let Some(saved) = saved {
            lane().run_isolated(move |_| drop(saved)).await.map_err(std::io::Error::other)?;
        }
        Ok(())
    }
    pub(super) fn saved_key(&self)->Option<(String,i64,u32)> {
        self.saved.lock().unwrap().as_ref().map(|s|(s.id.clone(),s.ticket,s.generation))
    }
    pub(super) fn discard_saved(&self,key:&(String,i64,u32)) {
        let removed={let mut saved=self.saved.lock().unwrap();
            if saved.as_ref().is_some_and(|s|(&s.id,s.ticket,s.generation)==(&key.0,key.1,key.2)) {saved.take()} else {None}};
        drop(removed);
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// The song downloading now and its progress (0-1, if known).
    pub fn active(&self) -> Option<(String, Option<f32>)> {
        self.current.lock().unwrap().as_ref().map(|(id, _, p)| {
            let f = p.snapshot();
            (id.clone(), f.total.filter(|t| *t > 0).map(|t| (f.written as f32 / t as f32).clamp(0.0, 1.0)))
        })
    }

    pub fn set_paused(&self, on: bool) {
        self.paused.store(on, Ordering::Relaxed);
        self.interrupt();
        self.wake();
    }

    pub(super) fn interrupt(&self) { self.interrupt.notify_waiters(); }
    pub(super) fn changed(&self)->tokio::sync::futures::Notified<'_> { self.interrupt.notified() }

    pub fn status(&self, lib: &Library) -> DownloadStatus {
        let counts = lib.db_worker.run_blocking(|blocking, db| db.download_counts(blocking));
        self.status_with_counts(counts)
    }

    pub(super) fn status_with_counts(&self, (queued, failed): (usize, usize)) -> DownloadStatus {
        let current = self.current.lock().unwrap().as_ref().map(|(_, title, p)| {
            let f = p.snapshot();
            (
                title.clone(),
                f.total
                    .filter(|t| *t > 0)
                    .map(|t| f.written as f32 / t as f32),
            )
        });
        DownloadStatus {
            queued,
            failed,
            current,
            paused: self.paused.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Downloads {
    fn drop(&mut self) { self.wake.notify_one(); }
}

/// Selection binds a queue revision and transport while account replacement is
/// excluded. No network work happens while the client or DB locks are held.
#[derive(Clone)]
struct Work {
    song: subsonic::Song,
    ticket: i64,
    automatic: bool,
    client: Arc<subsonic::Client>,
    generation: u32,
    revision: Arc<()>,
}
fn selected_work(blocking: &crate::rt::Blocking, db: &super::Db, client: Arc<subsonic::Client>, generation: u32, revision:Arc<()>, allow_auto: bool) -> Result<Option<Work>, LibError> {
    let request = db.next_eligible_download(blocking, allow_auto).map_err(|_| fail("Unable to read download queue"))?;
    Ok(request.map(|request| Work { song: request.song, ticket: request.ticket, automatic: request.automatic, client, generation, revision }))
}
#[cfg(test)]
fn next_work(lib: &Library) -> Result<Option<Work>, LibError> {
    let slot = lib.client.read().unwrap();
    let Some(client) = slot.as_ref() else { return Ok(None); };
    selected_work(&crate::rt::Blocking::dedicated_thread(), &lib.db(), client.clone(), lib.client_generation(),lib.downloads.revision.lock().unwrap().clone(), true)
}
async fn next_work_async(lib: &Arc<Library>) -> Result<Option<Work>, LibError> {
    let owner = lib.clone();
    let snapshot = crate::rt::spawn_blocking(move |_| {
        let slot = owner.client.read().unwrap();
        slot.as_ref().map(|client| (client.clone(), owner.client_generation()))
    }).await.map_err(|_| fail("Download worker stopped"))?;
    let Some((client, epoch)) = snapshot else { return Ok(None); };
    let current = lib.client_generation.clone();
    let revision=lib.downloads.revision.lock().unwrap().clone();
    let allow_auto = lib.automatic_downloads_enabled() && !lib.mix_space.manual_active();
    let namespace = lib.shelf_namespace();
    lib.db_run(move |blocking, db| {
        if current.load(Ordering::SeqCst) != epoch { return Ok(None); }
        let allow_auto = allow_auto && !db.manual_download_waiting(&namespace).unwrap_or(true);
        selected_work(blocking, db, client, epoch,revision,allow_auto)
    }).await
}

fn fail(message: &str) -> LibError {
    LibError::Server(message.into())
}
fn current(lib: &Library, work: &Work) -> bool {
    lib.client
        .read()
        .unwrap()
        .as_ref()
        .is_some_and(|client| Arc::ptr_eq(client, &work.client))
}
fn save_error(blocking: &crate::rt::Blocking, db: &super::Db, id: &str, ticket: i64, error: &str) -> Result<(), LibError> {
    db.finish_download_request(blocking, id, ticket, Some(error)).map(|_| ()).map_err(|_| fail("Unable to save download failure"))
}
#[cfg(test)]
fn finish_error(lib: &Library, work: &Work, error: &LibError) -> Result<(), LibError> {
    crate::rt::Ui::new().expect("generated download error runtime").block_on(finish_error_async(lib, work, error))
}
async fn finish_error_async(lib: &Library, work: &Work, error: &LibError) -> Result<(), LibError> {
    let current = lib.client_generation.clone();
    let generation = work.generation;
    let id = work.song.id.clone();
    let ticket = work.ticket;
    let error = error.to_string();
    lib.db_run(move |blocking, db| {
        if current.load(Ordering::SeqCst) != generation { return Ok(()); }
        save_error(blocking, db, &id, ticket, &error)
    }).await
}

pub fn start(lib: &Arc<Library>) {
    crate::rt::io().spawn(run(Arc::downgrade(lib), lib.downloads.wake.clone()));
}

// Local preparation/publication has independent capacity, so saturated UI jobs
// cannot prevent a download starting. Network waits run on the I/O executor.
// The one demand worker retires after the ordinary blocking-pool idle timeout.
pub(super) fn lane() -> &'static crate::rt::Jobs {
    static LANE: OnceLock<crate::rt::Jobs> = OnceLock::new();
    LANE.get_or_init(|| crate::rt::Jobs::serial("downloads").expect("downloads lane"))
}

async fn wait_for_work(wake: &Notify, queued: usize) {
    if queued == 0 {
        wake.notified().await;
    } else {
        tokio::select! {
            () = wake.notified() => {},
            () = tokio::time::sleep(Duration::from_secs(15)) => {},
        }
    }
}

async fn queued(weak: &Weak<Library>) -> Option<usize> {
    let lib = weak.upgrade()?;
    let allow_auto = lib.automatic_downloads_enabled();
    let pending = lib.db_run(move |blocking, db| {
        let count = db.download_counts(blocking).0;
        if allow_auto { count } else { count.saturating_sub(db.automatic_pending_music()) }
    });
    drop(lib);
    Some(pending.await)
}

/// Only active blocking work owns the library. Notify retains a wake arriving
/// during either database lookup or worker completion, just as the old bool
/// did. Dropping the library wakes an idle waiter immediately.
async fn run(weak: Weak<Library>, wake: Arc<Notify>) {
    loop {
        let Some(count) = queued(&weak).await else { return };
        wait_for_work(&wake, count).await;
        let Some(count) = queued(&weak).await else { return };
        if count == 0 { continue; }
        let Some(lib) = weak.upgrade() else { return };
        if lib.downloads.is_paused() || !lib.is_online() { continue; }
        // Detached ownership survives coordinator cancellation until writes and
        // publication finish. No externally exposed handle aborts this task.
        if let Err(error) = crate::rt::io().spawn(async move { drain(lib).await }).await {
            log::error!("downloads worker: {error}");
            return;
        }
    }
}

fn transient_download(error:&LibError)->bool {
    match error {
        LibError::Offline=>true,
        LibError::Server(message) if matches!(message.as_str(),"Download connection failed"|"Download connection interrupted")=>true,
        LibError::Server(message)=>message.strip_prefix("Download HTTP ").and_then(|s|s.parse::<u16>().ok()).is_some_and(|s|matches!(s,408|429|500..=599)),
        _=>false,
    }
}

async fn drain(lib: Arc<Library>) {
    while !lib.downloads.is_paused() && lib.is_online() {
        let work = match next_work_async(&lib).await {
            Ok(Some(work)) => work,
            Ok(None) => break,
            _ => { log::warn!("Unable to read download queue"); break; }
        };
        let transfer=download_one_mode(lib.clone(),work.clone(),true);
        tokio::pin!(transfer);
        let mut observed=None;
        let result=loop {
            tokio::select! {
                result=&mut transfer => break result,
                ()=tokio::time::sleep(Duration::from_secs(1)) => {
                    // The Downloads screen owns OfflineSet rows and cannot use
                    // the generic per-song badge refresh. Publish only changed
                    // progress, including unknown -> known response length.
                    let active=lib.downloads.active();
                    if active.is_some() && active!=observed { observed=active;lib.notify(LibEvent::Downloads); }
                },
            }
        };
        *lib.downloads.current.lock().unwrap()=None;
        if let Err(error)=result {
            // Keep the exact durable ticket queued across transport outages.
            // The coordinator retries on a wake or its bounded 15s timer;
            // authentication, media and card failures remain explicit errors.
            if transient_download(&error) { lib.notify(LibEvent::Downloads); break; }
            // Say why it failed (the card is full, ...) and report the code.
            lib.notify(LibEvent::Notice(crate::ux::present_in(&error.to_string(), crate::ux::Journey::Download)));
            if let Err(error)=finish_error_async(&lib,&work,&error).await {
                log::warn!("download completion: {error}");
                lib.notify(LibEvent::Downloads);
                break;
            }
        }
        lib.notify(LibEvent::Downloads);
    }
}

use md5::{Digest, Md5};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

/// New files have unguessable, exclusive ownership and cannot overwrite older
/// downloads, including distinct server IDs with the same filename-safe form.
struct AudioFile {
    path: PathBuf,
    pinned: Option<std::fs::File>,
    published: bool,
    storage: Option<Arc<super::mix_space::Ticket>>,
    #[cfg(test)]
    reclaim_retire_hold: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    #[cfg(test)]
    retired: Option<std::sync::mpsc::Sender<bool>>,
}
impl AudioFile {
    /// The exclusively created descriptor establishes ownership before another
    /// fallible operation can fail. Keep it pinned even if cloning the writer
    /// fails, so cleanup can compare the still-owned inode with the pathname.
    fn from_created(
        path: PathBuf,
        pinned: std::fs::File,
        clone_writer: impl FnOnce(&std::fs::File) -> std::io::Result<std::fs::File>,
    ) -> Result<(Self, std::fs::File), LibError> {
        let audio = Self {
            path,
            pinned: Some(pinned),
            published: false,
            storage: None,
            #[cfg(test)]
            reclaim_retire_hold: None,
            #[cfg(test)]
            retired: None,
        };
        let output =
            clone_writer(audio.pinned.as_ref().expect("owned audio inode")).map_err(|_| fail("Unable to retain audio ownership"))?;
        Ok((audio, output))
    }
}
// A result can be abandoned on an executor after its card job completed.
// Keep both inode-checked deletion and final descriptor close with one resource
// so neither operation can escape onto that executor during cancellation.
struct AudioRetirement {
    path: PathBuf,
    pinned: std::fs::File,
    published: bool,
    _storage: Option<Arc<super::mix_space::Ticket>>,
    #[cfg(test)]
    reclaim_retire_hold: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    #[cfg(test)]
    retired: Option<std::sync::mpsc::Sender<bool>>,
}
impl AudioFile {
    fn retirement(&mut self) -> Option<AudioRetirement> {
        self.pinned.take().map(|pinned| AudioRetirement {
            path: std::mem::take(&mut self.path), pinned, published: self.published,
            _storage: self.storage.take(),
            #[cfg(test)]
            reclaim_retire_hold: self.reclaim_retire_hold.take(),
            #[cfg(test)]
            retired: self.retired.take(),
        })
    }
}
impl Drop for AudioFile {
    fn drop(&mut self) {
        if let Some(retirement) = self.retirement() {
            crate::rt::retire_blocking(move |blocking| retirement.retire(blocking));
        }
    }
}
impl AudioRetirement {
    fn retire(self, _blocking: &crate::rt::Blocking) {
        #[cfg(test)]
        if let Some((entered, release)) = self.reclaim_retire_hold {
            let _ = entered.send(());
            release.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        }
        if !self.published {
            if let (Ok(expected), Ok(actual)) = (
                self.pinned.metadata(),
                std::fs::symlink_metadata(&self.path),
            ) {
                if actual.file_type().is_file()
                    && (actual.dev(), actual.ino()) == (expected.dev(), expected.ino())
                {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
        drop(self.pinned);
        #[cfg(test)]
        if let Some(retired) = self.retired {
            let _ = retired.send(crate::rt::on_executor());
        }
    }
}
fn audio_file(
    blocking: &crate::rt::Blocking,
    lib: &Library,
    namespace: &str,
    song: &subsonic::Song,
    variant: &str,
) -> Result<(AudioFile, std::fs::File), LibError> {
    let directory = lib.paths.offline();
    crate::shard::create_directory(blocking, &directory).map_err(|_| fail("Unable to create audio directory"))?;
    if !crate::shard::metadata(blocking, &directory).ok().flatten().is_some_and(|metadata| metadata.file_type().is_dir()) {
        return Err(fail("Invalid audio directory"));
    }
    // Startup create_all may have created every directory below the mounted
    // card. Persist that whole chain, stopping before a different filesystem
    // (the installed read-only rootfs) rather than assuming a fixed depth.
    super::offline_art::sync_parents(blocking, lib.paths.media_root())
        .map_err(|_| fail("Unable to save audio directory"))?;
    let identity = format!(
        "{:x}",
        Md5::digest(serde_json::to_vec(&(namespace, &song.id)).expect("audio identity"))
    );
    let suffix = if variant == "raw" {
        song.suffix
            .as_deref()
            .map(str::to_ascii_lowercase)
            .filter(|suffix| super::DECODABLE.contains(&suffix.as_str()))
            .unwrap_or_else(|| "bin".into())
    } else {
        "mp3".into()
    };
    let basename = format!("{identity}-{:016x}", fastrand::u64(..));
    let guard = lib.paths.lock_store(blocking, crate::shard::Store::Offline);
    let path = lib.paths.prepare_sharded(blocking, &guard, format!("{basename}.{suffix}").as_ref())
        .map_err(|_| fail("Unable to create audio directory"))?;
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| fail("Unable to create downloaded audio"))?;
    drop(guard);
    AudioFile::from_created(path, output, std::fs::File::try_clone)
}

struct PreparedAudio {
    expected_upgrade_size: Option<u64>,
    namespace: String,
    path: PathBuf,
    size: u64,
    owned: Option<AudioFile>,
    network: Option<(subsonic::Request,super::transfer_io::FileOutput,Arc<Progress>)>,
}

struct SavedMusic {id:String,ticket:i64,generation:u32,automatic:bool,audio:PreparedAudio,request:subsonic::Request,progress:Arc<Progress>,state:super::resumable::State}

struct AudioLookup {
    upgrade: bool,
    quality: crate::config::Quality,
    offline: Option<PathBuf>,
    cached: Option<(String, PathBuf)>,
}
async fn audio_lookup(lib: &Arc<Library>, work: &Work) -> Result<Option<AudioLookup>, LibError> {
    let owner = lib.clone();
    let selected = work.clone();
    let Some((quality, epoch)) = crate::rt::spawn_blocking(move |_| {
        current(&owner, &selected).then(|| (
            owner.settings.read().unwrap().download_quality,
            owner.music_epoch.load(Ordering::SeqCst),
        ))
    }).await.map_err(|_| fail("Download worker stopped"))? else { return Ok(None); };
    let song = work.song.clone();
    let epoch_now = lib.music_epoch.clone();
    let ticket = work.ticket;
    lib.db_run(move |blocking, db| {
        if epoch_now.load(Ordering::SeqCst) != epoch { return Ok(None); }
        // Only a server-verified upgrade ticket bypasses both local sources.
        if db.upgrade_ticket(blocking, ticket).map_err(|_| fail("Unable to read download queue"))? {
            return Ok(Some(AudioLookup { upgrade: true, quality, offline: None, cached: None }));
        }
        let variants = acceptable(&song, quality);
        let references: Vec<&str> = variants.iter().map(String::as_str).collect();
        Ok(Some(AudioLookup { upgrade: false, quality, offline: db.offline_song(blocking, &song.id), cached: db.cached_song(blocking, &song.id, &references) }))
    }).await
}
fn prepare_audio(blocking: &crate::rt::Blocking, lib: &Arc<Library>, work: &Work, lookup: AudioLookup, storage: &super::mix_space::Ticket) -> Result<Option<PreparedAudio>, LibError> {
    if lib.paths.ephemeral_media {
        return Err(fail("Insert an SD card for offline downloads"));
    }
    if lib.client_generation() != work.generation {
        return Ok(None);
    }
    let song = &work.song;
    let credentials = work.client.credentials();
    let namespace = format!(
        "{}|{}",
        credentials.server.trim_end_matches('/'),
        credentials.user
    );
    let quality = lookup.quality;
    let expected_upgrade_size = if lookup.upgrade && variant(song, quality) == "raw" {
        song.size.filter(|size| *size > 0)
    } else { None };
    // Indexed paths may be flat (an earlier version) or sharded.
    let existing = lookup.offline
        .and_then(|path| lib.paths.media_path(&path))
        .and_then(|path| lib.paths.locate(blocking, &path));
    let mut owned = None;
    let mut network = None;
    let (path, size) = if let Some(path) = existing {
        let file = lib.paths.open_media(blocking, &path)
            .map_err(|_| fail("Unable to read downloaded audio"))?;
        file.sync_all()
            .map_err(|_| fail("Unable to save downloaded audio"))?;
        let size = file
            .metadata()
            .map_err(|_| fail("Unable to read downloaded audio"))?
            .len();
        if size == 0 {
            return Err(fail("Empty downloaded audio"));
        }
        (path, size)
    } else {
        let cached = lookup.cached.and_then(|(variant, path)| {
            let path = lib.paths.media_path(&path)?;
            let input = lib.paths.open_media(blocking, &path).ok()?;
            let metadata = input.metadata().ok()?;
            (metadata.is_file() && metadata.len() > 0).then_some((variant, input, metadata.len()))
        });
        let variant = cached
            .as_ref()
            .map(|(variant, _, _)| variant.clone())
            .unwrap_or_else(|| variant(song, quality));
        if cached.as_ref().is_some_and(|(_, _, size)| !storage.admitted(*size)) {
            return Err(fail("Unable to prepare cached download"));
        }
        let (audio, mut output) = audio_file(blocking, lib, &namespace, song, &variant)?;
        let size = if let Some((_, input, expected)) = cached {
            // Copy both owned and legacy cache files. Until checked publication
            // succeeds the cache's indexed source remains entirely untouched.
            use std::io::Read;
            let size = std::io::copy(&mut input.take(expected), &mut output)
                .map_err(|_| fail("Unable to copy cached audio"))?;
            if size != expected {
                return Err(fail("Incomplete cached audio"));
            }
            storage.consumed(size);
            output
                .sync_all()
                .map_err(|_| fail("Unable to save downloaded audio"))?;
            size
        } else {
            let request = work
                .client
                .stream_request(&song.id, &stream_options(&variant));
            let progress = Progress::new();
            *lib.downloads.current.lock().unwrap() = Some((song.id.clone(), song.title.clone(), progress.clone()));
            lib.notify(LibEvent::Downloads);
            network=Some((request,super::transfer_io::FileOutput::new(output),progress));
            0
        };
        // The exclusive random path stays invisible to offline readers until
        // the DB transaction publishes it. This also works on exFAT (no links).
        let path = audio.path.clone();
        owned = Some(audio);
        (path, size)
    };
    if lib.downloads.is_paused() || lib.offline_mode() {
        return Err(LibError::Offline);
    }
    Ok(Some(PreparedAudio {expected_upgrade_size,namespace,path,size,owned,network}))
}

async fn publication_current(lib: &Library, work: &Work) -> Result<bool, LibError> {
    let current = lib.client_generation.clone();
    let generation = work.generation;
    let id = work.song.id.clone();
    let ticket = work.ticket;
    lib.db_run(move |blocking, db| {
        if current.load(Ordering::SeqCst) != generation { return Ok(false); }
        db.download_request_current(blocking, &id, ticket).map_err(|_| fail("Unable to read download request"))
    }).await
}

async fn publish(lib:Arc<Library>,work:Work,mut audio:PreparedAudio,prepared:super::offline_art::Prepared,cancel:Arc<super::transfer_io::Cancel>, storage:Option<Arc<super::mix_space::Ticket>>)->Result<(),LibError> {
    let prepared = Arc::new(prepared);
    let admitted = if let Some(storage) = &storage {
        let admission = match publication_current(&lib, &work).await {
            Ok(true) => prepared.admit_download(&lib, storage, &cancel).await.map(Some),
            Ok(false) => Ok(None),
            Err(error) => Err(error),
        };
        match admission {
            Ok(Some(artwork)) => Some(artwork),
            rejected => {
                // A replaced queue or a failed artwork admission can leave
                // an unpublished audio inode. Join its retirement before the
                // download worker reports completion or releases its owner.
                return lane().run_isolated(move |_| {
                    drop(audio);
                    rejected.map(|_| ())
                }).await.map_err(|_| fail("Download worker stopped"))?;
            }
        }
    } else { None };
    let (gate, checked_cover, serial) = match admitted {
        Some(art) => (art.gate, Some(art.indexed), art.serial),
        None => (lib.offline_art_gate.acquire().await, None, None),
    };
    let mut commit_attempted = false;
    let mut has_art = false;
    let result = async {
        if lib.downloads.is_paused() || lib.offline_mode() { return Err(LibError::Offline); }
        if !publication_current(&lib, &work).await? { return Ok(None); }
        if !cancel.begin_publication() { return Err(LibError::Offline); }
        let indexed = match checked_cover { Some(indexed) => indexed, None => prepared.index(&lib).await? };
        let owner = lib.clone(); let artwork = prepared.clone();
        let credit = storage.clone();
        let art = lane().run_isolated(move |blocking| artwork.persist_admitted(blocking, &owner, indexed, credit.as_deref())).await
            .map_err(|_| fail("Download worker stopped"))??;
        has_art = art.is_some();
        let song = work.song.clone(); let path = audio.path.clone(); let size = audio.size;
        let generation = work.generation;
        let ticket = work.ticket;
        let current = lib.client_generation.clone();
        commit_attempted = true;
        lib.db_run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation { return Ok(None); }
            db.complete_music_download(blocking, &song, &path, size, art.as_ref(), ticket)
                .map_err(|_| fail("Unable to save download"))
        }).await
    }.await;
    if matches!(result, Ok(Some(_))) {
        if let Some(audio) = audio.owned.as_mut() { audio.published = true; }
    } else if result.is_err() && commit_attempted {
        if let Some(owned) = audio.owned.as_mut() {
            let path = owned.path.clone();
            // An ambiguous COMMIT retains any indexed or unverifiable inode.
            let indexed = lib.db_run(move |blocking, db| db.music_download_path_indexed(blocking, &path)).await;
            if !matches!(indexed, Ok(false)) { owned.published = true; }
        }
    }
    let rollback = if matches!(result, Ok(Some(_))) { None } else { Some(prepared.index(&lib).await) };
    lane().run_isolated(move |blocking| {
        if let Some(indexed) = rollback { prepared.rollback_indexed(blocking, indexed); }
        if let Ok(Some(obsolete)) = &result {
            for path in obsolete { lib.remove_media_file(blocking, path); }
            if has_art { if let Some(id) = work.song.cover_art.as_deref() { lib.art.downloaded_cover(id); } }
        }
        // Both descriptor/inode cleanup and gate retirement belong to this
        // finite worker. No worker was occupied during the DB awaits above.
        drop(audio); drop(prepared); drop(gate); drop(serial);
        result.map(|_| ())
    }).await.map_err(|_| fail("Download worker stopped"))?
}

/// Pause/offline interrupts transport without consuming the coordinator's wake
/// permit. Always join the transfer and its final local cleanup after cancelling.
async fn download_one_async(lib:Arc<Library>,work:Work)->Result<(),LibError> {
    download_one_mode(lib,work,false).await
}
async fn download_one_mode(lib:Arc<Library>,work:Work,retry:bool)->Result<(),LibError> {
    let cancel=Arc::new(super::transfer_io::Cancel::default());
    let _active = lib.downloads.start_transfer();
    let selected=work.clone();
    let transfer=transfer(lib.clone(),work,cancel.clone(),retry);
    tokio::pin!(transfer);
    loop {
        let changed=lib.downloads.interrupt.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if selected.automatic && (!lib.automatic_downloads_enabled() || lib.mix_space.preempting()) {
            cancel.cancel();
            return transfer.await;
        }
        if !retry && (lib.downloads.is_paused() || lib.offline_mode()) {
            cancel.cancel();
            return transfer.await;
        }
        if retry && (lib.downloads.is_paused() || lib.offline_mode()) {
            return transfer.await;
        }
        if retry {
            let id=selected.song.id.clone();let ticket=selected.ticket;
            let live=lib.client_generation()==selected.generation && lib.db_run(move |blocking,db|db.download_request_current(blocking,&id,ticket).unwrap_or(false)).await;
            if !live {cancel.cancel();return transfer.await;}
        }
        tokio::select! {
            result=&mut transfer => return result,
            ()=changed => {},
        }
    }
}

pub(super) fn space_error(error: std::io::Error) -> LibError {
    if error.kind() == std::io::ErrorKind::Interrupted { return LibError::Offline; }
    if error.raw_os_error() == Some(libc::ENOSPC) { return fail("Card full"); }
    fail("Unable to prepare space for download")
}

async fn transfer(lib:Arc<Library>,work:Work,cancel:Arc<super::transfer_io::Cancel>,retry:bool)->Result<(),LibError> {
    let storage = super::mix_space::Ticket::new(lib.clone(), cancel.clone(), !work.automatic, work.song.id.clone());
    let saved=if retry {lib.downloads.saved.lock().unwrap().take()} else {None};
    let saved=saved.filter(|saved|saved.id==work.song.id && saved.ticket==work.ticket && saved.generation==work.generation);
    let (mut audio,resume)=if let Some(saved)=saved {
        *lib.downloads.current.lock().unwrap()=Some((work.song.id.clone(),work.song.title.clone(),saved.progress.clone()));
        (saved.audio,Some((saved.request,saved.progress,saved.state)))
    } else {
        let Some(lookup)=audio_lookup(&lib,&work).await? else {return Ok(());};
        if let Some((_, path)) = &lookup.cached {
            let owner = lib.clone();
            let path = path.clone();
            let offline = lookup.offline.clone();
            let cached_size = lane().run_isolated(move |blocking| {
                let already_downloaded = offline.and_then(|path| owner.paths.media_path(&path))
                    .and_then(|path| owner.paths.locate(blocking, &path)).is_some();
                if already_downloaded { return None; }
                let path = owner.paths.media_path(&path)?;
                owner.paths.open_media(blocking, &path).ok()?.metadata().ok().map(|metadata| metadata.len())
            }).await.map_err(|_|fail("Download worker stopped"))?;
            if let Some(size) = cached_size { storage.ensure(size, 1).await.map_err(space_error)?; }
        }
        let owner=lib.clone();let selected=work.clone();let credit=storage.clone();
        let Some(audio)=lane().run_isolated(move |blocking| prepare_audio(blocking, &owner,&selected,lookup,&credit)).await.map_err(|_|fail("Download worker stopped"))?? else {return Ok(());};
        (audio,None)
    };
    if let Some(owned) = &mut audio.owned {
        owned.storage = Some(storage.clone());
        #[cfg(test)]
        if work.automatic {
            owned.reclaim_retire_hold = lib.downloads.reclaim_retire_hold.lock().unwrap().take();
        }
    }
    let network=if let Some((request,progress,state))=resume {Some((request,None,progress,state))}
        else {audio.network.take().map(|(request,output,progress)|(request,Some(output),progress,super::resumable::State::default()))};
    let result=if let Some((request,output,progress,state))=network {
        if !retry {lib.fetch_download_file_async(&work.client,&request,output.expect("initial writer"),progress,cancel.clone(),Some(storage.clone())).await}
        else {
            // Retire the unused initial writer off-executor; the inode owner
            // stays pinned by `audio` throughout every reconnect attempt.
            crate::rt::drop_off_executor(output);
            let (returned,pinned)=lane().run_isolated(move |_| {
                let pinned=audio.owned.as_ref().and_then(|owner|owner.pinned.as_ref()).ok_or_else(||fail("Missing owned download")).and_then(|file|file.try_clone().map_err(|_|fail("Unable to retain audio ownership")));
                (audio,pinned)
            }).await.map_err(|_|fail("Download worker stopped"))?;
            audio=returned;let pinned=pinned?;
            let path=audio.path.clone();let client=work.client.clone();let owner=lib.clone();let fetch_request=request.clone();
            let fetch:super::resumable::Fetch=Arc::new(move |offset,tag| {
                let client=client.clone();let request=fetch_request.clone();let owner=owner.clone();
                Box::pin(async move {
                    let response=client.download_response_async(&request,offset,tag.as_deref()).await;
                    owner.download_response_async(client,response).await.map_err(|error|if transient_download(&error) {super::resumable::WireError::Retry} else {super::resumable::WireError::Fatal(error.to_string())})
                })
            });
            let result=super::resumable::file(lib.clone(),path,Arc::new(pinned),audio.expected_upgrade_size,progress.clone(),cancel.clone(),true,state,fetch,storage.clone()).await;
            match result {
                Ok((size,_))=>Ok(size),
                Err(super::resumable::Error::Suspended(state))=>{
                    // Without a strong validator the prefix cannot be reused.
                    // Retire it immediately, without waiting on the queue DB.
                    if state.validator.is_none() {return Err(LibError::Offline);}
                    let gate=lib.offline_art_gate.acquire().await;
                    let live=Arc::ptr_eq(&work.revision,&lib.downloads.revision.lock().unwrap());
                    let mut saved=lib.downloads.saved.lock().unwrap();
                    if live && work.generation==lib.client_generation() {
                        // Saved partials belong to a later invocation. Keeping
                        // this ticket in Library's saved slot would form a cycle.
                        if let Some(owned) = &mut audio.owned { owned.storage = None; }
                        *saved=Some(SavedMusic {id:work.song.id.clone(),ticket:work.ticket,generation:work.generation,automatic:work.automatic,audio,request,progress,state});
                        drop(saved);drop(gate);return Err(LibError::Offline);
                    }
                    drop(saved);
                    drop(gate);Err(LibError::Offline)
                },
                Err(super::resumable::Error::Cancelled)=>Err(LibError::Offline),
                Err(super::resumable::Error::Fatal(message))=>Err(LibError::Server(message)),
            }
        }
    } else {Ok(audio.size)};
    let owner=lib.clone();let stopped=cancel.clone();
    let audio=lane().run_isolated(move |blocking| {
        audio.size=match result {
            Ok(size) => size,
            Err(error) => {
                // Join retirement before releasing the automatic space ticket.
                // Otherwise a manual writer may measure the old partial bytes
                // as occupied and evict completed Offline Mix unnecessarily.
                if let Some(retirement) = audio.owned.as_mut().and_then(AudioFile::retirement) {
                    retirement.retire(blocking);
                }
                return Err(error);
            },
        };
        if audio.expected_upgrade_size.is_some_and(|expected| expected != audio.size) {
            return Err(fail("Incomplete replacement download"));
        }
        if audio.size == 0 { return Err(fail("Empty downloaded audio")); }
        if stopped.is_cancelled() {return Err(LibError::Offline);}
        if audio.owned.is_some() {owner.paths.sync_sharded(blocking, &audio.path).map_err(|_| fail("Unable to save audio directory"))?;}
        Ok(audio)
    }).await.map_err(|_|fail("Download worker stopped"))??;
    let lookup=match super::offline_art::lookup(&lib,&audio.namespace,work.song.cover_art.as_deref()).await {
        Ok(lookup)=>lookup,
        Err(error)=>return lane().run_isolated(move |_| { drop(audio); Err(error) }).await
            .map_err(|_|fail("Download worker stopped"))?,
    };
    let owner=lib.clone();
    let (audio,plan)=lane().run_isolated(move |blocking| {
        let plan=lookup.inspect(blocking, &owner)?;
        Ok::<_,LibError>((audio,plan))
    }).await.map_err(|_|fail("Download worker stopped"))??;
    let bytes=if let Some((id,size))=plan.request() {
        tokio::select! {
            biased;
            ()=cancel.cancelled()=>Err(LibError::Offline),
            response=work.client.cover_art_async(&id,size)=>lib.download_response_async(work.client.clone(),response).await,
        }
    } else {Ok(None)};
    let (audio,prepared)=lane().run_isolated(move |_| {
        let prepared=plan.complete(bytes?)?;
        Ok::<_,LibError>((audio,prepared))
    }).await.map_err(|_|fail("Download worker stopped"))??;
    publish(lib,work,audio,prepared,cancel,Some(storage)).await
}

#[cfg(test)]
fn download_one(lib:&Arc<Library>,work:&Work)->Result<(),LibError> {
    let lib=lib.clone();let work=work.clone();
    let (send,recv)=std::sync::mpsc::sync_channel(1);
    crate::rt::io().spawn(async move {let _=send.send(download_one_async(lib,work).await);});
    recv.recv_timeout(Duration::from_secs(10)).expect("download task completes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use crate::gfx::Image;
    use crate::library::db::OfflineSet;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;

    #[derive(Clone)]
    struct Response {
        route: &'static str,
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
        advertised_length: Option<usize>,
    }
    fn audio() -> Response {
        Response {
            route: "/rest/stream?",
            status: 200,
            content_type: "audio/mpeg",
            body: b"generated complete audio bytes".to_vec(),
            advertised_length: None,
        }
    }
    fn cover() -> Response {
        Response {
            route: "/rest/getCoverArt?",
            status: 200,
            content_type: "image/png",
            body: Image::placeholder(24).png().unwrap(),
            advertised_length: None,
        }
    }
    fn server(responses:Vec<Response>,blocked:bool)->(String,mpsc::Receiver<()>,mpsc::Sender<()>,std::thread::JoinHandle<()>) {
        server_with_disconnect(responses,blocked,false)
    }
    fn server_with_disconnect(responses:Vec<Response>,blocked:bool,allow_disconnect:bool)->(String,mpsc::Receiver<()>,mpsc::Sender<()>,std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for (index, response) in responses.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let route = request.split_whitespace().nth(1).and_then(|url| url.split('?').next());
                assert!(request.starts_with(&format!("GET {}", response.route)),
                    "unexpected generated fixture route {route:?}; expected {}", response.route);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                if index == 0 {
                    accepted_tx.send(()).unwrap();
                    if blocked {
                        release_rx.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                    }
                }
                let sent=write!(socket, "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.status, response.content_type, response.advertised_length.unwrap_or(response.body.len()))
                    .and_then(|()|socket.write_all(&response.body));
                if let Err(error)=sent {
                    assert!(allow_disconnect && matches!(error.kind(),std::io::ErrorKind::BrokenPipe|std::io::ErrorKind::ConnectionReset),"unexpected fixture write: {error}");
                }
            }
        });
        (base, accepted_rx, release_tx, worker)
    }
    fn config(base: &str) -> Config {
        let mut config = Config::default();
        config.storage.reserve_mb = 0;
        config.server.url = base.into();
        config.server.user = "generated-user".into();
        config.server.password = "synthetic-only".into();
        config
    }
    fn fixture(base: &str) -> Arc<Library> {
        let root = super::super::generated_test_directory("music-cover");
        Library::open(Paths::new(root), &config(base), Arc::new(|_| {}))
    }
    fn song(id: &str, art: bool) -> subsonic::Song {
        subsonic::Song {
            id: id.into(),
            title: "Generated music".into(),
            suffix: Some("mp3".into()),
            bit_rate: Some(128),
            cover_art: art.then(|| "generated-cover".into()),
            ..Default::default()
        }
    }
    fn set(song: &subsonic::Song) -> OfflineSet {
        OfflineSet {
            kind: "album".into(),
            id: "generated-set".into(),
            name: "Generated album".into(),
            artist: String::new(),
            cover_art: song.cover_art.clone(),
            song_ids: vec![song.id.clone()],
        }
    }
    fn queue(lib: &Library, song: &subsonic::Song) -> Work {
        lib.db()
            .enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set(song), std::slice::from_ref(song))
            .unwrap();
        next_work(lib).unwrap().unwrap()
    }
    fn namespace(base: &str) -> String {
        format!("{base}|generated-user")
    }

    fn owner_fixture(base: &str) -> (Arc<Library>, mpsc::Receiver<()>) {
        let root = super::super::generated_test_directory("download-owner");
        let (send, events) = mpsc::channel();
        let (lib, _) = Library::create(Paths::new(root), &config(base), Arc::new(move |event| {
            if matches!(event, LibEvent::Downloads) { let _ = send.send(()); }
        }));
        lib.set_online(true);
        (lib, events)
    }

    fn until(mut condition: impl FnMut() -> bool) {
        let end = std::time::Instant::now() + crate::library::TEST_LIVENESS;
        while !condition() {
            assert!(std::time::Instant::now() < end, "download owner did not reach expected state");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn publication_awaits_db_keeps_card_lane_available_and_orders_later_removal() {
        use std::future::Future;
        use std::task::Poll;
        let base = "http://127.0.0.1:9";
        let lib = fixture(base);
        let song = song("generated-publication", true);
        let work = queue(&lib, &song);
        let (owned, mut output) = audio_file(&crate::rt::Blocking::dedicated_thread(), &lib, &namespace(base), &song, "raw").unwrap();
        output.write_all(b"generated audio").unwrap(); output.sync_all().unwrap(); drop(output);
        let path = owned.path.clone();
        let prepared_audio = PreparedAudio { expected_upgrade_size: None, namespace: namespace(base), path: path.clone(), size: 15, owned: Some(owned), network: None };
        let artwork = super::super::offline_art::prepare(&lib, &namespace(base), song.cover_art.as_deref(), |_,_| Ok(Image::placeholder(24).png())).unwrap();
        let removal = lib.begin_music_download("album", "generated-set").unwrap();
        let (entered, begun) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv(); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            let pending = publish(lib.clone(), work, prepared_audio, artwork, Arc::new(super::super::transfer_io::Cancel::default()), None);
            tokio::pin!(pending);
            std::future::poll_fn(|cx| { assert!(pending.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            // Publication owns the gate but neither a worker nor a DB guard.
            assert_eq!(tokio::time::timeout(crate::library::TEST_LIVENESS, lane().run_isolated(|_| 31)).await.unwrap().unwrap(), 31);
            let removing = lib.remove_download_requested_async(removal, "album".into(), "generated-set".into());
            tokio::pin!(removing);
            tokio::time::timeout(crate::library::TEST_LIVENESS, lib.offline_art_gate.waiting_async.notified()).await.unwrap();
            std::future::poll_fn(|cx| { assert!(removing.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            release.send(()).unwrap();
            tokio::time::timeout(crate::library::TEST_LIVENESS, pending).await.unwrap().unwrap();
            tokio::time::timeout(crate::library::TEST_LIVENESS, removing).await.unwrap().unwrap();
        });
        assert!(!path.exists());
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        assert!(lib.db().offline_set(&crate::rt::Blocking::dedicated_thread(), "album", "generated-set").is_none());
        assert_eq!(crate::library::stored_files(&lib.paths.offline_art()).len(), 0);
    }

    #[test]
    fn publication_and_completed_response_progress_while_configuration_waits() {
        use std::future::Future;
        use std::task::Poll;
        let base = "http://127.0.0.1:9";
        let lib = fixture(base);
        let song = song("generated-config-publication", false);
        let work = queue(&lib, &song);
        let client = work.client.clone();
        let (owned, mut output) = audio_file(&crate::rt::Blocking::dedicated_thread(), &lib, &namespace(base), &song, "raw").unwrap();
        output.write_all(b"generated audio").unwrap(); output.sync_all().unwrap(); drop(output);
        let path = owned.path.clone();
        let audio = PreparedAudio { expected_upgrade_size: None, namespace: namespace(base), path: path.clone(), size: 15, owned: Some(owned), network: None };
        let artwork = super::super::offline_art::prepare(&lib, &namespace(base), None, |_,_| unreachable!()).unwrap();
        let (entered, begun) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv(); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            let publishing = publish(lib.clone(), work, audio, artwork, Arc::new(super::super::transfer_io::Cancel::default()), None);
            tokio::pin!(publishing);
            std::future::poll_fn(|cx| { assert!(publishing.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            let mut changed = config(base); changed.server.user = "generated-replacement-account".into();
            let configuring = lib.configure_async(&changed);
            tokio::pin!(configuring);
            tokio::time::timeout(crate::library::TEST_LIVENESS, lib.offline_art_gate.waiting_async.notified()).await.unwrap();
            // Configuration invalidated the old client and reached the async
            // gate; response classification and finite workers still progress.
            assert_eq!(tokio::time::timeout(crate::library::TEST_LIVENESS, lib.download_response_async(client, Ok(37))).await.unwrap().unwrap(), 37);
            assert_eq!(tokio::time::timeout(crate::library::TEST_LIVENESS, lane().run_isolated(|_| 43)).await.unwrap().unwrap(), 43);
            release.send(()).unwrap();
            tokio::time::timeout(crate::library::TEST_LIVENESS, publishing).await.unwrap().unwrap();
            tokio::time::timeout(crate::library::TEST_LIVENESS, configuring).await.unwrap().unwrap();
        });
        assert!(!path.exists(), "new account binding removes the old committed download");
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
    }

    #[test]
    fn audio_lookup_waits_for_db_without_parking_the_download_lane() {
        let lib = fixture("http://127.0.0.1:9");
        let song = song("generated-lookup", false);
        let work = queue(&lib, &song);
        let offline = lib.paths.offline().join("generated-existing.mp3");
        let cached = lib.paths.media_root().join("generated-cache.mp3");
        lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &offline, 24);
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, "raw", &cached, 24).unwrap();
        let (entered, begun) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv(); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            let pending = audio_lookup(&lib, &work);
            tokio::pin!(pending);
            tokio::select! {
                _ = &mut pending => panic!("audio lookup passed an occupied DB owner"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            assert_eq!(tokio::time::timeout(crate::library::TEST_LIVENESS, lane().run_isolated(|_| 23)).await.unwrap().unwrap(), 23);
            release.send(()).unwrap();
            let found = tokio::time::timeout(crate::library::TEST_LIVENESS, pending).await.unwrap().unwrap().unwrap();
            assert_eq!(found.offline, Some(offline));
            assert_eq!(found.cached, Some(("raw".into(), cached)));
        });
    }
    #[test]
    fn download_errors_follow_transport_generation_without_losing_current_failures() {
        let base = "http://127.0.0.1:9";
        let lib = fixture(base);
        let song = song("generated-error-generation", false);
        let old = queue(&lib, &song);
        finish_error(&lib, &old, &LibError::Auth).unwrap();
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 1));
        lib.db().retry_failed_downloads(&crate::rt::Blocking::dedicated_thread());
        let epoch = lib.music_epoch.load(Ordering::SeqCst);
        let mut changed = config(base);
        changed.server.password = "generated-replacement-credential".into();
        lib.configure(&changed);
        assert_eq!(lib.music_epoch.load(Ordering::SeqCst), epoch, "same account keeps its stored data");
        assert_ne!(lib.client_generation(), old.generation);
        finish_error(&lib, &old, &LibError::Auth).unwrap();
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0), "old transport cannot fail retried work");
        let current = crate::rt::Ui::new().unwrap().block_on(next_work_async(&lib)).unwrap().unwrap();
        assert_eq!(current.ticket, old.ticket);
        finish_error(&lib, &current, &LibError::Auth).unwrap();
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 1));
    }
    #[test]
    fn idle_owner_releases_library_during_queue_lookup_and_wait() {
        for stalled_lookup in [false, true] {
            let (lib, _) = owner_fixture("http://127.0.0.1:9");
            let weak = Arc::downgrade(&lib);
            let (release, released) = mpsc::channel();
            let (entered, seen) = mpsc::channel();
            if stalled_lookup {
                lib.db_post(move |_, _| {
                    entered.send(()).unwrap();
                    released.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                });
                seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            }
            let task = crate::rt::io().spawn(run(weak.clone(), lib.downloads.wake.clone()));
            // Ensure the owner has been polled and has submitted its DB work.
            let (polled, observed) = mpsc::channel();
            crate::rt::io().spawn(async move { polled.send(()).unwrap(); });
            observed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            until(|| Arc::strong_count(&lib) == 1);
            drop(lib);
            assert!(weak.upgrade().is_none());
            if stalled_lookup { release.send(()).unwrap(); }
            until(|| task.is_finished());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn empty_download_queue_waits_for_a_kick_and_queued_work_retains_retry() {
        let wake = Arc::new(Notify::new());
        let task_wake = wake.clone();
        let empty = tokio::spawn(async move { wait_for_work(&task_wake, 0).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(7200)).await;
        assert!(!empty.is_finished(), "an empty queue has no hourly liveness poll");
        wake.notify_one();
        empty.await.unwrap();
        // A kick delivered before registration is retained too.
        wake.notify_one();
        let now = tokio::time::Instant::now();
        wait_for_work(&wake, 0).await;
        assert_eq!(tokio::time::Instant::now(), now);
        wait_for_work(&wake, 1).await;
        assert_eq!(tokio::time::Instant::now() - now, Duration::from_secs(15));
    }

    #[test]
    fn wake_during_queue_lookup_reaches_first_transfer_and_pause_keeps_request() {
        let (base, accepted, release_server, server) = server_with_disconnect(vec![audio(),audio()], true,true);
        let (lib, _) = owner_fixture(&base);
        let song = song("wake-during-count", false);
        queue(&lib, &song);
        let (release_db, released_db) = mpsc::channel();
        let (entered, seen) = mpsc::channel();
        lib.db_post(move |_, _| {
            entered.send(()).unwrap();
            released_db.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        });
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let task = crate::rt::io().spawn(run(Arc::downgrade(&lib), lib.downloads.wake.clone()));
        lib.downloads.wake();
        release_db.send(()).unwrap();
        accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        lib.downloads.set_paused(true);
        release_server.send(()).unwrap();
        until(|| lib.downloads.current.lock().unwrap().is_none() && Arc::strong_count(&lib)==1);
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0));
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        lib.downloads.set_paused(false);
        until(||lib.db().download_counts(&crate::rt::Blocking::dedicated_thread())==(0,0));
        server.join().unwrap();
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_some());
        drop(lib);
        until(|| task.is_finished());
    }

    #[test]
    fn aborting_coordinator_during_transfer_keeps_publication_owned_until_completion() {
        let (base, accepted, release, server) = server(vec![audio()], true);
        let (lib, events) = owner_fixture(&base);
        let song = song("cancel-owner-active", false);
        queue(&lib, &song);
        let weak = Arc::downgrade(&lib);
        let paths = lib.paths.clone();
        let task = crate::rt::io().spawn(run(weak.clone(), lib.downloads.wake.clone()));
        lib.downloads.wake();
        accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        task.abort();
        until(|| task.is_finished());
        drop(lib);
        assert!(weak.upgrade().is_some(), "blocking transfer must retain its publication owner");
        release.send(()).unwrap();
        server.join().unwrap();
        events.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        until(|| weak.upgrade().is_none());
        // Reopen from disk only after the last transfer owner has gone.
        let (reopened, _) = Library::create(paths, &config(&base), Arc::new(|_| {}));
        assert_eq!(reopened.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 0));
        let published = reopened.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap();
        assert_eq!(std::fs::read(published).unwrap(), audio().body);
    }

    #[test]
    fn manual_pressure_waits_for_unpublished_mix_audio_in_cover_cancellation() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (cover_requested, waiting_cover) = mpsc::channel();
        let server = std::thread::spawn(move || {
            use std::io::Read;
            for route in ["/rest/stream?", "/rest/getCoverArt?"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                assert!(request.starts_with(&format!("GET {route}")));
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" { break; }
                }
                if route == "/rest/stream?" {
                    write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: 32768\r\nConnection: close\r\n\r\n").unwrap();
                    socket.write_all(&vec![47; 32768]).unwrap();
                } else {
                    cover_requested.send(()).unwrap();
                    assert_eq!(reader.read(&mut [0]).unwrap(), 0, "preemption closes the held cover request");
                }
            }
        });
        let (lib, _) = owner_fixture(&base);
        lib.settings.write().unwrap().fill_target = Some(50);
        let generated = song("generated-pressure-auto", true);
        let blocking = &crate::rt::Blocking::dedicated_thread();
        assert_eq!(lib.db().auto_music(blocking, None, &[(generated.clone(), 32768)], 32768).unwrap(), 1);
        let work = next_work(&lib).unwrap().unwrap();
        assert!(work.automatic);
        let (retiring, retirement_started) = mpsc::channel();
        let (release, hold) = mpsc::channel();
        *lib.downloads.reclaim_retire_hold.lock().unwrap() = Some((retiring, hold));
        let background = lib.clone();
        let (old_done, old_result) = mpsc::channel();
        crate::rt::io().spawn(async move { let _ = old_done.send(download_one_mode(background, work, true).await); });
        waiting_cover.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let files = crate::library::stored_files(&lib.paths.offline());
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::metadata(&files[0]).unwrap().len(), 32768);
        lib.settings.write().unwrap().reserve_bytes = u64::MAX;
        let (done, manual_result) = mpsc::channel();
        let requester = lib.clone();
        crate::rt::io().spawn(async move {
            let request = super::super::mix_space::Ticket::new(requester, Arc::new(super::super::transfer_io::Cancel::default()), true, "generated-manual".into());
            let _ = done.send(request.ensure(1, 1).await);
        });
        retirement_started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(files[0].is_file());
        assert!(manual_result.try_recv().is_err(), "manual space admission must wait for the unpublished inode");
        release.send(()).unwrap();
        assert_eq!(manual_result.recv_timeout(crate::library::TEST_LIVENESS).unwrap().unwrap_err().raw_os_error(), Some(libc::ENOSPC));
        assert!(matches!(old_result.recv_timeout(crate::library::TEST_LIVENESS).unwrap(), Err(LibError::Offline)));
        assert!(!files[0].exists());
        assert!(lib.db().offline_song(blocking, &generated.id).is_none());
        assert!(lib.db().download_ticket(blocking, &generated.id).unwrap().is_some());
        server.join().unwrap();
    }

    #[test]
    fn queued_music_resumes_validated_prefix_after_real_disconnect() {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let base=format!("http://{}",listener.local_addr().unwrap());
        let (sent,first)=mpsc::channel();let (first_release,first_continue)=mpsc::channel();let (range,observed)=mpsc::channel();let (release_body,continue_body)=mpsc::channel();
        let server=std::thread::spawn(move || {
            let body=(0..100000).map(|n|(n%251) as u8).collect::<Vec<_>>();
            for attempt in 0..2 {
                let (mut socket,_)=listener.accept().unwrap();let mut reader=BufReader::new(socket.try_clone().unwrap());let mut headers=String::new();
                loop {let mut line=String::new();assert!(reader.read_line(&mut line).unwrap()>0);if line=="\r\n" {break;}headers.push_str(&line);}
                if attempt==0 {
                    write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nDate: Sun, 06 Nov 1994 08:51:37 GMT\r\nConnection: close\r\n\r\n").unwrap();
                    socket.write_all(&body[..25000]).unwrap();socket.flush().unwrap();sent.send(()).unwrap();first_continue.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                } else {
                    let lower=headers.to_ascii_lowercase();let value=lower.lines().find_map(|l|l.strip_prefix("range: bytes=")).unwrap().trim();
                    let start:usize=value.strip_suffix('-').unwrap().parse().unwrap();
                    assert!(lower.contains("if-unmodified-since: sun, 06 nov 1994 08:49:37 gmt"));
                    write!(socket,"HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-99999/100000\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nDate: Sun, 06 Nov 1994 08:51:37 GMT\r\nConnection: close\r\n\r\n",100000-start).unwrap();
                    range.send(start).unwrap();continue_body.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                    socket.write_all(&body[start..]).unwrap();
                }
            }
        });
        let (lib,_)=owner_fixture(&base);let song=song("generated-range-resume",false);queue(&lib,&song);start(&lib);lib.downloads.wake();
        first.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        until(|| lib.downloads.active().is_some_and(|(_,fraction)|fraction.is_some_and(|f|f>=0.25)));
        let (entered,blocked)=mpsc::channel();let (release_db,continue_db)=mpsc::channel();
        lib.db_post(move |_,_| {entered.send(()).unwrap();continue_db.recv_timeout(crate::library::TEST_LIVENESS).unwrap();});
        blocked.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        lib.downloads.set_paused(true);
        // The coordinator clears `current` before its task drops its library
        // handle, so wait for that drop too. A transfer that waited for the
        // held database would never drop it.
        until(||lib.downloads.current.lock().unwrap().is_none() && lib.downloads.saved_key().is_some() && Arc::strong_count(&lib)==1);
        release_db.send(()).unwrap();first_release.send(()).unwrap();
        lib.downloads.set_paused(false);
        lib.downloads.wake();
        assert_eq!(observed.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),25000);
        until(|| lib.downloads.active().is_some_and(|(_,fraction)|fraction.is_some_and(|f|(f-0.25).abs()<0.001)));
        release_body.send(()).unwrap();
        let deadline=std::time::Instant::now()+crate::library::TEST_LIVENESS;
        while lib.db().download_counts(&crate::rt::Blocking::dedicated_thread())!=(0,0) {assert!(std::time::Instant::now()<deadline);std::thread::sleep(Duration::from_millis(10));}
        let path=lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(),&song.id).unwrap();
        assert_eq!(std::fs::read(path).unwrap(),(0..100000).map(|n|(n%251) as u8).collect::<Vec<_>>());server.join().unwrap();
    }
    #[test]
    fn removing_paused_partial_then_requeue_starts_clean() {
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base=format!("http://{}",listener.local_addr().unwrap());
        let (sent,first)=mpsc::channel();let (release,proceed)=mpsc::channel();
        let server=std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut socket,_)=listener.accept().unwrap();
                let mut reader=BufReader::new(socket.try_clone().unwrap());let mut headers=String::new();
                loop {let mut line=String::new();assert!(reader.read_line(&mut line).unwrap()>0);if line=="\r\n" {break;}headers.push_str(&line);}
                assert!(!headers.to_ascii_lowercase().contains("range:"),"requeued item must not reuse removed bytes");
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nETag: \"generated-v1\"\r\nConnection: close\r\n\r\n").unwrap();
                if attempt==0 {socket.write_all(&vec![1;25000]).unwrap();socket.flush().unwrap();sent.send(()).unwrap();proceed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();}
                else {socket.write_all(&vec![2;100000]).unwrap();}
            }
        });
        let (lib,_)=owner_fixture(&base);let song=song("removed-generated-partial",false);
        queue(&lib,&song);start(&lib);lib.downloads.wake();first.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        until(||lib.downloads.active().is_some_and(|(_,f)|f.is_some_and(|v|v>=0.25)));
        lib.downloads.set_paused(true);
        until(||lib.downloads.current.lock().unwrap().is_none() && lib.downloads.saved_key().is_some());
        release.send(()).unwrap();
        lib.remove_download("album","generated-set").unwrap();
        assert!(lib.downloads.saved_key().is_none());
        lib.download(set(&song),std::slice::from_ref(&song)).unwrap();
        lib.downloads.set_paused(false);
        until(||lib.db().download_counts(&crate::rt::Blocking::dedicated_thread())==(0,0));
        let path=lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(),&song.id).unwrap();
        assert_eq!(std::fs::read(path).unwrap(),vec![2;100000]);server.join().unwrap();
    }
    #[test]
    fn releasing_manual_space_does_not_schedule_an_immediate_retry() {
        let (lib, _) = owner_fixture("http://127.0.0.1:9");
        let reservation = super::super::mix_space::Ticket::new(
            lib.clone(),
            Arc::new(super::super::transfer_io::Cancel::default()),
            true,
            "generated-reservation".into(),
        );
        assert!(lib.mix_space.manual_active());
        let runtime = crate::rt::Ui::new().unwrap();
        // Discard any wake left by fixture configuration before testing Drop.
        runtime.block_on(async {
            tokio::select! {
                biased;
                () = lib.downloads.wake.notified() => {},
                () = std::future::ready(()) => {},
            }
        });
        drop(reservation);
        assert!(!lib.mix_space.manual_active());
        runtime.block_on(async {
            tokio::select! {
                biased;
                () = lib.downloads.wake.notified() => panic!("releasing the card reservation bypassed retry backoff"),
                () = std::future::ready(()) => {},
            }
        });
    }

    #[test]
    fn temporary_server_failure_stays_queued_and_retries_after_reconnect() {
        let mut unavailable=audio();unavailable.status=503;unavailable.body.clear();
        // One failed download is not evidence enough (docs/server-status.md):
        // the health probe's two pings confirm the server is unavailable.
        let mut ping=unavailable.clone();ping.route="/rest/ping?";ping.content_type="text/html";
        let (base,accepted,_,server)=server(vec![unavailable,ping.clone(),ping,audio()],false);
        let (lib,_events)=owner_fixture(&base);let song=song("generated-reconnect",false);queue(&lib,&song);start(&lib);lib.downloads.wake();
        accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        until(||!lib.is_online());
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()),(1,0));
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(),&song.id).is_none());
        lib.set_online(true);lib.downloads.wake();
        until(||lib.db().download_counts(&crate::rt::Blocking::dedicated_thread())==(0,0));
        let path=lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(),&song.id).unwrap();
        assert_eq!(std::fs::read(path).unwrap(),audio().body);server.join().unwrap();
    }
    #[test]
    fn only_transient_download_errors_are_retried_automatically() {
        for error in [LibError::Offline, fail("Download connection failed"), fail("Download connection interrupted"), fail("Download HTTP 408"),fail("Download HTTP 429"),fail("Download HTTP 503")] {assert!(transient_download(&error));}
        for error in [LibError::Auth, fail("Download HTTP 404"),fail("Unable to write downloaded audio"),fail("Incomplete downloaded audio")] {assert!(!transient_download(&error));}
    }
    #[test]
    fn downloads_source_refreshes_album_pie_after_network_progress() {
        use crate::ui::list::Badge;
        use crate::ui::screens::menu::Source;
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base=format!("http://{}",listener.local_addr().unwrap());
        let (first,received)=mpsc::channel();let (next,continue_body)=mpsc::channel();
        let (second,advanced)=mpsc::channel();let (release,finish)=mpsc::channel();
        let server=std::thread::spawn(move || {
            let (mut socket,_)=listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader=BufReader::new(socket.try_clone().unwrap());
            loop {let mut line=String::new();assert!(reader.read_line(&mut line).unwrap()>0);if line=="\r\n" {break;}}
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: 32768\r\nConnection: close\r\n\r\n").unwrap();
            socket.write_all(&vec![7;8192]).unwrap();socket.flush().unwrap();first.send(()).unwrap();
            continue_body.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            socket.write_all(&vec![7;8192]).unwrap();socket.flush().unwrap();second.send(()).unwrap();
            finish.recv_timeout(crate::library::TEST_LIVENESS).unwrap();socket.write_all(&vec![7;16384]).unwrap();
        });
        let (lib,events)=owner_fixture(&base);let song=song("progress-song",false);queue(&lib,&song);start(&lib);lib.downloads.wake();
        received.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        until(||lib.downloads.active().is_some_and(|(_,f)|f.is_some_and(|v|v>=0.24)));
        while events.try_recv().is_ok() {}
        next.send(()).unwrap();advanced.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        until(||lib.downloads.active().is_some_and(|(_,f)|f.is_some_and(|v|v>=0.49)));
        events.recv_timeout(crate::library::TEST_LIVENESS).expect("changed progress must publish a Downloads refresh");
        let rows=crate::rt::Ui::new().unwrap().block_on(Arc::new(crate::ui::screens::sources::Downloads).load(lib.clone(),crate::library::Policy::CacheOnly)).unwrap();
        let row=rows.iter().find(|row|row.title=="Generated album").unwrap();
        assert!(matches!(row.badge,Badge::Downloading(50)),"actual Downloads source must show the active half-filled album pie");
        release.send(()).unwrap();server.join().unwrap();
        until(||lib.db().download_counts(&crate::rt::Blocking::dedicated_thread())==(0,0));
    }

    #[test]
    fn wake_during_worker_completion_survives_next_queue_lookup() {
        let (base, _accepted, _release, server) = server(vec![audio(), audio()], false);
        let (entered, seen) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let resumed = Mutex::new(Some(resumed));
        let root = super::super::generated_test_directory("download-completion-wake");
        let (lib, _) = Library::create(Paths::new(root), &config(&base), Arc::new(move |event| {
            if matches!(event, LibEvent::Downloads) {
                if let Some(resumed) = resumed.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    resumed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
                }
            }
        }));
        lib.set_online(true);
        queue(&lib, &song("completion-first", false));
        start(&lib);
        lib.downloads.wake();
        seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        queue(&lib, &song("completion-second", false));
        lib.downloads.set_paused(true); // Coalesced wake while the worker finishes.
        let (release_db, released_db) = mpsc::channel();
        let (entered_db, seen_db) = mpsc::channel();
        lib.db_post(move |_, _| {
            entered_db.send(()).unwrap();
            released_db.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        });
        seen_db.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        resume.send(()).unwrap();
        until(|| Arc::strong_count(&lib) == 1);
        // Deliberately provide no second notification: only the wake delivered
        // during completion may drive this second request before its 15s retry.
        lib.downloads.paused.store(false, Ordering::Relaxed);
        release_db.send(()).unwrap();
        until(|| lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()) == (0, 0));
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), "completion-first").is_some());
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), "completion-second").is_some());
        server.join().unwrap();
    }

    #[test]
    fn first_download_does_not_wait_for_saturated_general_jobs() {
        // A guard releases every blocker even if an assertion fails.
        struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_all();
            }
        }
        let release = Release(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
        let (entered, seen) = mpsc::channel();
        for _ in 0..3 {
            let shared = release.0.clone();
            let entered = entered.clone();
            crate::rt::jobs().run_isolated(move |_| {
                let mut done = shared.0.lock().unwrap();
                entered.send(()).unwrap();
                while !*done { done = shared.1.wait(done).unwrap(); }
            });
        }
        for _ in 0..3 { seen.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); }
        let (base, accepted, _, server) = server(vec![audio()], false);
        let (lib, events) = owner_fixture(&base);
        queue(&lib, &song("independent-first-download", false));
        start(&lib);
        let first = std::time::Instant::now();
        lib.downloads.wake();
        accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let request_us = first.elapsed().as_micros();
        events.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        // Downloads also emits a progress-start event before publication.
        until(|| lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()) == (0, 0));
        println!("DOWNLOAD_OWNER first_request_us={request_us} saturated_general_jobs=3 durable=1");
        server.join().unwrap();
        drop(release);
    }

    #[test]
    fn integrated_download_keeps_art_after_server_shutdown_cache_clear_and_restart() {
        let (base, _accepted, _release, server) = server(vec![audio(), cover()], false);
        let lib = fixture(&base);
        let song = song("a/b", true);
        let work = queue(&lib, &song);
        download_one(&lib, &work).unwrap();
        server.join().unwrap();
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 0));
        let audio_path = lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap();
        assert_eq!(std::fs::read(&audio_path).unwrap(), audio().body);
        std::fs::remove_dir_all(lib.paths.art_cache()).unwrap();
        let reopened = Library::open(lib.paths.clone(), &config(&base), Arc::new(|_| {}));
        reopened.set_offline_mode(true);
        for size in [50, 150, 320] {
            let image = super::super::offline_art::load(
                &reopened,
                &namespace(&base),
                "generated-cover",
                size,
            )
            .unwrap();
            assert_eq!((image.width, image.height), (size, size));
        }
        reopened.remove_download("album", "generated-set").unwrap();
        assert!(!audio_path.exists());
        assert!(
            crate::library::stored_files(&reopened.paths.offline_art()).is_empty()
        );
    }

    #[test]
    fn declared_missing_cover_completes_but_http_auth_failure_is_visible() {
        for (status, body, succeeds) in [
            (200, br#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"missing"}}}"#.to_vec(), true),
            (401, b"generated authentication failure".to_vec(), false),
        ] {
            let response = Response { route: "/rest/getCoverArt?", status, content_type: "application/json", body, advertised_length: None };
            let (base, _accepted, _release, server) = server(vec![audio(), response], false);
            let lib = fixture(&base); let song = song("cover-status", true); let work = queue(&lib, &song);
            let result = download_one(&lib, &work); server.join().unwrap();
            if succeeds { result.unwrap(); assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_some()); assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 0)); }
            else { let error = result.unwrap_err(); assert_eq!(error, LibError::Auth); finish_error(&lib, &work, &error).unwrap(); assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 1)); assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none()); assert!(crate::library::stored_files(&lib.paths.offline()).is_empty()); }
        }
    }

    #[test]
    fn truncated_http_audio_is_a_visible_failure_and_cannot_publish() {
        let mut response = audio();
        response.advertised_length = Some(response.body.len() + 1000);
        let (base, _accepted, _release, server) = server(vec![response], false);
        let lib = fixture(&base);
        let song = song("truncated-audio", false);
        let work = queue(&lib, &song);
        let error = download_one(&lib, &work).unwrap_err();
        server.join().unwrap();
        finish_error(&lib, &work, &error).unwrap();
        assert!(!error.to_string().contains("synthetic-only"));
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 1));
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        assert!(
            crate::library::stored_files(&lib.paths.offline()).is_empty()
        );
    }

    #[test]
    fn integrated_shared_cover_lives_until_final_music_owner_is_removed() {
        let (base, _accepted, _release, server) = server(vec![audio(), cover(), audio()], false);
        let lib = fixture(&base);
        for id in ["shared-first", "shared-second"] {
            let song = song(id, true);
            let mut set = set(&song);
            set.id = id.into();
            lib.db()
                .enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set, std::slice::from_ref(&song))
                .unwrap();
            download_one(&lib, &next_work(&lib).unwrap().unwrap()).unwrap();
        }
        server.join().unwrap();
        lib.remove_download("album", "shared-first").unwrap();
        assert!(
            super::super::offline_art::load(&lib, &namespace(&base), "generated-cover", 150)
                .is_some()
        );
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), "shared-second").is_some());
        lib.remove_download("album", "shared-second").unwrap();
        assert!(
            crate::library::stored_files(&lib.paths.offline_art()).is_empty()
        );
    }

    #[test]
    fn indexed_audio_is_requeued_to_repair_and_pin_missing_cover() {
        let (base, _accepted, _release, server) = server(vec![cover()], false);
        let lib = fixture(&base);
        let song = song("existing", true);
        let path = lib.paths.offline().join("generated-existing.mp3");
        std::fs::write(&path, b"existing generated audio").unwrap();
        lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song, &path, 24);
        let work = queue(&lib, &song);
        download_one(&lib, &work).unwrap();
        server.join().unwrap();
        assert_eq!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap(), path);
        assert!(
            super::super::offline_art::load(&lib, &namespace(&base), "generated-cover", 50)
                .is_some()
        );
    }

    #[test]
    fn vanished_disposable_cache_falls_back_to_the_captured_http_stream() {
        let (base, _accepted, _release, server) = server(vec![audio()], false);
        let lib = fixture(&base);
        let song = song("evicted-cache", false);
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(),
            &song.id,
            "raw",
            &lib.paths.stream_cache().join("generated-missing.mp3"),
            100,
        ).unwrap();
        let work = queue(&lib, &song);
        download_one(&lib, &work).unwrap();
        server.join().unwrap();
        assert_eq!(
            std::fs::read(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap()).unwrap(),
            audio().body
        );
    }

    #[test]
    fn invalid_cover_is_saved_without_art_and_card_failure_leaves_no_audio_publication() {
        // A cover the player cannot decode would fail every retry: the song
        // is saved without it (fault journey bad-cover).
        let mut invalid = cover();
        invalid.body = b"malformed generated art".to_vec();
        let (base, _accepted, _release, server) = server(vec![audio(), invalid], false);
        let lib = fixture(&base);
        let song = song("bad-art", true);
        let work = queue(&lib, &song);
        download_one(&lib, &work).unwrap();
        server.join().unwrap();
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (0, 0));
        let saved = lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), audio().body);
        std::fs::remove_dir_all(lib.paths.offline()).unwrap();
        std::fs::write(lib.paths.offline(), b"generated directory fault").unwrap();
        let song = subsonic::Song { id: "card-fault".into(), ..song };
        let work = queue(&lib, &song);
        assert!(download_one(&lib, &work).is_err());
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
    }

    #[test]
    fn publication_db_failure_preserves_indexed_cache_and_cleans_only_new_files() {
        let (base, _accepted, _release, server) = server(vec![cover()], false);
        let lib = fixture(&base);
        let song = song("db-fault", true);
        let source = lib.paths.stream_cache().join("generated-cache.mp3");
        std::fs::write(&source, b"generated cache source").unwrap();
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, "raw", &source, 22).unwrap();
        let connection = rusqlite::Connection::open(lib.paths.database()).unwrap();
        connection.execute_batch("CREATE TRIGGER generated_failure BEFORE INSERT ON offline_songs BEGIN SELECT RAISE(ABORT,'generated publication fault'); END;").unwrap();
        let work = queue(&lib, &song);
        assert!(download_one(&lib, &work).is_err());
        server.join().unwrap();
        assert!(source.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"generated cache source");
        assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, &["raw"]).is_some());
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        assert!(
            crate::library::stored_files(&lib.paths.offline()).is_empty()
        );
        assert!(
            crate::library::stored_files(&lib.paths.offline_art()).is_empty()
        );
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0));
    }

    #[test]
    fn remove_requeue_during_http_transfer_rejects_late_success_and_error() {
        let (base, accepted, release, server) = server(vec![audio(), cover()], true);
        let lib = fixture(&base);
        let song = song("requeued", true);
        let old = queue(&lib, &song);
        let background = lib.clone();
        let transfer = std::thread::spawn(move || download_one(&background, &old));
        accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        lib.remove_download("album", "generated-set").unwrap();
        let newer = queue(&lib, &song);
        release.send(()).unwrap();
        transfer.join().unwrap().unwrap();
        server.join().unwrap();
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        assert!(
            lib.db()
                .download_request_current(&crate::rt::Blocking::dedicated_thread(), &song.id, newer.ticket)
                .unwrap()
        );
        assert!(
            crate::library::stored_files(&lib.paths.offline()).is_empty()
        );
        let old = Work {
            song: song.clone(),
            ticket: newer.ticket - 1,
            automatic: false,
            client: newer.client.clone(),
            generation: newer.generation,
            revision: newer.revision.clone(),
        };
        finish_error(&lib, &old, &fail("Old generated failure")).unwrap();
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0));
    }

    #[test]
    fn late_http_auth_failures_cannot_fail_requeued_or_replacement_account_work() {
        for replace_account in [false, true] {
            let mut response = audio();
            response.status = 401;
            let (base, accepted, release, server) = server(vec![response], true);
            let lib = fixture(&base);
            let song = song("late-auth-failure", false);
            let old = queue(&lib, &song);
            let background = lib.clone();
            let transfer = std::thread::spawn(move || {
                let error = download_one(&background, &old).unwrap_err();
                assert_eq!(error, LibError::Auth);
                finish_error(&background, &old, &error).unwrap();
            });
            accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            if replace_account {
                lib.configure(&config("http://127.0.0.1:9"));
            } else {
                lib.remove_download("album", "generated-set").unwrap();
            }
            let newer = queue(&lib, &song);
            release.send(()).unwrap();
            transfer.join().unwrap();
            server.join().unwrap();
            assert!(
                lib.db()
                    .download_request_current(&crate::rt::Blocking::dedicated_thread(), &song.id, newer.ticket)
                    .unwrap()
            );
            assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0));
            assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
            assert!(
                crate::library::stored_files(&lib.paths.offline()).is_empty()
            );
        }
    }

    #[test]
    fn account_switch_keeps_old_http_transfer_bound_and_does_not_complete_new_queue() {
        let (base, accepted, release, server) = server(vec![audio(), cover()], true);
        let lib = fixture(&base);
        let song = song("same-id", true);
        let old = queue(&lib, &song);
        let old_client = old.client.clone();
        let old_ticket = old.ticket;
        let old_generation = old.generation;
        let background = lib.clone();
        let transfer = std::thread::spawn(move || download_one(&background, &old));
        accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let new_config = config("http://127.0.0.1:9");
        lib.configure(&new_config);
        let newer = queue(&lib, &song);
        release.send(()).unwrap();
        transfer.join().unwrap().unwrap();
        server.join().unwrap();
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        assert!(
            lib.db()
                .download_request_current(&crate::rt::Blocking::dedicated_thread(), &song.id, newer.ticket)
                .unwrap()
        );
        finish_error(
            &lib,
            &Work {
                song: song.clone(),
                ticket: old_ticket,
                automatic: false,
                client: old_client,
                generation: old_generation,
                revision: lib.downloads.revision.lock().unwrap().clone(),
            },
            &fail("old failure"),
        )
        .unwrap();
        assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0));
        assert!(
            crate::library::stored_files(&lib.paths.offline()).is_empty()
        );
        assert!(
            !lib.is_online(),
            "stale old-server fetch cannot mark new account online"
        );
    }

    #[test]
    fn cached_download_copies_legacy_source_and_names_are_opaque_and_exclusive() {
        let base = super::super::generated_test_directory("music-legacy");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let old = base.join("card/Q2Navidrome");
        std::fs::create_dir_all(old.join("cache/songs")).unwrap();
        let source = old.join("cache/songs/generated.mp3");
        std::fs::write(&source, b"generated legacy audio").unwrap();
        let paths = Paths::installed(
            base.join("internal"),
            base.join("card/Q2Mainline"),
            base.join("run/media"),
            true,
            Some(old),
        );
        let lib = Library::open(paths, &config("http://127.0.0.1:9"), Arc::new(|_| {}));
        let reserved = song("legacy-reserve-fault", false);
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &reserved.id, "raw", &source, 22).unwrap();
        lib.settings.write().unwrap().reserve_bytes = u64::MAX;
        let work = queue(&lib, &reserved);
        assert_eq!(download_one(&lib, &work).unwrap_err(), fail("Card full"));
        assert_eq!(std::fs::read(&source).unwrap(), b"generated legacy audio");
        lib.db()
            .finish_download_request(&crate::rt::Blocking::dedicated_thread(), &reserved.id, work.ticket, None)
            .unwrap();
        lib.settings.write().unwrap().reserve_bytes = 0;
        for id in ["a/b", "a?b"] {
            let song = song(id, false);
            lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), id, "raw", &source, 22).unwrap();
            let work = queue(&lib, &song);
            download_one(&lib, &work).unwrap();
        }
        let first = lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), "a/b").unwrap();
        let second = lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), "a?b").unwrap();
        assert_ne!(first, second);
        assert!(first.exists() && second.exists() && source.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"generated legacy audio");
        let malicious = subsonic::Song {
            suffix: Some("../../escape".into()),
            ..song("suffix", false)
        };
        let (audio, _file) = audio_file(&crate::rt::Blocking::dedicated_thread(), &lib, "generated", &malicious, "raw").unwrap();
        assert_eq!(audio.path.parent().unwrap(), lib.paths.sharded(crate::shard::Store::Offline, audio.path.file_name().unwrap()).parent().unwrap(), "in its shard");
        assert_eq!(audio.path.parent().unwrap().parent().unwrap(), lib.paths.offline());
        assert_eq!(audio.path.extension().unwrap(), "bin");
    }

    #[test]
    fn copying_cache_requires_extra_space_and_never_deletes_legacy_or_owned_source_on_reserve_failure()
     {
        let lib = fixture("http://127.0.0.1:9");
        let source = lib
            .paths
            .stream_cache()
            .join("generated-reserve-source.mp3");
        std::fs::write(&source, b"retained generated cache source").unwrap();
        let song = song("reserve-fault", false);
        lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, "raw", &source, 31).unwrap();
        // A safe copy needs another audio-sized allocation; unlike a rename,
        // retaining the source intentionally costs space. An impossible reserve
        // makes this card-space fault deterministic despite other host tests.
        lib.settings.write().unwrap().reserve_bytes = u64::MAX;
        let work = queue(&lib, &song);
        assert_eq!(download_one(&lib, &work).unwrap_err(), fail("Card full"));
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"retained generated cache source"
        );
        assert!(lib.db().cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, &["raw"]).is_some());
        assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        assert!(
            crate::library::stored_files(&lib.paths.offline()).is_empty()
        );
    }

    #[test]
    fn stalled_audio_headers_release_the_worker_and_pause_or_offline_closes_transport() {
        use std::io::Read;
        for offline in [false,true] {
            let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base=format!("http://{}",listener.local_addr().unwrap());
            let (accepted,ready)=mpsc::channel();
            let server=std::thread::spawn(move || {
                let (socket,_)=listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader=BufReader::new(socket);
                loop {let mut line=String::new();assert!(reader.read_line(&mut line).unwrap()>0);if line=="\r\n" {break;}}
                accepted.send(()).unwrap();
                let mut byte=[0];
                assert_eq!(reader.read(&mut byte).unwrap(),0,"cancellation closes a request without waiting for headers");
            });
            let lib=fixture(&base);let song=song("stalled-download",false);
            queue(&lib,&song);
            let work=next_work(&lib).unwrap().unwrap();let owner=lib.clone();let (done,finished)=mpsc::channel();
            crate::rt::io().spawn(async move {done.send(download_one_async(owner,work).await).unwrap();});
            ready.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let (pulse,pulsed)=mpsc::channel();
            lane().run_isolated(move |_|pulse.send(()).unwrap());
            pulsed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            if offline {lib.set_offline_mode(true);} else {lib.downloads.set_paused(true);}
            assert_eq!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),Err(LibError::Offline));
            server.join().unwrap();
            assert!(lib.db().offline_ids(&crate::rt::Blocking::dedicated_thread()).is_empty());
            assert!(lib.db().next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_some());
            assert!(crate::library::stored_files(&lib.paths.offline()).is_empty());
        }
    }

    #[test]
    fn host_write_and_sync_faults_are_returned_without_publishing_audio() {
        for (target, message) in [
            ("/dev/full", "Card full"),
            ("/dev/null", "Unable to save downloaded audio"),
        ] {
            let (base, _accepted, _release, server) = server(vec![audio()], false);
            let lib = fixture(&base);
            let client = lib.client().unwrap();
            let request = client.stream_request("generated-io-fault", &stream_options("raw"));
            let output = super::super::transfer_io::FileOutput::new(std::fs::OpenOptions::new().write(true).open(target).unwrap());
            let owner=lib.clone();let (send,recv)=mpsc::channel();
            crate::rt::io().spawn(async move {
                send.send(owner.fetch_download_file_async(&client,&request,output,Progress::new(),Arc::new(super::super::transfer_io::Cancel::default()),None).await).unwrap();
            });
            let error=recv.recv_timeout(crate::library::TEST_LIVENESS).unwrap().unwrap_err();
            server.join().unwrap();
            assert_eq!(error, fail(message));
            assert!(lib.db().offline_ids(&crate::rt::Blocking::dedicated_thread()).is_empty());
        }
    }

    #[test]
    fn pause_or_offline_mode_during_cover_fetch_preserves_the_queued_request() {
        for offline_mode in [false, true] {
            let (base, accepted, release, server) = server_with_disconnect(vec![cover()], true,true);
            let lib = fixture(&base);
            let song = song("interrupted-cover", true);
            let source = lib.paths.stream_cache().join("generated-pause-source.mp3");
            std::fs::write(&source, b"generated retained cache").unwrap();
            lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, "raw", &source, 24).unwrap();
            let work = queue(&lib, &song);
            let ticket = work.ticket;
            let background = lib.clone();
            let transfer = std::thread::spawn(move || download_one(&background, &work));
            accepted.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            if offline_mode {
                lib.set_offline_mode(true);
            } else {
                lib.downloads.set_paused(true);
            }
            release.send(()).unwrap();
            assert_eq!(transfer.join().unwrap().unwrap_err(), LibError::Offline);
            server.join().unwrap();
            assert!(lib.db().download_request_current(&crate::rt::Blocking::dedicated_thread(), &song.id, ticket).unwrap());
            assert_eq!(lib.db().download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 0));
            assert!(source.exists());
            assert!(lib.db().offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
            assert!(
                crate::library::stored_files(&lib.paths.offline()).is_empty()
            );
        }
    }

    #[test]
    fn unpublished_audio_cleanup_removes_only_the_pinned_inode() {
        let lib = fixture("http://127.0.0.1:9");
        let song = song("pinned-audio", false);
        let (audio, writer) = audio_file(&crate::rt::Blocking::dedicated_thread(), &lib, "generated", &song, "raw").unwrap();
        let path = audio.path.clone();
        drop(audio);
        assert!(!path.exists());
        drop(writer);
        for symlink in [false, true] {
            let (audio, writer) = audio_file(&crate::rt::Blocking::dedicated_thread(), &lib, "generated", &song, "raw").unwrap();
            let path = audio.path.clone();
            std::fs::remove_file(&path).unwrap();
            let target = lib.paths.offline().join("generated-replacement-target.mp3");
            std::fs::write(&target, b"generated replacement audio").unwrap();
            if symlink {
                std::os::unix::fs::symlink(&target, &path).unwrap();
            } else {
                std::fs::write(&path, b"generated replacement audio").unwrap();
                assert_ne!(
                    std::fs::metadata(&path).unwrap().ino(),
                    audio.pinned.as_ref().unwrap().metadata().unwrap().ino()
                );
            }
            drop(audio);
            drop(writer);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"generated replacement audio"
            );
            assert_eq!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                symlink
            );
        }
    }

    #[test]
    fn abandoned_audio_ownership_retires_off_executor_and_keeps_published_files() {
        let lib = fixture("http://127.0.0.1:9");
        let ui = crate::rt::Ui::new().unwrap();
        for published in [false, true] {
            let (mut audio, writer) = audio_file(&crate::rt::Blocking::dedicated_thread(), &lib,
                "generated", &song("abandoned-audio", false), "raw").unwrap();
            drop(writer);
            let path = audio.path.clone();
            let (sent, received) = mpsc::channel();
            audio.published = published;
            audio.retired = Some(sent);
            ui.block_on(async { drop(audio); });
            assert!(!received.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),
                "inode checks and removal must not execute on the UI runtime");
            assert_eq!(path.exists(), published, "publication decides retention");
        }
    }

    #[test]
    fn audio_clone_failure_cleans_owned_file_and_preserves_replacements() {
        let lib = fixture("http://127.0.0.1:9");
        for replacement in 0..3 {
            let path = lib
                .paths
                .offline()
                .join(format!("generated-clone-failure-{replacement}.mp3"));
            let output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let result = AudioFile::from_created(path.clone(), output, |_| {
                if replacement != 0 {
                    std::fs::remove_file(&path).unwrap();
                    if replacement == 1 {
                        std::fs::write(&path, b"generated replacement").unwrap();
                    } else {
                        let target = lib.paths.offline().join("generated-clone-target.mp3");
                        std::fs::write(&target, b"generated replacement").unwrap();
                        std::os::unix::fs::symlink(target, &path).unwrap();
                    }
                }
                // Inject descriptor exhaustion without changing process limits
                // or interfering with concurrent generated host tests.
                Err(std::io::Error::from_raw_os_error(libc::EMFILE))
            });
            assert_eq!(
                result.err().unwrap(),
                fail("Unable to retain audio ownership")
            );
            if replacement == 0 {
                assert!(!path.exists());
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), b"generated replacement");
            }
        }
    }

    #[test]
    fn captured_transport_failures_never_expose_authenticated_urls() {
        let lib = fixture("http://127.0.0.1:9");
        let client = lib.client().unwrap();
        for error in [
            subsonic::Error::Network("http://synthetic/?apiKey=generated-secret".into()),
            subsonic::Error::Json("s=generated-secret".into()),
            subsonic::Error::Api {
                code: 50,
                message: "apiKey=generated-secret".into(),
            },
        ] {
            let error = lib
                .download_response::<()>(&client, Err(error))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("generated-secret"));
            assert!(!error.contains("http://"));
        }
    }
}
