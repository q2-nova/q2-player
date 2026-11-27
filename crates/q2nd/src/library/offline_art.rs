//! Download-owned artwork. Network/decode work happens before the short file/
//! database publication gate. Immutable files protect earlier shared downloads.
use super::{Library,LibError,db::OfflineArt};
use crate::gfx::Image;
use md5::{Digest,Md5};
use std::io::{Read,Write};
use std::os::unix::fs::{OpenOptionsExt,MetadataExt};
use std::path::{Path,PathBuf};
use std::sync::{Arc, Mutex};

/// One file/index publication owner. Every production caller awaits the same
/// owned permit; no synchronous compatibility waiter occupies a card worker.
pub(super) struct PublicationGate {
    available: std::sync::Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    pub(super) waiting_async: tokio::sync::Notify,
}
pub(super) struct PublicationPermit { _permit: tokio::sync::OwnedSemaphorePermit }
impl Default for PublicationGate {
    fn default() -> Self {
        Self { available: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)] waiting_async: tokio::sync::Notify::new() }
    }
}
impl PublicationGate {
    /// Generated fixtures reserve an initially available permit without a
    /// runtime. Contended production admission must use acquire().await.
    #[cfg(test)]
    pub(super) fn lock(self: &std::sync::Arc<Self>) -> PublicationPermit {
        PublicationPermit { _permit: self.available.clone().try_acquire_owned().expect("fixture permit available") }
    }
    pub(super) async fn acquire(self: &std::sync::Arc<Self>) -> PublicationPermit {
        let permit = match self.available.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
            #[cfg(test)]
            self.waiting_async.notify_one();
                self.available.clone().acquire_owned().await.expect("publication gate never closes")
            }
        };
        PublicationPermit { _permit: permit }
    }
}

const SIZE:u32=320;
const PNG_LIMIT:usize=512*1024;
struct Observed {path:PathBuf,valid:bool}
struct Created {path:PathBuf,file:std::fs::File}
pub(super) struct Prepared {key:Option<String>,png:Vec<u8>,observed:Option<Observed>,created:Mutex<Option<Created>>}
pub(super) struct Plan {ready:Option<Prepared>,missing:Option<(String,String,Option<Observed>)>}
fn empty()->Prepared {Prepared {key:None,png:Vec::new(),observed:None,created:Mutex::new(None)}}
fn fail(message:&str)->LibError {LibError::Server(message.into())}
fn key(namespace:&str,id:&str)->String {
    let namespace=if id.starts_with("shelf-book-cover:") || id.starts_with("local-book-cover:") {""} else {namespace};
    format!("1-{:x}",Md5::digest(serde_json::to_vec(&(namespace,id)).expect("art identity")))
}
/// Directly in the artwork directory (an earlier version) or in its shard.
fn owned_path(lib:&Library,key:&str,path:&Path)->bool {
    let root=lib.paths.offline_art();
    let Some(name)=path.file_name() else {return false;};
    let parent=path.parent();
    (parent==Some(root.as_path()) || parent==lib.paths.sharded(crate::shard::Store::OfflineArt,name).parent())
        && name.to_str()
        .and_then(|name|name.strip_prefix(&format!("{key}-")))
        .and_then(|name|name.strip_suffix(".png"))
        .is_some_and(|nonce|nonce.len()==16 && nonce.bytes().all(|byte|byte.is_ascii_hexdigit()))
}
/// An indexed file in either layout; never through a link.
fn read_png(blocking: &crate::rt::Blocking, lib:&Library, path:&Path)->Option<Vec<u8>> {
    let input=lib.paths.open_media(blocking, path).ok()?;
    let mut bytes=Vec::new();input.take(PNG_LIMIT as u64+1).read_to_end(&mut bytes).ok()?;
    (bytes.len()<=PNG_LIMIT).then_some(bytes)
}
/// Database snapshot first; card lookup/decode remains on the caller's finite
/// media lane. Neither a DB guard nor a worker is retained while awaiting SQL.
pub(super) struct Lookup { identity: Option<(String, String)>, indexed: Option<OfflineArt> }
fn identity(namespace: &str, id: Option<&str>) -> Result<Option<(String, String)>, LibError> {
    let Some(id) = id.filter(|id| !id.trim().is_empty()) else { return Ok(None); };
    if id.len() > 8192 { return Err(fail("Unsupported cover identity")); }
    Ok(Some((key(namespace, id), id.to_owned())))
}
pub(super) async fn lookup(lib: &Library, namespace: &str, id: Option<&str>) -> Result<Lookup, LibError> {
    let identity = identity(namespace, id)?;
    let indexed = if let Some((key, _)) = &identity {
        let key = key.clone();
        lib.db_run(move |blocking, db| db.offline_art(blocking, &key)).await.map_err(|_| fail("Unable to read artwork index"))?
    } else { None };
    Ok(Lookup { identity, indexed })
}
impl Lookup {
    pub(super) fn inspect(self, blocking: &crate::rt::Blocking, lib: &Library) -> Result<Plan, LibError> {
        let ready = |prepared| Plan { ready: Some(prepared), missing: None };
        let Some((key, id)) = self.identity else { return Ok(ready(empty())); };
        let mut observed = None;
        if let Some(art) = self.indexed {
            if !owned_path(lib, &key, &art.path) { return Err(fail("Invalid artwork index path")); }
            let bytes = read_png(blocking, lib, &art.path);
            let valid = bytes.as_ref().is_some_and(|bytes| Image::decode_square(bytes, SIZE).is_some());
            observed = Some(Observed { path: art.path, valid });
            if valid { return Ok(ready(Prepared { key: Some(key), png: bytes.unwrap(), observed, created: Mutex::new(None) })); }
        }
        Ok(Plan { ready: None, missing: Some((key, id, observed)) })
    }
}
#[cfg(test)]
fn plan(lib: &Library, namespace: &str, id: Option<&str>) -> Result<Plan, LibError> {
    let identity = identity(namespace, id)?;
    let indexed = if let Some((key, _)) = &identity {
        lib.db().offline_art(&crate::rt::Blocking::dedicated_thread(), key).map_err(|_| fail("Unable to read artwork index"))?
    } else { None };
    Lookup { identity, indexed }.inspect(&crate::rt::Blocking::dedicated_thread(), lib)
}

impl Plan {
    pub(super) fn request(&self)->Option<(String,u32)> {self.missing.as_ref().map(|(_,id,_)|(id.clone(),SIZE))}
    /// Decode/encode on a blocking worker after the optional request completes.
    pub(super) fn complete(self,bytes:Option<Vec<u8>>)->Result<Prepared,LibError> {
        if let Some(ready)=self.ready {return Ok(ready);}
        let (key,_,observed)=self.missing.expect("missing artwork request");
        let Some(bytes)=bytes else {return Ok(empty());};
        // A cover the player cannot use is published as no cover: failing the
        // download would fail it again on every retry. Fetch failures above
        // remain errors, so a transient outage never stores a coverless copy.
        let png=(bytes.len()<=4*1024*1024).then(||Image::decode_square(&bytes,SIZE)).flatten()
            .and_then(|image|image.png()).filter(|png|png.len()<=PNG_LIMIT);
        let Some(png)=png else {log::warn!("offline art: unusable cover image; saving without it");return Ok(empty());};
        Ok(Prepared {key:Some(key),png,observed,created:Mutex::new(None)})
    }
}
#[cfg(test)]
pub(super) fn prepare(lib:&Library,namespace:&str,id:Option<&str>,fetch:impl FnOnce(&str,u32)->Result<Option<Vec<u8>>,LibError>)->Result<Prepared,LibError> {
    let plan=plan(lib,namespace,id)?;
    let bytes=if let Some((id,size))=plan.request() {fetch(&id,size)?} else {None};
    plan.complete(bytes)
}
pub(super) fn sync_parents(blocking: &crate::rt::Blocking, root:&Path)->Result<(),LibError> {
    let root=root.canonicalize().map_err(|_|fail("Unable to inspect artwork storage"))?;
    let device=std::fs::metadata(&root).map_err(|_|fail("Unable to inspect artwork storage"))?.dev();
    let mut current=Some(root.as_path());
    while let Some(directory)=current {
        let metadata=std::fs::metadata(directory).map_err(|_|fail("Unable to inspect artwork directory"))?;
        if metadata.dev()!=device {break;}
        crate::shard::sync_directory(blocking, directory).map_err(|_|fail("Unable to save artwork directory"))?;
        current=directory.parent();
    }
    Ok(())
}
pub(super) struct ArmedArtwork {
    pub gate: PublicationPermit,
    pub indexed: Option<OfflineArt>,
    pub serial: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Prepared {
    /// Caller holds Library::offline_art_gate through DB publication and old
    /// file cleanup. No network/decode under the gate. Already committed files
    /// are immutable: reuse a qualified current row rather than overwrite it.
    pub(super) async fn index(&self, lib: &Library) -> Result<Option<OfflineArt>, LibError> {
        let Some(key) = self.key.clone() else { return Ok(None); };
        lib.db_run(move |blocking, db| db.offline_art(blocking, &key)).await.map_err(|_| fail("Unable to read artwork index"))
    }
    /// Recheck existing immutable art under the publication gate. A missing
    /// cover reserves its actual encoded length after releasing that gate,
    /// since pressure admission may need to retire another indexed file.
    pub(super) async fn admit_download(
        self: &Arc<Self>, lib: &Arc<Library>, storage: &Arc<super::mix_space::Ticket>, cancel: &super::transfer_io::Cancel,
    ) -> Result<ArmedArtwork, LibError> {
        let Some(key) = &self.key else {
            return Ok(ArmedArtwork { gate: lib.offline_art_gate.acquire().await, indexed: None, serial: None });
        };
        // Another download of the same cover waits without holding the
        // publication permit. Once the first commits, it needs no new credit.
        let serial = {
            let mut locks = lib.fetch_locks.lock().unwrap();
            if locks.len() > 64 { locks.retain(|_, lock| lock.strong_count() > 0); }
            let key = format!("offline-art:{key}");
            let lock = locks.get(&key).and_then(std::sync::Weak::upgrade).unwrap_or_default();
            locks.insert(key, Arc::downgrade(&lock));
            lock
        };
        let serial = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LibError::Offline),
            guard = serial.lock_owned() => guard,
        };
        let bytes = self.png.len() as u64;
        loop {
            let gate = lib.offline_art_gate.acquire().await;
            if cancel.is_cancelled() { return Err(LibError::Offline); }
            let indexed = self.index(lib).await?;
            if storage.covers(bytes, 1) {
                return Ok(ArmedArtwork { gate, indexed, serial: Some(serial) });
            }
            let owner = lib.clone();
            let prepared = self.clone();
            let candidate = indexed.clone();
            let reuse = super::downloads::lane().run_isolated(move |blocking| {
                prepared.valid_index(blocking, &owner, candidate.as_ref())
            }).await.map_err(|_| fail("Artwork worker stopped"))??;
            if reuse {
                return Ok(ArmedArtwork { gate, indexed, serial: Some(serial) });
            }
            drop(gate);
            storage.ensure(bytes, 1).await.map_err(super::downloads::space_error)?;
        }
    }

    pub(super) fn persist_admitted(
        &self, blocking: &crate::rt::Blocking, lib: &Library, indexed: Option<OfflineArt>,
        storage: Option<&super::mix_space::Ticket>,
    ) -> Result<Option<OfflineArt>, LibError> {
        self.persist_indexed_with_clone(blocking, lib, indexed, std::fs::File::try_clone, storage)
    }

    fn valid_index(
        &self, blocking: &crate::rt::Blocking, lib: &Library, indexed: Option<&OfflineArt>,
    ) -> Result<bool, LibError> {
        let (Some(key), Some(indexed)) = (&self.key, indexed) else { return Ok(false); };
        if !owned_path(lib, key, &indexed.path) { return Err(fail("Invalid artwork index path")); }
        let Some(bytes) = read_png(blocking, lib, &indexed.path).filter(|bytes| bytes.len() as u64 == indexed.size) else { return Ok(false); };
        // The same observed file was decoded outside the gate. A new
        // immutable row can only have come from another valid publisher.
        Ok(self.observed.as_ref().is_none_or(|old| old.path != indexed.path || old.valid && bytes == self.png))
    }

    #[cfg(test)]
    pub(super) fn persist(&self,lib:&Library)->Result<Option<OfflineArt>,LibError> {
        self.persist_with_clone(lib,std::fs::File::try_clone)
    }
    #[cfg(test)]
    fn persist_with_clone(&self,lib:&Library,clone:impl FnOnce(&std::fs::File)->std::io::Result<std::fs::File>)->Result<Option<OfflineArt>,LibError> {
        let indexed = self.key.as_ref().map(|key| lib.db().offline_art(&crate::rt::Blocking::dedicated_thread(), key)).transpose()
            .map_err(|_| fail("Unable to read artwork index"))?.flatten();
        self.persist_indexed_with_clone(&crate::rt::Blocking::dedicated_thread(), lib, indexed, clone, None)
    }
    fn persist_indexed_with_clone(&self,blocking: &crate::rt::Blocking,lib:&Library,indexed:Option<OfflineArt>,clone:impl FnOnce(&std::fs::File)->std::io::Result<std::fs::File>, storage:Option<&super::mix_space::Ticket>)->Result<Option<OfflineArt>,LibError> {
        let Some(key)=self.key.as_deref() else {return Ok(None);};
        if lib.paths.ephemeral_media {return Err(fail("Insert a card for offline artwork"));}
        let dir=lib.paths.offline_art();
        crate::shard::create_directory(blocking, &dir).map_err(|_|fail("Unable to create offline artwork directory"))?;
        if !crate::shard::metadata(blocking, &dir).ok().flatten().is_some_and(|metadata|metadata.file_type().is_dir()) {return Err(fail("Invalid offline artwork directory"));}
        sync_parents(blocking, lib.paths.media_root())?;
        if let Some(indexed) = indexed {
            if self.valid_index(blocking, lib, Some(&indexed))? {
                let found = lib.paths.locate(blocking, &indexed.path).ok_or_else(|| fail("Unable to verify saved artwork"))?;
                lib.paths.open_media(blocking, &found).and_then(|file| file.sync_all())
                    .and_then(|_| lib.paths.sync_sharded(blocking, &found)).map_err(|_| fail("Unable to verify saved artwork"))?;
                return Ok(Some(indexed));
            }
        }
        if storage.is_some_and(|ticket| !ticket.covers(self.png.len() as u64, 1)) {
            return Err(fail("Not enough card space for artwork"));
        }
        let reserve=lib.settings.read().unwrap().reserve_bytes;
        if super::free_space(blocking,&dir).is_none_or(|free|free<reserve.saturating_add(self.png.len() as u64)) {return Err(fail("Not enough card space for artwork"));}
        let guard=lib.paths.lock_store(blocking,crate::shard::Store::OfflineArt);
        let path=lib.paths.prepare_sharded(blocking,&guard,format!("{key}-{:016x}.png",fastrand::u64(..)).as_ref())
            .map_err(|_|fail("Unable to create offline artwork directory"))?;
        let original=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path).map_err(|_|fail("Unable to create offline artwork"))?;
        drop(guard);
        // Pin ownership before attempting any fallible descriptor duplication.
        // Even clone failure must only unlink this exact created inode.
        *self.created.lock().unwrap()=Some(Created {path:path.clone(),file:original});
        let writer={let created=self.created.lock().unwrap();clone(&created.as_ref().expect("created artwork owner").file)};
        let mut output=match writer {Ok(file)=>file,Err(_)=>{self.rollback_unpublished(blocking);return Err(fail("Unable to retain artwork ownership"));}};
        if output.write_all(&self.png).and_then(|_|output.sync_all()).and_then(|_|lib.paths.sync_sharded(blocking,&path)).is_err() {
            self.rollback_unpublished(blocking);return Err(fail("Unable to save offline artwork"));
        }
        if let Some(storage) = storage { storage.consumed(self.png.len() as u64); }
        Ok(Some(OfflineArt {key:key.into(),path,size:self.png.len() as u64}))
    }
    /// Preparation has not returned this fresh random path to a publisher yet.
    /// Under the publication gate it cannot have become an indexed file.
    fn rollback_unpublished(&self, blocking: &crate::rt::Blocking) {
        if let Some(created) = self.created.lock().unwrap().take() { remove_created(blocking, created); }
    }
    /// A failed/ambiguous publication keeps any indexed or unverifiable file.
    /// The caller still owns the publication gate across query and cleanup.
    pub(super) fn rollback_indexed(&self, blocking: &crate::rt::Blocking, indexed: Result<Option<OfflineArt>, LibError>) {
        let Some(created) = self.created.lock().unwrap().take() else { return; };
        match indexed {
            Err(_) => return,
            Ok(Some(art)) if art.path == created.path => return,
            _ => remove_created(blocking, created),
        }
    }
    #[cfg(test)]
    pub(super) fn rollback(&self, lib: &Library) {
        let indexed = self.key.as_ref().map(|key| lib.db().offline_art(&crate::rt::Blocking::dedicated_thread(), key)).transpose()
            .map(|value| value.flatten()).map_err(|_| fail("Unable to read artwork index"));
        self.rollback_indexed(&crate::rt::Blocking::dedicated_thread(), indexed);
    }

}
fn remove_created(blocking: &crate::rt::Blocking, created: Created) {
    crate::shard::remove_if_same(blocking, &created.path, &created.file);
}
pub(super) struct IndexedCover { key: String, art: Option<OfflineArt> }
impl IndexedCover {
    pub(super) fn load(self, blocking: &crate::rt::Blocking, lib: &Library, size: u32) -> Option<Image> {
        let art = self.art?;
        if !owned_path(lib, &self.key, &art.path) { return None; }
        Image::decode_square(&read_png(blocking, lib, &art.path)?, size)
    }
}
pub(super) fn indexed_cover(lib: &Library, namespace: &str, id: &str)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = IndexedCover> + Send>> {
    let key = key(namespace, id);
    let query = key.clone();
    let indexed = lib.db_run(move |blocking, db| db.offline_art(blocking, &query).ok().flatten());
    Box::pin(async move { IndexedCover { key, art: indexed.await } })
}
#[cfg(test)]
pub(super) fn load(lib:&Library,namespace:&str,id:&str,size:u32)->Option<Image> {
    crate::rt::Ui::new().unwrap().block_on(indexed_cover(lib, namespace, id)).load(&crate::rt::Blocking::dedicated_thread(), lib, size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn publication_gate_handles_cancelled_async_waiters_and_releases_owned_permits() {
        use std::future::Future;
        use std::task::Poll;
        let gate = Arc::new(PublicationGate::default());
        crate::rt::Ui::new().unwrap().block_on(async {
            let first = gate.acquire().await;
            let mut cancelled = Box::pin(gate.acquire());
            std::future::poll_fn(|cx| { assert!(cancelled.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            drop(cancelled);
            let mut surviving = Box::pin(gate.acquire());
            std::future::poll_fn(|cx| { assert!(surviving.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            drop(first);
            let second = tokio::time::timeout(std::time::Duration::from_secs(2), surviving).await.unwrap();
            let owner = gate.clone();
            let queued = crate::rt::io().spawn(async move { let permit = owner.acquire().await; drop(permit); 41 });
            // An unavailable publication permit does not tie up a finite worker.
            assert_eq!(crate::rt::spawn_blocking(|_| 17).await.unwrap(), 17);
            assert!(!queued.is_finished());
            drop(second);
            assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(2), queued).await.unwrap().unwrap(), 41);
            let last = gate.acquire().await;
            drop(last);
        });
    }

    #[test]
    fn encoded_cover_is_the_deciding_cost_and_shared_reuse_needs_no_credit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::os::unix::fs::MetadataExt;
        let root = super::super::generated_test_directory("mix-cover-budget");
        let lib = Library::create(
            crate::config::Paths::new(root), &crate::config::Config::default(), Arc::new(|_| {}),
        ).0;
        let blocking = &crate::rt::Blocking::dedicated_thread();
        let victim = lib.paths.offline().join("generated-mix.wav");
        std::fs::write(&victim, vec![91; 65536]).unwrap();
        let voice = subsonic::Song { id: "generated-mix".into(), ..Default::default() };
        let allocated = victim.metadata().unwrap().blocks() * 512;
        {
            let db = lib.db();
            assert_eq!(db.auto_music(blocking, None, &[(voice.clone(), 65536)], 65536).unwrap(), 1);
            db.put_offline_song(blocking, &voice, &victim, 65536);
            db.finish_download(blocking, &voice.id, None);
        }
        let prepared = Arc::new(prepare(&lib, "generated", Some("cover"), |_, _|
            Ok(Image::placeholder(24).png())).unwrap());
        let encoded = prepared.png.len() as u64;
        assert!(encoded > 1 && allocated >= encoded);
        let reserve = 10_000;
        let block = 4096;
        let audio = 1024;
        lib.settings.write().unwrap().reserve_bytes = reserve;
        let audio_written = Arc::new(AtomicBool::new(false));
        let observed = audio_written.clone();
        let exists = victim.clone();
        // Report exactly enough room for the audio but only half the encoded
        // cover after the audio write, until the real victim inode is removed.
        lib.mix_space.model_card(move || (
            reserve + block + encoded / 2
                + if observed.load(Ordering::Acquire) { 0 } else { audio }
                + if exists.exists() { 0 } else { allocated },
            block,
        ));
        let cancel = Arc::new(super::super::transfer_io::Cancel::default());
        let storage = super::super::mix_space::Ticket::new(lib.clone(), cancel.clone(), true, "generated-request".into());
        let runtime = crate::rt::Ui::new().unwrap();
        runtime.block_on(async {
            storage.ensure(audio, 1).await.unwrap();
            assert!(victim.exists(), "audio alone needs no reclamation");
            audio_written.store(true, Ordering::Release);
            storage.consumed(audio);
            let ArmedArtwork { gate, indexed, serial } = prepared.admit_download(&lib, &storage, &cancel).await.unwrap();
            assert!(!victim.exists(), "the actual encoded cover required this eviction");
            assert!(storage.covers(encoded, 1));
            let ready = prepared.clone();
            let owner = lib.clone();
            let credit = storage.clone();
            let cover = super::super::downloads::lane()
                .run_isolated(move |blocking| ready.persist_admitted(blocking, &owner, indexed, Some(&credit)))
                .await.unwrap().unwrap().unwrap();
            assert_eq!(cover.size, encoded);
            lib.db_run(move |blocking, db| db.put_spoken_downloads_with_art(
                blocking, "generated-first-owner", &[], Some(&cover),
            )).await.unwrap();
            drop(gate);
            drop(serial);
        });
        drop(storage);

        // The same already published immutable cover can be shared even
        // with no writable headroom. A PUT of another copy would fail.
        let shared = Arc::new(prepare(&lib, "generated", Some("cover"), |_, _| panic!("expected local cover")).unwrap());
        lib.mix_space.model_card(|| (0, 4096));
        let storage = super::super::mix_space::Ticket::new(lib.clone(), cancel.clone(), true, "generated-share".into());
        runtime.block_on(async {
            let armed = shared.admit_download(&lib, &storage, &cancel).await.unwrap();
            assert!(!storage.covers(encoded, 1));
            let first = armed.indexed.unwrap();
            let ready = shared.clone();
            let owner = lib.clone();
            let existing = first.clone();
            let cover = super::super::downloads::lane()
                .run_isolated(move |blocking| ready.persist_admitted(blocking, &owner, Some(existing), Some(&storage)))
                .await.unwrap().unwrap().unwrap();
            assert_eq!(cover.path, first.path);
            drop(armed.gate);
            drop(armed.serial);
        });
    }

    #[test]
    fn queued_cover_index_lookup_does_not_retain_library_or_a_card_worker() {
        use std::{sync::mpsc, time::Duration};
        let root = super::super::generated_test_directory("art-index-owner");
        let (lib, _) = Library::create(crate::config::Paths::new(root), &crate::config::Config::default(), Arc::new(|_| {}));
        let weak = Arc::downgrade(&lib);
        let (entered, begun) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv_timeout(Duration::from_secs(5)); });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let indexed = indexed_cover(&lib, "generated", "cover");
        drop(lib);
        assert!(weak.upgrade().is_none(), "a queued index read owns only the database request");
        release.send(()).unwrap();
        let result = crate::rt::Ui::new().unwrap().block_on(indexed);
        assert!(result.art.is_none());
        assert_eq!(result.key, key("generated", "cover"));
    }

    #[test]
    fn cover_identity_is_account_scoped_and_abs_independent_of_music_account() {
        for i in 0..1024 {
            let id=format!("cover-{i}");assert_ne!(key("account-a",&id),key("account-b",&id));
            let spoken=format!("shelf-book-cover:generated-account:{id}");assert_eq!(key("music-a",&spoken),key("music-b",&spoken));
            assert_ne!(key("",&spoken),key("",&format!("shelf-book-cover:other-account:{id}")));
        }
    }
    fn fixture()->Arc<Library> {
        let root=crate::library::generated_test_directory("offline-art");
        let mut config=crate::config::Config::default();config.storage.reserve_mb=0;
        Library::open(crate::config::Paths::new(root),&config,Arc::new(|_|{}))
    }
    #[test]
    fn declared_cover_failure_is_not_absence_and_unpublished_files_are_cleaned() {
        let lib=fixture();
        assert!(prepare(&lib,"generated",Some("cover"),|_,_|Err(LibError::Offline)).is_err());
        // An image the player cannot decode is no cover, not a download failure.
        assert!(prepare(&lib,"generated",Some("cover"),|_,_|Ok(Some(b"invalid image".to_vec()))).unwrap().key.is_none());
        assert!(prepare(&lib,"generated",Some("cover"),|_,_|Ok(Some(vec![0;4*1024*1024+1]))).unwrap().key.is_none());
        assert!(prepare(&lib,"generated",Some("cover"),|_,_|Ok(None)).unwrap().key.is_none());
        let prepared=prepare(&lib,"generated",Some("cover"),|_,_|Ok(Image::placeholder(SIZE).png())).unwrap();
        let _gate=lib.offline_art_gate.lock();let art=prepared.persist(&lib).unwrap().unwrap();assert!(art.path.exists());
        prepared.rollback(&lib);assert!(!art.path.exists());
        assert!(crate::library::stored_files(&lib.paths.offline_art()).is_empty());
    }
    #[test]
    fn create_collision_and_replaced_inode_are_never_deleted_as_owned_artwork() {
        let lib=fixture();let prepared=prepare(&lib,"generated",Some("cover"),|_,_|Ok(Image::placeholder(SIZE).png())).unwrap();
        std::fs::create_dir_all(lib.paths.offline_art()).unwrap();
        let seed=0xC0_11_1DE;let nonce=fastrand::Rng::with_seed(seed).u64(..);
        let collision=lib.paths.sharded(crate::shard::Store::OfflineArt,format!("{}-{nonce:016x}.png",prepared.key.as_ref().unwrap()).as_ref());
        std::fs::create_dir_all(collision.parent().unwrap()).unwrap();
        std::fs::write(&collision,b"generated preexisting sentinel").unwrap();
        let _gate=lib.offline_art_gate.lock();fastrand::seed(seed);
        assert!(prepared.persist(&lib).is_err());prepared.rollback(&lib);
        assert_eq!(std::fs::read(&collision).unwrap(),b"generated preexisting sentinel");
        std::fs::remove_file(&collision).unwrap();
        let art=prepared.persist(&lib).unwrap().unwrap();std::fs::remove_file(&art.path).unwrap();std::fs::write(&art.path,b"generated replacement inode").unwrap();
        prepared.rollback(&lib);assert_eq!(std::fs::read(&art.path).unwrap(),b"generated replacement inode");
    }
    #[test]
    fn artwork_lookup_awaits_database_before_card_inspection() {
        use std::{sync::mpsc, time::Duration};
        let lib = fixture();
        let (entered, begun) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        lib.db_post(move |_, _| {
            entered.send(()).unwrap();
            let _ = held.recv_timeout(Duration::from_secs(3));
        });
        begun.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            let pending = lookup(&lib, "generated", Some("cover"));
            tokio::pin!(pending);
            tokio::select! {
                _ = &mut pending => panic!("artwork lookup passed an occupied DB owner"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            assert_eq!(tokio::time::timeout(crate::library::TEST_LIVENESS, crate::rt::spawn_blocking(|_| 17)).await.unwrap().unwrap(), 17);
            release.send(()).unwrap();
            let observed = tokio::time::timeout(crate::library::TEST_LIVENESS, pending).await.unwrap().unwrap();
            let owner = lib.clone();
            let plan = crate::rt::spawn_blocking(move |blocking| observed.inspect(blocking, &owner)).await.unwrap().unwrap();
            assert_eq!(plan.request(), Some(("cover".into(), SIZE)));
            assert!(lib.paths.offline_art().read_dir().map(|mut rows| rows.next().is_none()).unwrap_or(true));
        });
    }
    #[test]
    fn failed_writer_clone_cleans_only_the_original_inode_and_preserves_replacements() {
        use std::os::unix::fs::symlink;
        for replacement in [None,Some(false),Some(true)] {
            let lib=fixture();let prepared=prepare(&lib,"generated",Some("cover"),|_,_|Ok(Image::placeholder(SIZE).png())).unwrap();
            let target=lib.paths.media_root().join("generated-clone-target");
            std::fs::write(&target,b"generated untouched target").unwrap();
            let _gate=lib.offline_art_gate.lock();let mut attempted=None;
            let result=prepared.persist_with_clone(&lib,|original| {
                let path=crate::library::stored_files(&lib.paths.offline_art())[0].clone();
                let owned=original.metadata().unwrap();let before=std::fs::symlink_metadata(&path).unwrap();
                assert_eq!((owned.dev(),owned.ino()),(before.dev(),before.ino()));
                if let Some(symbolic)=replacement {
                    std::fs::remove_file(&path).unwrap();
                    if symbolic {symlink(&target,&path).unwrap();}
                    else {std::fs::write(&path,b"generated replacement inode").unwrap();}
                }
                attempted=Some(path);
                Err(std::io::Error::other("generated descriptor clone failure"))
            });
            assert!(result.is_err());prepared.rollback(&lib);
            let path=attempted.unwrap();
            match replacement {
                None=>assert!(!path.exists(),"the original unindexed file is cleaned"),
                Some(false)=>assert_eq!(std::fs::read(&path).unwrap(),b"generated replacement inode"),
                Some(true)=>{assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());assert_eq!(std::fs::read_link(&path).unwrap(),target);},
            }
            assert_eq!(std::fs::read(&target).unwrap(),b"generated untouched target");
        }
    }
    #[test]
    fn concurrent_prepares_and_failed_second_commit_preserve_the_first_shared_image() {
        let lib=fixture();let mut red=Image::placeholder(SIZE);red.pixels.fill(0xFF0000);
        let mut blue=Image::placeholder(SIZE);blue.pixels.fill(0x0000FF);
        let first=prepare(&lib,"generated",Some("shared"),|_,_|Ok(red.png())).unwrap();
        let second=prepare(&lib,"generated",Some("shared"),|_,_|Ok(blue.png())).unwrap();
        let _gate=lib.offline_art_gate.lock();
        let original=first.persist(&lib).unwrap().unwrap();lib.db().put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), "first",&[("part-a".into(),PathBuf::from("a.m4b"),1)],Some(&original)).unwrap();
        let bytes=std::fs::read(&original.path).unwrap();
        let connection=rusqlite::Connection::open(lib.paths.database()).unwrap();connection.execute_batch("CREATE TRIGGER fail_art BEFORE INSERT ON offline_art_refs BEGIN SELECT RAISE(ABORT,'generated second owner fault'); END;").unwrap();
        let reused=second.persist(&lib).unwrap().unwrap();assert_eq!(reused,original);
        assert!(lib.db().put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), "second",&[("part-b".into(),PathBuf::from("b.m4b"),1)],Some(&reused)).is_err());second.rollback(&lib);
        assert_eq!(std::fs::read(&original.path).unwrap(),bytes);assert_eq!(lib.db().offline_art(&crate::rt::Blocking::dedicated_thread(), &original.key).unwrap(),Some(original));
        assert_eq!(crate::library::stored_files(&lib.paths.offline_art()).len(),1);
    }
    #[test]
    fn persistent_cover_survives_disposable_cache_clear_and_reopens_at_multiple_sizes() {
        let lib=fixture();let prepared=prepare(&lib,"generated",Some("cover"),|_,_|Ok(Image::placeholder(SIZE).png())).unwrap();
        {let _gate=lib.offline_art_gate.lock();let art=prepared.persist(&lib).unwrap().unwrap();lib.db().put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), "generated-book",&[("part".into(),PathBuf::from("generated.m4b"),1)],Some(&art)).unwrap();prepared.rollback(&lib);}
        std::fs::remove_dir_all(lib.paths.art_cache()).unwrap();lib.set_offline_mode(true);
        for size in [50,150,320] {let image=load(&lib,"generated","cover",size).unwrap();assert_eq!((image.width,image.height),(size,size));}
        let reopened=Library::open(lib.paths.clone(),&crate::config::Config::default(),Arc::new(|_|{}));reopened.set_offline_mode(true);
        assert!(load(&reopened,"generated","cover",150).is_some());
        assert!(load(&reopened,"other-account","cover",150).is_none());
    }
}
