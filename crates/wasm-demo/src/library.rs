//! In-memory library backed by the browser demo’s licensed music catalog.
use crate::gfx::Image;
use crate::{Album, AlbumList, AlbumListType, Artist, ItemKind, Song};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub mod art {
    pub const NOW_PLAYING_SIZE: u32 = 150;
    pub const THUMBNAIL_SIZE: u32 = 50;
    pub const MEMORY_AFTER_BROWSING: usize = 0;
}

pub mod spoken {
    pub struct Availability {
        pub books: Option<bool>,
        pub podcasts: Option<bool>,
    }
}

pub struct OfflineSet {
    pub cover_art: Option<String>,
}

pub enum Policy {
    CacheOnly,
    NetworkFirst,
    PreferCache,
}

pub enum Priority {
    Low,
}

pub async fn with_priority<T>(_priority: Priority, result: impl Future<Output = T>) -> T {
    result.await
}

pub enum LibEvent {
    ArtReady,
    Downloads,
    Online(bool),
}

pub type LibError = crate::Error;

type ArtCache = HashMap<(String, u32), Arc<Image>>;

#[derive(Default)]
pub struct Art {
    covers: RefCell<ArtCache>,
}

impl Art {
    pub fn get(&self, id: &str, size: u32) -> Option<Arc<Image>> {
        Some(
            self.covers
                .borrow_mut()
                .entry((id.to_owned(), size))
                .or_insert_with(|| Arc::new(crate::artwork::cover_image(id, size)))
                .clone(),
        )
    }

    pub fn get_artist(&self, artist: &Artist, size: u32) -> Option<Arc<Image>> {
        self.get(artist.cover_art.as_deref()?, size)
    }

    pub fn peek_cover(&self, id: &str, size: u32) -> Option<Arc<Image>> {
        self.get(id, size)
    }

    pub fn prefetch_cover(&self, id: &str, size: u32) {
        self.get(id, size);
    }

    pub fn frame_covers<const N: usize>(&self, ids: [Option<&str>; N], size: u32) -> [Option<Arc<Image>>; N] {
        ids.map(|id| id.and_then(|id| self.get(id, size)))
    }

    pub fn warm_window<'a>(&self, ids: impl Iterator<Item = &'a str>, size: u32) {
        for id in ids { self.get(id, size); }
    }

    // Bundled images are decoded synchronously, so there is no pending queue to cancel.
    pub fn close_window(&self) {}

    pub fn shrink_memory(&self, _bytes: usize) {
        self.covers
            .borrow_mut()
            .retain(|_, img| Arc::strong_count(img) > 1);
    }
}

pub struct Fetched<T> {
    pub value: T,
}

pub struct Library {
    pub albums: Vec<Album>,
    pub tracks: Arc<Vec<Song>>,
    pub art: Art,
    history: RefCell<Vec<String>>,
    saved_albums: RefCell<HashSet<String>>,
    loved: RefCell<HashMap<String, bool>>,
    ratings: RefCell<HashMap<String, u8>>,
}

impl Library {
    pub fn new() -> Self {
        let mut albums = Vec::new();
        let mut tracks = Vec::new();
        let catalog: HashMap<String, CatalogTrack> =
            serde_json::from_str(include_str!("../../../web-demo/catalog/music.json"))
                .expect("the bundled music catalog must be valid");
        for (id, rating) in ALBUMS {
            let mut album_tracks: Vec<_> = catalog
                .iter()
                .filter_map(|(key, track)| {
                    let (album_id, position) = key.rsplit_once('-')?;
                    (album_id == id).then(|| {
                        let position = position.parse::<u32>().expect("numeric track position");
                        (position, key, track)
                    })
                })
                .collect();
            album_tracks.sort_unstable_by_key(|(position, _, _)| *position);
            let album = album_tracks.first().expect("each demo album has tracks").2;
            albums.push(Album {
                id: id.into(),
                name: album.album.clone(),
                artist: album.artist.clone(),
                artist_id: Some(album.artist.clone()),
                cover_art: Some(id.into()),
                song_count: album_tracks.len() as u32,
                year: Some(album.year),
                genre: Some(album.genre.clone()),
                duration: album_tracks
                    .iter()
                    .map(|(_, _, track)| track.duration)
                    .sum(),
                created: Some(format!("2026-10-{:02}", albums.len() + 1)),
                starred: matches!(id, "tidal" | "copper").then(|| "demo".into()),
                user_rating: Some(rating),
                ..Default::default()
            });
            tracks.extend(album_tracks.into_iter().map(|(position, key, track)| {
                Song {
                    id: key.clone(),
                    title: track.title.clone(),
                    album: track.album.clone(),
                    artist: track.artist.clone(),
                    track: Some(position),
                    year: Some(track.year),
                    genre: Some(track.genre.clone()),
                    cover_art: Some(id.into()),
                    duration: track.duration,
                    suffix: Some(
                        track
                            .url
                            .rsplit_once('.')
                            .expect("media URL has an extension")
                            .1
                            .into(),
                    ),
                    sampling_rate: Some(track.sample_rate),
                    bit_rate: Some(track.bit_rate),
                    album_id: Some(id.into()),
                    artist_id: Some(track.artist.clone()),
                    starred: (position == 2).then(|| "demo".to_owned()),
                    ..Default::default()
                }
            }));
        }
        let history = [
            "copper",
            "sundial",
            "copper",
            "tidal",
            "sundial",
            "northline",
        ];
        Self {
            albums,
            tracks: Arc::new(tracks),
            art: Art::default(),
            history: RefCell::new(history.into_iter().map(str::to_owned).collect()),
            saved_albums: RefCell::new(HashSet::from(["tidal".to_owned()])),
            loved: RefCell::default(),
            ratings: RefCell::default(),
        }
    }

    pub fn is_online(&self) -> bool {
        true
    }
    pub fn is_configured(&self) -> bool {
        true
    }

    pub async fn spoken_availability_async(&self) -> spoken::Availability {
        spoken::Availability {
            books: Some(true),
            podcasts: Some(true),
        }
    }

    pub fn album_downloaded(&self, id: &str) -> bool {
        self.saved_albums.borrow().contains(id)
    }

    pub fn set_downloaded(&self, id: &str, downloaded: bool) {
        if downloaded {
            self.saved_albums.borrow_mut().insert(id.into());
        } else {
            self.saved_albums.borrow_mut().remove(id);
        }
    }

    pub fn record_play(&self, song: &Song) {
        if let Some(id) = song.album_id.as_ref() {
            let mut played = self.history.borrow_mut();
            if played.len() == 256 {
                played.remove(0);
            }
            played.push(id.clone());
        }
    }

    pub fn ordered_albums(&self, kind: AlbumListType) -> Vec<&Album> {
        use std::cmp::Reverse;
        let played = self.history.borrow();
        let mut albums: Vec<_> = self.albums.iter().collect();
        match kind {
            AlbumListType::AlphabeticalByName => albums.sort_by_key(|a| &a.name),
            AlbumListType::AlphabeticalByArtist => albums.sort_by_key(|a| (&a.artist, &a.name)),
            AlbumListType::Newest => albums.sort_by_key(|a| Reverse(&a.created)),
            AlbumListType::Recent | AlbumListType::Frequent => {
                albums.retain(|album| played.contains(&album.id));
                if matches!(kind, AlbumListType::Frequent) {
                    albums
                        .sort_by_key(|a| Reverse(played.iter().filter(|id| *id == &a.id).count()));
                } else {
                    albums.sort_by_key(|a| Reverse(played.iter().rposition(|id| id == &a.id)));
                }
            }
            AlbumListType::Highest => albums.sort_by_key(|a| Reverse(a.user_rating)),
            AlbumListType::Starred => albums.retain(|a| a.starred.is_some()),
            // A seeded permutation keeps this fixture repeatable across demos.
            AlbumListType::Random => albums.sort_by_key(|a| {
                a.id.bytes()
                    .fold(17u32, |n, b| n.wrapping_mul(31) ^ u32::from(b))
            }),
            _ => {}
        }
        albums
    }

    pub async fn fetch_async(
        self: Arc<Self>,
        request: crate::calls::AlbumRequest,
        _: Policy,
    ) -> Result<Fetched<AlbumList>, LibError> {
        let album = self
            .ordered_albums(request.kind)
            .into_iter()
            .skip(request.offset as usize)
            .take(request.size as usize)
            .cloned()
            .collect();
        Ok(Fetched {
            value: AlbumList { album },
        })
    }

    pub async fn offline_sets_async(&self) -> Result<Vec<OfflineSet>, LibError> {
        Ok(Vec::new())
    }

    pub fn loved(&self, song: &Song) -> bool {
        self.loved
            .borrow()
            .get(&song.id)
            .copied()
            .unwrap_or(song.starred.is_some())
    }

    pub fn rating(&self, song: &Song) -> u8 {
        self.ratings
            .borrow()
            .get(&song.id)
            .copied()
            .unwrap_or(song.user_rating.unwrap_or_default())
    }

    pub fn star(&self, _: ItemKind, id: &str, on: bool) {
        self.loved.borrow_mut().insert(id.into(), on);
    }

    pub fn rate(&self, id: &str, rating: u8) {
        self.ratings.borrow_mut().insert(id.into(), rating);
    }
}

// Stable album keys also select the bundled generic cover and offline preview.
const ALBUMS: [(&str, u8); 4] = [
    ("tidal", 5),
    ("northline", 4),
    ("sundial", 3),
    ("copper", 4),
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogTrack {
    title: String,
    album: String,
    artist: String,
    year: i32,
    genre: String,
    duration: u32,
    url: String,
    sample_rate: u32,
    bit_rate: u32,
}
