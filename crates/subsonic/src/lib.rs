#![forbid(unsafe_code)]
//! An async Subsonic / OpenSubsonic client.
//!
//! A [`Call`] names an endpoint and its parameters (and so a cache key) and
//! the typed result. [`Client::get_async`] parses the response as it arrives
//! (`document.rs`): no complete text and no JSON tree. The app's response
//! cache stores [`Call::write_stored`]'s copy of a result and reads it back
//! with [`Call::parse_reader`].

pub mod audiobookshelf;
pub mod audiobookshelf_browse;
mod client;
pub mod http;
mod cover;
pub mod radio;
pub mod listenbrainz;
pub mod lastfm;
mod scrobble_http;
pub mod autoeq;
mod error;
pub mod document;
pub mod models;

pub use client::{AsyncStreamResponse, Auth, Client, Credentials, StreamOptions};
#[cfg(test)]
pub use client::StreamResponse;
pub use error::{Error, Failure, Result};
pub use models::*;
pub use document::Streamed;

use serde_json::Value;
use std::marker::PhantomData;

/// The Subsonic API version requested. Navidrome speaks 1.16.1.
pub const API_VERSION: &str = "1.16.1";

/// An endpoint and its parameters, without credentials.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Request {
    pub endpoint: &'static str,
    pub params: Vec<(&'static str, String)>,
}

impl Request {
    pub fn new(endpoint: &'static str) -> Self {
        Request { endpoint, params: Vec::new() }
    }

    pub fn param(mut self, key: &'static str, value: impl ToString) -> Self {
        self.params.push((key, value.to_string()));
        self
    }

    pub fn opt(self, key: &'static str, value: Option<impl ToString>) -> Self {
        match value {
            Some(v) => self.param(key, v),
            None => self,
        }
    }

    /// A stable key for caching this request's response.
    pub fn cache_key(&self) -> String {
        let mut key = String::from(self.endpoint);
        for (k, v) in &self.params {
            key.push(if key.len() == self.endpoint.len() { '?' } else { '&' });
            key.push_str(k);
            key.push('=');
            key.push_str(v);
        }
        key
    }
}

/// A request whose response body holds a `T` under `field`.
#[derive(Debug)]
pub struct Call<T> {
    pub request: Request,
    field: Option<&'static str>,
    _t: PhantomData<fn() -> T>,
}

// By hand: a derive would require `T: Clone`, which the marker does not need.
impl<T> Clone for Call<T> {
    fn clone(&self) -> Self {
        Call { request: self.request.clone(), field: self.field, _t: PhantomData }
    }
}

impl<T: Streamed> Call<T> {
    fn new(request: Request, field: Option<&'static str>) -> Self {
        Call { request, field, _t: PhantomData }
    }

    /// The field holding the result in the response body.
    pub fn field(&self) -> Option<&'static str> { self.field }

    /// The streaming parser for this call's network document.
    pub fn parser(&self) -> document::Parser<T> {
        document::Parser::new(self.field, self.request.endpoint)
    }

    /// The streaming parser for this call's stored copy ([`Call::write_stored`]).
    pub fn stored_parser(&self) -> document::Parser<T> {
        document::Parser::stored(self.field, self.request.endpoint)
    }

    /// Parses a complete network document text.
    pub fn parse_text(&self, text: &[u8]) -> Result<T> {
        let mut parser = self.parser();
        parser.feed(text)?;
        parser.finish()
    }

    /// Parses a stored copy from a reader in bounded pieces.
    #[cfg(test)]
    pub fn parse_reader(&self, reader: impl std::io::Read) -> Result<T> {
        document::parse_reader(reader, self.field, self.request.endpoint)
    }

    /// Writes the copy the response cache stores: `{"field": value}`, or the
    /// value for a call without a field. [`Call::stored_parser`] reads it.
    pub fn write_stored(&self, value: &T, out: impl std::io::Write) -> serde_json::Result<()> where T: serde::Serialize {
        struct Stored<'a, T> { field: &'static str, value: &'a T }
        impl<T: serde::Serialize> serde::Serialize for Stored<'_, T> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let mut map = s.serialize_map(Some(1))?;
                map.serialize_entry(self.field, self.value)?;
                map.end()
            }
        }
        match self.field {
            Some(field) => serde_json::to_writer(out, &Stored { field, value }),
            None => serde_json::to_writer(out, value),
        }
    }

    /// The reference parse of a complete body tree (the inner
    /// `subsonic-response` object), kept for tests that compare the streamed
    /// parse with it.
    pub fn parse(&self, body: &Value) -> Result<T> {
        let v = match self.field {
            Some(f) => body.get(f).cloned().unwrap_or(Value::Null),
            None => body.clone(),
        };
        // An empty list comes back as a missing field or an empty object.
        let v = if v.is_null() { Value::Object(Default::default()) } else { v };
        serde_json::from_value(v).map_err(|e| Error::Json(format!("{}: {e}", self.request.endpoint)))
    }
}

/// Album list orderings for `getAlbumList2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumListType {
    Random,
    Newest,
    Frequent,
    Recent,
    Starred,
    AlphabeticalByName,
    AlphabeticalByArtist,
    Highest,
    ByGenre,
    ByYear,
}

impl AlbumListType {
    pub fn as_str(self) -> &'static str {
        match self {
            AlbumListType::Random => "random",
            AlbumListType::Newest => "newest",
            AlbumListType::Frequent => "frequent",
            AlbumListType::Recent => "recent",
            AlbumListType::Starred => "starred",
            AlbumListType::AlphabeticalByName => "alphabeticalByName",
            AlbumListType::AlphabeticalByArtist => "alphabeticalByArtist",
            AlbumListType::Highest => "highest",
            AlbumListType::ByGenre => "byGenre",
            AlbumListType::ByYear => "byYear",
        }
    }
}

/// Constructors for every call the app makes.
pub mod calls {
    use super::*;

    pub fn ping() -> Call<Value> {
        Call::new(Request::new("ping"), None)
    }

    pub fn open_subsonic_extensions() -> Call<Vec<Extension>> {
        Call::new(Request::new("getOpenSubsonicExtensions"), Some("openSubsonicExtensions"))
    }

    pub fn artists() -> Call<ArtistsIndex> {
        Call::new(Request::new("getArtists"), Some("artists"))
    }

    pub fn artist(id: &str) -> Call<ArtistWithAlbums> {
        Call::new(Request::new("getArtist").param("id", id), Some("artist"))
    }

    pub fn artist_info(id: &str) -> Call<ArtistInfo> {
        Call::new(Request::new("getArtistInfo2").param("id", id).param("count", 0), Some("artistInfo2"))
    }

    pub fn album(id: &str) -> Call<AlbumWithSongs> {
        Call::new(Request::new("getAlbum").param("id", id), Some("album"))
    }

    pub fn song(id: &str) -> Call<Song> {
        Call::new(Request::new("getSong").param("id", id), Some("song"))
    }

    pub fn album_list(kind: AlbumListType, size: u32, offset: u32) -> Call<AlbumList> {
        album_list_filtered(kind, size, offset, None, None)
    }

    pub fn album_list_filtered(
        kind: AlbumListType,
        size: u32,
        offset: u32,
        genre: Option<&str>,
        years: Option<(i32, i32)>,
    ) -> Call<AlbumList> {
        let mut r = Request::new("getAlbumList2")
            .param("type", kind.as_str())
            .param("size", size)
            .param("offset", offset)
            .opt("genre", genre);
        if let Some((from, to)) = years {
            r = r.param("fromYear", from).param("toYear", to);
        }
        Call::new(r, Some("albumList2"))
    }

    pub fn random_songs(size: u32, genre: Option<&str>) -> Call<SongList> {
        Call::new(
            Request::new("getRandomSongs").param("size", size).opt("genre", genre),
            Some("randomSongs"),
        )
    }

    pub fn songs_by_genre(genre: &str, count: u32, offset: u32) -> Call<SongList> {
        Call::new(
            Request::new("getSongsByGenre")
                .param("genre", genre)
                .param("count", count)
                .param("offset", offset),
            Some("songsByGenre"),
        )
    }

    pub fn genres() -> Call<Genres> {
        Call::new(Request::new("getGenres"), Some("genres"))
    }

    pub fn playlists() -> Call<Playlists> {
        Call::new(Request::new("getPlaylists"), Some("playlists"))
    }

    pub fn playlist(id: &str) -> Call<PlaylistWithSongs> {
        Call::new(Request::new("getPlaylist").param("id", id), Some("playlist"))
    }

    pub fn starred() -> Call<Starred> {
        Call::new(Request::new("getStarred2"), Some("starred2"))
    }

    /// Search across artists, albums and songs. Navidrome takes an empty
    /// query (`""`) as "everything", which pages through the whole library.
    pub fn search(query: &str, artists: u32, albums: u32, songs: u32, song_offset: u32) -> Call<SearchResult> {
        Call::new(
            Request::new("search3")
                .param("query", query)
                .param("artistCount", artists)
                .param("albumCount", albums)
                .param("songCount", songs)
                .param("songOffset", song_offset),
            Some("searchResult3"),
        )
    }

    pub fn top_songs(artist_name: &str, count: u32) -> Call<SongList> {
        Call::new(
            Request::new("getTopSongs").param("artist", artist_name).param("count", count),
            Some("topSongs"),
        )
    }

    pub fn similar_songs(id: &str, count: u32) -> Call<SongList> {
        Call::new(
            Request::new("getSimilarSongs2").param("id", id).param("count", count),
            Some("similarSongs2"),
        )
    }

    pub fn internet_radio_stations() -> Call<RadioStations> {
        Call::new(Request::new("getInternetRadioStations"), Some("internetRadioStations"))
    }

    pub fn create_radio_station(name: &str, url: &str) -> Call<Value> {
        Call::new(Request::new("createInternetRadioStation").param("name",name).param("streamUrl",url),None)
    }

    pub fn lyrics(song_id: &str) -> Call<LyricsList> {
        Call::new(Request::new("getLyricsBySongId").param("id", song_id), Some("lyricsList"))
    }

    pub fn play_queue() -> Call<PlayQueue> {
        Call::new(Request::new("getPlayQueue"), Some("playQueue"))
    }

    // Actions. Their responses carry nothing but the status.

    pub fn save_play_queue(ids: &[String], current: Option<&str>, position_ms: u64) -> Call<Value> {
        let mut r = Request::new("savePlayQueue");
        for id in ids {
            r = r.param("id", id);
        }
        let r = r.opt("current", current).param("position", position_ms);
        Call::new(r, None)
    }

    pub fn star(kind: ItemKind, id: &str) -> Call<Value> {
        Call::new(Request::new("star").param(kind.id_param(), id), None)
    }

    pub fn unstar(kind: ItemKind, id: &str) -> Call<Value> {
        Call::new(Request::new("unstar").param(kind.id_param(), id), None)
    }

    pub fn set_rating(id: &str, rating: u8) -> Call<Value> {
        Call::new(Request::new("setRating").param("id", id).param("rating", rating.min(5)), None)
    }

    /// `time_ms`: when the song was played (epoch ms), for submissions made
    /// later from the offline queue.
    pub fn scrobble(id: &str, time_ms: Option<u64>, submission: bool) -> Call<Value> {
        Call::new(
            Request::new("scrobble")
                .param("id", id)
                .opt("time", time_ms)
                .param("submission", submission),
            None,
        )
    }

    pub fn create_playlist(name: &str, song_ids: &[String]) -> Call<Value> {
        let mut r = Request::new("createPlaylist").param("name", name);
        for id in song_ids {
            r = r.param("songId", id);
        }
        Call::new(r, None)
    }

    pub fn add_to_playlist(playlist_id: &str, song_ids: &[String]) -> Call<Value> {
        let mut r = Request::new("updatePlaylist").param("playlistId", playlist_id);
        for id in song_ids {
            r = r.param("songIdToAdd", id);
        }
        Call::new(r, None)
    }

    pub fn scan_status() -> Call<ScanStatus> {
        Call::new(Request::new("getScanStatus"), Some("scanStatus"))
    }

    pub fn start_scan() -> Call<Value> {
        Call::new(Request::new("startScan"), None)
    }
}

/// What a star or unstar applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Song,
    Album,
    Artist,
}

impl ItemKind {
    fn id_param(self) -> &'static str {
        match self {
            ItemKind::Song => "id",
            ItemKind::Album => "albumId",
            ItemKind::Artist => "artistId",
        }
    }
}

#[cfg(test)]
mod tests;
