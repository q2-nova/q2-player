//! Account-bound asynchronous access to the catalog's existing database owner.
//! A refresh never holds that worker while waiting for the network.
use super::{Library, LibError, db::catalog};
use std::{future::Future, sync::Arc, time::{Duration, Instant}};
use subsonic::{Album, AlbumListType, calls};

/// Builds per browse when a scan ends during one: the second is published
/// either way, unmarked if it too saw a scan.
const CATALOG_ATTEMPTS: u32 = 2;

#[derive(Clone)]
pub(crate) struct Catalog {
    generation: u32,
    stored: catalog::Catalog,
}
impl Catalog {
    pub(crate) fn len(&self) -> u32 { self.stored.len }
    pub(crate) fn groups(&self) -> u32 { self.stored.groups }
    pub(crate) fn generation(&self) -> u32 { self.generation }
}

fn storage_error(error: rusqlite::Error) -> LibError {
    let message = match error {
        rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DiskFull => "Album catalog storage is full",
        rusqlite::Error::InvalidQuery => "Album catalog changed or repeated; try again",
        _ => "Unable to read or store album catalog",
    };
    LibError::Server(message.into())
}

impl Library {
    pub(crate) fn current_album_catalog(&self) -> impl Future<Output=Result<Option<Catalog>, LibError>> + Send + 'static + use<> {
        let generation = self.client_generation();
        self.music_db_run(generation, move |blocking, db| {
            db.catalog_current(blocking).map(|value| value.map(|stored| Catalog { generation, stored })).map_err(storage_error)
        })
    }

    /// Capture the account at admission, before the caller can defer polling.
    /// Each network page and each DB job enforce that same generation. Readers
    /// of completed catalogs do not acquire the refresh gate.
    pub(crate) fn refresh_album_catalog(self: &Arc<Self>, group: fn(&str) -> String)
        -> impl Future<Output=Result<Catalog, LibError>> + Send + 'static + use<> {
        self.load_album_catalog(group, true)
    }

    /// Reopening a browser need not rebuild a recently fetched catalog. An open
    /// browser pins its returned version for its whole browsing session.
    pub(crate) fn browse_album_catalog(self: &Arc<Self>, group: fn(&str) -> String)
        -> impl Future<Output=Result<Catalog, LibError>> + Send + 'static + use<> {
        self.load_album_catalog(group, false)
    }

    fn load_album_catalog(self: &Arc<Self>, group: fn(&str) -> String, force: bool)
        -> impl Future<Output=Result<Catalog, LibError>> + Send + 'static + use<> {
        let generation = self.client_generation();
        let lib = self.clone();
        async move {
            let mut refresh = lib.catalog_refresh.lock().await;
            if !force && refresh.is_some_and(|(owner, at)| owner == generation && at.elapsed() < Duration::from_secs(300)) {
                if let Some(stored) = lib.music_db_run(generation, |blocking, db| db.catalog_current(blocking).map_err(storage_error)).await? {
                    return Ok(Catalog { generation, stored });
                }
            }
            // The server's scan marker: a catalog built under the same marker
            // holds the server's albums, so an unchanged library costs one
            // small request and no rows (before: every page, every row).
            let mut before = lib.scan_marker(generation).await;
            if !force {
                if let Some(marker) = before.clone() {
                    let current = lib.music_db_run(generation, move |blocking, db| db.catalog_current_marked(blocking, &marker).map_err(storage_error)).await?;
                    if let Some(stored) = current {
                        *refresh = Some((generation, Instant::now()));
                        return Ok(Catalog { generation, stored });
                    }
                }
            }
            let mut attempt = 0;
            let result = loop {
                attempt += 1;
                let build = lib.build_album_catalog(generation, group).await?;
                // Offsets shift when a scan adds or removes albums between
                // pages: an album is then missed or seen twice. A marker that
                // held across the build proves the pages are one snapshot.
                let after = lib.scan_marker(generation).await;
                let consistent = before.is_some() && before == after;
                match build {
                    Some(build) if consistent || attempt == CATALOG_ATTEMPTS || (before.is_none() && after.is_none()) => {
                        let marker = after.filter(|_| consistent);
                        break lib.music_db_run(generation, move |blocking, db| {
                            db.catalog_publish_marked(blocking, &build, marker.as_deref()).map(|stored| Catalog { generation, stored }).map_err(storage_error)
                        }).await?;
                    }
                    // A rejected append can result from a shifted page. Retry
                    // only when both scan markers show an actual change.
                    None if attempt == CATALOG_ATTEMPTS || before.is_none() || after.is_none() || consistent => {
                        return Err(storage_error(rusqlite::Error::InvalidQuery));
                    }
                    _ => {}
                }
                log::info!("album catalog: the server scanned during the build; building again");
                before = after;
            };
            *refresh = Some((generation, Instant::now()));
            Ok(result)
        }
    }

    /// Every alphabetical page into a new unpublished build.
    /// None means the database rejected a page's contents or sequence.
    async fn build_album_catalog(&self, generation: u32, group: fn(&str) -> String) -> Result<Option<catalog::Build>, LibError> {
        let build = self.music_db_run(generation, |blocking, db| db.catalog_begin(blocking).map_err(storage_error)).await?;
        let mut offset = 0;
        loop {
            let call = calls::album_list(AlbumListType::AlphabeticalByName, catalog::BATCH_LIMIT as u32, offset);
            // Store the normalized catalog once, not another complete copy
            // of every raw page in the ordinary response cache as well.
            // Each page is parsed as it arrives; only its 500 typed
            // albums are held until the database has them.
            self.shelf_auto.catalog_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (page, bytes) = self.fetch_net_call(&call, generation, true).await?;
            let albums = page.album;
            let finished = albums.len() < catalog::BATCH_LIMIT;
            let albums = albums.into_iter().map(|album| { let label = group(&album.name); (album, label) }).collect::<Vec<_>>();
            let writer = build.clone();
            let appended = self.music_db_run(generation, move |blocking, db| {
                match db.catalog_append(blocking, &writer, offset, &albums) {
                    Ok(next) => Ok(Some(next)),
                    Err(rusqlite::Error::InvalidQuery) => Ok(None),
                    Err(error) => Err(storage_error(error)),
                }
            }).await?;
            // The batch is in the database: return its freed heap.
            crate::platform::resident::trim_after_document(bytes);
            let Some(next) = appended else { return Ok(None); };
            offset = next;
            if finished { return Ok(Some(build)); }
        }
    }

    /// The server's scan marker (`getScanStatus`), recorded so stored
    /// responses from before a scan stop counting as fresh. `None` while a
    /// scan runs, when the server reports no scan time, or on failure.
    pub(super) async fn scan_marker(&self, generation: u32) -> Option<String> {
        self.shelf_auto.catalog_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (status, _) = self.fetch_net_call(&calls::scan_status(), generation, true).await.ok()?;
        let marker = status.marker()?;
        let seen = marker.clone();
        let _ = self.music_db_run(generation, move |blocking, db| db.observe_scan_marker(blocking, &seen).map_err(storage_error)).await;
        Some(marker)
    }

    pub(crate) fn album_catalog_page(&self, catalog: Catalog, start: u32, count: u32)
        -> impl Future<Output=Result<Vec<Album>, LibError>> + Send + 'static + use<> {
        self.music_db_run(catalog.generation, move |blocking, db| {
            db.catalog_page(blocking, &catalog.stored, start, count).map_err(storage_error)
        })
    }

    pub(crate) fn album_catalog_jump(&self, catalog: Catalog, position: u32, delta: i32)
        -> impl Future<Output=Result<Option<catalog::Group>, LibError>> + Send + 'static + use<> {
        self.music_db_run(catalog.generation, move |blocking, db| {
            db.catalog_jump(blocking, &catalog.stored, position, delta).map_err(storage_error)
        })
    }


}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod sync_tests;
#[cfg(test)]
mod owner_measure;
