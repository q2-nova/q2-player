//! Account-bound subscriptions and opt-in automatic selections. Audio I/O
//! remains owned by the existing music and Audiobookshelf download workers.
use super::{
    LibError, LibEvent, Library, Policy, Priority,
    db::OfflineSet,
    health,
    shelf_browse::{EntryKind, Media, Section},
};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use subsonic::{AlbumListType, PlaylistWithSongs, Song, calls};

#[derive(Default)]
pub(super) struct Background {
    wake: Arc<tokio::sync::Notify>,
    queue: Mutex<Arc<Vec<Song>>>,
}

impl Library {
    pub(crate) fn set_offline_group_queue(&self, queue: &Arc<Vec<Song>>) {
        let mut current = self.offline_groups.queue.lock().unwrap();
        if !Arc::ptr_eq(&current, queue) {
            *current = queue.clone();
        }
    }

    pub(super) fn offline_group_queue(&self) -> Arc<Vec<Song>> {
        self.offline_groups.queue.lock().unwrap().clone()
    }

    pub(super) fn automatic_downloads_enabled(&self) -> bool {
        self.settings.read().unwrap().fill_target.is_some()
    }

    pub(super) fn wake_offline_groups(&self) {
        self.offline_groups.wake.notify_one();
    }

    pub(crate) async fn playlist_followed(&self, id: String) -> bool {
        self.db_run(move |_, db| db.playlist_followed(&id).unwrap_or(false))
            .await
    }

    pub(crate) async fn set_playlist_follow(
        &self,
        action: super::MusicDownloadAction,
        data: PlaylistWithSongs,
        followed: bool,
    ) -> Result<(), String> {
        let generation = action.generation;
        self.music_binding_ready(generation)
            .await
            .map_err(|error| error.to_string())?;
        let _gate = self.offline_art_gate.acquire().await;
        let current = self.client_generation.clone();
        let playlist_id = data.playlist.id.clone();
        self.db_run(move |blocking, db| {
            super::download_actions::music_admission(
                &action,
                current.load(Ordering::SeqCst),
                "playlist",
                &data.playlist.id,
            )?;
            if followed {
                let set = playlist_set(&data);
                db.follow_playlist(blocking, &set, &data.entry, true)
            } else {
                db.stop_following_playlist(blocking, &data.playlist.id)
            }
            .map_err(|_| "Unable to save playlist following".to_string())
        })
        .await?;
        self.downloads.wake();
        self.wake_offline_groups();
        self.notify(LibEvent::PlaylistFollowChanged(playlist_id));
        self.notify(LibEvent::Downloads);
        Ok(())
    }

    pub(crate) async fn collection_download(
        self: &Arc<Self>,
        generation: Arc<()>,
        library: String,
        id: String,
        toggle_follow: bool,
    ) -> Result<String, String> {
        if self.paths.ephemeral_media {
            return Err("Insert a card to download audiobooks".into());
        }
        let (namespace, _) = self.spoken_download_identity().await?;
        if namespace.is_empty() {
            return Err("Set up Audiobookshelf before downloading".into());
        }
        if toggle_follow {
            let (account, group, collection) = (namespace.clone(), library.clone(), id.clone());
            let followed = self
                .shelf_db_run(generation, move |blocking, db| {
                    db.toggle_collection_follow(blocking, &account, &group, &collection)
                        .map_err(fail)
                })
                .await
                .map_err(|e| e.to_string())?;
            self.wake_offline_groups();
            return Ok(if followed {
                "Following new books".into()
            } else {
                "Stopped following".into()
            });
        }
        let policy = if self.offline_mode() {
            Policy::CacheOnly
        } else {
            Policy::NetworkOnly
        };
        let count = collection_pages(self, generation, &namespace, &library, &id, policy, false)
            .await
            .map_err(|e| e.to_string())?;
        Ok(format!("Queued collection · {count} books"))
    }
}

fn playlist_set(data: &PlaylistWithSongs) -> OfflineSet {
    OfflineSet {
        kind: "playlist".into(),
        id: data.playlist.id.clone(),
        name: data.playlist.name.clone(),
        artist: data.playlist.owner.clone().unwrap_or_default(),
        cover_art: data.playlist.cover_art.clone(),
        song_ids: data.entry.iter().map(|song| song.id.clone()).collect(),
    }
}

fn fail(_: rusqlite::Error) -> LibError {
    LibError::Server("Unable to update offline downloads".into())
}

async fn collection_pages(
    lib: &Arc<Library>,
    generation: Arc<()>,
    namespace: &str,
    library: &str,
    id: &str,
    policy: Policy,
    refresh: bool,
) -> Result<usize, LibError> {
    let section = Section::Collection(id.into());
    let mut count = 0;
    for page in 0..1000 {
        let data = lib
            .shelf_browse_async(library, Media::Book, &section, page, 50, policy)
            .await?;
        if data
            .generation
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &generation))
        {
            return Err(LibError::Offline);
        }
        let books: Vec<_> = data
            .entries
            .into_iter()
            .filter_map(|entry| match entry.kind {
                EntryKind::Book { item } => Some((item, entry.title)),
                _ => None,
            })
            .collect();
        count += books.len();
        let (account, group, collection) =
            (namespace.to_string(), library.to_string(), id.to_string());
        lib.shelf_db_run(generation.clone(), move |blocking, db| {
            db.remember_collection_page(blocking, &account, &group, &collection, &books, refresh)
                .map_err(fail)
        })
        .await?;
        wake_spoken(lib);
        if !data.has_more {
            return Ok(count);
        }
    }
    Err(LibError::Server("Collection is too large to queue".into()))
}

fn wake_spoken(lib: &Library) {
    lib.shelf.read().unwrap().queue.wake.notify_one();
    lib.notify(LibEvent::Downloads);
}

fn can_refresh(lib: &Library) -> bool {
    lib.is_online_for_shelf()
        && lib.maintenance_link_ready()
        && !lib.paths.ephemeral_media
        && !lib.downloads.is_paused()
        && !lib.background_should_yield()
        && !lib.playback_transfer_running()
        && !lib.spoken_playback_active()
}

impl Drop for Background {
    fn drop(&mut self) {
        self.wake.notify_one();
    }
}

pub(super) fn start(lib: &Arc<Library>) {
    let weak = Arc::downgrade(lib);
    let wake = lib.offline_groups.wake.clone();
    wake.notify_one();
    crate::rt::io().spawn(async move {
        let mut follow = None::<Instant>;
        let mut rotate = None::<Instant>;
        let mut interval = Duration::ZERO;
        let mut recover = true;
        loop {
            if interval.is_zero() {
                wake.notified().await;
                follow = None;
            } else {
                tokio::select! {
                    () = wake.notified() => follow = None,
                    () = tokio::time::sleep(interval) => {},
                }
            }
            let Some(lib) = weak.upgrade() else { break };
            if recover {
                super::mix_space::recover_on_open(&lib).await;
                recover = false;
            }
            let enabled = lib.automatic_downloads_enabled();
            let following = lib
                .db_run(|_, db| db.any_offline_group_follows().unwrap_or(false))
                .await;
            interval = if enabled {
                Duration::from_secs(60)
            } else if following {
                Duration::from_secs(15 * 60)
            } else {
                Duration::ZERO
            };
            if interval.is_zero() || !can_refresh(&lib) {
                continue;
            }
            let Ok((namespace, generation)) = lib.spoken_download_identity().await else {
                continue;
            };
            if follow.is_none_or(|at| at.elapsed() >= Duration::from_secs(15 * 60)) {
                health::with_priority(
                    Priority::Background,
                    refresh_followed(&lib, &namespace, &generation),
                )
                .await;
                follow = Some(Instant::now());
            }
            let target = lib.settings.read().unwrap().fill_target;
            if let Some(target) = target {
                let due = rotate.is_none_or(|at| at.elapsed() >= Duration::from_secs(24 * 60 * 60));
                if health::with_priority(
                    Priority::Background,
                    fill(&lib, &namespace, &generation, target, due),
                )
                .await
                {
                    rotate = Some(Instant::now());
                }
            }
        }
    });
}

async fn refresh_followed(lib: &Arc<Library>, namespace: &str, generation: &Arc<()>) {
    let music = lib.client_generation();
    if lib.music_binding_ready(music).await.is_ok() {
        let ids = lib
            .db_run(|_, db| db.followed_playlists().unwrap_or_default())
            .await;
        for id in ids {
            if !can_refresh(lib) || lib.client_generation() != music {
                break;
            }
            let Ok(result) = lib
                .clone()
                .fetch_async(calls::playlist(&id), Policy::NetworkOnly)
                .await
            else {
                continue;
            };
            let set = playlist_set(&result.value);
            if set.id != id {
                continue;
            }
            let _gate = lib.offline_art_gate.acquire().await;
            let current = lib.client_generation.clone();
            let _ = lib
                .db_run(move |blocking, db| {
                    if current.load(Ordering::SeqCst) != music {
                        return Ok(());
                    }
                    db.follow_playlist(blocking, &set, &result.value.entry, false)
                })
                .await;
            lib.downloads.wake();
            lib.notify(LibEvent::Downloads);
        }
    }
    if namespace.is_empty() {
        return;
    }
    let account = namespace.to_owned();
    let Ok(groups) = lib
        .shelf_db_run(generation.clone(), move |_, db| {
            db.followed_collections(&account).map_err(fail)
        })
        .await
    else {
        return;
    };
    for (library, id) in groups {
        if !can_refresh(lib) {
            break;
        }
        let _ = collection_pages(
            lib,
            generation.clone(),
            namespace,
            &library,
            &id,
            Policy::NetworkOnly,
            true,
        )
        .await;
    }
}

async fn deferred_for_manual(lib: &Library) -> bool {
    let namespace = lib.shelf_namespace();
    lib.mix_space.manual_active() || lib.db_run(move |_, db| db.manual_download_waiting(&namespace).unwrap_or(true)).await
}

async fn fill(
    lib: &Arc<Library>,
    namespace: &str,
    generation: &Arc<()>,
    target: u8,
    rotate: bool,
) -> bool {
    if !can_refresh(lib) || deferred_for_manual(lib).await {
        return false;
    }
    let music = lib.client_generation();
    let ready = lib.music_binding_ready(music).await.is_ok();
    let budget = card_budget(lib, namespace, target).await;
    let Some((mut budget, above_target)) = budget else {
        return false;
    };
    let mut rotated = false;
    if (above_target || (rotate && budget < 16 << 20)) && (ready || !namespace.is_empty()) {
        let _gate = lib.offline_art_gate.acquire().await;
        let queue = lib.offline_groups.queue.lock().unwrap().clone();
        let account = namespace.to_owned();
        let generation = generation.clone();
        let current = lib.client_generation.clone();
        let owner = lib.clone();
        let result = lib
            .db_run(move |blocking, db| {
                if !can_refresh(&owner) || owner.mix_space.manual_active() || db.manual_download_waiting(&account)?
                    || owner.settings.read().unwrap().fill_target != Some(target)
                {
                    return Ok(None);
                }
                let account = if db.shelf_generation_current(&generation) {
                    account.as_str()
                } else {
                    ""
                };
                let music_ready = ready && current.load(Ordering::SeqCst) == music;
                let protected = queue
                    .iter()
                    .map(|song| {
                        song.spoken
                            .as_ref()
                            .map_or(song.id.as_str(), |book| book.id.as_str())
                    })
                    .collect();
                let before = (!above_target).then(|| super::db::now() - 7 * 24 * 60 * 60);
                db.rotate_automatic(blocking, account, before, &protected, music_ready)
            })
            .await;
        if let Ok(files) = result {
            rotated = true;
            if let Some(files) = files {
                let owner = lib.clone();
                let _ = super::downloads::lane()
                    .run_isolated(move |blocking| {
                        for path in files {
                            if (path.starts_with(owner.paths.offline())
                                || path.starts_with(owner.paths.spoken_downloads())
                                || path.starts_with(owner.paths.offline_art()))
                                && owner.paths.can_modify_media(&path)
                            {
                                let _ = owner.paths.remove_media(blocking, &path);
                            }
                        }
                    })
                    .await;
                lib.notify(LibEvent::Downloads);
            }
        }
    }
    if budget < 1 << 20 {
        return rotated;
    }
    if !namespace.is_empty() {
        for media in [Media::Book, Media::Podcast] {
            budget = budget.saturating_sub(
                fill_spoken(lib, namespace, generation, media, target, budget / 2).await,
            );
        }
    }
    if !ready || !can_refresh(lib) {
        return rotated;
    }
    fill_music(lib, namespace, music, target, budget).await;
    rotated
}

async fn card_budget(lib: &Arc<Library>, namespace: &str, target: u8) -> Option<(u64, bool)> {
    let owner = lib.clone();
    let account = namespace.to_owned();
    let (total, free) = crate::rt::spawn_blocking(move |blocking| {
        let stat = crate::platform::system::filesystem(blocking, owner.paths.media_root()).ok()?;
        Some((
            (stat.f_blocks as u64).checked_mul(stat.f_frsize as u64)?,
            (stat.f_bavail as u64).checked_mul(stat.f_frsize as u64)?,
        ))
    })
    .await
    .ok()
    .flatten()?;
    let (filled, pending) = lib
        .db_run(move |_, db| db.auto_bytes(&account))
        .await
        .ok()?;
    let reserve = lib.settings.read().unwrap().reserve_bytes;
    let goal = target_bytes(total, target);
    Some((
        capacity(goal, free, filled, pending, reserve),
        filled > goal,
    ))
}

fn target_bytes(total: u64, target: u8) -> u64 {
    (u128::from(total) * u128::from(target) / 100) as u64
}

fn capacity(goal: u64, free: u64, filled: u64, pending: u64, reserve: u64) -> u64 {
    goal.saturating_sub(filled)
        .min(free.saturating_sub(reserve))
        .saturating_sub(pending)
}

fn sized(song: Song) -> Option<(Song, u64)> {
    let size = song.size.filter(|size| *size > 0)?.checked_add(4096)?;
    Some((song, size))
}

async fn fill_music(lib: &Arc<Library>, namespace: &str, generation: u32, target: u8, budget: u64) {
    let mut budget = budget;
    let favorites = lib
        .clone()
        .fetch_async(calls::starred(), Policy::PreferCache)
        .await;
    if let Ok(favorites) = favorites {
        let mut favorites = favorites.value;
        fastrand::shuffle(&mut favorites.song);
        let tracks = favorites
            .song
            .into_iter()
            .filter_map(sized)
            .take(20)
            .collect();
        let used =
            queue_automatic_music(lib, namespace, generation, target, None, tracks, budget / 3)
                .await;
        budget = budget.saturating_sub(used);
    }
    if !can_refresh(lib) {
        return;
    }
    for list_type in [AlbumListType::Random, AlbumListType::Starred] {
        let Ok(result) = lib
            .clone()
            .fetch_async(calls::album_list(list_type, 4, 0), Policy::NetworkOnly)
            .await
        else {
            continue;
        };
        for album in result.value.album {
            if !can_refresh(lib) || lib.client_generation() != generation {
                return;
            }
            let Ok(fetched) = lib
                .clone()
                .fetch_async(calls::album(&album.id), Policy::PreferCache)
                .await
            else {
                continue;
            };
            if fetched.value.album.id != album.id {
                continue;
            }
            let Some(tracks) = fetched.value.song.into_iter().map(sized).collect() else {
                continue;
            };
            if queue_automatic_music(
                lib,
                namespace,
                generation,
                target,
                Some(album.id),
                tracks,
                budget,
            )
            .await
                != 0
            {
                return;
            }
        }
    }
}

async fn queue_automatic_music(
    lib: &Arc<Library>,
    namespace: &str,
    generation: u32,
    target: u8,
    album: Option<String>,
    tracks: Vec<(Song, u64)>,
    budget: u64,
) -> u64 {
    if tracks.is_empty() {
        return 0;
    }
    let _gate = lib.offline_art_gate.acquire().await;
    let Some((fresh, _)) = card_budget(lib, namespace, target).await else {
        return 0;
    };
    let budget = budget.min(fresh);
    let current = lib.client_generation.clone();
    let owner = lib.clone();
    let account = namespace.to_owned();
    // Keep a conservative budget for any duplicates in this batch. Exact
    // downloaded and pending sizes are read again before the next round.
    let size = tracks
        .iter()
        .fold(0u64, |sum, (_, bytes)| sum.saturating_add(*bytes))
        .min(budget);
    if lib
        .db_run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != generation
                || !can_refresh(&owner) || owner.mix_space.manual_active() || db.manual_download_waiting(&account)?
                || owner.settings.read().unwrap().fill_target != Some(target)
            {
                return Ok(0);
            }
            db.auto_music(blocking, album.as_deref(), &tracks, budget)
        })
        .await
        .unwrap_or(0)
        == 0
    {
        return 0;
    }
    lib.downloads.wake();
    lib.notify(LibEvent::Downloads);
    size
}

async fn fill_spoken(
    lib: &Arc<Library>,
    namespace: &str,
    generation: &Arc<()>,
    media: Media,
    target: u8,
    budget: u64,
) -> u64 {
    if budget == 0 || !can_refresh(lib) {
        return 0;
    }
    let Ok(libraries) = lib.shelf_libraries_async(media, Policy::PreferCache).await else {
        return 0;
    };
    let Some(library) = fastrand::choice(libraries.iter()) else {
        return 0;
    };
    let section = if media == Media::Book {
        Section::Books
    } else {
        Section::LatestEpisodes
    };
    let Ok(first) = lib
        .shelf_browse_async(&library.id, media, &section, 0, 50, Policy::PreferCache)
        .await
    else {
        return 0;
    };
    let mut entries = first.entries;
    if let Some(total) = first.total.filter(|total| *total > 50) {
        let page = fastrand::u64(0..total.div_ceil(50)).min(u32::MAX as u64) as u32;
        if page > 0 {
            if let Ok(data) = lib
                .shelf_browse_async(&library.id, media, &section, page, 50, Policy::PreferCache)
                .await
            {
                entries = data.entries;
            }
        }
    }
    fastrand::shuffle(&mut entries);
    let Ok(client) = lib.shelf_client_async().await else {
        return 0;
    };
    for entry in entries.into_iter().take(3) {
        let (item, episode) = match entry.kind {
            EntryKind::Book { item } => (item, None),
            EntryKind::Episode { item, episode } => (item, Some(episode)),
            _ => continue,
        };
        let Ok(info) = client.item_async(&item, episode.as_deref()).await else {
            continue;
        };
        let Some(size) =
            spoken_size(&info, episode.as_deref()).filter(|size| *size > 0 && *size <= budget)
        else {
            continue;
        };
        let _gate = lib.offline_art_gate.acquire().await;
        let Some((fresh, _)) = card_budget(lib, namespace, target).await else {
            return 0;
        };
        if size > fresh {
            return 0;
        }
        let account = namespace.to_owned();
        let owner = lib.clone();
        let Ok(added) = lib
            .shelf_db_run(generation.clone(), move |blocking, db| {
                if !can_refresh(&owner) || owner.mix_space.manual_active() || db.manual_download_waiting(&account).map_err(fail)?
                    || owner.settings.read().unwrap().fill_target != Some(target)
                {
                    return Ok(false);
                }
                db.auto_spoken(
                    blocking,
                    &account,
                    &item,
                    episode.as_deref(),
                    &entry.title,
                    size,
                )
                .map_err(fail)
            })
            .await
        else {
            return 0;
        };
        if added {
            wake_spoken(lib);
            return size;
        }
    }
    0
}

fn spoken_size(info: &serde_json::Value, episode: Option<&str>) -> Option<u64> {
    if let Some(id) = episode {
        let row = info
            .get("media")?
            .get("episodes")?
            .as_array()?
            .iter()
            .find(|row| row["id"].as_str() == Some(id))?;
        return row["audioFile"]["metadata"]["size"]
            .as_u64()
            .or_else(|| row["audioFile"]["size"].as_u64())
            .and_then(|size| size.checked_add(4096));
    }
    let size = info["media"]["size"].as_u64().filter(|size| *size > 0)?;
    let overhead = info["media"]["numAudioFiles"]
        .as_u64()
        .unwrap_or(1)
        .checked_mul(4096)?;
    size.checked_add(overhead)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_counts_only_rotating_bytes_and_honors_free_reserve() {
        let goal = target_bytes(1000, 50);
        assert_eq!(capacity(goal, 800, 100, 50, 100), 350);
        assert_eq!(capacity(goal, 200, 100, 50, 100), 50);
        assert_eq!(capacity(goal, 70, 100, 50, 100), 0);
        assert_eq!(capacity(goal, 800, 501, 0, 100), 0);
    }
}
