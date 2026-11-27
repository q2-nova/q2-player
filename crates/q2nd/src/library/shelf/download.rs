//! One owned async audiobook transaction, with finite local file/DB jobs.
use super::{Client, Library, Policy, Song, SpokenDownload, SpokenSource, fail};
use crate::audio::source::Progress;
use crate::library::{offline_art, transfer_io::Cancel};
use md5::{Digest, Md5};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Default)]
pub(in crate::library) struct Queue {
    pub(in crate::library) wake: tokio::sync::Notify,
    current: std::sync::Mutex<Option<(String, String, Arc<Progress>, u64, u64)>>,
    saved: std::sync::Mutex<Option<SavedBook>>,
    generation: std::sync::Mutex<Option<Arc<()>>>,
    #[cfg(test)] pub(super) after_clear: std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>,std::sync::mpsc::Receiver<()>)>>,
}
#[derive(Default)]
pub(crate) struct Status { pub queued: usize, pub failed: usize, pub current: Option<(String,Option<f32>)> }
impl Queue {
    pub(in crate::library) fn set_generation(&self, generation: Arc<()>) {
        let mut current=self.generation.lock().unwrap();
        *current=Some(generation);
        self.discard_all();
    }
    pub(in crate::library) fn discard_all(&self) {let saved=self.saved.lock().unwrap().take();drop(saved);}
    pub(in crate::library) fn discard_saved(&self,logical:&str) {
        let saved={let mut slot=self.saved.lock().unwrap();if slot.as_ref().is_some_and(|saved|saved.logical==logical) {slot.take()} else {None}};
        drop(saved);
    }
    pub(in crate::library) async fn discard_saved_automatic(&self) -> std::io::Result<()> {
        let saved = {
            let mut slot = self.saved.lock().unwrap();
            if slot.as_ref().is_some_and(|saved| saved.automatic) { slot.take() } else { None }
        };
        if let Some(saved) = saved {
            crate::library::downloads::lane().run_isolated(move |_| drop(saved)).await.map_err(std::io::Error::other)?;
        }
        Ok(())
    }
    pub(super) fn active_item(&self) -> Option<(String, Option<f32>)> {
        self.current.lock().unwrap().as_ref().map(|(logical,_,p,base,total)| {
            (logical.clone(),(*total>0).then_some((base.saturating_add(p.snapshot().written) as f32 / *total as f32).clamp(0.0,1.0)))
        })
    }
    pub(super) fn status(&self, (queued,failed):(usize,usize))->Status {
        let current=self.current.lock().unwrap().as_ref().map(|(_,title,p,base,total)| {
            let written=base.saturating_add(p.snapshot().written);
            (title.clone(),(*total>0).then_some((written as f32 / *total as f32).clamp(0.0,1.0)))
        });
        Status {queued,failed,current}
    }
}
#[cfg(test)]
async fn test_after_clear(queue:&Queue) {
    let hook={queue.after_clear.lock().unwrap().take()};
    if let Some((entered,release))=hook {
        let _=crate::rt::spawn_blocking(move |_| {let _=entered.send(());let _=release.recv_timeout(std::time::Duration::from_secs(5));}).await;
    }
}
pub(super) fn start(lib:&Arc<Library>) {
    let weak=Arc::downgrade(lib);
    let queue=lib.shelf.read().unwrap().queue.clone();
    crate::rt::io().spawn(async move {
        loop {
            tokio::select! { ()=queue.wake.notified()=>{}, ()=tokio::time::sleep(std::time::Duration::from_secs(15))=>{} }
            let Some(lib)=weak.upgrade() else {break};
            if lib.offline_mode() || lib.downloads.is_paused() {continue;}
            // Configuration may hold the shelf lock while acquiring the file
            // gate. Snapshot on a finite worker before taking that gate.
            let owner=lib.clone();
            let snapshot=crate::rt::spawn_blocking(move |_| {
                let slot=owner.shelf.read().unwrap();
                (slot.namespace.clone(),slot.generation.clone(),slot.downloads.clone())
            }).await;
            let Ok((namespace,generation,registry))=snapshot else {continue};
            if namespace.is_empty() {continue;}
            let gate=lib.offline_art_gate.acquire().await;
            let account=namespace.clone();
            let allow_auto = lib.automatic_downloads_enabled() && !lib.mix_space.manual_active() && lib.is_online_for_shelf()
                && !lib.link_pending.load(std::sync::atomic::Ordering::Acquire);
            let next = lib.shelf_db_run(generation.clone(), move |blocking, db| {
                db.admit_collection_members(blocking, &account).map_err(super::fail)?;
                let allow_auto = allow_auto && !db.manual_download_waiting(&account).unwrap_or(true);
                db.next_eligible_spoken(blocking, &account, allow_auto).map_err(super::fail)
            }).await;
            let Ok(Some((ticket,logical,item,episode,title,automatic)))=next else {drop(gate);continue};
            let Some(intent)=registry.claim(logical.clone()) else {drop(gate);continue};
            let request=SpokenDownload {intent,item,episode,configuration:generation.clone(),namespace,ticket:Some(ticket),automatic};
            drop(gate);
            let progress=Progress::new();
            *queue.current.lock().unwrap()=Some((logical,title,progress.clone(),0,0));
            lib.notify(crate::library::LibEvent::Downloads);
            let operation=run_with_progress(lib.clone(),request,Some((queue.clone(),progress)));
            tokio::pin!(operation);
            let result=loop {tokio::select! {
                result=&mut operation=>break result,
                ()=tokio::time::sleep(std::time::Duration::from_secs(1))=>lib.notify(crate::library::LibEvent::Downloads),
            }};
            *queue.current.lock().unwrap()=None;
            #[cfg(test)]
            test_after_clear(&queue).await;
            let owner=lib.clone();
            let current=crate::rt::spawn_blocking(move |_|owner.shelf_generation_current(&generation)).await.unwrap_or(false);
            let retry=result.as_ref().err().is_some_and(|e| e==&subsonic::audiobookshelf::Error::Network.to_string() || e==&subsonic::audiobookshelf::Error::Server.to_string() || e==interrupted() || lib.offline_mode() || lib.downloads.is_paused() || !current);
            if !retry {
                let error=result.err();
                let _=lib.db_run(move |blocking,db|db.finish_spoken_queue(blocking,ticket,error.as_deref())).await;
                queue.wake.notify_one();
            }
            lib.notify(crate::library::LibEvent::Downloads);
        }
    });
}

struct SavedBook {logical:String,ticket:i64,generation:Arc<()>,automatic:bool, ids:Vec<String>, created:Vec<(PathBuf,std::fs::File)>, states:Vec<super::super::resumable::State>}
fn retire_saved(blocking:&crate::rt::Blocking,created:Vec<(PathBuf,std::fs::File)>) {
    for (path,file) in created { crate::shard::remove_if_same(blocking,&path,&file); }
}
impl Drop for SavedBook {
    fn drop(&mut self) {
        let created=std::mem::take(&mut self.created);
        crate::rt::retire_blocking(move |blocking|retire_saved(blocking,created));
    }
}

/// Each new file's shard directory, and the download directory above them.
fn sync_directories(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, files: Vec<PathBuf>) -> Result<(), String> {
    let mut synced = std::collections::HashSet::new();
    for file in files {
        if !synced.insert(file.parent().map(PathBuf::from)) { continue; }
        paths.sync_sharded(blocking, &file).map_err(|_| "Unable to save audiobook directory".to_string())?;
    }
    Ok(())
}

fn changed() -> String { subsonic::audiobookshelf::Error::Changed.to_string() }
fn stopped() -> String { "Audiobook download worker stopped".into() }
fn interrupted() -> &'static str { "Audiobook download interrupted" }
fn observe_result(lib:&Library, queued:bool, result:Result<(),String>)->Result<(),String> {
    if result.is_err() && (lib.offline_mode() || (queued && lib.downloads.is_paused())) { Err(interrupted().into()) } else {result}
}

pub(super) async fn run(lib: Arc<Library>, request: SpokenDownload) -> Result<(), String> {
    run_with_progress(lib,request,None).await
}

async fn run_with_progress(lib: Arc<Library>, request: SpokenDownload, progress: Option<(Arc<Queue>,Arc<Progress>)>) -> Result<(), String> {
    let _active = lib.downloads.start_transfer();
    let request = Arc::new(request);
    let cancel = Arc::new(Cancel::default());
    let queued = progress.is_some();
    let operation = transfer(lib.clone(), request.clone(), cancel.clone(), progress,queued);
    tokio::pin!(operation);
    loop {
        let wake = lib.downloads.changed();
        tokio::pin!(wake);
        wake.as_mut().enable();
        if request.automatic && (!lib.automatic_downloads_enabled() || lib.mix_space.preempting() || !lib.is_online_for_shelf()) {
            cancel.cancel();
            return operation.await.or_else(|_| Err(interrupted().into()));
        }
        let owner=lib.clone();let generation=request.configuration.clone();
        let checking=crate::rt::spawn_blocking(move |_| owner.shelf_generation_current(&generation));
        let current=tokio::select! {
            result = &mut operation => return observe_result(&lib,queued,result),
            current = checking => current.unwrap_or(false),
            () = request.intent.superseded() => {
                cancel.cancel();
                return operation.await;
            },
        };
        if !queued && lib.offline_mode() {
            cancel.cancel();
            let result=operation.await;
            // Publication may already have won cancellation. Preserve that
            // success; otherwise remember the interruption even if reconnect
            // happens while the worker is joining its owned cleanup.
            return result.or_else(|_|Err(interrupted().into()));
        }
        if !current { cancel.cancel(); return operation.await; }
        tokio::select! {
            result = &mut operation => return observe_result(&lib,queued,result),
            () = request.intent.superseded() => {
                cancel.cancel();
                return operation.await;
            },
            () = wake => {},

        }
    }
}

async fn network<T>(cancel: &Cancel, future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(changed()),
        result = future => result,
    }
}

async fn transfer(lib: Arc<Library>, request: Arc<SpokenDownload>, cancel: Arc<Cancel>, progress: Option<(Arc<Queue>,Arc<Progress>)>, queued:bool) -> Result<(), String> {
    request.intent.check()?;
    let storage = crate::library::mix_space::Ticket::new(lib.clone(), cancel.clone(), !request.automatic, request.intent.logical.clone());
    if lib.paths.ephemeral_media { return Err("Insert a card to download audiobooks".into()); }
    let client = network(&cancel, async { lib.shelf_client_async().await.map_err(|e| e.to_string()) }).await?;
    let songs = network(&cancel, lib.shelf_open_async(request.item.clone(), request.episode.clone())).await?;
    let owner=lib.clone();let active=client.clone();let pending=request.clone();
    let songs=crate::rt::spawn_blocking(move |_| {
        if !owner.shelf_current(&active) || songs.is_empty() || !songs.iter().all(|song| {
            song.spoken.as_ref().is_some_and(|p| matches!(&p.source, SpokenSource::Audiobookshelf { namespace, .. } if Some(namespace.as_str()) == active.cached_namespace()))
        }) { return Err(changed()); }
        pending.intent.check()?;
        if songs[0].spoken.as_ref().is_none_or(|spoken| spoken.id != pending.intent.logical) { return Err(changed()); }
        Ok(songs)
    }).await.map_err(|_|stopped())??;
    let target = crate::library::book_info::BookInfoTarget::from_song(&songs[0]).ok_or("Missing audiobook information identity")?;
    network(&cancel, async { lib.book_info_async(target, Policy::NetworkOnly).await.map_err(|e| e.to_string()) }).await?;
    let owner = lib.clone();
    let cover = songs[0].cover_art.clone();
    let lookup = offline_art::lookup(&lib, "", cover.as_deref()).await.map_err(|e| e.to_string())?;
    let plan = crate::rt::spawn_blocking(move |blocking| lookup.inspect(blocking, &owner))
        .await.map_err(|_| stopped())?.map_err(|e| e.to_string())?;
    let bytes = if let Some((_, size)) = plan.request() {
        network(&cancel, async {
            match client.cover_async(&request.item, size).await {
                Ok(bytes) => Ok(Some(bytes)),
                Err(subsonic::audiobookshelf::Error::Missing) => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        }).await?
    } else { None };
    let active = client.clone();
    let pending = request.clone();
    let owner=lib.clone();
    let mut state = crate::rt::spawn_blocking(move |blocking| {
        let artwork = plan.complete(bytes).map_err(|e| e.to_string())?;
        Download::prepare(blocking, owner, active, pending, songs, artwork)
    }).await.map_err(|_| stopped())??;
    if let Some((queue,_))=&progress {
        let saved={queue.saved.lock().unwrap().take()};
        if let Some(mut saved)=saved {
            let matches=request.ticket==Some(saved.ticket) && request.intent.logical==saved.logical && Arc::ptr_eq(&request.configuration,&saved.generation)
                && saved.ids.len()<=state.songs.len() && saved.ids.iter().zip(&state.songs).all(|(id,song)|id==&song.id);
            if matches {state.created=std::mem::take(&mut saved.created);state.states=std::mem::take(&mut saved.states);}
        }
    }
    let total_book=state.songs.iter().fold(0u64,|sum,s|sum.saturating_add(s.size.unwrap_or(0)));
    let retained = state.states.iter().zip(&state.songs).fold(0u64, |sum, (saved, song)|
        sum.saturating_add(saved.offset.min(song.size.unwrap_or(0))));
    let planned = total_book.saturating_sub(retained);
    if let Err(error) = storage.ensure(planned, state.songs.len() as u64).await {
        let message = if error.kind() == std::io::ErrorKind::Interrupted { interrupted() }
            else if error.raw_os_error() == Some(libc::ENOSPC) { "Not enough free card space for this audiobook" }
            else { "Unable to prepare space for download" };
        return state.finish(Err(message.into()), cancel, storage).await;
    }
    let mut completed_bytes=0u64;
    let result = loop {
        let (returned, next) = crate::rt::spawn_blocking(move |blocking| {
            let next = state.open(blocking);
            (state, next)
        }).await.map_err(|_| stopped())?;
        state = returned;
        let (url, total, path, pinned, resume) = match next {
            Ok(Some(next)) => next,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        let file_progress=Progress::new();
        if let Some((queue,_))=&progress {
            if let Some((_,_,p,base,total))=queue.current.lock().unwrap().as_mut() { *p=file_progress.clone();*base=completed_bytes;*total=total_book; }
        }
        let active=client.clone();
        let fetch:super::super::resumable::Fetch=Arc::new(move |offset,tag| {
            let active=active.clone();let url=url.clone();
            Box::pin(async move {active.download_response_async(&url,offset,tag.as_deref()).await.map_err(|error|match error {
                subsonic::audiobookshelf::Error::Network|subsonic::audiobookshelf::Error::Server=>super::super::resumable::WireError::Retry,
                _=>super::super::resumable::WireError::Fatal(error.to_string()),
            })})
        });
        let copied=super::super::resumable::file(lib.clone(),path,pinned,Some(total),file_progress,cancel.clone(),queued,resume,fetch,storage.clone()).await;
        match copied {
            Ok((_,resume)) => {
                let index=state.completed.len();state.states[index]=resume;
                let song = &state.songs[state.completed.len()];
                let path = &state.created[index].0;
                state.completed.push((song.id.clone(), path.clone(), total));
                completed_bytes=completed_bytes.saturating_add(total);
            },
            Err(super::super::resumable::Error::Suspended(resume)) => {
                let index=state.completed.len();state.states[index]=resume;
                if let (Some((queue,_)),Some(ticket))=(&progress,request.ticket) {
                    let gate=lib.offline_art_gate.acquire().await;
                    let logical=request.intent.logical.clone();let pending=logical.clone();
                    let live=request.intent.current() && lib.db_run(move |blocking,db|db.spoken_download_ticket_current(blocking,&pending,ticket).unwrap_or(false)).await;
                    let generation=queue.generation.lock().unwrap();
                    if live && request.intent.current() && generation.as_ref().is_some_and(|g|Arc::ptr_eq(g,&request.configuration)) {
                        let ids=state.songs.iter().take(state.created.len()).map(|song|song.id.clone()).collect();
                        let saved=SavedBook {logical,ticket,generation:request.configuration.clone(),automatic:request.automatic,ids,created:std::mem::take(&mut state.created),states:std::mem::take(&mut state.states)};
                        *queue.saved.lock().unwrap()=Some(saved);drop(gate);
                        return Err(interrupted().into());
                    }
                    drop(gate);
                }
                break Err(changed());
            },
            Err(super::super::resumable::Error::Cancelled) => break Err(changed()),
            Err(super::super::resumable::Error::Fatal(message)) => break Err(message),
        }
    };
    state.finish(result, cancel, storage).await
}

struct Download {
    lib: Arc<Library>,
    client: Arc<Client>,
    request: Arc<SpokenDownload>,
    songs: Vec<Song>,
    artwork: Arc<offline_art::Prepared>,
    directory: PathBuf,
    created: Vec<(PathBuf, std::fs::File)>,
    states: Vec<super::super::resumable::State>,
    completed: Vec<(String, PathBuf, u64)>,
}
impl Drop for Download {
    fn drop(&mut self) {
        // The normal detached owner joins publication/rollback. If its reply or
        // task is abandoned, at least descriptor retirement must remain off the
        // executor; never guess database publication from cancellation alone.
        crate::rt::drop_off_executor(std::mem::take(&mut self.created));
    }
}
impl Download {
    fn prepare(blocking: &crate::rt::Blocking, lib: Arc<Library>, client: Arc<Client>, request: Arc<SpokenDownload>, songs: Vec<Song>, artwork: offline_art::Prepared) -> Result<Self, String> {
        let directory = lib.paths.spoken_downloads();
        crate::shard::create_directory(blocking, &directory).map_err(|_| "Unable to create durable audiobook download directory")?;
        if !crate::shard::metadata(blocking, &directory).ok().flatten().is_some_and(|metadata| metadata.file_type().is_dir()) { return Err("Invalid audiobook download directory".into()); }
        offline_art::sync_parents(blocking, lib.paths.media_root()).map_err(|e| e.to_string())?;
        Ok(Self { lib, client, request, songs, artwork: Arc::new(artwork), directory, created: Vec::new(), states:Vec::new(), completed: Vec::new() })
    }

    fn open(&mut self, blocking: &crate::rt::Blocking) -> Result<Option<(String, u64, PathBuf, Arc<std::fs::File>, super::super::resumable::State)>, String> {
        self.request.intent.check()?;
        if self.lib.offline_mode() || !self.lib.shelf_current(&self.client) { return Err(changed()); }
        let Some(song) = self.songs.get(self.completed.len()) else { return Ok(None); };
        let Some(SpokenSource::Audiobookshelf { content_url, .. }) = song.spoken.as_ref().map(|p| &p.source) else { return Err("Invalid audiobook source".into()); };
        let index=self.completed.len();
        if let Some((path,pinned))=self.created.get(index) {
            let output=pinned.try_clone().map_err(|_|"Unable to retain audiobook ownership")?;
            return Ok(Some((content_url.clone(),song.size.unwrap_or(0),path.clone(),Arc::new(output),self.states[index].clone())));
        }
        let suffix = song.suffix.as_deref().filter(|suffix| matches!(*suffix, "aac"|"m4b"|"m4a"|"mp3"|"flac"|"ogg"|"opus"|"wav"|"wma"|"aiff"|"alac")).unwrap_or("audio");
        let name = format!("{:x}-{:016x}.{suffix}", Md5::digest(song.id.as_bytes()), fastrand::u64(..));
        let guard = self.lib.paths.lock_store(blocking, crate::shard::Store::Spoken);
        let path = self.lib.paths.prepare_sharded(blocking, &guard, name.as_ref())
            .map_err(|_| "Unable to create durable audiobook download directory")?;
        let pinned = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|_| "Unable to create audiobook on the card")?;
        drop(guard);
        self.created.push((path, pinned));self.states.push(super::super::resumable::State::default());
        // Pin the original inode before this fallible clone so failure cannot
        // orphan the new file or delete a replacement during final cleanup.
        let (path,pinned)=self.created.last().unwrap();
        let output = pinned.try_clone().map_err(|_| "Unable to retain audiobook ownership")?;
        Ok(Some((content_url.clone(), song.size.unwrap_or(0), path.clone(), Arc::new(output),super::super::resumable::State::default())))
    }

    fn finish_files(&self, blocking: &crate::rt::Blocking, lib: &Library,
        published: &Result<Vec<PathBuf>, String>, keep: &std::collections::HashSet<PathBuf>, has_art: bool) {
        match published {
            Err(_) => {
                for (path, file) in &self.created {
                    if keep.contains(path) { continue; }
                    crate::shard::remove_if_same(blocking, path, file);
                }
            }
            Ok(old) => {
                // Replaced files, flat or sharded.
                for path in old {
                    if path.starts_with(&self.directory) || path.starts_with(lib.paths.offline_art()) {
                        lib.remove_media_file(blocking, path);
                    }
                }
                if has_art { if let Some(id) = self.songs[0].cover_art.as_deref() { lib.art.downloaded_cover(id); } }
                lib.notify(crate::library::LibEvent::Downloads);
            }
        }
    }

    async fn finish(self, mut result: Result<(), String>, cancel: Arc<Cancel>, storage: Arc<crate::library::mix_space::Ticket>) -> Result<(), String> {
        let lib = self.lib.clone();
        // Acquire shelf admission before the file gate: configuration may hold
        // its shelf/client guard while waiting for that gate in legacy paths.
        let generation = if result.is_ok() {
            lib.shelf_admission(self.client.clone()).await.map(Some).map_err(|e| e.to_string())
        } else { Ok(None) };
        let artwork = if result.is_ok() && generation.is_ok() && self.request.intent.check().is_ok() {
            match self.artwork.admit_download(&lib, &storage, &cancel).await {
                Ok(cover) => Some(cover),
                Err(error) => { result = Err(error.to_string()); None }
            }
        } else { None };
        let (gate, checked_cover, serial) = match artwork {
            Some(art) => (art.gate, Some(art.indexed), art.serial),
            None => (lib.offline_art_gate.acquire().await, None, None),
        };
        let mut has_art = false;
        let published = async {
            result?;
            let generation = generation?.expect("successful transfer has shelf admission");
            let files = self.completed.iter().map(|(_, path, _)| path.clone()).collect::<Vec<_>>();
            let paths = lib.paths.clone();
            crate::library::downloads::lane().run_isolated(move |blocking| sync_directories(blocking, &paths, files)).await.map_err(|_| stopped())??;
            self.request.intent.check()?;
            if lib.offline_mode() || !cancel.begin_publication() { return Err(changed()); }
            let indexed = match checked_cover {
                Some(indexed) => indexed,
                None => return Err(changed()),
            };
            let artwork = self.artwork.clone(); let owner = lib.clone();
            let credit = storage.clone();
            let art = crate::library::downloads::lane().run_isolated(move |blocking| artwork.persist_admitted(blocking, &owner, indexed, Some(&credit)))
                .await.map_err(|_| stopped())?.map_err(|e| e.to_string())?;
            has_art = art.is_some();
            let logical = self.request.intent.logical.clone();
            let completed = self.completed.clone();
            lib.shelf_db_run(generation, move |blocking, db| {
                db.put_spoken_downloads_with_art(blocking, &logical, &completed, art.as_ref()).map_err(fail)
            }).await.map_err(|e| e.to_string())
        }.await;
        let keep = if published.is_err() {
            let completed = self.completed.clone();
            lib.db_run(move |blocking, db| completed.into_iter().filter_map(|(id, path, _)| {
                match db.spoken_download(blocking, &id) {
                    Err(_) => Some(path),
                    Ok(Some(indexed)) if indexed == path => Some(path),
                    _ => None,
                }
            }).collect::<std::collections::HashSet<_>>()).await
        } else { Default::default() };
        let rollback = if published.is_err() { Some(self.artwork.index(&lib).await) } else { None };
        crate::library::downloads::lane().run_isolated(move |blocking| {
            if let Some(indexed) = rollback { self.artwork.rollback_indexed(blocking, indexed); }
            self.finish_files(blocking, &lib, &published, &keep, has_art);
            // Retire descriptors and the owned publication gate on this finite
            // card worker, after every accepted SQL operation has completed.
            drop(self); drop(gate); drop(serial);
            published.map(|_| ())
        }).await.map_err(|_| stopped())?
    }
}
