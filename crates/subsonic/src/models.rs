//! Response types. Every field the server may omit has a default, so a model
//! parses whatever subset a server or an older cache entry holds. Fields the
//! player never reads are not declared, so they are skipped while parsing
//! (each record is held once per list on a 128 MiB device).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub album_count: u32,
    pub cover_art: Option<String>,
    pub starred: Option<String>,
    pub artist_image_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtistIndex {
    pub name: String,
    pub artist: Vec<Artist>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtistsIndex {
    pub index: Vec<ArtistIndex>,
}

impl ArtistsIndex {
    /// Every artist, in the server's index order.
    pub fn all(&self) -> impl Iterator<Item = &Artist> {
        self.index.iter().flat_map(|i| i.artist.iter())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtistWithAlbums {
    #[serde(flatten)]
    pub artist: Artist,
    pub album: Vec<Album>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtistInfo {
    pub small_image_url: Option<String>,
    pub medium_image_url: Option<String>,
    pub large_image_url: Option<String>,
}

impl ArtistInfo {
    /// Prefer thumbnails on the Q2; originals can exceed its memory budget.
    pub fn image_urls(&self) -> impl Iterator<Item = &str> {
        [&self.small_image_url, &self.medium_image_url, &self.large_image_url]
            .into_iter().filter_map(|s| s.as_deref()).filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub cover_art: Option<String>,
    pub song_count: u32,
    /// Seconds.
    pub duration: u32,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub created: Option<String>,
    pub starred: Option<String>,
    pub user_rating: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlbumWithSongs {
    #[serde(flatten)]
    pub album: Album,
    pub song: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlbumList {
    pub album: Vec<Album>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReplayGain {
    pub track_gain: Option<f32>,
    pub album_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_peak: Option<f32>,
}

/// A song (Subsonic's "child").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Song {
    /// Card-owned spoken media, never sent as a Navidrome song identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spoken: Option<Box<Spoken>>,
    /// App-local live station; never a Subsonic song ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radio_url: Option<String>,
    pub id: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    pub track: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub cover_art: Option<String>,
    /// Bytes of the original file.
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub suffix: Option<String>,
    /// Seconds.
    pub duration: u32,
    /// kbit/s.
    pub bit_rate: Option<u32>,
    pub sampling_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
    pub starred: Option<String>,
    pub user_rating: Option<u8>,
    pub replay_gain: Option<ReplayGain>,
}

impl Song {
    /// A short quality label such as "FLAC 24/96" or "MP3 320".
    pub fn quality_label(&self) -> String {
        let fmt = self.suffix.as_deref().unwrap_or("").to_ascii_uppercase();
        match (self.bit_depth, self.sampling_rate, self.bit_rate) {
            (Some(d), Some(r), _) if d > 0 && r > 0 => {
                let khz = r as f32 / 1000.0;
                if (khz - khz.round()).abs() < 0.05 {
                    format!("{fmt} {d}/{}", khz.round() as u32)
                } else {
                    format!("{fmt} {d}/{khz:.1}")
                }
            }
            (_, _, Some(b)) if b > 0 => format!("{fmt} {b}"),
            _ => fmt,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SongList {
    pub song: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Genre {
    pub value: String,
    pub song_count: u32,
    pub album_count: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Genres {
    pub genre: Vec<Genre>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    pub song_count: u32,
    pub duration: u32,
    pub cover_art: Option<String>,
    pub changed: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Playlists {
    pub playlist: Vec<Playlist>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlaylistWithSongs {
    #[serde(flatten)]
    pub playlist: Playlist,
    pub entry: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Starred {
    pub artist: Vec<Artist>,
    pub album: Vec<Album>,
    pub song: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchResult {
    pub artist: Vec<Artist>,
    pub album: Vec<Album>,
    pub song: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RadioStation {
    pub id: String,
    pub name: String,
    pub stream_url: String,
    pub home_page_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RadioStations {
    pub internet_radio_station: Vec<RadioStation>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LyricLine {
    /// Milliseconds from the start, for synced lyrics.
    pub start: Option<u64>,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Lyrics {
    pub lang: Option<String>,
    pub synced: bool,
    pub offset: Option<i64>,
    pub line: Vec<LyricLine>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LyricsList {
    pub structured_lyrics: Vec<Lyrics>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlayQueue {
    pub current: Option<String>,
    /// Milliseconds into the current song.
    pub position: Option<u64>,
    pub changed: Option<String>,
    pub entry: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Extension {
    pub name: String,
    pub versions: Vec<u32>,
}

/// One logical book/episode can contain multiple physical files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spoken {
    pub id: String,
    pub offset_ms: u64,
    pub duration_ms: u64,
    pub chapters: Vec<Chapter>,
    pub source: SpokenSource,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub start_ms: u64,
    pub title: String,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum SpokenSource {
    #[default]
    Unknown,
    Local {
        relative_path: String,
    },
    Audiobookshelf {
        namespace: String,
        item: String,
        episode: Option<String>,
        content_url: String,
    },
}

// Record lists streamed from documents (document.rs): each list's path below
// the call's field and where its records go.
use crate::document::{Seg, Streamed};

/// Appends a streamed record, growing by a quarter rather than doubling: a
/// list of unknown length then peaks at most a quarter above its records
/// (realloc of a large block moves no data on glibc).
fn grow<T>(list: &mut Vec<T>, record: T) {
    if list.len() == list.capacity() { list.reserve_exact((list.len() / 4).max(16)); }
    list.push(record);
}

macro_rules! streamed {
    ($ty:ty { $($index:literal: $key:literal => $field:ident: $record:ty),+ $(,)? }) => {
        impl Streamed for $ty {
            fn shape() -> &'static [&'static [Seg]] { &[$(&[Seg::Key($key)]),+] }
            fn push(&mut self, target: usize, _parent: u32, record: &[u8]) -> serde_json::Result<()> {
                match target {
                    $($index => grow(&mut self.$field, serde_json::from_slice::<$record>(record)?),)+
                    _ => {}
                }
                Ok(())
            }
            fn merge(&mut self, records: Self) {
                $(if !records.$field.is_empty() { self.$field = records.$field; self.$field.shrink_to_fit(); })+
            }
        }
    };
}

streamed!(AlbumList { 0: "album" => album: Album });
streamed!(ArtistWithAlbums { 0: "album" => album: Album });
streamed!(AlbumWithSongs { 0: "song" => song: Song });
streamed!(SongList { 0: "song" => song: Song });
streamed!(Genres { 0: "genre" => genre: Genre });
streamed!(Playlists { 0: "playlist" => playlist: Playlist });
streamed!(PlaylistWithSongs { 0: "entry" => entry: Song });
streamed!(PlayQueue { 0: "entry" => entry: Song });
streamed!(RadioStations { 0: "internetRadioStation" => internet_radio_station: RadioStation });
streamed!(Starred { 0: "artist" => artist: Artist, 1: "album" => album: Album, 2: "song" => song: Song });
streamed!(SearchResult { 0: "artist" => artist: Artist, 1: "album" => album: Album, 2: "song" => song: Song });
impl Streamed for ScanStatus {}

/// `getScanStatus`. `count` is in the Subsonic API; `folderCount` and
/// `lastScan` are Navidrome's. Together they change when a scan ends (with
/// or without changes to the library).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanStatus {
    pub scanning: bool,
    pub count: Option<u64>,
    pub folder_count: Option<u64>,
    pub last_scan: Option<String>,
}
impl ScanStatus {
    /// The library's change marker: `None` while a scan runs or when the
    /// server reports no scan time (a caller then cannot skip a rebuild).
    pub fn marker(&self) -> Option<String> {
        if self.scanning { return None; }
        let last = self.last_scan.as_deref().filter(|t| !t.is_empty())?;
        Some(format!("{last}|{}|{}", self.count.unwrap_or(0), self.folder_count.unwrap_or(0)))
    }
}
impl Streamed for ArtistInfo {}
impl Streamed for Song {}
impl Streamed for LyricsList {}

impl Streamed for ArtistsIndex {
    fn shape() -> &'static [&'static [Seg]] { &[&[Seg::Key("index"), Seg::Each, Seg::Key("artist")]] }
    fn push(&mut self, _target: usize, parent: u32, record: &[u8]) -> serde_json::Result<()> {
        let parent = parent as usize;
        if self.index.len() <= parent { self.index.resize_with(parent + 1, ArtistIndex::default); }
        grow(&mut self.index[parent].artist, serde_json::from_slice(record)?);
        Ok(())
    }
    fn merge(&mut self, records: Self) {
        for (i, group) in records.index.into_iter().enumerate() {
            if let Some(index) = self.index.get_mut(i) { index.artist = group.artist; index.artist.shrink_to_fit(); }
        }
    }
}
