//! Card-owned spoken media and listening positions. No Navidrome IDs or resets.
mod quicktime;
#[cfg(test)]
mod db_tests;
#[cfg(test)]
mod m4b_tests;
use super::Library;
use crate::config::SpokenSettings;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use subsonic::{Chapter, Song, Spoken, SpokenSource};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub position_ms: u64,
    pub updated_ms: u64,
    pub revision: u64,
    pub dirty: bool,
}

#[derive(Clone)]
pub(crate) struct PlayedTarget {
    pub logical: String,
    pub(super) remote: Option<(String, String, Option<String>)>,
}

impl PlayedTarget {
    pub(crate) fn shelf(namespace: String, item: String, episode: Option<String>) -> Self {
        let logical = format!("abs:{namespace}:{item}:{}", episode.as_deref().unwrap_or_default());
        Self { logical, remote: Some((namespace, item, episode)) }
    }

    pub(crate) fn song(song: &Song) -> Option<Self> {
        let spoken = song.spoken.as_ref()?;
        Some(match &spoken.source {
            SpokenSource::Audiobookshelf { namespace, item, episode, .. } =>
                Self::shelf(namespace.clone(), item.clone(), episode.clone()),
            _ => Self { logical: spoken.id.clone(), remote: None },
        })
    }
}

pub(super) struct PlayedUpload {
    pub logical: String,
    pub item: String,
    pub episode: Option<String>,
    pub played: bool,
    pub revision: i64,
}

/// Durable cumulative session outbox; retries resend totals, never increments.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct SessionUpload {
    pub song: Song,
    pub session: crate::audio::listening::Session,
    pub position_ms: u64,
    pub updated_ms: u64,
    pub revision: u64,
}

/// Disk evidence only, computed by a worker. Unknown preserves the last known
/// Home availability across temporary card/index read failures.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Availability {
    pub books: Option<bool>,
    pub podcasts: Option<bool>,
}

/// Book identity spans all physical files; podcast identity is the parent
/// series item, deliberately excluding its episode and local download path.
pub fn speed_target(song: &Song) -> Option<String> {
    let spoken = song.spoken.as_ref()?;
    let target = match &spoken.source {
        SpokenSource::Audiobookshelf { namespace, item, episode, .. } => {
            (if episode.is_some() { "abs-podcast-series" } else { "abs-book" }, namespace.as_str(), item.as_str())
        }
        _ => ("local-book", "", spoken.id.as_str()),
    };
    Some(serde_json::to_string(&target).expect("string tuple serialization"))
}

pub fn skip_target(
    spoken: &Spoken,
    position_ms: u64,
    forward: bool,
    settings: SpokenSettings,
) -> u64 {
    let duration = spoken.duration_ms;
    let target = if settings.skip_chapter {
        if forward {
            spoken
                .chapters
                .iter()
                .map(|c| c.start_ms)
                .find(|&p| p > position_ms)
                .unwrap_or(duration)
        } else {
            // Select the previous chapter, not merely the current start.
            // Playback advances after a seek; a predecessor-boundary search
            // would then restart the current chapter on every button press.
            spoken.chapters.partition_point(|c|c.start_ms<=position_ms)
                .checked_sub(2)
                .and_then(|i|spoken.chapters.get(i))
                .map(|c|c.start_ms)
                .unwrap_or(0)
        }
    } else {
        let delta = u64::from((if forward { settings.skip_forward_seconds } else { settings.skip_backward_seconds }).clamp(1, 3600)) * 1000;
        if forward {
            position_ms.saturating_add(delta)
        } else {
            position_ms.saturating_sub(delta)
        }
    };
    target.min(duration.saturating_sub(1))
}
pub fn selection(queue: &[Song], selected: usize, absolute: u64) -> (usize, u64) {
    let Some(spoken) = queue.get(selected).and_then(|s| s.spoken.as_ref()) else {
        return (selected, 0);
    };
    let absolute = absolute.min(spoken.duration_ms.saturating_sub(1));
    let i = queue
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.spoken
                .as_ref()
                .is_some_and(|p| p.id == spoken.id && p.offset_ms <= absolute)
        })
        .max_by_key(|(_, s)| s.spoken.as_ref().unwrap().offset_ms)
        .map(|(i, _)| i)
        .unwrap_or(selected);
    (
        i,
        absolute.saturating_sub(queue[i].spoken.as_ref().unwrap().offset_ms),
    )
}
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

type Inventory = Vec<(Song, Result<Option<std::path::PathBuf>, ()>)>;
fn spoken_inventory(blocking: &crate::rt::Blocking, db: &mut super::db::Db) -> Result<Inventory, String> {
    let songs = db.spoken_songs(blocking).map_err(|_| "Unable to read downloaded audiobooks".to_string())?;
    Ok(songs.into_iter().filter(|song| song.spoken.as_ref().is_some_and(|book| matches!(book.source, SpokenSource::Audiobookshelf { .. }))).map(|song| {
        let path = db.spoken_download(blocking, &song.id).map_err(|_| ());
        (song, path)
    }).collect())
}
pub(super) fn stored_queue(blocking: &crate::rt::Blocking, db: &mut super::db::Db, logical: &str) -> Result<Vec<Song>, String> {
    let mut songs = db.spoken_songs(blocking).map_err(|_| "Unable to read the spoken library".to_string())?
        .into_iter().filter(|song| song.spoken.as_ref().is_some_and(|book| book.id == logical)).collect::<Vec<_>>();
    songs.sort_by_key(|song| song.spoken.as_ref().unwrap().offset_ms);
    Ok(songs)
}
/// A downloaded file this card owns and still holds.
pub(super) fn downloaded_file(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, path: &Path) -> bool {
    paths.can_modify_media(path) && paths.locate(blocking, path).is_some()
}
fn present_downloaded(blocking: &crate::rt::Blocking, paths: &crate::config::Paths, inventory: Inventory) -> Vec<Song> {
    let mut seen = std::collections::HashSet::new();
    inventory.into_iter().filter_map(|(song, path)| {
        let spoken = song.spoken.as_ref()?;
        (matches!(spoken.source, SpokenSource::Audiobookshelf { .. })
            && path.ok().flatten().is_some_and(|path| downloaded_file(blocking, paths, &path))
            && seen.insert(spoken.id.clone())).then_some(song)
    }).collect()
}
fn checkpoint(song: &Song, position_ms: u64) -> Option<(String, u64)> {
    let spoken = song.spoken.as_ref()?;
    Some((spoken.id.clone(), spoken.offset_ms.saturating_add(position_ms).min(spoken.duration_ms)))
}

impl Library {
    #[cfg(test)]
    pub(crate) fn spoken_availability(&self) -> Availability {
        let inventory = self.db_worker.run_blocking(spoken_inventory);
        self.inspect_spoken_availability(&crate::rt::Blocking::dedicated_thread(), inventory)
    }
    pub(crate) async fn spoken_availability_async(self: &Arc<Self>) -> Availability {
        if self.paths.ephemeral_media { return Availability { books: Some(false), podcasts: Some(false) }; }
        let inventory = self.db_run(spoken_inventory).await;
        let lib = self.clone();
        crate::rt::jobs().run_isolated(move |blocking| lib.inspect_spoken_availability(blocking, inventory)).await.unwrap_or_default()
    }
    fn inspect_spoken_availability(&self, blocking: &crate::rt::Blocking, inventory: Result<Inventory, String>) -> Availability {
        if self.paths.ephemeral_media {
            return Availability { books: Some(false), podcasts: Some(false) };
        }
        fn either(a: Option<bool>, b: Option<bool>) -> Option<bool> {
            if a == Some(true) || b == Some(true) { Some(true) }
            else if a.is_none() || b.is_none() { None } else { Some(false) }
        }
        let Ok(songs) = inventory else { return Availability::default(); };
        let mut availability = Availability { books: Some(false), podcasts: Some(false) };
        for (song, download) in songs {
            let Some(Spoken { source: SpokenSource::Audiobookshelf { episode, .. }, .. }) = song.spoken.as_deref() else { continue; };
            let present = match download {
                Ok(Some(path)) if self.paths.can_modify_media(&path) => self.paths.locate_metadata(blocking, &path).ok().map(|found| found.is_some()),
                Ok(_) => Some(false),
                Err(_) => None,
            };
            let kind = if episode.is_some() { &mut availability.podcasts } else { &mut availability.books };
            *kind = either(*kind, present);
        }
        if availability.books != Some(true) {
            let root = self.paths.media_root().join("audiobooks");
            let local = (|| {
                if std::fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Ok(Some(false));
                }
                let mut files = Vec::new();
                collect(blocking, &root, &root, &mut files, 0)?;
                let mut unknown = false;
                for relative in files {
                    match read_m4b_info(blocking, &root.join(relative)) {
                        Ok(_) => return Ok(Some(true)),
                        Err(error) => {
                            // Malformed metadata and concurrently removed files
                            // are negative evidence; actual I/O failures are not.
                            unknown |= error.raw_os_error().is_some() && error.kind() != io::ErrorKind::NotFound;
                        }
                    }
                }
                Ok::<_, io::Error>(if unknown { None } else { Some(false) })
            })();
            availability.books = match local {
                Ok(local) => either(availability.books, local),
                Err(_) => None,
            };
        }
        availability
    }

    #[cfg(test)]
    pub fn spoken_queue(&self, logical: &str) -> Result<Vec<Song>, String> {
        let logical = logical.to_owned();
        self.db_worker.run_blocking(move |blocking, db| stored_queue(blocking, db, &logical))
    }
    pub fn spoken_queue_async(&self, logical: String) -> impl std::future::Future<Output = Result<Vec<Song>, String>> + Send + 'static {
        self.db_run(move |blocking, db| stored_queue(blocking, db, &logical))
    }
    #[cfg(test)]
    pub fn downloaded_spoken(&self) -> Result<Vec<Song>, String> {
        let inventory = self.db_worker.run_blocking(spoken_inventory)?;
        Ok(present_downloaded(&crate::rt::Blocking::dedicated_thread(), &self.paths, inventory))
    }
    pub async fn downloaded_spoken_async(self: &Arc<Self>) -> Result<Vec<Song>, String> {
        let inventory = self.db_run(spoken_inventory).await?;
        let paths = self.paths.clone();
        crate::rt::jobs().run_isolated(move |blocking| present_downloaded(blocking, &paths, inventory)).await
            .map_err(|_| "Unable to inspect downloaded audiobooks".into())
    }
    pub fn spoken_skip(&self) -> SpokenSettings {
        *self.spoken_settings.read().unwrap()
    }
    pub fn spoken_speed(&self, song: &Song) -> Result<Option<u16>, String> {
        let Some(target) = speed_target(song) else { return Ok(None); };
        self.db_worker.run_blocking(move |blocking, db| db.spoken_speed(blocking, &target))
            .map_err(|_| "Unable to read saved playback speed on the card".into())
    }
    pub fn set_spoken_speed(&self, song: &Song, percent: Option<u16>) -> Result<(), String> {
        let Some(target) = speed_target(song) else { return Ok(()); };
        if percent.is_some_and(|value| !(50..=300).contains(&value) || value % 5 != 0) {
            return Err("Invalid spoken playback speed".into());
        }
        if self.paths.ephemeral_media {
            return Err("Insert a card to save custom playback speed".into());
        }
        self.db_worker.run_blocking(move |blocking, db| db.set_spoken_speed(blocking, &target, percent))
            .map_err(|_| "Unable to save custom playback speed on the card".into())
    }
    pub(crate) async fn spoken_percentages(&self, entries: Vec<(String, u64)>) -> std::collections::HashMap<String, u8> {
        self.db_run(move |blocking, db| entries.into_iter().filter_map(|(key, duration)| {
            db.spoken_percent(blocking, &key, duration).ok().map(|percent| (key, percent))
        }).collect()).await
    }

    pub(crate) async fn mark_spoken_played(self: &Arc<Self>, target: PlayedTarget, played: bool) -> Result<(), String> {
        if self.paths.ephemeral_media { return Err("Insert a card to save listening progress".into()); }
        let result = if let Some((namespace, _, _)) = &target.remote {
            let generation = self.shelf_generation();
            if !self.shelf_namespace_current(namespace) { return Err("Audiobookshelf changed; select the item again".into()); }
            self.shelf_db_run(generation, move |blocking, db| {
                db.set_spoken_played(blocking, &target, played, epoch_ms())
                    .map_err(|_| super::LibError::Server("Unable to save listening progress on the card".into()))
            }).await.map_err(|error| error.to_string())
        } else {
            self.db_run(move |blocking, db| db.set_spoken_played(blocking, &target, played, epoch_ms()))
                .await.map_err(|_| "Unable to save listening progress on the card".into())
        };
        result?;
        self.notify(super::LibEvent::SpokenProgress);
        self.sync_urgent.store(true, std::sync::atomic::Ordering::Relaxed);
        self.wake_sync();
        Ok(())
    }

    pub fn spoken_position(&self, id: &str) -> Result<Option<Position>, String> {
        let id = id.to_owned();
        self.db_worker.run_blocking(move |blocking, db| db.spoken_position(blocking, &id))
            .map_err(|_| "Unable to read audiobook listening location on the card".into())
    }
    pub fn save_spoken(&self, song: &Song, position_ms: u64) -> Result<(), String> {
        let Some((id, position)) = checkpoint(song, position_ms) else { return Ok(()); };
        if self.paths.ephemeral_media { return Err("Insert a card to save audiobook listening locations".into()); }
        self.db_worker.run_blocking(move |blocking, db| db.save_spoken_position(blocking, &id, position, epoch_ms()))
            .map_err(|_| "Unable to save listening position on the card".to_string())?;
        self.wake_sync();
        Ok(())
    }
    /// Posts before returning; dropping the observer cannot cancel an accepted
    /// durable checkpoint. The coalescing writer awaits this owner directly.
    pub fn save_spoken_async(&self, song: &Song, position_ms: u64) -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
        self.save_spoken_session_async(song, position_ms, None)
    }
    pub fn save_spoken_session_async(&self, song: &Song, position_ms: u64, session: Option<crate::audio::listening::Session>) -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
        let request = checkpoint(song, position_ms);
        let song = song.clone();
        let no_card = request.is_some() && self.paths.ephemeral_media;
        let wake = self.sync_wake.clone();
        let urgent = self.sync_urgent.clone();
        let notify = self.notify.clone();
        let stored = request.filter(|_| !no_card).map(|(id, position)| self.db_run(move |blocking, db| {
            db.save_spoken_checkpoint(blocking, &id, position, epoch_ms(), session.as_ref().map(|session| (&song, session)))
                .map_err(|_| "Unable to save listening position on the card".to_string())?;
            urgent.store(true, std::sync::atomic::Ordering::Relaxed);
            wake.notify_one();
            notify(super::LibEvent::SpokenProgress);
            Ok(())
        }));
        async move {
            if no_card { return Err("Insert a card to save audiobook listening locations".into()); }
            match stored { Some(stored) => stored.await, None => Ok(()) }
        }
    }
    /// Ordinary-thread diagnostic compatibility; UI awaits the same core.
    pub fn local_audiobooks(&self) -> Result<(Vec<Song>, Vec<String>), String> {
        let lib = self.me.upgrade().ok_or("Library closed")?;
        super::shelf_browse::wait(async move { lib.local_audiobooks_async().await.map_err(super::LibError::Server) })
            .map_err(|error| error.to_string())
    }
    pub async fn local_audiobooks_async(self: &Arc<Self>) -> Result<(Vec<Song>, Vec<String>), String> {
        let lib = self.clone();
        let files = crate::rt::jobs().run_isolated(move |blocking| lib.local_book_files(blocking)).await
            .map_err(|_| "Unable to scan audiobooks on the card")??;
        let mut books = Vec::new();
        let mut issues = Vec::new();
        // One parsed metadata record at a time: never retain all descriptions
        // while waiting for SQLite, nor occupy a card worker during that wait.
        for relative in files {
            let lib = self.clone();
            let issue = format!("Cannot read audiobook: {}", relative.display());
            let read = crate::rt::jobs().run_isolated(move |blocking| lib.read_local_book(blocking, &relative)).await
                .map_err(|_| "Unable to read audiobooks on the card")??;
            let Some((song, key, body)) = read else { issues.push(issue); continue; };
            let song = self.db_run(move |blocking, db| {
                db.put_spoken_response(blocking, &key, &body).map_err(|_| "Unable to save audiobook information on the card")?;
                db.put_spoken(blocking, &song).map_err(|_| "Unable to save audiobook catalog on the card")?;
                Ok::<_, String>(song)
            }).await?;
            books.push(song);
        }
        crate::rt::jobs().run_isolated(move |_| {
            books.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
            (books, issues)
        }).await.map_err(|_| "Unable to sort audiobooks".into())
    }
    fn local_book_files(&self, blocking: &crate::rt::Blocking) -> Result<Vec<std::path::PathBuf>, String> {
        if self.paths.ephemeral_media { return Err("Insert a card for local audiobooks".into()); }
        let root = self.paths.media_root().join("audiobooks");
        if std::fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Audiobooks directory must stay on the card".into());
        }
        let mut files = Vec::new();
        collect(blocking, &root, &root, &mut files, 0).map_err(|_| "Unable to read audiobooks on the card")?;
        Ok(files)
    }
    fn read_local_book(&self, blocking: &crate::rt::Blocking, relative: &Path) -> Result<Option<(Song, String, String)>, String> {
        let path = self.paths.media_root().join("audiobooks").join(relative);
        let Ok(info) = read_m4b_info(blocking, &path) else { return Ok(None); };
        let readable_info = super::book_info::BookInfo::from_m4b(&info,
            path.file_stem().unwrap_or_default().to_string_lossy().as_ref());
        let M4bInfo { duration_ms, chapters, title, artist, .. } = info;
        // The card-relative path is stable across mounts and restarts.
        let relative_path = relative.to_string_lossy().into_owned();
        let (key, body) = super::book_info::local_cache_record(&relative_path, readable_info)
            .map_err(|_| "Unable to save audiobook information on the card")?;
        let id = format!("local-m4b:{relative_path}");
        let song = Song {
            id: id.clone(),
            title: title.unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned()),
            artist: artist.unwrap_or_else(|| "Audiobook".into()),
            cover_art: Some(format!("local-book-cover:{relative_path}")),
            duration: (duration_ms / 1000).min(u32::MAX as u64) as u32,
            suffix: Some("m4b".into()), size: path.metadata().ok().map(|m| m.len()),
            spoken: Some(Box::new(Spoken { id, offset_ms: 0, duration_ms, chapters, source: SpokenSource::Local { relative_path } })),
            ..Default::default()
        };
        Ok(Some((song, key, body)))
    }
    pub fn local_book_path(&self, _blocking: &crate::rt::Blocking, relative: &str) -> Result<std::path::PathBuf, String> {
        use std::path::Component;
        let relative = Path::new(relative);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || self.paths.ephemeral_media
        {
            return Err("Invalid local audiobook path".into());
        }
        let root = self
            .paths
            .media_root()
            .join("audiobooks")
            .canonicalize()
            .map_err(|_| "Audiobook card unavailable")?;
        let card = self
            .paths
            .media_root()
            .canonicalize()
            .map_err(|_| "Audiobook card unavailable")?;
        if !root.starts_with(&card) {
            return Err("Audiobooks directory leaves the card library".into());
        }
        let path = root
            .join(relative)
            .canonicalize()
            .map_err(|_| "Audiobook file unavailable")?;
        if !path.starts_with(root) {
            return Err("Audiobook path leaves the card library".into());
        }
        Ok(path)
    }
}
fn collect(
    blocking: &crate::rt::Blocking,
    root: &Path,
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
    depth: usize,
) -> io::Result<()> {
    if depth > 16 {
        return Err(io::Error::other("audiobook nesting limit"));
    }
    if !dir.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect(blocking, root, &entry.path(), out, depth + 1)?;
        } else if kind.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("m4b"))
        {
            if out.len() >= 10000 {
                return Err(io::Error::other("audiobook catalog limit"));
            }
            out.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(io::Error::other)?
                    .to_path_buf(),
            );
        }
    }
    Ok(())
}
fn bad() -> io::Error {
    io::Error::other("invalid M4B metadata")
}
fn be32(data: &[u8], at: usize) -> io::Result<u32> {
    Ok(u32::from_be_bytes(
        data.get(at..at + 4).ok_or_else(bad)?.try_into().unwrap(),
    ))
}
fn be64(data: &[u8], at: usize) -> io::Result<u64> {
    Ok(u64::from_be_bytes(
        data.get(at..at + 8).ok_or_else(bad)?.try_into().unwrap(),
    ))
}
/// Read bounded metadata only; `mdat` audio bytes are skipped, never loaded.
pub struct M4bInfo {
    pub duration_ms: u64,
    pub chapters: Vec<Chapter>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub description: Option<String>,
    pub narrator: Option<String>,
    pub series: Option<String>,
    pub cover: Option<Vec<u8>>,
}
pub fn read_m4b_info(blocking: &crate::rt::Blocking, path: &Path) -> io::Result<M4bInfo> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut pos = 0;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos))?;
        let mut head = [0; 8];
        file.read_exact(&mut head)?;
        let mut size = u32::from_be_bytes(head[..4].try_into().unwrap()) as u64;
        let mut header = 8;
        if size == 1 {
            let mut large = [0; 8];
            file.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            header = 16;
        }
        if size == 0 {
            size = len - pos;
        }
        if size < header || size > len - pos {
            return Err(bad());
        }
        if &head[4..] == b"moov" {
            if size - header > 16 * 1024 * 1024 {
                return Err(bad());
            }
            let mut data = vec![0; (size - header) as usize];
            file.read_exact(&mut data)?;
            let mut duration = 0;
            let mut chapters = Vec::new();
            parse_atoms(&data, 0, &mut duration, &mut chapters)?;
            if duration == 0 {
                return Err(bad());
            }
            let text_chapters = quicktime::read(blocking, &mut file, &data, duration)?;
            if !text_chapters.is_empty() {
                chapters = text_chapters;
            }
            chapters.retain(|c| c.start_ms < duration);
            chapters.sort_by_key(|c| c.start_ms);
            chapters.dedup_by_key(|c| c.start_ms);
            let metadata = quicktime::metadata(&data)?;
            return Ok(M4bInfo {
                duration_ms: duration,
                chapters,
                title: metadata.title,
                artist: metadata.artist,
                description: metadata.description,
                narrator: metadata.narrator,
                series: metadata.series,
                cover: metadata.cover,
            });
        }
        pos += size;
    }
    Err(bad())
}
fn parse_atoms(
    data: &[u8],
    depth: u8,
    duration: &mut u64,
    chapters: &mut Vec<Chapter>,
) -> io::Result<()> {
    if depth > 12 {
        return Err(bad());
    }
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let mut size = be32(data, pos)? as usize;
        let mut head = 8;
        if size == 1 {
            size = usize::try_from(be64(data, pos + 8)?).map_err(|_| bad())?;
            head = 16;
        }
        if size == 0 {
            size = data.len() - pos;
        }
        if size < head || size > data.len() - pos {
            return Err(bad());
        }
        let body = &data[pos + head..pos + size];
        match &data[pos + 4..pos + 8] {
            b"mvhd" => {
                let version = *body.first().ok_or_else(bad)?;
                let (scale, ticks) = if version == 0 {
                    (be32(body, 12)?, be32(body, 16)? as u64)
                } else if version == 1 {
                    (be32(body, 20)?, be64(body, 24)?)
                } else {
                    return Err(bad());
                };
                if scale == 0 {
                    return Err(bad());
                }
                *duration = ticks.saturating_mul(1000) / scale as u64;
            }
            b"udta" => parse_atoms(body, depth + 1, duration, chapters)?,
            b"chpl" => {
                // Nero chapter atom: full-box header, reserved u32, count u8.
                let count = *body.get(8).ok_or_else(bad)? as usize;
                let mut at = 9;
                // A file may repeat the atom; the text-track reader's limit
                // holds for the sum.
                if chapters.len() + count > quicktime::MAX_CHAPTERS {
                    return Err(bad());
                }
                for _ in 0..count {
                    let start_ms = be64(body, at)? / 10000;
                    at += 8;
                    let n = *body.get(at).ok_or_else(bad)? as usize;
                    at += 1;
                    let title =
                        String::from_utf8_lossy(body.get(at..at + n).ok_or_else(bad)?).into_owned();
                    at += n;
                    chapters.push(Chapter { start_ms, title });
                }
            }
            _ => {}
        }
        pos += size;
    }
    if pos != data.len() {
        return Err(bad());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn speed_identity_is_book_or_podcast_parent_and_never_episode_or_file() {
        let mut a = Song { spoken: Some(Box::new(Spoken { id: "book-logical".into(), source: SpokenSource::Local { relative_path: "a.m4b".into() }, ..Default::default() })), ..Default::default() };
        let mut b = a.clone();
        b.id = "second-file".into();
        b.spoken.as_mut().unwrap().offset_ms = 5000;
        b.spoken.as_mut().unwrap().source = SpokenSource::Local { relative_path: "b.m4b".into() };
        assert_eq!(speed_target(&a), speed_target(&b));
        for episode in [None, Some("episode-a")] {
            a.spoken.as_mut().unwrap().source = SpokenSource::Audiobookshelf { namespace: "account-a".into(), item: "parent".into(), episode: episode.map(str::to_owned), content_url: "file-a".into() };
            b = a.clone();
            b.spoken.as_mut().unwrap().id = "different-logical-file-or-episode".into();
            if let SpokenSource::Audiobookshelf { episode: other, content_url, .. } = &mut b.spoken.as_mut().unwrap().source {
                *other = episode.map(|_| "episode-b".into()); *content_url = "file-b".into();
            }
            assert_eq!(speed_target(&a), speed_target(&b));
            if let SpokenSource::Audiobookshelf { namespace, .. } = &mut b.spoken.as_mut().unwrap().source { *namespace = "account-b".into(); }
            assert_ne!(speed_target(&a), speed_target(&b));
        }
        b = a.clone();
        if let SpokenSource::Audiobookshelf { episode, .. } = &mut b.spoken.as_mut().unwrap().source { *episode = None; }
        assert_ne!(speed_target(&a), speed_target(&b));
        assert_eq!(speed_target(&Song::default()), None);
    }

    use super::*;
    #[test]
    fn seeded_skips_never_cross_book_bounds_or_wrap() {
        let mut rng = fastrand::Rng::with_seed(0xB00C);
        for _ in 0..10000 {
            let duration = rng.u64(1..10_000_000);
            let p = rng.u64(0..duration);
            let book = Spoken {
                duration_ms: duration,
                chapters: vec![
                    Chapter {
                        start_ms: 0,
                        title: "A".into(),
                    },
                    Chapter {
                        start_ms: duration / 2,
                        title: "B".into(),
                    },
                ],
                ..Default::default()
            };
            for chapter in [false, true] {
                for forward in [false, true] {
                    let to = skip_target(
                        &book,
                        p,
                        forward,
                        SpokenSettings {
                            skip_forward_seconds: rng.u32(..),
                            skip_backward_seconds: rng.u32(..),
                            speed_percent: 100,
                            skip_chapter: chapter,
                        },
                    );
                    assert!(to < duration);
                    if forward {
                        assert!(to >= p);
                    } else {
                        assert!(to <= p);
                    }
                }
            }
        }
    }
    #[test]
    fn repeated_chapter_back_moves_one_chapter_despite_active_playback_advance() {
        let book=Spoken {duration_ms:400000,chapters:[0,150000,310000].into_iter()
            .map(|start_ms|Chapter {start_ms,title:format!("Chapter {start_ms}")}).collect(),..Default::default()};
        let settings=SpokenSettings {skip_chapter:true,..Default::default()};
        for advance in [1,17,200] {
            let first=skip_target(&book,310000+advance,false,settings);
            assert_eq!(first,150000);
            let second=skip_target(&book,first+advance,false,settings);
            assert_eq!(second,0);
            assert_eq!(skip_target(&book,second+advance,false,settings),0);
        }
        assert_eq!(skip_target(&book,150000,false,settings),0);
    }

    #[test]
    fn physical_files_select_by_logical_position() {
        let queue: Vec<_> = (0..4)
            .map(|i| Song {
                spoken: Some(Box::new(Spoken {
                    id: "book".into(),
                    offset_ms: i * 1000,
                    duration_ms: 4000,
                    ..Default::default()
                })),
                ..Default::default()
            })
            .collect();
        for p in 0..4000 {
            assert_eq!(selection(&queue, 0, p), ((p / 1000) as usize, p % 1000));
        }
        assert_eq!(selection(&queue, 0, u64::MAX), (3, 999));
    }
    #[test]
    fn malformed_atoms_never_panic() {
        let mut rng = fastrand::Rng::with_seed(420);
        for len in 0..512 {
            for _ in 0..8 {
                let mut bytes = vec![0; len];
                rng.fill(&mut bytes);
                let _ = parse_atoms(&bytes, 0, &mut 0, &mut Vec::new());
            }
        }
    }
}
