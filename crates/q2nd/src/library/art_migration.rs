//! Incremental import and coalescing of artwork from the current card cache.
use super::{Library, art_pack::CoalesceProgress, layout::Pacing};
use crate::gfx::Image;
use crate::rt::Blocking;
use crate::shard::{self, Progress, Store};
use std::ffi::OsStr;
use std::fs::{self, DirEntry, ReadDir};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Weak;

const MAX_PNG_BYTES: usize = 5 * 1024 * 1024;
const MAX_BATCH_ROWS: usize = 8;
const SCAN_ENTRIES: usize = 64;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Keep directory offsets between jobs. No list proportional to the cache
/// and no fresh walk over the already inspected files after each batch.
struct Scan {
    root: ReadDir,
    shard: Option<(PathBuf, ReadDir)>,
    pack_parents_synced: bool,
}

impl Scan {
    fn next(&mut self, _blocking: &Blocking) -> io::Result<Step> {
        if let Some((dir, entries)) = self.shard.as_mut() {
            return match entries.next().transpose()? {
                Some(entry) => Ok(Step::Entry(entry, Some(dir.clone()))),
                None => Ok(Step::EmptyShardCandidate(self.shard.take().unwrap().0)),
            };
        }
        let Some(entry) = self.root.next().transpose()? else {
            return Ok(Step::End);
        };
        let name = entry.file_name();
        if shard::is_shard(&name) && entry.file_type()?.is_dir() {
            let path = entry.path();
            match fs::read_dir(&path) {
                Ok(entries) => self.shard = Some((path, entries)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            return Ok(Step::Other);
        }
        Ok(Step::Entry(entry, None))
    }
}

enum Step {
    Entry(DirEntry, Option<PathBuf>),
    EmptyShardCandidate(PathBuf),
    Other,
    End,
}

enum Phase {
    Loose(Option<Scan>),
    Packed,
}

pub(super) async fn run(weak: &Weak<Library>, pacing: Pacing) -> Option<Progress> {
    let paths = weak.upgrade()?.paths.clone();
    let root = paths.art_cache();
    if paths.ephemeral_media || !paths.can_modify_media(&root) {
        return Some(Progress::default());
    }

    let mut total = Progress::default();
    let mut phase = Phase::Loose(None);
    loop {
        tokio::time::sleep(pacing.pause).await;
        loop {
            let library = weak.upgrade()?;
            let busy = library.playback_transfer_running() || library.background_should_yield();
            drop(library);
            if !busy {
                break;
            }
            tokio::time::sleep(pacing.busy).await;
        }
        let owner = weak.clone();
        let result = crate::rt::jobs()
            .run_isolated(move |blocking| {
                let library = owner.upgrade()?;
                // Check again after waiting for a job slot.
                if library.playback_transfer_running() || library.background_should_yield() {
                    return Some((Some(phase), Progress::default()));
                }
                let mut progress = Progress::default();
                let next = match phase {
                    Phase::Loose(scan) => run_batch(
                        blocking,
                        &library,
                        scan,
                        pacing.batch.clamp(1, MAX_BATCH_ROWS),
                        &mut progress,
                    ).map(|scan| Some(scan.map_or(Phase::Packed, |s| Phase::Loose(Some(s))))),
                    Phase::Packed => {
                        let reserve = library.settings.read().unwrap().reserve_bytes;
                        let free = super::free_space(blocking, library.paths.media_root()).unwrap_or(0);
                        library.art.pack.coalesce_batch(blocking, free.saturating_sub(reserve))
                            .map(|result| match result {
                                CoalesceProgress::More => Some(Phase::Packed),
                                CoalesceProgress::Complete => None,
                                CoalesceProgress::LowSpace => {
                                    log::info!("deferring artwork coalescing at the card reserve");
                                    None
                                }
                            })
                            .map_err(|error| log::warn!("artwork coalescing: {error}"))
                    }
                };
                if next.is_err() {
                    progress.failed += 1;
                    progress.stopped += 1;
                }
                Some((next.ok().flatten(), progress))
            })
            .await
            .ok()??;
        total += result.1;
        match result.0 {
            Some(next) => phase = next,
            None => return Some(total),
        }
    }
}

struct Candidate {
    key: String,
    path: PathBuf,
    file: fs::File,
    bytes: Vec<u8>,
}

fn run_batch(
    blocking: &Blocking,
    library: &Library,
    scan: Option<Scan>,
    limit: usize,
    progress: &mut Progress,
) -> Result<Option<Scan>, ()> {
    let paths = &library.paths;
    // Serialize work on the loose cache directory with layout operations.
    // Packed writes serialize separately through the database connection.
    let _guard = paths.lock_store(blocking, Store::Art);
    let root = paths.art_cache();
    let mut scan = match scan {
        Some(scan) => scan,
        None => match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.is_dir() => Scan {
                root: fs::read_dir(&root).map_err(|_| ())?,
                shard: None,
                pack_parents_synced: false,
            },
            Ok(_) => return Err(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(()),
        },
    };
    let mut rows = Vec::new();
    let mut payload = 0;
    let mut empty = Vec::new();
    let mut done = false;
    for _ in 0..SCAN_ENTRIES {
        // Bound both transfer count and memory. One additional row may
        // cross the size threshold, within the pack's transaction limit.
        if rows.len() >= limit || payload >= MAX_PNG_BYTES || empty.len() >= limit {
            break;
        }
        match scan.next(blocking).map_err(|_| ())? {
            Step::End => {
                done = true;
                break;
            }
            Step::Other => {}
            Step::EmptyShardCandidate(path) => empty.push(path),
            Step::Entry(entry, shard) => match candidate(blocking, entry, shard.as_deref()) {
                Ok(Some(row)) => {
                    payload += row.bytes.len();
                    rows.push(row);
                }
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => progress.gone += 1,
                Err(error) if shard::fatal(&error) => return Err(()),
                Err(_) => progress.failed += 1,
            },
        }
    }
    let pack = &library.art.pack;
    let reserve = library.settings.read().unwrap().reserve_bytes;
    let mut free = None;
    let mut admitted = Vec::new();
    for row in rows {
        match loose_conflict(blocking, paths, &row) {
            Ok(true) => {
                progress.conflicts += 1;
                continue;
            }
            Ok(false) => {}
            Err(error) if shard::fatal(&error) => return Err(()),
            Err(_) => {
                progress.failed += 1;
                continue;
            }
        }
        match pack.get(blocking, &row.key).map_err(|_| ())? {
            Some(bytes) if bytes != row.bytes => progress.conflicts += 1,
            Some(_) => admitted.push(row),
            // Room for pages, transaction scratch and exFAT allocation
            // rounding. Already imported rows can be unlinked at low space.
            None => {
                let free = free.get_or_insert_with(|| {
                    super::free_space(blocking, paths.media_root()).unwrap_or(0)
                });
                let needed = 2 * row.bytes.len() as u64;
                if *free >= reserve.saturating_add(1024 * 1024 + needed) {
                    *free -= needed;
                    admitted.push(row);
                } else {
                    progress.failed += 1;
                }
            }
        }
    }
    if !admitted.is_empty() {
        let rows: Vec<_> = admitted
            .iter()
            .map(|row| (row.key.as_str(), row.bytes.as_slice()))
            .collect();
        pack.import(blocking, &rows).map_err(|_| ())?;
        if !scan.pack_parents_synced {
            // An earlier process may have stopped just after publishing the
            // pack. Make its directory ancestry durable even on resume.
            super::offline_art::sync_parents(blocking, &root).map_err(|_| ())?;
            scan.pack_parents_synced = true;
        }
        for row in admitted {
            // An INSERT OR IGNORE may have encountered different bytes;
            // only remove a source after its exact payload can be read back.
            if pack.get(blocking, &row.key).map_err(|_| ())?.as_deref()
                != Some(row.bytes.as_slice())
            {
                progress.conflicts += 1;
                continue;
            }
            match unlink(blocking, &row) {
                Ok(true) => progress.moved += 1,
                Ok(false) => progress.gone += 1,
                Err(error) if shard::fatal(&error) => return Err(()),
                Err(_) => progress.failed += 1,
            }
        }
    }
    for path in empty {
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                ) => {}
            Err(error) if shard::fatal(&error) => return Err(()),
            Err(_) => progress.failed += 1,
        }
    }
    Ok((!done).then_some(scan))
}

fn key(name: &OsStr) -> Option<&str> {
    let key = name.to_str()?.strip_suffix(".png")?;
    (key.len() == 32
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(key)
}

fn candidate(
    _blocking: &Blocking,
    entry: DirEntry,
    directory: Option<&Path>,
) -> io::Result<Option<Candidate>> {
    let name = entry.file_name();
    let Some(key) = key(&name) else {
        return Ok(None);
    };
    if directory.is_some_and(|dir| dir.file_name() != Some(shard::shard(&name).as_ref()))
        || !entry.file_type()?.is_file()
    {
        return Ok(None);
    }
    let path = entry.path();
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_PNG_BYTES as u64 {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&file)
        .take((MAX_PNG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PNG_BYTES
        || !bytes.starts_with(PNG_SIGNATURE)
        || Image::decode_square(&bytes, 1).is_none()
    {
        return Ok(None);
    }
    Ok(Some(Candidate {
        key: key.to_owned(),
        path,
        file,
        bytes,
    }))
}

/// Older releases can leave both placements. Preserve both if their
/// contents differ instead of arbitrarily making the first scanned visible.
fn loose_conflict(
    _blocking: &Blocking,
    paths: &crate::config::Paths,
    row: &Candidate,
) -> io::Result<bool> {
    let name = row.path.file_name().expect("cached artwork file name");
    let root = paths.art_cache();
    let flat = root.join(name);
    let other = if row.path == flat {
        paths.sharded(Store::Art, name)
    } else {
        flat
    };
    // The sharded placement belongs to this store only when its parent
    // is a real shard directory.
    if let Some(parent) = other.parent().filter(|parent| *parent != root.as_path()) {
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&other)
    {
        Ok(file) => file,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(false);
    }
    if metadata.len() != row.bytes.len() as u64 {
        return Ok(true);
    }
    let mut bytes = Vec::with_capacity(row.bytes.len());
    file.take((row.bytes.len() + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes != row.bytes)
}

fn unlink(_blocking: &Blocking, row: &Candidate) -> io::Result<bool> {
    let opened = row.file.metadata()?;
    let named = match fs::symlink_metadata(&row.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !named.is_file() || (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
        return Ok(false);
    }
    fs::remove_file(&row.path)?;
    Ok(true)
}

impl Library {
    /// The perf scene admits the exact generated loose tree before calling.
    pub(crate) async fn migrate_generated_artwork(self: &std::sync::Arc<Self>) -> Option<Progress> {
        let pacing = Pacing {
            start: std::time::Duration::ZERO,
            pause: std::time::Duration::from_millis(2),
            ..Pacing::DEVICE
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(180),
            run(&std::sync::Arc::downgrade(self), pacing),
        )
        .await
        .ok()
        .flatten()
    }
}
