#![forbid(unsafe_code)]
//! Per-item media stores keep their files in one level of 256 subdirectories,
//! named by two lowercase hex characters from an MD5 of the file name. One
//! flat exFAT directory of 6,102 cover files made 40% of the player's samples
//! directory scans while art was normalized.
//!
//! Files written before the change stay readable where they are: every lookup
//! tries the sharded path, the flat path, then the sharded path again, so a
//! background move that lands between the first two checks is still found.
//! Moves only go from flat to sharded, by one rename each, so an interrupted
//! migration leaves every file at one of the two places.
//!
//! Every creation or rename into a store's directories holds that store's
//! directory lock (`Paths::lock_store`), so the migration's moves and the
//! writers' creates never interleave. The first read-only or I/O error stops
//! the migration.
use md5::{Digest, Md5};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Store { Art, Songs, Offline, OfflineArt, Spoken }

impl Store {
    pub(crate) const ALL: [Store; 5] = [Store::Art, Store::Songs, Store::Offline, Store::OfflineArt, Store::Spoken];
    /// The store's directory under the media root.
    pub(crate) fn relative(self) -> &'static str {
        match self {
            Store::Art => "cache/art",
            Store::Songs => "cache/songs",
            Store::Offline => "offline",
            Store::OfflineArt => "offline-art",
            Store::Spoken => "spoken-downloads",
        }
    }
    fn index(self) -> usize { self as usize }
    /// Disposable caches: a flat file whose sharded name already exists is a
    /// stale copy, and leftover temporary files are removed.
    fn disposable(self) -> bool { matches!(self, Store::Art | Store::Songs) }
}

/// Which stores have no flat files left, and each store's directory lock.
/// Shared by every clone of the paths.
#[derive(Debug, Default)]
pub(crate) struct Layout {
    complete: [AtomicBool; 5],
    directories: [Mutex<()>; 5],
    #[cfg(test)] faults: Mutex<Faults>,
}

impl Layout {
    pub(crate) fn complete(&self, store: Store) -> bool { self.complete[store.index()].load(Ordering::Relaxed) }
    fn set_complete(&self, store: Store) { self.complete[store.index()].store(true, Ordering::Relaxed); }
}

/// Injected migration failures, for tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Faults {
    /// Renames that succeed before every later one fails with `EROFS`.
    pub read_only_after: Option<u64>,
    /// File names whose rename always fails with this error number.
    pub names: Vec<(OsString, i32)>,
    /// Rename attempts so far.
    pub attempts: u64,
}

#[cfg(test)]
impl crate::config::Paths {
    pub(crate) fn faults(&self) -> MutexGuard<'_, Faults> { self.layout().faults.lock().unwrap() }
}

/// Held while entries are created in or renamed into a store's directories.
pub(crate) struct StoreGuard<'a> { store: Store, _held: MutexGuard<'a, ()> }

/// A read-only filesystem or an I/O error: the rest of a migration would fail
/// the same way.
pub(crate) fn fatal(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EROFS | libc::EIO))
}

/// The marker a store's directory holds once its migration finished.
pub(crate) const MARKER: &str = ".sharded";

/// Two lowercase hex characters from the MD5 of the file name.
pub(crate) fn shard(name: &OsStr) -> String {
    format!("{:02x}", Md5::digest(name.as_bytes())[0])
}

pub(crate) fn is_shard(name: &OsStr) -> bool {
    name.len() == 2 && name.as_bytes().iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

fn temporary(name: &OsStr) -> bool {
    let name = name.as_bytes();
    name.ends_with(b".tmp") || name.ends_with(b".part") || name.windows(6).any(|w| w == b".sync-")
}

impl crate::config::Paths {
    pub(crate) fn store_root(&self, store: Store) -> PathBuf { self.media_root().join(store.relative()) }

    /// The store, its directory and the file name of a path in either layout.
    fn stored<'a>(&self, path: &'a Path) -> Option<(Store, PathBuf, &'a OsStr)> {
        let name = path.file_name()?;
        let parent = path.parent()?;
        Store::ALL.into_iter().find_map(|store| {
            let root = self.store_root(store);
            if parent == root { return Some((store, root, name)); }
            (parent.parent() == Some(root.as_path()) && parent.file_name() == Some(shard(name).as_ref()))
                .then_some((store, root, name))
        })
    }

    /// Both layouts, including a flat duplicate left after the store was marked complete.
    pub(crate) fn song_cache_copies(&self, path: &Path) -> Option<[PathBuf; 2]> {
        let (Store::Songs, root, name) = self.stored(path)? else { return None; };
        Some([root.join(name), root.join(shard(name)).join(name)])
    }

    /// Where a file of `store` named `name` belongs.
    pub(crate) fn sharded(&self, store: Store, name: &OsStr) -> PathBuf {
        self.store_root(store).join(shard(name)).join(name)
    }

    /// Lookup order for a stored path. Paths outside the stores (legacy
    /// media, partial downloads) are used as they are.
    pub(crate) fn candidates(&self, path: &Path) -> Vec<PathBuf> {
        let Some((store, root, name)) = self.stored(path) else { return vec![path.to_path_buf()]; };
        let sharded = root.join(shard(name)).join(name);
        let flat = root.join(name);
        // A finished store has no flat files except ones an index still names.
        if self.layout().complete(store) && path != flat { return vec![sharded]; }
        vec![sharded.clone(), flat, sharded]
    }

    /// The regular file a stored path refers to now, in either layout.
    pub(crate) fn locate(&self, blocking: &crate::rt::Blocking, path: &Path) -> Option<PathBuf> {
        self.locate_metadata(blocking, path).ok().flatten().map(|(path, _)| path)
    }

    /// As `locate`, with the file's metadata. A failure other than a missing
    /// file is an error.
    pub(crate) fn locate_metadata(&self, blocking: &crate::rt::Blocking, path: &Path)
        -> std::io::Result<Option<(PathBuf, std::fs::Metadata)>> {
        for candidate in self.candidates(path) {
            if let Some(metadata) = metadata(blocking, &candidate)?.filter(std::fs::Metadata::is_file) {
                return Ok(Some((candidate, metadata)));
            }
        }
        Ok(None)
    }

    /// Opens a stored regular file for reading, in either layout. Never
    /// follows a final symbolic link or waits on a special file.
    pub(crate) fn open_media(&self, _blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut last = std::io::Error::from(std::io::ErrorKind::NotFound);
        for candidate in self.candidates(path) {
            match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&candidate) {
                Ok(file) if file.metadata()?.is_file() => return Ok(file),
                Ok(_) => return Err(std::io::ErrorKind::InvalidInput.into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    /// Removes a stored file in every layout it may be in. A missing file is
    /// not an error.
    pub(crate) fn remove_media(&self, blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<()> {
        let mut result = Ok(());
        for candidate in self.candidates(path) {
            if let Err(error) = remove(blocking, &candidate) { result = Err(error); }
        }
        result
    }

    /// The store's directory lock. Hold it from creating a shard directory
    /// until the new file's final name exists; never across an `.await`.
    pub(crate) fn lock_store(&self, _blocking: &crate::rt::Blocking, store: Store) -> StoreGuard<'_> {
        StoreGuard { store, _held: self.layout().directories[store.index()].lock().unwrap_or_else(PoisonError::into_inner) }
    }

    /// Creates the shard directory for a new file and returns the file's path.
    pub(crate) fn prepare_sharded(&self, blocking: &crate::rt::Blocking, guard: &StoreGuard<'_>, name: &OsStr) -> std::io::Result<PathBuf> {
        let path = self.sharded(guard.store, name);
        create_directory(blocking, path.parent().expect("shard directory"))?;
        Ok(path)
    }

    /// Syncs a new file's shard directory and the store directory above it,
    /// which holds a shard directory that may be new.
    pub(crate) fn sync_sharded(&self, blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<()> {
        let shard = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
        sync_directory(blocking, shard)?;
        sync_directory(blocking, shard.parent().ok_or(std::io::ErrorKind::InvalidInput)?)
    }

    /// Whether a store finished migrating, from its marker; remembered.
    pub(crate) fn store_complete(&self, blocking: &crate::rt::Blocking, store: Store) -> bool {
        if self.layout().complete(store) { return true; }
        let marked = metadata(blocking, &self.store_root(store).join(MARKER)).is_ok_and(|found| found.is_some());
        if marked { self.layout().set_complete(store); }
        marked
    }

    /// Records that a store has no flat files left. The marker's temporary
    /// file and rename go through the store's directory lock.
    pub(crate) fn mark_complete(&self, blocking: &crate::rt::Blocking, store: Store) -> std::io::Result<()> {
        let _guard = self.lock_store(blocking, store);
        crate::config::write_atomic(blocking, &self.store_root(store).join(MARKER), b"sharded v1\n")?;
        self.layout().set_complete(store);
        Ok(())
    }
}

/// Counts for the log; no file names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Progress {
    pub moved: u64,
    /// Already moved or removed, by this run or an interrupted one.
    pub gone: u64,
    /// Stale flat copies of a cache file already present in its shard.
    pub duplicates: u64,
    /// Leftover temporary files removed from a cache.
    pub temporary: u64,
    /// Flat files left in place: another file holds the sharded name.
    pub conflicts: u64,
    pub failed: u64,
    /// Batches ended by a read-only or I/O error.
    pub stopped: u64,
}

impl std::ops::AddAssign for Progress {
    fn add_assign(&mut self, other: Progress) {
        self.moved += other.moved; self.gone += other.gone; self.duplicates += other.duplicates;
        self.temporary += other.temporary; self.conflicts += other.conflicts; self.failed += other.failed;
        self.stopped += other.stopped;
    }
}

/// One pass over a store's directory: at most `limit` flat file names. The
/// marker, shard directories and anything that is not a regular file stay
/// out. `None`: the store's directory does not exist.
pub(crate) fn flat_names(blocking: &crate::rt::Blocking, root: &Path, limit: usize) -> std::io::Result<Option<Vec<OsString>>> {
    let Some(entries) = entries(blocking, root)? else { return Ok(None) };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name == MARKER || !entry.file_type().is_ok_and(|kind| kind.is_file()) { continue; }
        names.push(name);
        if names.len() == limit { break; }
    }
    Ok(Some(names))
}

/// Moves flat files into their shards, one rename each. A name already gone
/// was moved or removed earlier and is skipped, so a repeated or interrupted
/// batch is harmless. Never copies.
pub(crate) fn migrate(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, store: Store, names: &[OsString]) -> Progress {
    migrate_batch(blocking, paths, store, names, &mut Vec::new())
}

/// As `migrate`, adding the names that failed to `failed`. Stops at the
/// first read-only or I/O error and counts it in `stopped`.
pub(crate) fn migrate_batch(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, store: Store, names: &[OsString],
                            failed: &mut Vec<OsString>) -> Progress {
    let root = paths.store_root(store);
    let mut progress = Progress::default();
    for name in names {
        // One file at a time under the store's lock: a writer's create and
        // rename never interleave with a move into the same directory.
        let guard = paths.lock_store(blocking, store);
        let result = migrate_one(blocking, paths, store, &root, name, &mut progress);
        drop(guard);
        if let Err(error) = result {
            progress.failed += 1;
            failed.push(name.clone());
            if fatal(&error) { progress.stopped += 1; break; }
        }
    }
    progress
}

/// The shard directories `names` belong in, sorted, each once.
pub(crate) fn shards_of(names: &[OsString]) -> Vec<String> {
    let mut shards: Vec<String> = names.iter().map(|name| shard(name)).collect();
    shards.sort_unstable();
    shards.dedup();
    shards
}

/// Creates shard directories before the moves of a pass, so the store's
/// directory gains its new entries before any move and no move lands in a
/// directory created a moment before. Returns a read-only or I/O error;
/// other failures are left to each file's move.
pub(crate) fn prepare_shards(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, store: Store, shards: &[String]) -> std::io::Result<()> {
    let root = paths.store_root(store);
    let mut created = false;
    for name in shards {
        let directory = root.join(name);
        match metadata(blocking, &directory) {
            Ok(Some(found)) if found.is_dir() => continue,
            Err(error) if fatal(&error) => return Err(error),
            _ => {}
        }
        let _guard = paths.lock_store(blocking, store);
        match create_directory(blocking, &directory) {
            Ok(()) => created = true,
            Err(error) if fatal(&error) => return Err(error),
            Err(_) => {}
        }
    }
    match if created { sync_directory(blocking, &root) } else { Ok(()) } {
        Err(error) if fatal(&error) => Err(error),
        _ => Ok(()),
    }
}

/// One flat file. An error is a failure for the caller to count.
fn migrate_one(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, store: Store, root: &Path, name: &OsStr,
               progress: &mut Progress) -> std::io::Result<()> {
    let flat = root.join(name);
    match metadata(blocking, &flat)? {
        Some(found) if found.is_file() => {}
        _ => { progress.gone += 1; return Ok(()); }
    }
    if temporary(name) && store.disposable() {
        if remove(blocking, &flat)? { progress.temporary += 1 } else { progress.gone += 1 }
        return Ok(());
    }
    let target = paths.sharded(store, name);
    match metadata(blocking, &target)? {
        Some(_) if store.disposable() => {
            if remove(blocking, &flat)? { progress.duplicates += 1 } else { progress.gone += 1 }
        }
        Some(_) => progress.conflicts += 1,
        None => {
            match create_directory(blocking, target.parent().expect("shard directory")).and_then(|()| move_file(blocking, paths, &flat, &target)) {
                Ok(()) => progress.moved += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => progress.gone += 1,
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

/// The migration's rename; tests inject failures here.
fn move_file(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    {
        let mut faults = paths.faults();
        faults.attempts += 1;
        let named = from.file_name().and_then(|name| faults.names.iter().find(|(n, _)| n == name)).map(|(_, errno)| *errno);
        if let Some(errno) = named { return Err(std::io::Error::from_raw_os_error(errno)); }
        if let Some(left) = faults.read_only_after.as_mut() {
            if *left == 0 { return Err(std::io::Error::from_raw_os_error(libc::EROFS)); }
            *left -= 1;
        }
    }
    #[cfg(not(test))]
    let _ = paths;
    rename(blocking, from, to)
}

/// A directory's entries, for the Storage drilldown. For a store (`shards`),
/// its files in both layouts: flat files, then each shard's. At most `limit`
/// entries are scanned; `true` when the listing stopped there. A missing
/// directory is empty.
pub(crate) fn walk(blocking: &crate::rt::Blocking, root: &Path, shards: bool, limit: usize) -> std::io::Result<(Vec<(OsString, PathBuf, std::fs::Metadata)>, bool)> {
    let mut found = Vec::new();
    let mut scanned = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Some(entries) = entries(blocking, &directory)? else { continue };
        for entry in entries {
            if scanned == limit { return Ok((found, true)); }
            scanned += 1;
            let entry = entry?;
            let name = entry.file_name();
            let Some(metadata) = metadata(blocking, &entry.path())? else { continue };
            if shards && directory == root {
                if is_shard(&name) && metadata.is_dir() { pending.push(entry.path()); continue; }
                if name == MARKER { continue; }
            }
            found.push((name, entry.path(), metadata));
        }
    }
    Ok((found, false))
}

// The filesystem calls the stores share. A missing path is `None`/`false`.

pub(crate) fn metadata(_blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Removes one path; whether it was there.
pub(crate) fn remove(_blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn create_directory(_blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

pub(crate) fn rename(_blocking: &crate::rt::Blocking, from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

fn entries(_blocking: &crate::rt::Blocking, directory: &Path) -> std::io::Result<Option<std::fs::ReadDir>> {
    match std::fs::read_dir(directory) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Removes `path` only while it still names the file `file` has open, so a
/// failed writer never unlinks a replacement.
pub(crate) fn remove_if_same(blocking: &crate::rt::Blocking, path: &Path, file: &std::fs::File) {
    use std::os::unix::fs::MetadataExt;
    if let (Ok(open), Ok(Some(named))) = (file.metadata(), metadata(blocking, path)) {
        if named.file_type().is_file() && (open.dev(), open.ino()) == (named.dev(), named.ino()) {
            let _ = remove(blocking, path);
        }
    }
}

/// Makes a directory's new or renamed entries durable.
pub(crate) fn sync_directory(_blocking: &crate::rt::Blocking, directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory).and_then(|directory| directory.sync_all())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn blocking() -> crate::rt::Blocking { crate::rt::Blocking::dedicated_thread() }
    fn fixture(label: &str) -> Paths {
        let paths = Paths::new(crate::library::generated_test_directory(label));
        paths.create_all(&blocking()).unwrap();
        for store in Store::ALL { std::fs::create_dir_all(paths.store_root(store)).unwrap(); }
        paths
    }
    /// Generated files with distinct contents, flat as an earlier version left them.
    fn flat_files(paths: &Paths, store: Store, count: usize) -> Vec<OsString> {
        (0..count).map(|i| {
            let name = OsString::from(format!("{:x}-{i:04}.bin", Md5::digest(format!("generated-{i}"))));
            std::fs::write(paths.store_root(store).join(&name), format!("generated contents {i}")).unwrap();
            name
        }).collect()
    }
    fn migrate_all(paths: &Paths, store: Store, batch: usize) -> Progress {
        let mut total = Progress::default();
        while let Some(names) = flat_names(&blocking(), &paths.store_root(store), 64).unwrap().filter(|names| !names.is_empty()) {
            for chunk in names.chunks(batch) { total += migrate(&blocking(), paths, store, chunk); }
            if total.conflicts > 0 { break; }
        }
        total
    }

    #[test]
    fn shards_are_two_lowercase_hex_characters_and_spread_a_large_cache() {
        let mut counts = std::collections::HashMap::new();
        for i in 0..6102 {
            let name = format!("{:x}.png", Md5::digest(format!("generated cover {i}")));
            let shard = shard(name.as_ref());
            assert!(is_shard(shard.as_ref()), "{shard}");
            *counts.entry(shard).or_insert(0) += 1;
        }
        assert_eq!(counts.len(), 256);
        assert!(counts.values().all(|&n| n < 60), "largest shard {}", counts.values().max().unwrap());
        assert!(!is_shard("AB".as_ref()) && !is_shard("partial".as_ref()) && !is_shard("0g".as_ref()));
    }

    #[test]
    fn lookups_try_the_shard_then_the_flat_file_then_the_shard_again() {
        let paths = fixture("shard-lookup");
        let name = OsStr::new("generated.flac");
        let flat = paths.offline().join(name);
        let sharded = paths.sharded(Store::Offline, name);
        assert_eq!(sharded.parent().unwrap().parent().unwrap(), paths.offline());
        assert_eq!(paths.candidates(&flat), vec![sharded.clone(), flat.clone(), sharded.clone()]);
        assert_eq!(paths.candidates(&sharded), vec![sharded.clone(), flat.clone(), sharded.clone()]);
        // Outside the stores: legacy media and partial fetches are used as given.
        let partial = paths.stream_partials().join("generated.raw.mp3.0000abcd.part");
        assert_eq!(paths.candidates(&partial), vec![partial.clone()]);
        let legacy = std::path::PathBuf::from("/mnt/mmc/Q2Navidrome/offline").join(name);
        assert_eq!(paths.candidates(&legacy), vec![legacy.clone()]);
        // A shard directory that does not match the name is not this layout.
        let wrong = paths.offline().join("zz").join(name);
        assert_eq!(paths.candidates(&wrong), vec![wrong.clone()]);
        assert!(paths.locate(&blocking(), &flat).is_none());
        std::fs::write(&flat, b"generated audio").unwrap();
        assert_eq!(paths.locate(&blocking(), &sharded), Some(flat.clone()), "flat fallback");
        assert_eq!(migrate(&blocking(), &paths, Store::Offline, &[name.into()]).moved, 1);
        assert_eq!(paths.locate(&blocking(), &flat), Some(sharded.clone()), "an index naming the flat path finds the shard");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut paths.open_media(&blocking(), &flat).unwrap(), &mut text).unwrap();
        assert_eq!(text, "generated audio");
        // A finished store skips the flat check for sharded paths only.
        paths.mark_complete(&blocking(), Store::Offline).unwrap();
        assert_eq!(paths.candidates(&sharded), vec![sharded.clone()]);
        assert_eq!(paths.candidates(&flat), vec![sharded.clone(), flat.clone(), sharded.clone()]);
        let restarted = Paths::new(paths.root.clone());
        assert!(!restarted.layout().complete(Store::Offline));
        assert!(restarted.store_complete(&blocking(), Store::Offline), "the marker survives a restart");
        assert!(!restarted.store_complete(&blocking(), Store::Art));
        paths.remove_media(&blocking(), &flat).unwrap();
        assert!(!sharded.exists() && !flat.exists());
        paths.remove_media(&blocking(), &flat).unwrap();
        // A directory at the path is refused, without blocking.
        std::fs::create_dir_all(&sharded).unwrap();
        assert!(paths.open_media(&blocking(), &sharded).is_err());
    }

    #[test]
    fn an_interrupted_migration_resumes_and_loses_nothing() {
        let paths = fixture("shard-interrupted");
        let names = flat_files(&paths, Store::Offline, 50);
        let listed = flat_names(&blocking(), &paths.offline(), 1000).unwrap().unwrap();
        assert_eq!(listed.len(), 50);
        // Stopped part way through a pass, as by power loss between renames.
        let first = migrate(&blocking(), &paths, Store::Offline, &listed[..20]);
        assert_eq!((first.moved, first.failed), (20, 0));
        let resumed = Paths::new(paths.root.clone());
        let remaining = flat_names(&blocking(), &resumed.offline(), 1000).unwrap().unwrap();
        assert_eq!(remaining.len(), 30);
        // Every file is found in between, whichever path an index holds.
        for name in &names { assert!(resumed.locate(&blocking(), &resumed.offline().join(name)).is_some()); }
        assert_eq!(migrate_all(&resumed, Store::Offline, 8).moved, 30);
        // Repeating the whole first list is harmless.
        let repeated = migrate(&blocking(), &resumed, Store::Offline, &listed);
        assert_eq!((repeated.moved, repeated.gone), (0, 50));
        for (i, name) in names.iter().enumerate() {
            let sharded = resumed.sharded(Store::Offline, name);
            assert_eq!(std::fs::read_to_string(&sharded).unwrap(), format!("generated contents {i}"));
            assert!(!resumed.offline().join(name).exists());
        }
        assert!(flat_names(&blocking(), &resumed.offline(), 1000).unwrap().unwrap().is_empty());
    }

    #[test]
    fn caches_drop_stale_copies_and_temporary_files_but_downloads_keep_both() {
        let paths = fixture("shard-conflicts");
        for store in [Store::Art, Store::Offline] {
            let name = OsString::from("generated-both.png");
            let sharded = paths.prepare_sharded(&blocking(), &paths.lock_store(&blocking(), store), &name).unwrap();
            std::fs::write(&sharded, b"generated current").unwrap();
            std::fs::write(paths.store_root(store).join(&name), b"generated older").unwrap();
            std::fs::write(paths.store_root(store).join("generated.png.tmp"), b"generated leftover").unwrap();
            std::fs::write(paths.store_root(store).join(MARKER), b"not yet").unwrap();
            let progress = migrate_all(&paths, store, 8);
            assert_eq!(std::fs::read(&sharded).unwrap(), b"generated current");
            if store == Store::Art {
                assert_eq!((progress.duplicates, progress.temporary, progress.conflicts), (1, 1, 0));
                assert!(!paths.store_root(store).join(&name).exists());
            } else {
                // A download is never deleted because a name repeats.
                assert_eq!((progress.moved, progress.conflicts), (1, 1));
                assert_eq!(std::fs::read(paths.store_root(store).join(&name)).unwrap(), b"generated older");
            }
        }
    }

    #[test]
    fn the_drilldown_lists_both_layouts_without_shards_or_marker() {
        let paths = fixture("shard-walk");
        let names = flat_files(&paths, Store::Songs, 6);
        migrate(&blocking(), &paths, Store::Songs, &names[..3]);
        std::fs::write(paths.stream_cache().join(MARKER), b"").unwrap();
        let (found, truncated) = walk(&blocking(), &paths.stream_cache(), true, 1000).unwrap();
        assert!(!truncated);
        let mut listed: Vec<_> = found.iter().filter(|(_, _, m)| m.is_file()).map(|(name, _, _)| name.clone()).collect();
        listed.sort();
        let mut expected = names.clone(); expected.sort();
        assert_eq!(listed, expected);
        assert!(found.iter().any(|(name, _, m)| name == "partial" && m.is_dir()));
        let (plain, _) = walk(&blocking(), &paths.stream_cache(), false, 1000).unwrap();
        assert!(plain.iter().any(|(name, _, _)| is_shard(name)), "a plain folder lists shard directories");
        assert!(walk(&blocking(), &paths.stream_cache(), true, 2).unwrap().1, "bounded");
        assert!(walk(&blocking(), &paths.root.join("missing"), true, 10).unwrap().0.is_empty());
    }

    /// Host-filesystem cost of moving a generated 6,000-file cover cache.
    /// `cargo test -p q2nd shard::tests::migration_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "a host measurement, run by hand"]
    fn migration_cost_on_a_generated_6000_file_tree() {
        let paths = fixture("shard-cost");
        let root = paths.art_cache();
        let names: Vec<OsString> = (0..6000).map(|i| {
            let name = OsString::from(format!("{:x}.png", Md5::digest(format!("generated cover {i}"))));
            std::fs::write(root.join(&name), [0u8; 3000]).unwrap();
            name
        }).collect();
        let lookup = |paths: &Paths| {
            let started = std::time::Instant::now();
            for name in names.iter().step_by(10) { assert!(paths.locate(&blocking(), &paths.sharded(Store::Art, name)).is_some()); }
            started.elapsed().as_secs_f64() * 1e6 / 600.0
        };
        let flat_lookup = lookup(&paths);
        let started = std::time::Instant::now();
        let listed = flat_names(&blocking(), &root, 4096).unwrap().unwrap();
        let pass = started.elapsed();
        let mut batches = Vec::new();
        let mut total = Progress::default();
        let migration = std::time::Instant::now();
        loop {
            let names = flat_names(&blocking(), &root, 4096).unwrap().unwrap();
            if names.is_empty() { break; }
            for chunk in names.chunks(8) {
                let started = std::time::Instant::now();
                total += migrate(&blocking(), &paths, Store::Art, chunk);
                batches.push(started.elapsed().as_secs_f64() * 1e3);
            }
        }
        let migration = migration.elapsed();
        assert_eq!(total.moved, 6000);
        paths.mark_complete(&blocking(), Store::Art).unwrap();
        let sharded_lookup = lookup(&paths);
        batches.sort_by(f64::total_cmp);
        let largest = std::fs::read_dir(&root).unwrap().flatten().filter(|e| e.file_type().unwrap().is_dir())
            .map(|e| std::fs::read_dir(e.path()).unwrap().count()).max().unwrap();
        println!("Q2_SHARD_COST {}", serde_json::json!({
            "files": 6000, "first_pass_names": listed.len(), "first_pass_ms": pass.as_secs_f64() * 1e3,
            "migration_s_unpaced": migration.as_secs_f64(), "batches": batches.len(),
            "batch_ms_median": batches[batches.len() / 2], "batch_ms_p99": batches[batches.len() * 99 / 100],
            "batch_ms_max": batches[batches.len() - 1], "lookup_us_before": flat_lookup, "lookup_us_after": sharded_lookup,
            "largest_shard": largest, "paced_s_at_8_per_second": 6000.0 / 8.0,
        }));
        std::fs::remove_dir_all(&paths.root).unwrap();
    }
}
