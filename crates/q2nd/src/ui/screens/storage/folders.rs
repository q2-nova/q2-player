//! Count the Other Files subtree on a worker. Only the largest page of rows
//! is retained, even when a shard contains thousands of small covers.
use crate::{
    config::Paths,
    rt::Blocking,
    shard::{self, Store},
};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

const ROW_LIMIT: usize = 256;

#[derive(Eq, PartialEq)]
pub(super) struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub directory: bool,
    pub allocated: u64,
}

// BinaryHeap drops its greatest element: the smallest file, last name on ties.
impl Ord for FileEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .allocated
            .cmp(&self.allocated)
            .then_with(|| self.name.cmp(&other.name))
            .then_with(|| self.path.cmp(&other.path))
            .then_with(|| self.directory.cmp(&other.directory))
    }
}
impl PartialOrd for FileEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct Scan<'a> {
    cancelled: &'a AtomicBool,
    device: u64,
    excluded: [PathBuf; 4],
    art: PathBuf,
    rows: BinaryHeap<FileEntry>,
    truncated: bool,
}

impl Scan<'_> {
    fn children(
        &mut self,
        blocking: &Blocking,
        path: &Path,
        depth: usize,
        list: bool,
    ) -> io::Result<u64> {
        // Bound open directory descriptors on malformed or deeply nested media.
        if depth > 64 {
            return Err(io::Error::other("Folder nesting limit"));
        }
        let mut total = 0;
        for entry in fs::read_dir(path)? {
            if self.cancelled.load(Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let entry = entry?;
            let child = entry.path();
            if self.excluded.contains(&child) {
                continue;
            }
            let Some(metadata) = shard::metadata(blocking, &child)? else {
                continue;
            };
            if metadata.dev() != self.device || !(metadata.is_file() || metadata.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let shard = path == self.art && metadata.is_dir() && shard::is_shard(&name);
            let mut allocated = metadata.blocks() * 512;
            if metadata.is_dir() {
                allocated += self.children(blocking, &child, depth + 1, list && shard)?;
            }
            total += allocated;
            if list && !shard && !(path == self.art && name == shard::MARKER) {
                self.rows.push(FileEntry {
                    name: name.to_string_lossy().into_owned(),
                    path: child,
                    directory: metadata.is_dir(),
                    allocated,
                });
                if self.rows.len() > ROW_LIMIT {
                    self.rows.pop();
                    self.truncated = true;
                }
            }
        }
        Ok(total)
    }
}

pub(super) fn read(
    blocking: &Blocking,
    paths: &Paths,
    path: &Path,
    cancelled: &AtomicBool,
) -> Result<(Vec<FileEntry>, bool), String> {
    let read_error = |_| "Couldn't read the card".to_string();
    let root = paths.media_root();
    let root_metadata = fs::symlink_metadata(root).map_err(read_error)?;
    if !root_metadata.is_dir() {
        return Err("Couldn't read the card".into());
    }
    let device = root_metadata.dev();
    // An already listed folder may have been replaced since the listener opened it.
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "Couldn't read the card")?;
    let mut ancestor = root.to_owned();
    for component in relative.components() {
        ancestor.push(component);
        let metadata = fs::symlink_metadata(&ancestor).map_err(read_error)?;
        if !metadata.is_dir() || metadata.dev() != device {
            return Err("Couldn't read the card".into());
        }
    }
    let mut scan = Scan {
        cancelled,
        device,
        art: paths.art_cache(),
        excluded: [
            Store::Songs,
            Store::Offline,
            Store::OfflineArt,
            Store::Spoken,
        ]
        .map(|store| paths.store_root(store)),
        rows: BinaryHeap::new(),
        truncated: false,
    };
    scan.children(blocking, path, 0, true).map_err(read_error)?;
    Ok((scan.rows.into_sorted_vec(), scan.truncated))
}
