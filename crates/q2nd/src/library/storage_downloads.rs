//! On-card audio behind the Downloads slice, independent of transfer queues.
use super::{LibError, Library, Paths};
use crate::rt::Blocking;
use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use subsonic::{Song, SpokenSource};

impl Library {
    pub(crate) async fn storage_downloads(self: &Arc<Self>) -> Result<Vec<(Song, u64)>, LibError> {
        let generation = self.client_generation();
        let (namespace, shelf_generation) = self
            .spoken_download_identity()
            .await
            .map_err(LibError::Server)?;
        let admitted_shelf = shelf_generation.clone();
        let (mut music, spoken) = self
            .music_db_run(generation, move |blocking, db| {
                if !db.shelf_generation_current(&admitted_shelf) {
                    return Err(LibError::Offline);
                }
                let music = db.downloaded_songs(blocking).map_err(|_| unreadable())?;
                let spoken = db
                    .downloaded_spoken_files(blocking)
                    .map_err(|_| unreadable())?;
                Ok((music, spoken))
            })
            .await?;
        let owner = self.clone();
        crate::rt::jobs()
            .run_isolated(move |blocking| {
                music.retain(|(song, _)| song.spoken.is_none() && song.radio_url.is_none());
                music.extend(spoken.into_iter().filter(|(song, _)| {
                    song.spoken.as_ref().is_some_and(|spoken| {
                        matches!(&spoken.source, SpokenSource::Audiobookshelf { namespace: account, .. }
                            if namespace.is_empty() || account == &namespace)
                    })
                }));
                let rows = inspect(blocking, &owner.paths, music)?;
                if owner.client_generation() != generation
                    || !owner.shelf_generation_current(&shelf_generation)
                {
                    return Err(LibError::Offline);
                }
                Ok(rows)
            })
            .await
            .map_err(|_| unreadable())?
    }
}

fn unreadable() -> LibError {
    LibError::Server("Unable to read downloaded items".into())
}

#[derive(Default)]
struct Item {
    song: Song,
    bytes: u64,
    inodes: HashSet<u64>,
}

fn inspect(
    blocking: &Blocking,
    paths: &Paths,
    rows: Vec<(Song, PathBuf)>,
) -> Result<Vec<(Song, u64)>, LibError> {
    let root = paths
        .media_root()
        .canonicalize()
        .map_err(|_| unreadable())?;
    let device = root.metadata().map_err(|_| unreadable())?.dev();
    let mut items = BTreeMap::<(bool, String), Item>::new();
    let mut inspected = std::collections::HashMap::new();
    for (song, path) in rows {
        let Some(path) = paths.media_path(&path) else {
            continue;
        };
        let Some((inode, bytes)) = (match inspected.get(&path) {
            Some(&file) => file,
            None => {
                let file = audio_allocation(blocking, paths, &path, &root, device)?;
                inspected.insert(path, file);
                file
            }
        }) else {
            continue;
        };
        let (key, offset) = match song.spoken.as_ref() {
            Some(spoken) => ((true, spoken.id.clone()), spoken.offset_ms),
            None => ((false, song.id.clone()), 0),
        };
        let item = items.entry(key).or_default();
        if item.inodes.is_empty()
            || item
                .song
                .spoken
                .as_ref()
                .is_some_and(|spoken| offset < spoken.offset_ms)
        {
            item.song = song;
        }
        if item.inodes.insert(inode) {
            item.bytes = item.bytes.checked_add(bytes).ok_or_else(unreadable)?;
        }
    }
    let mut items = items
        .into_values()
        .map(|item| (item.song, item.bytes))
        .collect::<Vec<_>>();
    items.sort_by(|(left, left_bytes), (right, right_bytes)| {
        right_bytes
            .cmp(left_bytes)
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(items)
}

/// Artwork shared by downloads belongs to the wedge total. Item sizes
/// describe audio allocation and do not charge a shared cover to each item.
fn audio_allocation(
    blocking: &Blocking,
    paths: &Paths,
    path: &Path,
    root: &Path,
    device: u64,
) -> Result<Option<(u64, u64)>, LibError> {
    let Some((actual, metadata)) = paths
        .locate_metadata(blocking, path)
        .map_err(|_| unreadable())?
    else {
        return Ok(None);
    };
    if metadata.dev() != device || metadata.len() == 0 {
        return Ok(None);
    }
    let actual = match actual.canonicalize() {
        Ok(actual) => actual,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(unreadable()),
    };
    if !actual.starts_with(root) {
        return Ok(None);
    }
    let bytes = metadata.blocks().checked_mul(512).ok_or_else(unreadable)?;
    Ok(Some((metadata.ino(), bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(id: &str, logical: &str, offset_ms: u64, episode: bool) -> Song {
        Song {
            id: id.into(),
            title: logical.into(),
            spoken: Some(Box::new(subsonic::Spoken {
                id: logical.into(),
                offset_ms,
                source: SpokenSource::Audiobookshelf {
                    namespace: "generated".into(),
                    item: logical.into(),
                    episode: episode.then(|| "generated-episode".into()),
                    content_url: String::new(),
                },
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[test]
    fn storage_downloads_rank_real_audio_and_count_chapter_files_once() {
        let paths = Paths::new(super::super::generated_test_directory(
            "storage-download-items",
        ));
        let (lib, _) = Library::create(paths, &crate::config::Config::default(), Arc::new(|_| {}));
        lib.set_offline_mode(true);
        let blocking = Blocking::dedicated_thread();
        let write_file = |path: &Path, size: usize| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = std::fs::File::create(path).unwrap();
            std::io::Write::write_all(&mut file, &vec![0x3c; size]).unwrap();
            file.sync_all().unwrap();
            file.metadata().unwrap().blocks() * 512
        };
        let offline = lib.paths.offline();
        let spoken = lib.paths.spoken_downloads();
        let song_name = "generated-song.wav";
        let song_flat = offline.join(song_name);
        let song_shard = lib
            .paths
            .sharded(crate::shard::Store::Offline, song_name.as_ref());
        let song_bytes = write_file(&song_shard, 128);
        let book_a = spoken.join("generated-first.m4b");
        let book_b = spoken.join("generated-second.m4b");
        let book_bytes = write_file(&book_a, 160_000) + write_file(&book_b, 160_000);
        let episode = spoken.join("generated-podcast.m4a");
        let episode_bytes = write_file(&episode, 80_000);
        assert!(book_bytes > episode_bytes && episode_bytes > song_bytes);
        {
            let db = lib.db();
            let put_song = |id: &str, path: &Path| {
                db.put_offline_song(
                    &blocking,
                    &Song {
                        id: id.into(),
                        title: id.into(),
                        ..Default::default()
                    },
                    path,
                    1,
                );
            };
            put_song("generated-song", &song_flat);
            put_song("generated-missing", &offline.join("missing.wav"));
            put_song("generated-empty", &offline.join("empty.wav"));
            let empty = offline.join("empty.wav");
            std::fs::write(empty, b"").unwrap();
            db.put_cached_song(&blocking, "generated-cache", "raw", &song_flat, 1)
                .unwrap();

            let parts = [
                (chapter("generated-c", "Book", 0, false), &book_a),
                (chapter("generated-b", "Book", 1_000, false), &book_a),
                (chapter("generated-a", "Book", 2_000, false), &book_b),
                (chapter("generated-episode", "Episode", 0, true), &episode),
            ];
            for (song, _) in &parts {
                db.put_spoken(&blocking, song).unwrap();
            }
            let book_parts = parts[..3]
                .iter()
                .map(|(song, path)| (song.id.clone(), (*path).clone(), 1))
                .collect::<Vec<_>>();
            db.put_spoken_downloads(&blocking, "Book", &book_parts)
                .unwrap();
            db.put_spoken_downloads(&blocking, "Episode", &[(parts[3].0.id.clone(), episode, 1)])
                .unwrap();
            db.put_spoken(
                &blocking,
                &chapter("generated-unfinished", "Unfinished", 0, false),
            )
            .unwrap();
        }

        let runtime = crate::rt::Ui::new().unwrap();
        let rows = runtime.block_on(lib.storage_downloads()).unwrap();
        assert_eq!(
            rows.iter()
                .map(|(song, size)| (song.id.as_str(), *size))
                .collect::<Vec<_>>(),
            [
                ("generated-c", book_bytes),
                ("generated-episode", episode_bytes),
                ("generated-song", song_bytes),
            ]
        );
        std::fs::remove_file(&book_a).unwrap();
        std::fs::remove_file(&book_b).unwrap();
        let rows = runtime.block_on(lib.storage_downloads()).unwrap();
        assert_eq!(rows.len(), 2);
    }
}
