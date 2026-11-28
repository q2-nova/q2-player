#![forbid(unsafe_code)]
//! Music browsing from the card. While Navidrome can't be used (Offline Mode,
//! a server that is unavailable or not yet reached), Artists, an artist,
//! Albums, an album and Search list the artists, albums and songs of the
//! completed downloads (MUSIC-003). The card database is the source, so a
//! cold start offline needs no stored server response or album catalog.
use super::{LibError, Library, Policy, db::OfflineSet};
use std::{collections::HashMap, sync::Arc};
use subsonic::{Album, AlbumWithSongs, Artist, ArtistIndex, ArtistWithAlbums, ArtistsIndex, Call, SearchResult, Song, Streamed};

/// Completed downloads: every song on the card and the download sets that
/// name and order them.
pub(crate) struct CardMusic {
    songs: Vec<Song>,
    sets: Vec<OfflineSet>,
}

/// A music page and whether it came from the card rather than the server.
pub(crate) struct Listed<T> {
    pub value: T,
    pub card: bool,
}

fn fold(text: &str) -> String { text.to_lowercase() }

fn by_name(a: &str, b: &str) -> std::cmp::Ordering { fold(a).cmp(&fold(b)).then_with(|| a.cmp(b)) }

impl CardMusic {
    pub(crate) fn new(songs: Vec<Song>, sets: Vec<OfflineSet>) -> CardMusic {
        // Spoken and radio items are not music, and a song without an ID is
        // no download.
        let songs = songs.into_iter().filter(|s| !s.id.is_empty() && s.spoken.is_none() && s.radio_url.is_none()).collect();
        CardMusic { songs, sets }
    }

    /// The album download that names and orders this album, if any.
    fn album_set(&self, id: &str) -> Option<&OfflineSet> {
        self.sets.iter().find(|set| set.kind == "album" && set.id == id)
    }

    /// Each album with a song on the card, by album ID, in name order.
    pub(crate) fn albums(&self) -> Vec<Album> {
        let mut order = Vec::new();
        let mut songs: HashMap<&str, Vec<&Song>> = HashMap::new();
        for song in &self.songs {
            let Some(id) = song.album_id.as_deref().filter(|id| !id.is_empty()) else { continue };
            songs.entry(id).or_insert_with(|| { order.push(id); Vec::new() }).push(song);
        }
        let mut albums: Vec<Album> = order.into_iter().map(|id| self.summary(id, &songs[id])).collect();
        albums.sort_by(|a, b| by_name(&a.name, &b.name).then_with(|| a.id.cmp(&b.id)));
        albums
    }

    fn summary(&self, id: &str, songs: &[&Song]) -> Album {
        let first = songs.iter().min_by_key(|s| (s.track.unwrap_or(u32::MAX), s.title.clone())).expect("an album with songs");
        let set = self.album_set(id);
        let artist_ids: std::collections::HashSet<_> = songs.iter().filter_map(|s| s.artist_id.as_deref()).collect();
        Album {
            id: id.into(),
            name: set.map_or_else(|| first.album.clone(), |set| set.name.clone()),
            artist: set.filter(|set| !set.artist.is_empty()).map_or_else(|| first.artist.clone(), |set| set.artist.clone()),
            artist_id: (artist_ids.len() == 1).then(|| first.artist_id.clone()).flatten(),
            cover_art: set.and_then(|set| set.cover_art.clone()).or_else(|| songs.iter().find_map(|s| s.cover_art.clone())),
            song_count: songs.len() as u32,
            duration: songs.iter().map(|s| s.duration).sum(),
            year: songs.iter().filter_map(|s| s.year).max(),
            genre: first.genre.clone(),
            ..Default::default()
        }
    }

    /// The artists of the songs on the card that have an album, in name order.
    pub(crate) fn artists(&self) -> ArtistsIndex {
        let mut artists: HashMap<&str, (Artist, std::collections::HashSet<&str>)> = HashMap::new();
        for song in &self.songs {
            let Some(id) = song.artist_id.as_deref().filter(|id| !id.is_empty()) else { continue };
            let entry = artists.entry(id).or_insert_with(|| (Artist { id: id.into(), name: song.artist.clone(), ..Default::default() }, Default::default()));
            if let Some(album) = song.album_id.as_deref().filter(|id| !id.is_empty()) {
                if entry.1.insert(album) && entry.0.cover_art.is_none() { entry.0.cover_art = song.cover_art.clone(); }
            }
        }
        let mut artists: Vec<Artist> = artists.into_values().filter(|(_, albums)| !albums.is_empty())
            .map(|(mut artist, albums)| { artist.album_count = albums.len() as u32; artist }).collect();
        artists.sort_by(|a, b| by_name(&a.name, &b.name).then_with(|| a.id.cmp(&b.id)));
        ArtistsIndex { index: if artists.is_empty() { Vec::new() } else { vec![ArtistIndex { name: String::new(), artist: artists }] } }
    }

    /// An artist with the albums of their songs on the card.
    pub(crate) fn artist(&self, id: &str) -> Option<ArtistWithAlbums> {
        let artist = self.artists().index.into_iter().flat_map(|index| index.artist).find(|artist| artist.id == id)?;
        let albums: std::collections::HashSet<&str> = self.songs.iter()
            .filter(|s| s.artist_id.as_deref() == Some(id)).filter_map(|s| s.album_id.as_deref()).collect();
        let album = self.albums().into_iter().filter(|album| albums.contains(album.id.as_str())).collect();
        Some(ArtistWithAlbums { artist, album })
    }

    /// An album's songs on the card: in its download's order, then by track.
    pub(crate) fn album(&self, id: &str) -> Option<AlbumWithSongs> {
        let set = self.album_set(id);
        let mut songs: Vec<&Song> = self.songs.iter()
            .filter(|s| s.album_id.as_deref() == Some(id) || set.is_some_and(|set| set.song_ids.contains(&s.id))).collect();
        if songs.is_empty() { return None; }
        let album = self.summary(id, &songs);
        let position = |song: &Song| set.and_then(|set| set.song_ids.iter().position(|s| *s == song.id)).unwrap_or(usize::MAX);
        songs.sort_by_key(|s| (position(s), s.track.unwrap_or(u32::MAX), s.title.clone()));
        Some(AlbumWithSongs { album, song: songs.into_iter().cloned().collect() })
    }

    /// Artists, albums and songs whose name contains every word of `query`,
    /// in any case. None when nothing on the card matches.
    pub(crate) fn search(&self, query: &str) -> Option<SearchResult> {
        let words: Vec<String> = query.split_whitespace().map(fold).collect();
        if words.is_empty() { return None; }
        let matches = |text: &str| { let text = fold(text); words.iter().all(|word| text.contains(word.as_str())) };
        let mut result = SearchResult {
            artist: self.artists().index.into_iter().flat_map(|index| index.artist).filter(|a| matches(&a.name)).collect(),
            album: self.albums().into_iter().filter(|a| matches(&a.name)).collect(),
            song: self.songs.iter().filter(|s| matches(&s.title)).cloned().collect(),
        };
        result.song.sort_by(|a, b| by_name(&a.title, &b.title).then_with(|| a.id.cmp(&b.id)));
        (!result.artist.is_empty() || !result.album.is_empty() || !result.song.is_empty()).then_some(result)
    }
}

impl Library {
    /// Whether Navidrome can't be used now, so the card answers music pages.
    pub(crate) fn music_from_card(&self) -> bool { !self.is_online() }

    /// `pick` over the completed downloads, on the `db` owner. Song rows
    /// are read once per page: the card holds at most some thousands.
    pub(crate) async fn card_music<T: Send + 'static>(&self, pick: impl FnOnce(&CardMusic) -> Option<T> + Send + 'static) -> Result<Option<T>, LibError> {
        let generation = self.client_generation();
        self.music_db_run(generation, move |blocking, db| {
            let unreadable = |_| LibError::Server("Unable to read downloads".into());
            let songs = db.downloaded_songs(blocking).map_err(unreadable)?.into_iter().map(|(song, _)| song).collect();
            let sets = db.offline_sets_checked(blocking).map_err(unreadable)?;
            Ok(pick(&CardMusic::new(songs, sets)))
        }).await
    }

    /// A music page: from the server (or its stored copy) while Navidrome
    /// can be used, otherwise from the card when the card has it. A network
    /// request that reaches neither the server nor a stored copy also falls
    /// back to the card, before the server is marked unavailable. With
    /// nothing on the card, the stored copy and the usual offline error
    /// remain.
    pub(crate) async fn music_page<T, F>(self: &Arc<Self>, call: Call<T>, policy: Policy, pick: F) -> Result<Listed<T>, LibError>
    where T: Streamed + serde::Serialize + Send + 'static, F: Fn(&CardMusic) -> Option<T> + Clone + Send + 'static {
        if self.music_from_card() {
            if let Ok(Some(value)) = self.card_music(pick.clone()).await { return Ok(Listed { value, card: true }); }
        }
        match self.clone().fetch_async(call, policy).await {
            Ok(fetched) => Ok(Listed { value: fetched.value, card: false }),
            // A cache-only miss while online is no failure: the network
            // request that follows decides.
            Err(error @ (LibError::Offline | LibError::NotConfigured)) if policy != Policy::CacheOnly => match self.card_music(pick).await {
                Ok(Some(value)) => Ok(Listed { value, card: true }),
                _ => Err(error),
            },
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str, album_id: Option<&str>, artist_id: &str, track: u32) -> Song {
        Song { id: id.into(), title: format!("Song {id}"), album: format!("Album {}", album_id.unwrap_or("none")),
            album_id: album_id.map(Into::into), artist: format!("Artist {artist_id}"), artist_id: Some(artist_id.into()),
            track: Some(track), duration: 10, ..Default::default() }
    }

    #[test]
    fn artists_and_albums_come_from_the_songs_on_the_card() {
        let songs = vec![song("b2", Some("b"), "y", 2), song("a1", Some("a"), "x", 1), song("b1", Some("b"), "y", 1),
            song("c1", Some("c"), "x", 1), song("loose", None, "z", 1),
            Song { spoken: Some(Default::default()), ..song("book", Some("d"), "w", 1) }];
        let card = CardMusic::new(songs, vec![OfflineSet { kind: "album".into(), id: "a".into(), name: "Named By Download".into(),
            artist: "Album Artist".into(), cover_art: Some("set-cover".into()), song_ids: vec!["a1".into()] }]);
        let artists: Vec<_> = card.artists().index.into_iter().flat_map(|i| i.artist).map(|a| (a.name, a.album_count)).collect();
        assert_eq!(artists, [("Artist x".into(), 2), ("Artist y".into(), 1)], "no spoken items, no artist without an album");
        let albums: Vec<_> = card.albums().into_iter().map(|a| (a.id, a.name, a.song_count)).collect();
        assert_eq!(albums, [("b".into(), "Album b".into(), 2), ("c".into(), "Album c".into(), 1), ("a".into(), "Named By Download".into(), 1)]);
        let x = card.artist("x").unwrap();
        assert_eq!(x.album.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["c", "a"]);
        assert!(card.artist("missing").is_none());
        let b = card.album("b").unwrap();
        assert_eq!(b.song.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["b1", "b2"], "track order");
        assert_eq!(b.album.artist_id.as_deref(), Some("y"));
        let a = card.album("a").unwrap();
        assert_eq!((a.album.artist.as_str(), a.album.cover_art.as_deref()), ("Album Artist", Some("set-cover")));
        assert!(card.album("d").is_none() && card.album("missing").is_none());
    }

    #[test]
    fn an_album_download_keeps_its_order_and_search_needs_every_word() {
        let songs = vec![song("t1", Some("a"), "x", 1), song("t2", Some("a"), "x", 1)];
        let card = CardMusic::new(songs, vec![OfflineSet { kind: "album".into(), id: "a".into(), name: "Two Discs".into(),
            artist: String::new(), cover_art: None, song_ids: vec!["t2".into(), "t1".into()] }]);
        assert_eq!(card.album("a").unwrap().song.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["t2", "t1"]);
        let found = card.search("song T1").unwrap();
        assert_eq!((found.artist.len(), found.album.len(), found.song.len()), (0, 0, 1));
        assert_eq!(card.search("two DISCS").unwrap().album.len(), 1);
        assert!(card.search("artist y").is_none() && card.search("  ").is_none());
    }
}
