//! Full Sync compares saved music with the authenticated server. A changed
//! representation uses an ordinary download ticket, leaving the old file
//! playable until the replacement and its metadata commit together.
use super::{LibError, LibEvent, Library, NetStart, Parsed};
use std::{path::PathBuf, sync::Arc};
use subsonic::Song;

#[derive(Clone)]
pub(crate) struct MissingSong {
    pub song: Song,
    pub path: PathBuf,
}

pub(crate) struct Review {
    pub generation: u32,
    pub upgrades: usize,
    pub missing: Vec<MissingSong>,
}

pub(crate) fn media_changed(old: &Song, new: &Song) -> bool {
    old.size != new.size
        || old.suffix != new.suffix
        || old.content_type != new.content_type
        || old.bit_rate != new.bit_rate
        || old.bit_depth != new.bit_depth
        || old.sampling_rate != new.sampling_rate
        || old.duration != new.duration
}

fn storage(_: rusqlite::Error) -> LibError {
    LibError::Server("Unable to update downloads".into())
}

impl Library {
    async fn server_song(&self, generation: u32, id: String) -> Result<Option<Song>, LibError> {
        let call = subsonic::calls::song(&id);
        let start: NetStart = Box::new(move |_, client| {
            let response = client.document_async(&call);
            Box::pin(async move {
                let (song, bytes) = match response.await {
                    Ok((song, bytes)) if song.id == id => (Some(song), bytes),
                    Ok(_) => return Err(subsonic::Error::Json("Song identity changed".into())),
                    Err(subsonic::Error::Api { code: 70, .. }) => (None, 0),
                    Err(error) => return Err(error),
                };
                Ok((Box::new(song) as Parsed, bytes))
            })
        });
        let (value, _) = self.fetch_net_parsed(start, generation, true).await?;
        Ok(*value.downcast::<Option<Song>>().expect("getSong result"))
    }

    async fn deletion_scan_marker(&self, generation: u32) -> Result<Option<String>, LibError> {
        let (status, _) = self
            .fetch_net_call(&subsonic::calls::scan_status(), generation, false)
            .await?;
        if status.scanning {
            return Err(LibError::Server(
                "Library scan in progress. Try again when it finishes.".into(),
            ));
        }
        Ok(status.marker())
    }

    pub(crate) async fn review_downloads(
        &self,
        group: fn(&str) -> String,
        cancelled: &std::sync::atomic::AtomicBool,
        progress: impl Fn(usize),
    ) -> Result<Review, LibError> {
        let generation = self.client_generation();
        let lib = self.me.upgrade().ok_or(LibError::Offline)?;
        lib.refresh_album_catalog(group).await?;
        let mut review = Review {
            generation,
            upgrades: 0,
            missing: Vec::new(),
        };
        let mut after = String::new();
        let mut checked = 0;
        loop {
            let page = self
                .music_db_run(generation, move |blocking, db| {
                    db.download_page(blocking, &after).map_err(storage)
                })
                .await?;
            if page.is_empty() {
                return Ok(review);
            }
            after = page.last().unwrap().song.id.clone();
            for local in page {
                if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(LibError::Offline);
                }
                match self.server_song(generation, local.song.id.clone()).await? {
                    None => review.missing.push(local),
                    Some(new) if media_changed(&local.song, &new) => {
                        let queued = self
                            .music_db_run(generation, move |blocking, db| {
                                db.queue_upgrade(blocking, &local, &new).map_err(storage)
                            })
                            .await?;
                        if queued {
                            review.upgrades += 1;
                            self.downloads.wake();
                            self.notify(LibEvent::Downloads);
                        }
                    }
                    Some(_) => {}
                }
                checked += 1;
                if checked % 10 == 0 {
                    progress(checked);
                }
            }
        }
    }

    /// Eager owner: once the deletion transaction commits, navigating Back
    /// cannot cancel retirement of the indexed files returned by it.
    pub(crate) fn confirm_missing(
        self: &Arc<Self>,
        review: Arc<Review>,
    ) -> impl std::future::Future<Output = Result<usize, LibError>> + Send + 'static {
        let lib = self.clone();
        let task = crate::rt::io().spawn(async move {
            let before = lib.deletion_scan_marker(review.generation).await?;
            let mut missing = Vec::new();
            for saved in &review.missing {
                if lib
                    .server_song(review.generation, saved.song.id.clone())
                    .await?
                    .is_none()
                {
                    missing.push(saved.clone());
                }
            }
            if before != lib.deletion_scan_marker(review.generation).await? {
                return Err(LibError::Server(
                    "Library changed. Run Full Sync again.".into(),
                ));
            }
            let _gate = lib.offline_art_gate.acquire().await;
            let current = lib.client_generation.clone();
            let generation = review.generation;
            let (count, files) = lib
                .db_run(move |blocking, db| {
                    if current.load(std::sync::atomic::Ordering::SeqCst) != generation {
                        return Err(LibError::Offline);
                    }
                    db.remove_confirmed_music(blocking, &missing)
                        .map_err(storage)
                })
                .await?;
            lib.downloads.invalidate_revision();
            lib.downloads.interrupt();
            let owner = lib.clone();
            let removed = super::downloads::lane()
                .run_isolated(move |blocking| {
                    let mut ok = true;
                    for path in files {
                        if owner.paths.can_modify_media(&path) {
                            ok &= owner.paths.remove_media(blocking, &path).is_ok();
                        }
                    }
                    ok
                })
                .await
                .unwrap_or(false);
            lib.notify(LibEvent::Downloads);
            if !removed {
                return Err(LibError::Server(
                    "Unable to remove files from the card".into(),
                ));
            }
            Ok(count)
        });
        async move {
            task.await
                .map_err(|_| LibError::Server("Download worker stopped".into()))?
        }
    }
}
