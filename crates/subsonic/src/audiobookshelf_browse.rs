//! Pinned ABS2.37.2 browsing contract. Server pagination is distinct from
//! bounded home previews and streamed windows of unpaginated metadata.
use crate::audiobookshelf::{Client, Error, segment};
use serde::{
    Deserialize, Serialize,
    de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use std::fmt;
#[cfg(test)]
use {std::io::Read,serde::de::DeserializeSeed};

#[path = "audiobookshelf_browse/stream.rs"]
mod stream;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Media {
    #[default]
    Book,
    Podcast,
}
impl Media {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Book => "book",
            Self::Podcast => "podcast",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryChoice {
    pub id: String,
    pub name: String,
    pub media: Media,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Section {
    ContinueListening,
    ContinueSeries,
    RecentlyAdded,
    RecentSeries,
    Discover,
    ListenAgain,
    NewestEpisodes,
    Books,
    /// The server exposes a bounded search preview without offset support.
    Search(String),
    Genres,
    Series,
    Collections,
    Playlists,
    Authors,
    Narrators,
    LatestEpisodes,
    ByGenre(String),
    BySeries(String),
    ByAuthor(String),
    ByNarrator(String),
    Collection(String),
    Playlist(String),
    Episodes(String),
}
impl Section {
    /// A shelf of the library's `personalized` document.
    pub fn is_home(&self) -> bool { self.home().is_some() }
    fn home(&self) -> Option<&'static str> {
        Some(match self {
            Self::ContinueListening => "continue-listening",
            Self::ContinueSeries => "continue-series",
            Self::RecentSeries => "recent-series",
            Self::Discover => "discover",
            Self::ListenAgain => "listen-again",
            Self::NewestEpisodes => "newest-episodes",
            _ => return None,
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Book { item: String },
    Podcast { item: String },
    Episode { item: String, episode: String },
    Group { section: Section },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub duration_ms: u64,
    pub art: Option<String>,
    pub kind: EntryKind,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Paging {
    #[default]
    Server,
    Snapshot,
    Preview,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub total: Option<u64>,
    pub has_more: bool,
    pub paging: Paging,
    /// What each entry's row said about its groups, by entry (empty when no
    /// row said anything). The library sync derives series, author,
    /// narrator, genre and latest-episode pages from these; the browser
    /// ignores them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<Facts>,
}
/// A row's group fields, as Audiobookshelf sends them in a minified item:
/// names joined with ", " (series as `Name #sequence`), no ids.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrators: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Milliseconds: an item's `addedAt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u64>,
    /// Milliseconds: an episode's `publishedAt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<u64>,
    /// A group row's book count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}
impl Facts {
    fn of(r: &Row) -> Self {
        let m = &r.media.metadata;
        let count = (r.num_books.is_some() || r.books.0 > 0 || r.items.0 > 0).then(|| r.num_books.unwrap_or(r.books.0.max(r.items.0)));
        Facts { series: m.series_name.clone(), authors: m.author_name.clone(), narrators: m.narrator_name.clone(), genres: m.genres.clone(),
            added: r.added_at, published: r.published_at, count }
    }
}
const MAX_PAGE: u32 = 50;
const MAX_RESPONSE: u64 = 64 * 1024 * 1024;
fn protocol<T>() -> Result<T, Error> {
    Err(Error::Protocol)
}
fn text(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("")
}
fn string(v: &str) -> Result<String, Error> {
    if v.len() > 4096 {
        return protocol();
    }
    Ok(v.to_owned())
}
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let x = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(x >> 18) as usize] as char);
        out.push(T[((x >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 {
            T[((x >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(x & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
#[derive(Default)]
struct Count(u64);
impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct C;
        impl<'de> Visitor<'de> for C {
            type Value = Count;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Count, A::Error> {
                let mut n = 0;
                while a.next_element::<IgnoredAny>()?.is_some() {
                    n += 1;
                }
                Ok(Count(n))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Count, E> {
                Ok(Count(0))
            }
        }
        d.deserialize_any(C)
    }
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Metadata {
    title: Option<String>,
    subtitle: Option<String>,
    author_name: Option<String>,
    series_name: Option<String>,
    narrator_name: Option<String>,
    genres: Vec<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawMedia {
    metadata: Metadata,
    num_tracks: Option<u64>,
    duration: Option<f64>,
}

/// ABS uses this key for two different shapes: scalar RSS episode numbering on
/// PodcastEpisode and an embedded episode object on playlist/stream wrappers.
/// Numbering is metadata; it must never become the stable episode identity.
#[derive(Default)]
struct EpisodeReference { number: Option<String>, item: Option<Box<Row>> }
fn episode_reference<'de, D: de::Deserializer<'de>>(d: D) -> Result<EpisodeReference, D::Error> {
    struct Episode;
    impl<'de> Visitor<'de> for Episode {
        type Value = EpisodeReference;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("episode number, null, or an embedded episode") }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok(EpisodeReference::default()) }
        fn visit_str<E: de::Error>(self, n: &str) -> Result<Self::Value, E> {
            if n.len() > 4096 { return Err(E::custom("episode metadata exceeds bound")); }
            Ok(EpisodeReference { number: Some(n.into()), item: None })
        }
        fn visit_u64<E: de::Error>(self, n: u64) -> Result<Self::Value, E> { Ok(EpisodeReference {number: Some(n.to_string()), item: None}) }
        fn visit_i64<E: de::Error>(self, n: i64) -> Result<Self::Value, E> { Ok(EpisodeReference {number: Some(n.to_string()), item: None}) }
        fn visit_map<A: MapAccess<'de>>(self, fields: A) -> Result<Self::Value, A::Error> {
            Row::deserialize(de::value::MapAccessDeserializer::new(fields)).map(|r| EpisodeReference {number: None, item: Some(Box::new(r))})
        }
    }
    d.deserialize_any(Episode)
}
#[derive(Default)]
struct AudioFile { duration: Option<f64> }
fn audio_file<'de, D: de::Deserializer<'de>>(d: D) -> Result<Option<AudioFile>, D::Error> {
    struct Audio;
    impl<'de> Visitor<'de> for Audio {
        type Value = Option<AudioFile>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("audio file or null") }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> { Ok(Some(AudioFile::default())) }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> { Ok(Some(AudioFile::default())) }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> { Ok(Some(AudioFile::default())) }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> { Ok(Some(AudioFile::default())) }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> { Ok(Some(AudioFile::default())) }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            while a.next_element::<IgnoredAny>()?.is_some() {} Ok(Some(AudioFile::default()))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            let mut duration = None; let mut seen = false;
            while let Some(k) = a.next_key::<String>()? {
                if k == "duration" { if seen { return Err(de::Error::duplicate_field("duration")); } seen = true; duration = a.next_value()?; }
                else { a.next_value::<IgnoredAny>()?; }
            }
            Ok(Some(AudioFile { duration }))
        }
    }
    d.deserialize_any(Audio)
}
fn optional_scalar<'de, D: de::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    struct Scalar;
    impl<'de> Visitor<'de> for Scalar {
        type Value = Option<String>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("string, integer, or null") }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
        fn visit_str<E: de::Error>(self, n: &str) -> Result<Self::Value, E> {
            if n.len() > 4096 { return Err(E::custom("season exceeds bound")); }
            Ok(Some(n.into()))
        }
        fn visit_u64<E: de::Error>(self, n: u64) -> Result<Self::Value, E> { Ok(Some(n.to_string())) }
        fn visit_i64<E: de::Error>(self, n: i64) -> Result<Self::Value, E> { Ok(Some(n.to_string())) }
    }
    d.deserialize_any(Scalar)
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Row {
    id: Option<String>,
    name: Option<String>,
    title: Option<String>,
    library_id: Option<String>,
    library_item_id: Option<String>,
    episode_id: Option<String>,
    media_type: Option<String>,
    num_books: Option<u64>,
    num_episodes: Option<u64>,
    media: RawMedia,
    podcast: RawMedia,
    recent_episode: Option<Box<Row>>,
    #[serde(deserialize_with = "episode_reference")]
    episode: EpisodeReference,
    library_item: Option<Box<Row>>,
    #[serde(deserialize_with = "audio_file")]
    audio_file: Option<AudioFile>,
    books: Count,
    items: Count,
    #[serde(deserialize_with = "optional_scalar")]
    season: Option<String>,
    published_at: Option<u64>,
    duration: Option<f64>,
    added_at: Option<u64>,
}
#[derive(Clone, Copy)]
enum Rows {
    Items,
    Series,
    Collections,
    Playlists,
    Authors,
    Narrators,
    Genres,
    Episodes,
    PlaylistItems,
    SearchItems,
    PodcastEpisodes,
}
#[derive(Default)]
struct Window {
    rows: Vec<Row>,
    count: u64,
    total: Option<u64>,
    page: Option<u32>,
    limit: Option<u32>,
    library: Option<String>,
    found: bool,
}
#[cfg(test)]
struct WindowSeed<'a> {
    path: &'a [&'a str],
    start: u64,
    limit: u32,
    rows: Rows,
}
#[cfg(test)]
impl<'de> DeserializeSeed<'de> for WindowSeed<'_> {
    type Value = Window;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Window, D::Error> {
        d.deserialize_any(self)
    }
}
#[cfg(test)]
impl<'de> Visitor<'de> for WindowSeed<'_> {
    type Value = Window;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an ABS browse object or array")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Window, A::Error> {
        let mut w = Window::default();
        while let Some(k) = a.next_key::<String>()? {
            if self.path.first().is_some_and(|p| *p == k) {
                let nested = a.next_value_seed(WindowSeed {
                    path: &self.path[1..],
                    ..self
                })?;
                w.rows = nested.rows;
                w.count = nested.count;
                w.found = nested.found;
                if nested.total.is_some() {
                    w.total = nested.total;
                }
            } else {
                match k.as_str() {
                    "total" => w.total = a.next_value()?,
                    "page" => w.page = a.next_value()?,
                    "limit" => w.limit = a.next_value()?,
                    "libraryId" => w.library = a.next_value()?,
                    _ => {
                        a.next_value::<IgnoredAny>()?;
                    }
                }
            }
        }
        Ok(w)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Window, A::Error> {
        if !self.path.is_empty() {
            return Err(de::Error::custom("wrong array path"));
        }
        let mut w = Window {
            found: true,
            ..Default::default()
        };
        loop {
            let row = if matches!(self.rows, Rows::Genres) {
                a.next_element::<String>()?.map(|s| Row {
                    id: Some(s.clone()),
                    name: Some(s),
                    ..Default::default()
                })
            } else {
                a.next_element::<Row>()?
            };
            let Some(row) = row else { break };
            if matches!(self.rows, Rows::PodcastEpisodes) && row.audio_file.is_none() {
                continue;
            }
            if w.count >= self.start && w.rows.len() < (self.limit as usize) {
                w.rows.push(row);
            }
            w.count += 1;
        }
        Ok(w)
    }
}
#[cfg(test)]
struct HomeSeed<'a> {
    id: &'a str,
    limit: u32,
}
#[cfg(test)]
impl<'de> DeserializeSeed<'de> for HomeSeed<'_> {
    type Value = Window;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Window, D::Error> {
        d.deserialize_seq(self)
    }
}
#[cfg(test)]
impl<'de> Visitor<'de> for HomeSeed<'_> {
    type Value = Window;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("personalized shelves")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Window, A::Error> {
        let mut result = Window {
            found: true,
            ..Default::default()
        };
        let mut n = 0;
        while let Some((id, w)) = a.next_element_seed(ShelfSeed { limit: self.limit })? {
            n += 1;
            if n > 32 {
                return Err(de::Error::custom("too many shelves"));
            }
            if id == self.id {
                result = w;
            }
        }
        Ok(result)
    }
}
#[cfg(test)]
struct ShelfSeed {
    limit: u32,
}
#[cfg(test)]
impl<'de> DeserializeSeed<'de> for ShelfSeed {
    type Value = (String, Window);
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }
}
#[cfg(test)]
impl<'de> Visitor<'de> for ShelfSeed {
    type Value = (String, Window);
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a shelf")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let mut id = String::new();
        let mut w = Window::default();
        let mut total = None;
        while let Some(k) = a.next_key::<String>()? {
            match k.as_str() {
                "id" => id = a.next_value()?,
                "total" => total = a.next_value()?,
                "entities" => {
                    w = a.next_value_seed(WindowSeed {
                        path: &[],
                        start: 0,
                        limit: self.limit,
                        rows: Rows::Items,
                    })?
                }
                _ => {
                    a.next_value::<IgnoredAny>()?;
                }
            }
        }
        w.total = total;
        Ok((id, w))
    }
}
fn network(e: crate::http::Error) -> Error {
    match e {
        crate::http::Error::Status(401 | 403, _) => Error::Auth,
        crate::http::Error::Status(404, _) => Error::Missing,
        crate::http::Error::Status(408 | 429 | 500..=599, _) => Error::Server,
        crate::http::Error::Status(_, _) => Error::Protocol,
        _ => Error::Network,
    }
}
#[cfg(test)]
fn decode<'a, S: for<'de> DeserializeSeed<'de, Value = Window>>(
    reader: impl Read,
    seed: S,
) -> Result<Window, Error> {
    let mut json = serde_json::Deserializer::from_reader(reader.take(MAX_RESPONSE + 1));
    let w = seed.deserialize(&mut json).map_err(|_| Error::Protocol)?;
    json.end().map_err(|_| Error::Protocol)?;
    if !w.found {
        return protocol();
    }
    Ok(w)
}
fn duration_ms(duration: Option<f64>) -> u64 {
    duration.filter(|duration| duration.is_finite() && *duration > 0.0 && *duration <= 10_000_000.0)
        .map_or(0, |duration| (duration * 1000.0) as u64)
}
fn episode_duration_ms(row: &Row) -> u64 {
    let duration = duration_ms(row.duration);
    if duration > 0 { duration } else { duration_ms(row.audio_file.as_ref().and_then(|audio| audio.duration)) }
}

// ABS timestamps are UTC epoch milliseconds; absent or invalid metadata is omitted.
fn episode_detail(row: &Row) -> String {
    let mut parts = Vec::new();
    if let Some(ms) = row.published_at.filter(|ms| *ms > 0 && *ms <= 253_402_300_799_999) {
        if let Some(time) = std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms)) {
            let formatted = httpdate::fmt_http_date(time);
            let date: Vec<_> = formatted.split_whitespace().collect();
            if date.len() >= 4 {
                let month = match date[2] { "Jan"=>"January", "Feb"=>"February", "Mar"=>"March", "Apr"=>"April", "May"=>"May", "Jun"=>"June", "Jul"=>"July", "Aug"=>"August", "Sep"=>"September", "Oct"=>"October", "Nov"=>"November", "Dec"=>"December", _=>"" };
                if let Ok(day) = date[1].parse::<u8>() { if !month.is_empty() { parts.push(format!("{month} {day}, {}", date[3])); } }
            }
        }
    }
    let number = |v: &Option<String>| v.as_deref().and_then(|s| s.trim().parse::<u32>().ok()).filter(|n| *n > 0);
    let season = number(&row.season); let episode = number(&row.episode.number);
    match (season, episode) {
        (Some(s), Some(e)) => parts.push(format!("S{s} E{e}")),
        (Some(s), None) => parts.push(format!("S{s}")),
        (None, Some(e)) => parts.push(format!("E{e}")),
        _ => {},
    }
    let valid_duration = |n: &f64| n.is_finite() && *n > 0.0 && *n <= 10_000_000.0;
    if let Some(seconds) = row.duration.filter(valid_duration).or_else(|| row.audio_file.as_ref().and_then(|a| a.duration).filter(valid_duration)) {
        let seconds = seconds.round() as u64;
        if seconds > 0 { parts.push(if seconds >= 3600 {format!("{}:{:02}:{:02}",seconds/3600,seconds/60%60,seconds%60)} else {format!("{}:{:02}",seconds/60,seconds%60)}); }
    }
    parts.join(" · ")
}
fn entry(
    mut r: Row,
    kind: Rows,
    media: Media,
    library: &str,
    item_hint: Option<&str>,
) -> Result<Entry, Error> {
    if r.library_id.as_deref().is_some_and(|id| id != library) {
        return protocol();
    }
    if matches!(kind, Rows::PlaylistItems | Rows::SearchItems) {
        if let Some(item) = r.library_item.take() {
            let mut item = *item;
            if matches!(kind, Rows::SearchItems) && item.library_id.as_deref().is_some_and(|id| id != library) {
                return protocol();
            }
            if let Some(ep) = r.episode.item.take() {
                item.recent_episode = Some(ep);
            } else if let Some(ep) = r.episode_id.take() {
                item.recent_episode = Some(Box::new(Row {
                    id: Some(ep),
                    title: r.title.clone(),
                    ..Default::default()
                }));
            }
            r = item;
        }
    }
    let id = string(text(&r.id))?;
    if id.is_empty() {
        return protocol();
    }
    let group = match kind {
        Rows::Series => Some(Section::BySeries(id.clone())),
        Rows::Collections => Some(Section::Collection(id.clone())),
        Rows::Playlists => Some(Section::Playlist(id.clone())),
        Rows::Authors => Some(Section::ByAuthor(id.clone())),
        Rows::Narrators => Some(Section::ByNarrator(string(text(&r.name))?)),
        Rows::Genres => Some(Section::ByGenre(string(text(&r.name))?)),
        _ => None,
    };
    if let Some(section) = group {
        let title = string(text(&r.name))?;
        if title.is_empty() {
            return protocol();
        }
        let count = r.num_books.unwrap_or(r.books.0.max(r.items.0));
        return Ok(Entry {
            id,
            title,
            detail: if count > 0 {
                format!("{count} items")
            } else {
                String::new()
            },
            duration_ms: 0,
            art: None,
            kind: EntryKind::Group { section },
        });
    }
    let episode = r.recent_episode.take().or_else(|| r.episode.item.take());
    if let Some(ep) = episode {
        let episode = string(text(&ep.id))?;
        if episode.is_empty() {
            return protocol();
        }
        return Ok(Entry {
            id: format!("{id}/{episode}"),
            title: string(text(&ep.title))?,
            detail: episode_detail(&ep),
            duration_ms: episode_duration_ms(&ep),
            art: Some(id.clone()),
            kind: EntryKind::Episode { item: id, episode },
        });
    }
    if matches!(kind, Rows::Episodes | Rows::PodcastEpisodes) {
        let item = string(r.library_item_id.as_deref().or(item_hint).unwrap_or(""))?;
        if item.is_empty() {
            return protocol();
        }
        return Ok(Entry {
            id: id.clone(),
            title: string(text(&r.title))?,
            detail: episode_detail(&r),
            duration_ms: episode_duration_ms(&r),
            art: Some(item.clone()),
            kind: EntryKind::Episode { item, episode: id },
        });
    }
    if r.media_type.as_deref().is_some_and(|v| v != media.as_str()) {
        return protocol();
    }
    let title = string(text(&r.media.metadata.title))?;
    if title.is_empty() {
        return protocol();
    }
    let detail = string(text(&r.media.metadata.author_name))?;
    Ok(Entry {
        id: id.clone(),
        title,
        detail,
        duration_ms: duration_ms(r.media.duration),
        art: Some(id.clone()),
        kind: if media == Media::Book {
            EntryKind::Book { item: id }
        } else {
            EntryKind::Podcast { item: id }
        },
    })
}
impl Client {
    #[cfg(test)]
    pub fn browse_libraries(&self, media: Media) -> Result<Vec<LibraryChoice>, Error> {
        let client = self.clone();
        crate::http::block_on(async move { client.browse_libraries_async(media).await })
            .unwrap_or(Err(Error::Network))
    }
    pub async fn browse_libraries_async(&self, media: Media) -> Result<Vec<LibraryChoice>, Error> {
        let v = self.get_async("api/libraries").await?;
        let all = v["libraries"].as_array().ok_or(Error::Protocol)?;
        if all.len() > 1024 {
            return protocol();
        }
        let mut out = Vec::new();
        for l in all {
            if l["mediaType"].as_str() != Some(media.as_str()) {
                continue;
            }
            let id = string(l["id"].as_str().ok_or(Error::Protocol)?)?;
            let name = string(l["name"].as_str().ok_or(Error::Protocol)?)?;
            if id.is_empty() {
                return protocol();
            }
            out.push(LibraryChoice { id, name, media });
        }
        Ok(out)
    }
    #[cfg(test)]
    pub fn browse_page(
        &self,
        library: &str,
        media: Media,
        section: &Section,
        page: u32,
        limit: u32,
    ) -> Result<Page, Error> {
        let client = self.clone();
        let library = library.to_owned();
        let section = section.clone();
        crate::http::block_on(async move {
            match client.browse_page_validated_async(&library, media, &section, page, limit, None).await? {
                Validated::Changed(page, _) => Ok(page),
                Validated::Unchanged => protocol(),
            }
        }).unwrap_or(Err(Error::Network))
    }
    /// A browse page as a conditional request: with the
    /// `etag` of a stored copy, a 304 answers [`Validated::Unchanged`] with
    /// no body. Audiobookshelf (Express) sends an ETag for every JSON
    /// document and honours `If-None-Match`.
    pub async fn browse_page_validated_async(
        &self, library: &str, media: Media, section: &Section, page: u32, limit: u32, etag: Option<&str>,
    ) -> Result<Validated<Page>, Error> {
        let planned = plan(library, media, section, page, limit)?;
        let Some((response, tag)) = self.browse_page_async(&planned, etag).await? else { return Ok(Validated::Unchanged) };
        let mut parser = if planned.paging == Paging::Preview && !matches!(section, Section::Search(_)) {
            stream::Parser::home(section.home().ok_or(Error::Protocol)?, limit)
        } else {
            stream::Parser::window(stream::Plan { path: planned.keys.clone(),
                start: if planned.paging == Paging::Snapshot { u64::from(page) * u64::from(limit) } else { 0 }, limit, rows: planned.rows })
        };
        // Socket waits stay in this future. Each parser job consumes only one
        // bounded, already-decoded chunk and returns its resumable projection.
        let mut body = response.into_decoded();
        while let Some(chunk) = body.chunk().await {
            let chunk = chunk.map_err(|_| Error::Protocol)?;
            parser = tokio::task::spawn_blocking(move || { parser.feed(&chunk)?; Ok::<_, Error>(parser) })
                .await.map_err(|_| Error::Network)??;
        }
        let w = tokio::task::spawn_blocking(move || parser.finish()).await.map_err(|_| Error::Network)??;
        Ok(Validated::Changed(finish_page(w, &planned, library, media, section, page, limit)?, tag))
    }

    /// Every personalized shelf of `sections` (each [`Section::is_home`])
    /// from one `personalized` document: the sync asked for the same
    /// document once per shelf (four times for a book library).
    pub async fn browse_home_validated_async(
        &self, library: &str, media: Media, sections: &[Section], limit: u32, etag: Option<&str>,
    ) -> Result<Validated<Vec<Page>>, Error> {
        if sections.is_empty() || sections.len() > 8 || sections.iter().any(|s| !s.is_home()) { return protocol(); }
        let planned = plan(library, media, &sections[0], 0, limit)?;
        let Some((response, tag)) = self.browse_page_async(&planned, etag).await? else { return Ok(Validated::Unchanged) };
        let mut parsers = sections.iter().map(|s| Ok(stream::Parser::home(s.home().ok_or(Error::Protocol)?, limit))).collect::<Result<Vec<_>, Error>>()?;
        let mut body = response.into_decoded();
        while let Some(chunk) = body.chunk().await {
            let chunk = chunk.map_err(|_| Error::Protocol)?;
            parsers = tokio::task::spawn_blocking(move || {
                for parser in &mut parsers { parser.feed(&chunk)?; }
                Ok::<_, Error>(parsers)
            }).await.map_err(|_| Error::Network)??;
        }
        let windows = tokio::task::spawn_blocking(move || parsers.into_iter().map(stream::Parser::finish).collect::<Result<Vec<_>, Error>>())
            .await.map_err(|_| Error::Network)??;
        let pages = windows.into_iter().zip(sections).map(|(w, section)| {
            let planned = plan(library, media, section, 0, limit)?;
            finish_page(w, &planned, library, media, section, 0, limit)
        }).collect::<Result<Vec<_>, Error>>()?;
        Ok(Validated::Changed(pages, tag))
    }

    /// The library's newest items (`sort=addedAt&desc=1`) and its item
    /// count: the sync's marker for items added or removed. Audiobookshelf
    /// has no updated-at order (`sort=updatedAt` is ignored) and the
    /// library's `lastUpdate` does not move with its items.
    pub async fn newest_items_async(&self, library: &str, media: Media, limit: u32) -> Result<Page, Error> {
        let mut planned = plan(library, media, &Section::Books, 0, limit)?;
        planned.query.retain(|(k, _)| *k != "sort");
        planned.query.push(("sort", "addedAt".into()));
        planned.query.push(("desc", "1".into()));
        let Some((response, _)) = self.browse_page_async(&planned, None).await? else { return protocol() };
        let mut parser = stream::Parser::window(stream::Plan { path: planned.keys.clone(), start: 0, limit, rows: planned.rows });
        let mut body = response.into_decoded();
        while let Some(chunk) = body.chunk().await {
            let chunk = chunk.map_err(|_| Error::Protocol)?;
            parser = tokio::task::spawn_blocking(move || { parser.feed(&chunk)?; Ok::<_, Error>(parser) })
                .await.map_err(|_| Error::Network)??;
        }
        let w = tokio::task::spawn_blocking(move || parser.finish()).await.map_err(|_| Error::Network)??;
        finish_page(w, &planned, library, media, &Section::Books, 0, limit)
    }

    /// Sends a planned browse request. `None`: 304, the stored copy is current.
    async fn browse_page_async(&self, planned: &Planned, etag: Option<&str>) -> Result<Option<(crate::http::Response, Option<String>)>, Error> {
        let mut req = self.request("GET", &planned.path)?;
        for (k, v) in &planned.query {
            req = req.query(k, v);
        }
        if let Some(etag) = etag.filter(|e| e.len() <= 256 && e.is_ascii()) { req = req.set("If-None-Match", etag); }
        let response = req.send(hyper::body::Bytes::new()).await.map_err(network)?;
        if response.not_modified() { return Ok(None); }
        let tag = response.header("etag").filter(|e| e.len() <= 256 && e.is_ascii()).map(str::to_owned);
        Ok(Some((response, tag)))
    }
}

/// A conditional fetch: the server's copy is the stored one, or it is new
/// (with its ETag, when the server sent one).
#[derive(Debug)]
pub enum Validated<T> { Unchanged, Changed(T, Option<String>) }

struct Planned {
    path: String,
    query: Vec<(&'static str, String)>,
    keys: Vec<&'static str>,
    rows: Rows,
    paging: Paging,
}

fn plan(library: &str, media: Media, section: &Section, page: u32, limit: u32) -> Result<Planned, Error> {
        if library.is_empty() || library.len() > 4096 || limit == 0 || limit > MAX_PAGE {
            return protocol();
        }
        let root = format!("api/libraries/{}", segment(library));
        let mut path = format!("{root}/items");
        let mut query = vec![
            ("limit", limit.to_string()),
            ("page", page.to_string()),
            ("minified", "1".into()),
        ];
        let mut keys = vec!["results"];
        let mut rows = Rows::Items;
        let mut paging = Paging::Server;
        let mut filter = None;
        match section {
            Section::Books => {
                query.push(("sort", "media.metadata.title".into()));
            }
            Section::Search(term) => {
                if page != 0 || term.trim().is_empty() || term.len() > 4096 { return protocol(); }
                path = format!("{root}/search");
                query.retain(|(key, _)| *key == "limit");
                query.push(("q", term.trim().into()));
                keys = vec![if media == Media::Book { "book" } else { "podcast" }];
                rows = Rows::SearchItems;
                paging = Paging::Preview;
            }
            Section::RecentlyAdded => {
                query.push(("filter", "recent".into()));
                query.push(("sort", "addedAt".into()));
                query.push(("desc", "1".into()));
            }
            Section::Series => {
                path = format!("{root}/series");
                rows = Rows::Series;
                query.push(("sort", "name".into()));
            }
            Section::Collections => {
                path = format!("{root}/collections");
                rows = Rows::Collections;
            }
            Section::Playlists => {
                path = format!("{root}/playlists");
                rows = Rows::Playlists;
            }
            Section::Authors => {
                path = format!("{root}/authors");
                rows = Rows::Authors;
                query.push(("sort", "name".into()));
            }
            Section::Genres => {
                path = format!("{root}/filterdata");
                keys = vec!["genres"];
                rows = Rows::Genres;
                paging = Paging::Snapshot;
            }
            Section::Narrators => {
                path = format!("{root}/narrators");
                keys = vec!["narrators"];
                rows = Rows::Narrators;
                paging = Paging::Snapshot;
            }
            Section::Collection(id) => {
                path = format!("api/collections/{}", segment(id));
                keys = vec!["books"];
                paging = Paging::Snapshot;
            }
            Section::Playlist(id) => {
                path = format!("api/playlists/{}", segment(id));
                keys = vec!["items"];
                rows = Rows::PlaylistItems;
                paging = Paging::Snapshot;
            }
            Section::Episodes(id) => {
                path = format!("api/items/{}", segment(id));
                keys = vec!["media", "episodes"];
                rows = Rows::PodcastEpisodes;
                paging = Paging::Snapshot;
            }
            Section::LatestEpisodes => {
                path = format!("{root}/recent-episodes");
                keys = vec!["episodes"];
                rows = Rows::Episodes;
            }
            Section::ByGenre(v) => { filter = Some(("genres", v)); query.push(("sort", "addedAt".into())); }
            Section::ByNarrator(v) => { filter = Some(("narrators", v)); query.push(("sort", "addedAt".into())); }
            Section::ByAuthor(v) => { filter = Some(("authors", v)); query.push(("sort", "addedAt".into())); }
            Section::BySeries(v) => {
                filter = Some(("series", v));
                query.push(("sort", "sequence".into()));
            }
            _ => {
                if page != 0 {
                    return protocol();
                }
                paging = Paging::Preview;
                path = format!("{root}/personalized");
                query.retain(|(k, _)| *k == "limit");
                if matches!(section, Section::RecentSeries) {
                    rows = Rows::Series;
                }
            }
        }
        if let Some((group, value)) = filter {
            query.push(("filter", format!("{group}.{}", base64(value.as_bytes()))));
        }
        if paging == Paging::Snapshot {
            query.clear();
        }
        Ok(Planned { path, query, keys, rows, paging })
}

fn finish_page(w: Window, planned: &Planned, library: &str, media: Media, section: &Section, page: u32, limit: u32) -> Result<Page, Error> {
        let (rows, paging) = (planned.rows, planned.paging);
        if w.library.as_deref().is_some_and(|id| id != library)
            || w.page.is_some_and(|p| p != page)
            || w.limit.is_some_and(|n| n != limit)
        {
            return protocol();
        }
        if paging == Paging::Server && w.count > u64::from(limit) {
            return protocol();
        }
        let total = if paging == Paging::Snapshot {
            Some(w.count)
        } else {
            w.total
        };
        let has_more = match paging {
            Paging::Preview => false,
            Paging::Snapshot => u64::from(page) * u64::from(limit) + u64::from(limit) < w.count,
            Paging::Server => total
                .map(|n| (u64::from(page) + 1) * u64::from(limit) < n)
                .unwrap_or(w.count == u64::from(limit)),
        };
        let facts: Vec<Facts> = w.rows.iter().map(Facts::of).collect();
        let facts = if facts.iter().all(|f| *f == Facts::default()) { Vec::new() } else { facts };
        let entries = w
            .rows
            .into_iter()
            .map(|r| {
                entry(
                    r,
                    rows,
                    media,
                    library,
                    if let Section::Episodes(id) = section {
                        Some(id.as_str())
                    } else {
                        None
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Page {
            entries,
            total,
            has_more,
            paging,
            facts,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transient_http_browse_failures_are_server_errors_not_unreachable() {
        for status in [408,429,500,503,599] {
            let response=crate::http::Response::synthetic(status);
            assert!(matches!(network(crate::http::Error::Status(status,response)),Error::Server));
        }
        for (status,expected) in [(401,"auth"),(403,"auth"),(404,"missing"),(418,"protocol")] {
            let response=crate::http::Response::synthetic(status);
            let actual=network(crate::http::Error::Status(status,response));
            assert!(matches!((expected,actual),("auth",Error::Auth)|("missing",Error::Missing)|("protocol",Error::Protocol)));
        }
    }
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };
    struct Fixture {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
        accepted: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }
    fn fixture_header(stream: &mut TcpStream, stop: &AtomicBool) -> Option<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut data = Vec::new();
        while !data.ends_with(b"\r\n\r\n") {
            if stop.load(Ordering::Relaxed) || Instant::now() >= deadline || data.len() >= 16 * 1024 {
                return None;
            }
            let mut byte = [0; 1];
            match stream.read(&mut byte) {
                Ok(0) => return None,
                Ok(_) => data.push(byte[0]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return None,
            }
        }
        Some(data)
    }
    fn fixture() -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let (s, r, a) = (stop.clone(), requests.clone(), accepted.clone());
        let worker = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(50)))
                            .unwrap();
                        stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                        a.fetch_add(1, Ordering::Relaxed);
                        let Some(data) = fixture_header(&mut stream, &s) else {
                            continue;
                        };
                        let input = String::from_utf8(data).unwrap();
                        assert!(input.contains("Authorization: Bearer generated"));
                        let url = input.split_whitespace().nth(1).unwrap().to_string();
                        r.lock().unwrap().push(url.clone());
                        let parsed = url::Url::parse(&format!("http://fixture{url}")).unwrap();
                        let q = parsed
                            .query_pairs()
                            .into_owned()
                            .collect::<std::collections::HashMap<_, _>>();
                        let page = q
                            .get("page")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0);
                        let limit = q
                            .get("limit")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(50);
                        let lib = parsed.path().split('/').nth(3).unwrap_or("a");
                        let body = if parsed.path() == "/api/libraries" {
                            serde_json::json!({"libraries":[{"id":"a","name":"A","mediaType":"book"},{"id":"b","name":"B","mediaType":"book"},{"id":"p","name":"P","mediaType":"podcast"}]})
                        } else if parsed.path().ends_with("/personalized") {
                            serde_json::json!([{"id":"discover","type":"book","total":1000,"entities":(0..limit).map(|n|serde_json::json!({"id":format!("d{n}"),"libraryId":lib,"media":{"metadata":{"title":format!("Discover {n}")}}})).collect::<Vec<_>>()}])
                        } else if parsed.path().ends_with("/search") {
                            serde_json::json!({"book":[{"libraryItem":{"id":"search-book","libraryId":lib,"media":{"metadata":{"title":"Generated Search Match"}}}}],"authors":[]})
                        } else if parsed.path().ends_with("/items") {
                            serde_json::json!({"page":page,"limit":limit,"total":1_000_000,"results":(page*limit..((page+1)*limit).min(1_000_000)).map(|n|serde_json::json!({"id":format!("{lib}-{n}"),"libraryId":lib,"mediaType":"book","media":{"metadata":{"title":format!("Book {n}")},"numTracks":1}})).collect::<Vec<_>>()})
                        } else {
                            serde_json::json!({"unexpected":true})
                        };
                        let bytes = body.to_string();
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            bytes.len(),
                            bytes
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        Fixture {
            base,
            requests,
            accepted,
            stop,
            worker: Some(worker),
        }
    }
    #[test]
    fn fixture_shutdown_joins_after_accepted_idle_or_incomplete_header() {
        for partial in [
            b"".as_slice(),
            b"GET /unfinished HTTP/1.1\r\nHost: generated".as_slice(),
        ] {
            let f = fixture();
            let mut stalled = TcpStream::connect(f.base.strip_prefix("http://").unwrap()).unwrap();
            stalled.write_all(partial).unwrap();
            let accepted_deadline = Instant::now() + Duration::from_secs(2);
            while f.accepted.load(Ordering::Relaxed) == 0 {
                assert!(
                    Instant::now() < accepted_deadline,
                    "fixture did not accept generated stalled client"
                );
                thread::sleep(Duration::from_millis(1));
            }
            let requests = f.requests.clone();
            let (done, completed) = std::sync::mpsc::channel();
            let shutdown = thread::spawn(move || {
                drop(f);
                let _ = done.send(());
            });
            let result = completed.recv_timeout(Duration::from_secs(2));
            // Close only this generated client before joining, so a regressed
            // blocking header read is released even when the deadline fails.
            drop(stalled);
            let joined = shutdown.join();
            assert!(result.is_ok(), "fixture shutdown did not join promptly");
            assert!(joined.is_ok());
            assert!(requests.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn search_uses_bounded_native_preview_and_encodes_query_once() {
        let fixture = fixture();
        let client = Client::new(&fixture.base, "generated", None).unwrap();
        let term = "日本語 & book/+";
        let result = client.browse_page("a", Media::Book, &Section::Search(term.into()), 0, 50).unwrap();
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].title, "Generated Search Match");
        assert_eq!(result.entries[0].kind, EntryKind::Book {item: "search-book".into()});
        assert_eq!(result.paging, Paging::Preview); assert!(!result.has_more);
        let requests = fixture.requests.lock().unwrap();
        let url = url::Url::parse(&format!("http://fixture{}", requests[0])).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("q").map(String::as_str), Some(term));
        assert!(!query.contains_key("page"));
        drop(requests);
        assert!(client.browse_page("a", Media::Book, &Section::Search(term.into()), 1, 50).is_err());
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }
    #[test]
    fn million_item_catalog_uses_only_requested_server_pages_and_scopes_libraries() {
        let f = fixture();
        let c = Client::new(&f.base, "generated", None).unwrap();
        assert_eq!(c.browse_libraries(Media::Book).unwrap().len(), 2);
        assert_eq!(c.browse_libraries(Media::Podcast).unwrap()[0].id, "p");
        let rng = fastrand::Rng::with_seed(0xB205E);
        let mut rng = rng;
        for i in 0..120 {
            let page = if i == 0 { 19999 } else { rng.u32(0..20000) };
            let lib = if i % 2 == 0 { "a" } else { "b" };
            let data = c
                .browse_page(lib, Media::Book, &Section::Books, page, 50)
                .unwrap();
            assert_eq!(data.entries.len(), 50);
            assert_eq!(data.entries[0].id, format!("{lib}-{}", page as u64 * 50));
            assert_eq!(data.total, Some(1_000_000));
            assert_eq!(data.has_more, page < 19999);
            assert_eq!(data.paging, Paging::Server);
        }
        assert!(
            f.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.contains("/items?"))
                .all(|s| s.contains("limit=50") && s.contains("page="))
        );
    }
    #[test]
    fn personalized_is_honest_preview_and_filter_value_is_encoded_once() {
        let f = fixture();
        let c = Client::new(&f.base, "generated", None).unwrap();
        let preview = c
            .browse_page("a", Media::Book, &Section::Discover, 0, 12)
            .unwrap();
        assert_eq!(preview.entries.len(), 12);
        assert_eq!(preview.total, Some(1000));
        assert!(!preview.has_more);
        assert_eq!(preview.paging, Paging::Preview);
        assert!(
            c.browse_page("a", Media::Book, &Section::Discover, 1, 12)
                .is_err()
        );
        c.browse_page("a", Media::Book, &Section::ByGenre("A./ +é".into()), 0, 10)
            .unwrap();
        let requests = f.requests.lock().unwrap();
        let u = url::Url::parse(&format!("http://fixture{}", requests.last().unwrap())).unwrap();
        assert_eq!(
            u.query_pairs().find(|(k, _)| k == "filter").unwrap().1,
            "genres.QS4vICvDqQ=="
        );
        assert!(
            c.browse_page("a", Media::Book, &Section::Books, 0, 0)
                .is_err()
        );
        assert!(
            c.browse_page("a", Media::Book, &Section::Books, 0, 51)
                .is_err()
        );
    }
    #[test]
    fn snapshot_stream_projects_only_page_rows_and_ignores_large_nested_members() {
        let body=format!("{{\"books\":[{}],\"libraryId\":\"a\"}}",(0..10000).map(|i|format!("{{\"id\":\"{i}\",\"media\":{{\"metadata\":{{\"title\":\"Book {i}\"}},\"episodes\":[{{\"description\":\"{}\"}}]}}}}","x".repeat(128))).collect::<Vec<_>>().join(","));
        let w = decode(
            body.as_bytes(),
            WindowSeed {
                path: &["books"],
                start: 5000,
                limit: 50,
                rows: Rows::Items,
            },
        )
        .unwrap();
        assert_eq!(w.rows.len(), 50);
        assert_eq!(w.count, 10000);
        assert_eq!(text(&w.rows[0].id), "5000");
        assert_eq!(w.library.as_deref(), Some("a"));
        let groups=br#"{"results":[{"id":"c","name":"Collection","books":[{"id":"x","media":{"description":"ignored"}},{"id":"y"}]}],"total":1,"page":0,"limit":50}"#;
        let w = decode(
            &groups[..],
            WindowSeed {
                path: &["results"],
                start: 0,
                limit: 50,
                rows: Rows::Collections,
            },
        )
        .unwrap();
        let e = entry(
            w.rows.into_iter().next().unwrap(),
            Rows::Collections,
            Media::Book,
            "a",
            None,
        )
        .unwrap();
        assert_eq!(e.detail, "2 items");
        assert!(matches!(
            e.kind,
            EntryKind::Group {
                section: Section::Collection(_)
            }
        ));
    }
    #[test]
    fn podcast_episodes_playlist_members_and_facet_destinations_keep_identity() {
        // PodcastEpisode.toOldJSON uses "episode" for RSS episode numbering,
        // while playlist item wrappers use that same key for an episode object.
        // A populated metadata number must not replace the stable episode id.
        for number in [serde_json::json!("42"), serde_json::json!(42)] {
            let raw = serde_json::json!({"id":"episode-uuid", "libraryItemId":"podcast-uuid",
                "title":"Numbered Episode", "episode":number, "audioFile":{"duration":12}});
            let row: Row = serde_json::from_value(raw).expect("valid podcast episode numbering");
            let episode = entry(row, Rows::Episodes, Media::Podcast, "p", None).unwrap();
            assert_eq!(episode.kind, EntryKind::Episode { item:"podcast-uuid".into(), episode:"episode-uuid".into() });
        }
        let body=br#"{"libraryId":"p","media":{"episodes":[{"id":"not-downloaded","title":"Later","audioFile":null},{"id":"ep","title":"Downloaded","audioFile":{"path":"generated"}}]}}"#;
        let w = decode(
            &body[..],
            WindowSeed {
                path: &["media", "episodes"],
                start: 0,
                limit: 50,
                rows: Rows::PodcastEpisodes,
            },
        )
        .unwrap();
        assert_eq!(w.count, 1);
        let ep = entry(
            w.rows.into_iter().next().unwrap(),
            Rows::PodcastEpisodes,
            Media::Podcast,
            "p",
            Some("podcast-id"),
        )
        .unwrap();
        assert_eq!(
            ep.kind,
            EntryKind::Episode {
                item: "podcast-id".into(),
                episode: "ep".into()
            }
        );
        let row=serde_json::from_str::<Row>(r#"{"libraryItem":{"id":"podcast-id","libraryId":"p","media":{"metadata":{"title":"Podcast"}}},"episode":{"id":"ep","title":"Episode"}}"#).unwrap();
        let member = entry(row, Rows::PlaylistItems, Media::Podcast, "p", None).unwrap();
        assert_eq!(member.title, "Episode");
        assert_eq!(member.detail, "");
        assert_eq!(member.kind, ep.kind);
        let narrator =
            serde_json::from_str::<Row>(r#"{"id":"encoded-id","name":"Name + /é","numBooks":25}"#)
                .unwrap();
        let group = entry(narrator, Rows::Narrators, Media::Book, "a", None).unwrap();
        assert_eq!(
            group.kind,
            EntryKind::Group {
                section: Section::ByNarrator("Name + /é".into())
            }
        );
        let home=br#"[{"entities":[{"id":"podcast-id","media":{"metadata":{"title":"Podcast"}},"recentEpisode":{"id":"ep","title":"Episode"}}],"total":25,"id":"newest-episodes"}]"#;
        let w = decode(
            &home[..],
            HomeSeed {
                id: "newest-episodes",
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(w.total, Some(25));
        assert_eq!(
            entry(
                w.rows.into_iter().next().unwrap(),
                Rows::Items,
                Media::Podcast,
                "p",
                None
            )
            .unwrap()
            .kind,
            ep.kind
        );
    }

    #[test]
    fn episode_numbering_never_replaces_uuid_in_lists_recent_shelves_or_playlists() {
        let mut rng = fastrand::Rng::with_seed(0xE9150DE);
        for i in 0..1024 {
            let number = rng.u32(..).to_string();
            let episode_id = format!("episode-uuid-{i}");
            let episode = serde_json::json!({"id":episode_id,"libraryItemId":"show-uuid", "episode":number,
                "title":"Generated Episode", "audioFile":{"duration":12}});
            let show = serde_json::json!({"id":"show-uuid", "libraryId":"p", "mediaType":"podcast",
                "media":{"metadata":{"title":"Generated Show"}}, "recentEpisode":episode});
            let playlist = serde_json::json!({"libraryItem":{"id":"show-uuid", "libraryId":"p",
                "media":{"metadata":{"title":"Generated Show"}}}, "episode":episode});
            for (wire, kind, hint) in [(episode.clone(), Rows::Episodes, None),
                                       (episode, Rows::PodcastEpisodes, Some("show-uuid")),
                                       (show, Rows::Items, None), (playlist, Rows::PlaylistItems, None)] {
                let row: Row = serde_json::from_value(wire).unwrap();
                let actual = entry(row, kind, Media::Podcast, "p", hint).unwrap();
                assert_eq!(actual.kind, EntryKind::Episode { item:"show-uuid".into(), episode:episode_id.clone() });
            }
        }
        for malformed in [serde_json::json!(true), serde_json::json!([]), serde_json::json!("x".repeat(4097))] {
            assert!(serde_json::from_value::<Row>(serde_json::json!({"episode":malformed})).is_err());
        }
        let row: Row = serde_json::from_value(serde_json::json!({"libraryItem":{"id":"show"}, "episode":{}})).unwrap();
        assert!(entry(row, Rows::PlaylistItems, Media::Podcast, "p", None).is_err());
    }

    #[test]
    fn malformed_wrong_scope_and_absent_arrays_are_errors() {
        assert!(
            decode(
                &b"{\"results\":["[..],
                WindowSeed {
                    path: &["results"],
                    start: 0,
                    limit: 50,
                    rows: Rows::Items
                }
            )
            .is_err()
        );
        assert!(
            decode(
                &b"{}"[..],
                WindowSeed {
                    path: &["results"],
                    start: 0,
                    limit: 50,
                    rows: Rows::Items
                }
            )
            .is_err()
        );
        let r = serde_json::from_str::<Row>(
            r#"{"id":"x","libraryId":"other","media":{"metadata":{"title":"X"}}}"#,
        )
        .unwrap();
        assert!(entry(r, Rows::Items, Media::Book, "a", None).is_err());
    }
    #[test]
    fn episode_metadata_has_compact_utc_date_number_and_duration() {
        let episode = serde_json::json!({"id":"ep-uuid", "libraryItemId":"show-uuid", "title":"The Saga of Sagas", "publishedAt":1723593600000u64, "season":"4", "episode":"21", "duration":7077.1, "audioFile":{}});
        let row: Row = serde_json::from_value(episode.clone()).unwrap();
        let projected = entry(row, Rows::Episodes, Media::Podcast, "p", None).unwrap();
        assert_eq!(projected.title, "The Saga of Sagas");
        assert_eq!(projected.detail, "August 14, 2024 · S4 E21 · 1:57:57");
        assert_eq!(projected.kind, EntryKind::Episode { item:"show-uuid".into(), episode:"ep-uuid".into() });
        let nested = serde_json::json!({"id":"show-uuid", "recentEpisode":episode});
        let projected = entry(serde_json::from_value(nested).unwrap(), Rows::Items, Media::Podcast, "p", None).unwrap();
        assert_eq!(projected.detail, "August 14, 2024 · S4 E21 · 1:57:57");
        for (wire, expected) in [
            (serde_json::json!({"id":"e", "libraryItemId":"p", "title":"No metadata"}), ""),
            (serde_json::json!({"id":"e", "libraryItemId":"p", "title":"Part", "episode":21, "audioFile":{"duration":62.0}}), "E21 · 1:02"),
            (serde_json::json!({"id":"e", "libraryItemId":"p", "title":"Season", "season":4, "episode":"", "duration":0}), "S4"),
        ] {
            let projected = entry(serde_json::from_value(wire).unwrap(), Rows::Episodes, Media::Podcast, "p", None).unwrap();
            assert_eq!(projected.detail, expected);
        }
    }

}
