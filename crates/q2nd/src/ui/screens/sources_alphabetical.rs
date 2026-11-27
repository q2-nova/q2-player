//! Bounded album browser backed by one immutable, account-bound catalog.
use super::album_rows;
use super::super::menu::{WindowPage, on_pool};
use crate::library::{LibError, Library, Policy, music_catalog::Catalog};
use crate::ui::list::{Row, RowAction, index_letter};
use std::sync::Arc;

pub(super) struct AlphabeticalAlbums {
    pinned: tokio::sync::Mutex<Option<Catalog>>,
}
impl AlphabeticalAlbums {
    pub(super) fn new() -> Self { Self { pinned: tokio::sync::Mutex::new(None) } }

    async fn catalog(&self, lib: &Arc<Library>, policy: Policy) -> Result<(Catalog, bool), LibError> {
        let generation = lib.client_generation();
        let mut pinned = self.pinned.lock().await;
        if pinned.as_ref().is_some_and(|catalog| catalog.generation() != generation) { *pinned = None; }
        if let Some(catalog) = pinned.as_ref() { return Ok((catalog.clone(), true)); }
        let catalog = if policy == Policy::CacheOnly {
            match lib.current_album_catalog().await? {
                Some(catalog) => catalog,
                None => return Err(on_pool(lib.clone(), |lib| if lib.is_configured() { LibError::Offline } else { LibError::NotConfigured }).await),
            }
        } else {
            match lib.browse_album_catalog(index_letter).await {
                Ok(catalog) => catalog,
                Err(error) => match lib.current_album_catalog().await? { Some(catalog) => catalog, None => return Err(error) },
            }
        };
        // Online cache-only is only a preview; let the final network response
        // establish the session's stable version.
        let stable = policy != Policy::CacheOnly || lib.offline_mode();
        if stable { *pinned = Some(catalog.clone()); }
        Ok((catalog, stable))
    }

    pub(super) async fn page(&self, lib: Arc<Library>, page: u32, policy: Policy, shuffle_cmd: u32) -> Result<WindowPage, LibError> {
        let (catalog, stable) = self.catalog(&lib, policy).await?;
        let letters = stable && catalog.len() >= crate::ui::list::LETTER_MIN_ROWS && catalog.groups() > 1;
        let total = catalog.len().checked_add(u32::from(catalog.len() != 0))
            .ok_or_else(|| LibError::Server("Album catalog exceeds supported range".into()))?;
        let start = page.checked_mul(100).ok_or_else(|| LibError::Server("Album offset exceeds supported range".into()))?;
        let leading = u32::from(start == 0 && total != 0);
        let album_start = start.saturating_sub(u32::from(total != 0));
        let albums = lib.album_catalog_page(catalog, album_start, 100 - leading).await?;
        let mut rows = Vec::with_capacity(albums.len() + leading as usize);
        if leading != 0 { rows.push(Row::new("Shuffle Albums", RowAction::Cmd(shuffle_cmd)).key("#shuffle-albums")); }
        rows.extend(album_rows(lib, albums, true).await?);
        let more = start.saturating_add(rows.len() as u32) < total;
        Ok(WindowPage { rows, more, letters })
    }

    pub(super) async fn jump(&self, lib: Arc<Library>, position: u32, delta: i32) -> Result<Option<(u32, String)>, LibError> {
        // A jump can only be initiated from a displayed, pinned page.
        let generation = lib.client_generation();
        let pinned = self.pinned.lock().await;
        let catalog = pinned.as_ref().filter(|c| c.generation() == generation).cloned().ok_or(LibError::Offline)?;
        drop(pinned);
        Ok(lib.album_catalog_jump(catalog, position.saturating_sub(1), delta).await?
            .map(|group| (group.position + 1, group.label)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::{Config, Paths}, rt::Blocking, ui::screens::{menu::Source, sources::Albums}};
    use subsonic::{Album, AlbumListType};

    #[test]
    fn small_and_single_letter_catalogs_keep_precise_album_selection() {
        use crate::ui::list::ListView;
        use std::time::{Duration, Instant};

        for (count, split) in [(99, Some(50)), (1000, None)] {
            let config = Config::default();
            let lib = Library::open(Paths::new(crate::library::generated_test_directory("alphabet-precision")),
                &config, Arc::new(|_| {}));
            lib.set_offline_mode(true);
            let blocking = Blocking::dedicated_thread();
            {
                let mut db = lib.db();
                let build = db.catalog_begin(&blocking).unwrap();
                for offset in (0..=count).step_by(500) {
                    let rows: Vec<_> = (offset..(offset + 500).min(count)).map(|i| {
                        let letter = if split.is_some_and(|split| i >= split) { "B" } else { "A" };
                        (Album { id: format!("id-{i}"), name: format!("{letter} album {i}"),
                            ..Default::default() }, letter.into())
                    }).collect();
                    db.catalog_append(&blocking, &build, offset, &rows).unwrap();
                }
                db.catalog_publish(&blocking, &build).unwrap();
            }
            let source = Arc::new(Albums::new(AlbumListType::AlphabeticalByName, "Albums".into()));
            let runtime = crate::rt::Ui::new().unwrap();
            let page = runtime.block_on(source.clone().window_page(lib, 0, Policy::CacheOnly)).unwrap();
            assert!(!page.letters);
            let mut list = ListView::new(true);
            list.alphabetical = source.alphabetical();
            list.global_alphabet = source.global_alphabet();
            list.set_global_letters(page.letters);
            list.set_rows(page.rows);
            let start = Instant::now();
            for detent in 0..20 {
                list.wheel(1, start + Duration::from_millis(detent * 50));
            }
            assert_eq!(list.selected, 20);
            assert_eq!(list.take_alphabet_jump(), 0);
            assert!(!list.finish_letters(), "Center can act on the selected album");
        }
    }

    #[test]
    fn actual_source_browses_past_twenty_thousand_and_jumps_across_evicted_windows() {
        let mut config = Config::default(); config.server.url = "http://127.0.0.1:9".into();
        config.server.user = "generated-alphabet".into(); config.server.password = "synthetic-only".into();
        let lib = Library::open(Paths::new(crate::library::generated_test_directory("alphabet-source")), &config, Arc::new(|_| {}));
        lib.set_offline_mode(true);
        let b = Blocking::dedicated_thread();
        { let mut db = lib.db(); let build = db.catalog_begin(&b).unwrap();
          for offset in (0..20_500).step_by(500) {
              let rows: Vec<_> = (offset..offset+500).map(|i| {
                  let label = if i < 1000 { "A" } else if i < 19_000 { "M" } else { "Z" };
                  (Album { id: format!("id-{i}"), name: format!("{label} album {i}"), ..Default::default() }, label.into())
              }).collect();
              db.catalog_append(&b, &build, offset, &rows).unwrap();
          }
          db.catalog_append(&b, &build, 20_500, &[]).unwrap(); db.catalog_publish(&b, &build).unwrap();
        }
        let source = Arc::new(Albums::new(AlbumListType::AlphabeticalByName, "Albums".into()));
        assert!(source.windowed() && source.global_alphabet());
        let rt = crate::rt::Ui::new().unwrap();
        let first = rt.block_on(source.clone().window_page(lib.clone(), 0, Policy::CacheOnly)).unwrap();
        assert_eq!(first.rows.len(), 100); assert_eq!(first.rows[0].title, "Shuffle Albums"); assert_eq!(first.rows[99].key, "id-98");
        let adjacent = rt.block_on(source.clone().window_page(lib.clone(), 1, Policy::CacheOnly)).unwrap();
        assert_eq!(adjacent.rows[0].key, "id-99");
        assert_eq!(adjacent.rows[99].key, "id-198");
        let last = rt.block_on(source.clone().window_page(lib.clone(), 205, Policy::CacheOnly)).unwrap();
        assert!(last.letters, "a small retained window uses the full catalog metadata");
        assert_eq!(last.rows.len(), 1); assert_eq!(last.rows[0].key, "id-20499"); assert!(!last.more);
        let jump = rt.block_on(source.clone().alphabet_jump(lib.clone(), 1, 2)).unwrap().unwrap();
        assert_eq!(jump, (19_001, "Z".into()));
        let target = rt.block_on(source.clone().window_page(lib.clone(), jump.0 / 100, Policy::CacheOnly)).unwrap();
        assert_eq!(target.rows[(jump.0 % 100) as usize].key, "id-19000");
        let back = rt.block_on(source.clone().alphabet_jump(lib.clone(), jump.0, -2)).unwrap().unwrap();
        assert_eq!(back, (1, "A".into()));
        config.server.user = "another-generated-account".into(); lib.configure(&config);
        assert!(rt.block_on(source.alphabet_jump(lib, 1, 1)).is_err());
    }
}
