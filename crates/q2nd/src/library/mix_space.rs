//! Audio space admission shared by the two download workers. Outstanding
//! allocations are reserved, while the network and the card jobs stay parallel.
use super::{
    LibEvent, Library,
    db::mix_space::{MixAudio, RetiredAudio},
    transfer_io::Cancel,
};
use std::{
    collections::{HashMap, HashSet},
    io,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
pub(super) struct Space {
    admission: tokio::sync::Mutex<()>,
    reserved: Mutex<u64>,
    manual: AtomicUsize,
    automatic: AtomicUsize,
    preempt: AtomicBool,
    changed: tokio::sync::Notify,
    block_bytes: AtomicUsize,
    #[cfg(test)]
    modeled_card: Mutex<Option<Arc<dyn Fn() -> (u64, u64) + Send + Sync>>>,
}

impl Space {
    pub(super) fn manual_active(&self) -> bool {
        self.manual.load(Ordering::Acquire) != 0
    }

    #[cfg(test)]
    pub(super) fn model_card(&self, model: impl Fn() -> (u64, u64) + Send + Sync + 'static) {
        *self.modeled_card.lock().unwrap() = Some(Arc::new(model));
    }

    pub(super) fn preempting(&self) -> bool {
        self.preempt.load(Ordering::Acquire)
    }
}

pub(super) struct Ticket {
    lib: Arc<Library>,
    cancel: Arc<Cancel>,
    manual: bool,
    id: String,
    reserved: Mutex<u64>,
}

impl Ticket {
    pub(super) fn new(
        lib: Arc<Library>,
        cancel: Arc<Cancel>,
        manual: bool,
        id: String,
    ) -> Arc<Self> {
        if manual {
            lib.mix_space.manual.fetch_add(1, Ordering::AcqRel);
        } else {
            lib.mix_space.automatic.fetch_add(1, Ordering::AcqRel);
        }
        Arc::new(Self {
            lib,
            cancel,
            manual,
            id,
            reserved: Mutex::new(0),
        })
    }

    pub(super) fn admitted(&self, bytes: u64) -> bool {
        *self.reserved.lock().unwrap() >= bytes
    }

    pub(super) fn covers(&self, bytes: u64, files: u64) -> bool {
        let block = self.lib.mix_space.block_bytes.load(Ordering::Acquire) as u64;
        block != 0 && self.admitted(bytes.saturating_add(block.saturating_mul(files)))
    }

    pub(super) fn consumed(&self, bytes: u64) {
        *self.reserved.lock().unwrap() -= bytes;
        *self.lib.mix_space.reserved.lock().unwrap() -= bytes;
    }

    pub(super) async fn ensure(&self, bytes: u64, files: u64) -> io::Result<()> {
        let pad = |block_bytes: u64| bytes.saturating_add(block_bytes.saturating_mul(files));
        let remaining = *self.reserved.lock().unwrap();
        let known_block = self.lib.mix_space.block_bytes.load(Ordering::Acquire) as u64;
        if known_block != 0 && remaining >= pad(known_block) {
            return Ok(());
        }
        loop {
            let admission = self.lib.mix_space.admission.lock().await;
            if self.cancel.is_cancelled() || (!self.manual && self.lib.mix_space.preempting()) {
                return Err(cancelled());
            }
            // Observe reservations before free bytes: a completing writer can
            // only make this snapshot conservative, never manufacture headroom.
            let outstanding = *self.lib.mix_space.reserved.lock().unwrap();
            let (free, block_bytes) = card_space(&self.lib).await?;
            self.lib
                .mix_space
                .block_bytes
                .store(block_bytes as usize, Ordering::Release);
            let additional = pad(block_bytes).saturating_sub(remaining);
            if additional == 0 {
                return Ok(());
            }
            let required = self
                .lib
                .settings
                .read()
                .unwrap()
                .reserve_bytes
                .saturating_add(outstanding)
                .saturating_add(additional);
            if free < required {
                if !self.manual {
                    return Err(full());
                }
                if self.lib.mix_space.automatic.load(Ordering::Acquire) != 0 {
                    self.lib.mix_space.preempt.store(true, Ordering::Release);
                    self.lib.downloads.interrupt();
                    drop(admission);
                    self.wait_for_preempted().await?;
                    continue;
                }
                self.lib.downloads.discard_saved_automatic().await?;
                let shelf = self.lib.shelf.read().unwrap().queue.clone();
                shelf.discard_saved_automatic().await?;
                let _gate = self.lib.offline_art_gate.acquire().await;
                self.reclaim(additional).await?;
            }
            if self.cancel.is_cancelled() {
                return Err(cancelled());
            }
            *self.lib.mix_space.reserved.lock().unwrap() += additional;
            *self.reserved.lock().unwrap() += additional;
            return Ok(());
        }
    }

    async fn wait_for_preempted(&self) -> io::Result<()> {
        loop {
            let changed = self.lib.mix_space.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.lib.mix_space.automatic.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
            tokio::select! {
                () = self.cancel.cancelled() => return Err(cancelled()),
                () = changed => {},
            }
        }
    }

    fn required(&self, additional: u64) -> u64 {
        self.lib
            .settings
            .read()
            .unwrap()
            .reserve_bytes
            .saturating_add(*self.lib.mix_space.reserved.lock().unwrap())
            .saturating_add(additional)
    }

    async fn reclaim(&self, additional: u64) -> io::Result<()> {
        if self.cancel.is_cancelled() {
            return Err(cancelled());
        }
        // Unfinished cleanup from a committed prior eviction may be enough.
        // The journal stores the original inode, never a path-only deletion.
        cleanup_recovered(&self.lib, &self.cancel).await?;
        let goal = self.required(additional);
        let free = card_free(&self.lib).await?;
        if free >= goal {
            return Ok(());
        }

        let queue = self.lib.offline_group_queue();
        let id = self.id.clone();
        let (units, references) = self
            .lib
            .db_run(move |_, db| {
                let protected = protected(&queue, &id);
                Ok::<_, rusqlite::Error>((db.mix_audio(&protected)?, db.media_reference_paths()?))
            })
            .await
            .map_err(sql)?;
        let owner = self.lib.clone();
        let plan = super::downloads::lane()
            .run_isolated(move |blocking| plan(blocking, &owner, units, references))
            .await
            .map_err(io::Error::other)??;
        if plan
            .iter()
            .fold(free, |sum, unit| sum.saturating_add(unit.bytes))
            < goal
        {
            return Err(full());
        }
        for unit in plan {
            if self.cancel.is_cancelled() {
                return Err(cancelled());
            }
            let owner = self.lib.clone();
            let id = self.id.clone();
            let cancel = self.cancel.clone();
            let retired = self
                .lib
                .db_run(move |blocking, db| {
                    if cancel.is_cancelled() {
                        return Ok(None);
                    }
                    let queue = owner.offline_group_queue();
                    db.retire_mix_audio(
                        blocking,
                        &unit.members,
                        &unit.files,
                        &unit.art,
                        &protected(&queue, &id),
                    )
                })
                .await
                .map_err(sql)?;
            if let Some(retired) = retired {
                // A successfully committed removal is visible even when an
                // unlink fails. Retain its cleanup record and report only
                // bytes that became available on the actual filesystem.
                self.lib.notify(LibEvent::Downloads);
                cleanup(&self.lib, retired, HashSet::new(), &self.cancel).await?;
                let goal = self.required(additional);
                if card_free(&self.lib).await? >= goal {
                    return Ok(());
                }
            }
        }
        Err(full())
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        *self.lib.mix_space.reserved.lock().unwrap() -= *self.reserved.get_mut().unwrap();
        if self.manual {
            if self.lib.mix_space.manual.fetch_sub(1, Ordering::AcqRel) == 1 {
                // Release card priority without kicking the download
                // coordinator. The same ticket may be waiting to retry a
                // transient network failure after this reservation ends.
                self.lib.mix_space.preempt.store(false, Ordering::Release);
            }
        } else if self.lib.mix_space.automatic.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.lib.mix_space.changed.notify_waiters();
        }
    }
}

fn protected<'a>(queue: &'a [subsonic::Song], id: &'a str) -> std::collections::HashSet<&'a str> {
    queue
        .iter()
        .map(|song| {
            song.spoken
                .as_ref()
                .map_or(song.id.as_str(), |media| media.id.as_str())
        })
        .chain(std::iter::once(id))
        .collect()
}

fn full() -> io::Error {
    io::Error::from_raw_os_error(libc::ENOSPC)
}
fn cancelled() -> io::Error {
    io::ErrorKind::Interrupted.into()
}
fn sql(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

async fn card_free(lib: &Library) -> io::Result<u64> {
    card_space(lib).await.map(|(free, _)| free)
}

async fn card_space(lib: &Library) -> io::Result<(u64, u64)> {
    let root = lib.paths.media_root().to_path_buf();
    #[cfg(test)]
    let model = lib.mix_space.modeled_card.lock().unwrap().clone();
    super::downloads::lane()
        .run_isolated(move |blocking| {
            #[cfg(test)]
            if let Some(model) = model {
                return Ok(model());
            }
            let card = crate::platform::system::filesystem(blocking, &root)?;
            let block = card.f_frsize as u64;
            let free = (card.f_bavail as u64).checked_mul(block).ok_or_else(full)?;
            if block == 0 {
                return Err(full());
            }
            Ok((free, block))
        })
        .await
        .map_err(io::Error::other)?
}

struct Unit {
    members: Vec<MixAudio>,
    files: Vec<RetiredAudio>,
    art: HashMap<PathBuf, RetiredAudio>,
    bytes: u64,
}

fn managed_root<'a>(lib: &Library, path: &'a Path) -> Option<&'a Path> {
    if !lib.paths.can_modify_media(path) {
        return None;
    }
    for root in [
        lib.paths.offline(),
        lib.paths.spoken_downloads(),
        lib.paths.offline_art(),
    ] {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let mut length = 0;
        for component in relative.components() {
            if !matches!(component, Component::Normal(_)) {
                return None;
            }
            length += 1;
        }
        if (1..=2).contains(&length) {
            return path.ancestors().nth(length);
        }
    }
    None
}

fn safe_parent(blocking: &crate::rt::Blocking, lib: &Library, path: &Path) -> bool {
    let Some(root) = managed_root(lib, path) else {
        return false;
    };
    let parent = path.parent().expect("managed file");
    [root, parent].into_iter().all(|directory| {
        crate::shard::metadata(blocking, directory)
            .is_ok_and(|metadata| metadata.is_some_and(|m| m.is_dir()))
    })
}

fn reference_counts(lib: &Library, references: Vec<PathBuf>) -> HashMap<PathBuf, usize> {
    // Count aliases between stored flat and sharded names as shared too.
    let mut counts = HashMap::new();
    for reference in references {
        if managed_root(lib, &reference).is_some() {
            let mut aliases = lib.paths.candidates(&reference);
            aliases.sort_unstable();
            aliases.dedup();
            for alias in aliases {
                *counts.entry(alias).or_default() += 1;
            }
        }
    }
    counts
}

fn identify(
    blocking: &crate::rt::Blocking,
    lib: &Library,
    stored: &Path,
    device: u64,
    counts: &HashMap<PathBuf, usize>,
) -> Option<(RetiredAudio, u64)> {
    managed_root(lib, stored)?;
    let (path, metadata) = lib.paths.locate_metadata(blocking, stored).ok().flatten()?;
    if !safe_parent(blocking, lib, &path)
        || counts.get(&path) != Some(&1)
        || metadata.dev() != device
        || metadata.nlink() != 1
    {
        return None;
    }
    let bytes = metadata.blocks().saturating_mul(512);
    Some((
        RetiredAudio {
            path,
            device: metadata.dev().to_string(),
            inode: metadata.ino().to_string(),
        },
        bytes,
    ))
}

fn plan(
    blocking: &crate::rt::Blocking,
    lib: &Library,
    units: Vec<Vec<MixAudio>>,
    references: Vec<PathBuf>,
) -> io::Result<Vec<Unit>> {
    let device = std::fs::metadata(lib.paths.media_root())?.dev();
    let counts = reference_counts(lib, references);
    let mut result = Vec::new();
    for members in units {
        let mut files = Vec::new();
        let mut bytes = 0u64;
        let mut exclusive = true;
        for stored in members.iter().flat_map(|entry| &entry.paths) {
            let Some((record, allocated)) = identify(blocking, lib, stored, device, &counts) else {
                exclusive = false;
                break;
            };
            bytes = bytes.saturating_add(allocated);
            files.push(record);
        }
        if exclusive && bytes > 0 {
            let art = members
                .iter()
                .filter_map(|member| member.art.as_ref())
                .filter_map(|path| {
                    identify(blocking, lib, path, device, &counts)
                        .map(|(record, _)| (path.clone(), record))
                })
                .collect();
            result.push(Unit {
                members,
                files,
                art,
                bytes,
            });
        }
    }
    Ok(result)
}

pub(super) async fn recover_on_open(lib: &Arc<Library>) {
    if lib.paths.ephemeral_media {
        return;
    }
    let _gate = lib.offline_art_gate.acquire().await;
    if let Err(error) = cleanup_recovered(lib, &Arc::new(Cancel::default())).await {
        log::warn!("Unable to finish Offline Mix cleanup: {error}");
    }
}

async fn cleanup_recovered(lib: &Arc<Library>, cancel: &Arc<Cancel>) -> io::Result<()> {
    let retired = lib
        .db_run(|_, db| db.retired_mix_audio())
        .await
        .map_err(sql)?;
    if retired.is_empty() {
        return Ok(());
    }
    let references = lib
        .db_run(|_, db| db.media_reference_paths())
        .await
        .map_err(sql)?;
    let owned = reference_counts(lib, references).into_keys().collect();
    cleanup(lib, retired, owned, cancel).await
}

async fn cleanup(
    lib: &Arc<Library>,
    retired: Vec<RetiredAudio>,
    owned: HashSet<PathBuf>,
    cancel: &Arc<Cancel>,
) -> io::Result<()> {
    let owner = lib.clone();
    let cancel = cancel.clone();
    let cleared = super::downloads::lane()
        .run_isolated(move |blocking| {
            let mut cleared = Vec::new();
            for file in retired {
                if cancel.is_cancelled() {
                    break;
                }
                if owned.contains(&file.path) {
                    cleared.push(file);
                    continue;
                }
                if !safe_parent(blocking, &owner, &file.path) {
                    continue;
                }
                match crate::shard::metadata(blocking, &file.path) {
                    Ok(Some(metadata))
                        if metadata.is_file()
                            && metadata.dev().to_string() == file.device
                            && metadata.ino().to_string() == file.inode =>
                    {
                        if crate::shard::remove(blocking, &file.path).is_ok() {
                            cleared.push(file);
                        }
                    }
                    // A replaced name is outside this retirement's ownership.
                    Ok(None) if sync_parent(&file.path).is_ok() => cleared.push(file),
                    Ok(Some(_)) => cleared.push(file),
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
            cleared
        })
        .await
        .map_err(io::Error::other)?;
    if !cleared.is_empty() {
        lib.db_run(move |blocking, db| db.clear_retired_mix_audio(blocking, &cleared))
            .await
            .map_err(sql)?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> io::Result<()> {
    std::fs::File::open(path.parent().expect("managed file"))?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, Paths},
        rt,
    };
    use subsonic::Song;

    fn fixture() -> Arc<Library> {
        let root = crate::library::generated_test_directory("mix-space");
        Library::create(Paths::new(root), &Config::default(), Arc::new(|_| {})).0
    }

    fn published_mix(lib: &Library, name: &str) -> (Song, PathBuf, Vec<Unit>) {
        let blocking = &rt::Blocking::dedicated_thread();
        let song = Song {
            id: name.into(),
            ..Default::default()
        };
        let path = lib.paths.offline().join(format!("{name}.wav"));
        std::fs::write(&path, vec![31; 4096]).unwrap();
        let db = lib.db();
        assert_eq!(
            db.auto_music(blocking, None, &[(song.clone(), 4096)], 4096)
                .unwrap(),
            1
        );
        db.put_offline_song(blocking, &song, &path, 4096);
        db.finish_download(blocking, &song.id, None);
        let units = db.mix_audio(&HashSet::new()).unwrap();
        let refs = db.media_reference_paths().unwrap();
        let units = plan(blocking, lib, units, refs).unwrap();
        (song, path, units)
    }

    #[test]
    fn impossible_manual_admission_keeps_mix_index_and_real_file() {
        let lib = fixture();
        let (song, path, units) = published_mix(&lib, "generated-victim");
        assert_eq!(units.len(), 1);
        let storage = Ticket::new(
            lib.clone(),
            Arc::new(Cancel::default()),
            true,
            "generated-too-large".into(),
        );
        let failure = rt::Ui::new()
            .unwrap()
            .block_on(storage.ensure(u64::MAX, 1))
            .unwrap_err();
        assert_eq!(failure.raw_os_error(), Some(libc::ENOSPC));
        let blocking = &rt::Blocking::dedicated_thread();
        assert_eq!(
            lib.db().offline_song(blocking, &song.id),
            Some(path.clone())
        );
        assert!(path.is_file());
        assert!(lib.db().retired_mix_audio().unwrap().is_empty());
    }

    #[test]
    fn recovery_keeps_republished_inode_with_a_current_alias_owner() {
        let lib = fixture();
        let blocking = &rt::Blocking::dedicated_thread();
        let (_, path, mut units) = published_mix(&lib, "generated-retired");
        let unit = units.pop().unwrap();
        assert!(
            lib.db()
                .retire_mix_audio(
                    blocking,
                    &unit.members,
                    &unit.files,
                    &unit.art,
                    &HashSet::new()
                )
                .unwrap()
                .is_some()
        );
        let manual = Song {
            id: "generated-new-owner".into(),
            ..Default::default()
        };
        let alias = lib
            .paths
            .sharded(crate::shard::Store::Offline, path.file_name().unwrap());
        lib.db().put_offline_song(blocking, &manual, &alias, 4096);
        rt::Ui::new().unwrap().block_on(async {
            let _gate = lib.offline_art_gate.acquire().await;
            cleanup_recovered(&lib, &Arc::new(Cancel::default()))
                .await
                .unwrap();
        });
        assert_eq!(std::fs::read(&path).unwrap(), vec![31; 4096]);
        assert_eq!(lib.db().offline_song(blocking, &manual.id), Some(alias));
        assert!(lib.db().retired_mix_audio().unwrap().is_empty());
    }

    #[test]
    fn managed_planner_rejects_nested_links_and_shared_hardlinks() {
        let lib = fixture();
        let blocking = &rt::Blocking::dedicated_thread();
        let (_, path, _) = published_mix(&lib, "generated-shared");
        std::fs::hard_link(&path, lib.paths.media_root().join("generated-second-owner")).unwrap();
        let db = lib.db();
        let units = db.mix_audio(&HashSet::new()).unwrap();
        assert!(
            plan(blocking, &lib, units, db.media_reference_paths().unwrap())
                .unwrap()
                .is_empty()
        );
        let outside = lib.paths.media_root().join("generated-outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, lib.paths.offline().join("symlinked")).unwrap();
        assert!(!safe_parent(
            blocking,
            &lib,
            &lib.paths.offline().join("symlinked/media.wav")
        ));
        assert!(
            managed_root(
                &lib,
                &lib.paths.offline().join("../generated").join("media.wav")
            )
            .is_none()
        );
    }
}
