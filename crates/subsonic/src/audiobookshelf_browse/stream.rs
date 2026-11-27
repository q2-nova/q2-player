//! Resumable browse projection. Each feed consumes only already-buffered bytes;
//! no parser job owns a socket, channel reader, or a complete snapshot.
use super::{AudioFile, Count, Error, MAX_RESPONSE, Metadata, RawMedia, Row, Rows, Window, protocol};
#[path = "json.rs"]
mod json;
use json::{Lexer, Skip, Token};
#[derive(Clone)]
pub(super) struct Plan {
    pub path: Vec<&'static str>,
    pub start: u64,
    pub limit: u32,
    pub rows: Rows,
}
#[derive(Clone, Copy)]
enum Keep {
    Full,
    Basic,
    Episode,
    None,
}
impl Keep {
    fn fields(self) -> bool {
        matches!(self, Self::Full | Self::Basic)
    }
    fn episode(self) -> Self {
        if self.fields() {
            Self::Episode
        } else {
            Self::None
        }
    }
}
#[derive(Clone)]
enum Type {
    Window(Plan),
    Home(&'static str, u32),
    Shelf(u32),
    Row(Keep, bool),
    Episode(Keep),
    Scalar,
    Float,
    Audio,
    Media(Keep),
    Metadata(Keep),
    String(bool),
    U64,
    U32,
    Count,
    /// An array of strings (a book's genres), at most 64 kept.
    Strings,
    Ignore,
}
enum Value {
    Window(Window),
    Shelf(String, Window),
    Row(Option<Box<Row>>),
    Media(RawMedia),
    Metadata(Metadata),
    Audio(AudioFile),
    Scalar(Option<String>),
    Float(Option<f64>),
    String(Option<String>),
    Number(Option<u64>),
    Count(u64),
    Strings(Vec<String>),
    Present(bool),
}
enum Mode {
    Window(Plan, Window),
    Home {
        id: &'static str,
        limit: u32,
        result: Window,
        n: usize,
    },
    Shelf {
        limit: u32,
        id: String,
        w: Window,
        total: Option<u64>,
    },
    Row {
        row: Row,
        keep: Keep,
    },
    Audio { value: AudioFile },
    Media {
        value: RawMedia,
        keep: Keep,
    },
    Metadata {
        value: Metadata,
        keep: Keep,
    },
    Count(u64),
    Strings(Vec<String>),
}
const ROW_FIELDS: &[&str] = &[
    "id",
    "name",
    "title",
    "libraryId",
    "libraryItemId",
    "episodeId",
    "mediaType",
    "numBooks",
    "numEpisodes",
    "media",
    "podcast",
    "recentEpisode",
    "episode",
    "libraryItem",
    "audioFile",
    "books",
    "items",
    "season",
    "publishedAt",
    "duration",
    "addedAt",
];
const AUDIO_FIELDS: &[&str] = &["duration"];
const MEDIA_FIELDS: &[&str] = &["metadata", "numTracks"];
const META_FIELDS: &[&str] = &["title", "subtitle", "authorName", "seriesName", "narratorName", "genres"];
struct Frame {
    mode: Mode,
    state: u8,
    key: String,
    index: usize,
    seen: u32,
}
impl Frame {
    fn new(ty: Type, object: bool) -> Result<Self, Error> {
        let mode = match ty {
            Type::Window(plan) => {
                if !object && !plan.path.is_empty() {
                    return protocol();
                }
                let w = Window {
                    found: !object,
                    ..Default::default()
                };
                Mode::Window(plan, w)
            }
            Type::Home(id, limit) if !object => Mode::Home {
                id,
                limit,
                result: Window {
                    found: true,
                    ..Default::default()
                },
                n: 0,
            },
            Type::Shelf(limit) if object => Mode::Shelf {
                limit,
                id: String::new(),
                w: Window::default(),
                total: None,
            },
            Type::Row(keep, _) => Mode::Row {
                row: Row::default(),
                keep,
            },
            Type::Episode(keep) if object => Mode::Row {
                row: Row::default(),
                keep,
            },
            Type::Audio if object => Mode::Audio { value: AudioFile::default() },
            Type::Media(keep) => Mode::Media {
                value: RawMedia::default(),
                keep,
            },
            Type::Metadata(keep) => Mode::Metadata {
                value: Metadata::default(),
                keep,
            },
            Type::Count if !object => Mode::Count(0),
            Type::Strings if !object => Mode::Strings(Vec::new()),
            _ => return protocol(),
        };
        Ok(Self {
            mode,
            state: if object { 0 } else { 5 },
            key: String::new(),
            index: 0,
            seen: 0,
        })
    }
    fn field(&self) -> Result<&str, Error> {
        if self.state < 5 {
            return Ok(&self.key);
        }
        let names = match &self.mode {
            Mode::Row { .. } => ROW_FIELDS,
            Mode::Audio { .. } => AUDIO_FIELDS,
            Mode::Media { .. } => MEDIA_FIELDS,
            Mode::Metadata { .. } => META_FIELDS,
            _ => return Ok(""),
        };
        names.get(self.index).copied().ok_or(Error::Protocol)
    }
    fn child(&self) -> Result<Type, Error> {
        let key = self.field()?;
        Ok(match &self.mode {
            Mode::Window(plan, _) if self.state >= 5 => {
                if matches!(plan.rows, Rows::Genres) {
                    Type::String(false)
                } else {
                    Type::Row(Keep::Full, false)
                }
            }
            Mode::Window(plan, _) => {
                if plan.path.first().is_some_and(|p| *p == key) {
                    let mut nested = plan.clone();
                    nested.path.remove(0);
                    Type::Window(nested)
                } else {
                    match key {
                        "total" => Type::U64,
                        "page" | "limit" => Type::U32,
                        "libraryId" => Type::String(true),
                        _ => Type::Ignore,
                    }
                }
            }
            Mode::Home { limit, .. } => Type::Shelf(*limit),
            Mode::Shelf { limit, .. } => match key {
                "id" => Type::String(false),
                "total" => Type::U64,
                "entities" => Type::Window(Plan {
                    path: vec![],
                    start: 0,
                    limit: *limit,
                    rows: Rows::Items,
                }),
                _ => Type::Ignore,
            },
            Mode::Row { keep, .. } => match key {
                "id" | "name" | "title" | "libraryId" | "libraryItemId" | "episodeId"
                | "mediaType" => Type::String(true),
                "numBooks" | "numEpisodes" | "publishedAt" | "addedAt" => Type::U64,
                "season" => Type::Scalar,
                "duration" => Type::Float,
                "audioFile" => Type::Audio,
                "media" | "podcast" => Type::Media(if keep.fields() { *keep } else { Keep::None }),
                "recentEpisode" => Type::Row(keep.episode(), true),
                "episode" => Type::Episode(keep.episode()),
                "libraryItem" => Type::Row(
                    if matches!(keep, Keep::Full) {
                        Keep::Basic
                    } else {
                        Keep::None
                    },
                    true,
                ),
                "books" | "items" => Type::Count,
                _ => Type::Ignore,
            },
            Mode::Audio { .. } => match key { "duration" => Type::Float, _ => Type::Ignore },
            Mode::Media { keep, .. } => match key {
                "metadata" => Type::Metadata(*keep),
                "numTracks" => Type::U64,
                _ => Type::Ignore,
            },
            Mode::Metadata { .. } => match key {
                "title" | "subtitle" | "authorName" | "seriesName" | "narratorName" => Type::String(true),
                "genres" => Type::Strings,
                _ => Type::Ignore,
            },
            Mode::Count(_) => Type::Ignore,
            Mode::Strings(_) => Type::String(false),
        })
    }
    fn begin(&mut self) -> Result<(), Error> {
        if !matches!(self.state, 3 | 5 | 6) {
            return protocol();
        }
        let fields = match &self.mode {
            Mode::Row { .. } => ROW_FIELDS,
            Mode::Audio { .. } => AUDIO_FIELDS,
            Mode::Media { .. } => MEDIA_FIELDS,
            Mode::Metadata { .. } => META_FIELDS,
            _ => &[],
        };
        if let Some(index) = fields
            .iter()
            .position(|name| Some(*name) == self.field().ok())
        {
            let mask = 1 << index;
            if self.seen & mask != 0 {
                return protocol();
            }
            self.seen |= mask;
        }
        self.state = if self.state == 3 { 4 } else { 7 };
        Ok(())
    }
    fn finish_value(&mut self, value: Value) -> Result<(), Error> {
        // The parent is in its after-value state, so field() still names this child.
        let key = self.field()?.to_owned();
        match &mut self.mode {
            Mode::Window(plan, w) if self.state >= 5 => {
                let row = match value {
                    Value::Row(Some(row)) => *row,
                    Value::String(Some(name)) => Row {
                        id: Some(name.clone()),
                        name: Some(name),
                        ..Default::default()
                    },
                    _ => return protocol(),
                };
                if !matches!(plan.rows, Rows::PodcastEpisodes) || row.audio_file.is_some() {
                    if w.count >= plan.start && w.rows.len() < plan.limit as usize {
                        w.rows.push(row);
                    }
                    w.count += 1;
                }
            }
            Mode::Window(plan, w) => {
                if plan.path.first().is_some_and(|p| *p == key) {
                    let Value::Window(nested) = value else {
                        return protocol();
                    };
                    w.rows = nested.rows;
                    w.count = nested.count;
                    w.found = nested.found;
                    if nested.total.is_some() {
                        w.total = nested.total;
                    }
                } else {
                    match (key.as_str(), value) {
                        ("total", Value::Number(v)) => w.total = v,
                        ("page", Value::Number(v)) => w.page = v.map(|n| n as u32),
                        ("limit", Value::Number(v)) => w.limit = v.map(|n| n as u32),
                        ("libraryId", Value::String(v)) => w.library = v,
                        (_, Value::Present(_)) => {}
                        _ => return protocol(),
                    }
                }
            }
            Mode::Home { id, result, n, .. } => {
                let Value::Shelf(shelf, w) = value else {
                    return protocol();
                };
                *n += 1;
                if *n > 32 {
                    return protocol();
                }
                if shelf == *id {
                    *result = w;
                }
            }
            Mode::Shelf { id, w, total, .. } => match (key.as_str(), value) {
                ("id", Value::String(Some(v))) => *id = v,
                ("total", Value::Number(v)) => *total = v,
                ("entities", Value::Window(v)) => *w = v,
                (_, Value::Present(_)) => {}
                _ => return protocol(),
            },
            Mode::Row { row, keep } => {
                let retain = keep.fields()
                    || matches!(keep, Keep::Episode) && matches!(key.as_str(), "id" | "title" | "episode" | "season" | "publishedAt" | "duration" | "audioFile");
                if retain {
                    match (key.as_str(), value) {
                        ("id", Value::String(v)) => row.id = v,
                        ("name", Value::String(v)) => row.name = v,
                        ("title", Value::String(v)) => row.title = v,
                        ("libraryId", Value::String(v)) => row.library_id = v,
                        ("libraryItemId", Value::String(v)) => row.library_item_id = v,
                        ("episodeId", Value::String(v)) => row.episode_id = v,
                        ("mediaType", Value::String(v)) => row.media_type = v,
                        ("numBooks", Value::Number(v)) => row.num_books = v,
                        ("numEpisodes", Value::Number(v)) => row.num_episodes = v,
                        ("media", Value::Media(v)) => row.media = v,
                        ("podcast", Value::Media(v)) => row.podcast = v,
                        ("recentEpisode", Value::Row(v)) => row.recent_episode = v,
                        ("episode", Value::Row(v)) => row.episode.item = v,
                        ("episode", Value::Scalar(v)) => row.episode.number = v,
                        ("season", Value::Scalar(v)) => row.season = v,
                        ("publishedAt", Value::Number(v)) => row.published_at = v,
                        ("addedAt", Value::Number(v)) => row.added_at = v,
                        ("duration", Value::Float(v)) => row.duration = v,
                        ("audioFile", Value::Audio(v)) => row.audio_file = Some(v),
                        ("libraryItem", Value::Row(v)) => row.library_item = v,
                        ("audioFile", Value::Present(v)) => {
                            row.audio_file = v.then_some(AudioFile::default())
                        }
                        ("books", Value::Count(v)) => row.books = Count(v),
                        ("items", Value::Count(v)) => row.items = Count(v),
                        (_, Value::Present(_)) => {}
                        _ => return protocol(),
                    }
                }
            }
            Mode::Audio { value: audio } => match (key.as_str(), value) {
                ("duration", Value::Float(v)) => audio.duration = v,
                (_, Value::Present(_)) => {},
                _ => return protocol(),
            },
            Mode::Media { value: media, keep } => {
                if keep.fields() {
                    match (key.as_str(), value) {
                        ("metadata", Value::Metadata(v)) => media.metadata = v,
                        ("numTracks", Value::Number(v)) => media.num_tracks = v,
                        (_, Value::Present(_)) => {}
                        _ => return protocol(),
                    }
                }
            }
            Mode::Metadata {
                value: metadata,
                keep,
            } => {
                if keep.fields() {
                    match (key.as_str(), value) {
                        ("title", Value::String(v)) => metadata.title = v,
                        ("subtitle", Value::String(_)) => {}
                        ("authorName", Value::String(v)) => metadata.author_name = v,
                        ("seriesName", Value::String(v)) => metadata.series_name = v,
                        ("narratorName", Value::String(v)) => metadata.narrator_name = v,
                        ("genres", Value::Strings(v)) => metadata.genres = v,
                        (_, Value::Present(_)) => {}
                        _ => return protocol(),
                    }
                }
            }
            Mode::Count(n) => *n += 1,
            Mode::Strings(list) => {
                if let Value::String(Some(v)) = value {
                    if list.len() < 64 && v.len() <= 4096 { list.push(v); }
                }
            }
        }
        if self.state >= 5 {
            self.index += 1;
        }
        Ok(())
    }
    fn finish(self) -> Value {
        match self.mode {
            Mode::Window(_, w) => Value::Window(w),
            Mode::Home { result, .. } => Value::Window(result),
            Mode::Shelf {
                id, mut w, total, ..
            } => {
                w.total = total;
                Value::Shelf(id, w)
            }
            Mode::Row { row, .. } => Value::Row(Some(Box::new(row))),
            Mode::Audio { value } => Value::Audio(value),
            Mode::Media { value, .. } => Value::Media(value),
            Mode::Metadata { value, .. } => Value::Metadata(value),
            Mode::Count(n) => Value::Count(n),
            Mode::Strings(list) => Value::Strings(list),
        }
    }
}
fn scalar(ty: Type, token: Token) -> Result<Value, Error> {
    match (ty, token) {
        (Type::String(true), Token::Null) => Ok(Value::String(None)),
        (Type::String(_), Token::String(s)) => Ok(Value::String(Some(s))),
        (Type::U64 | Type::U32, Token::Null) => Ok(Value::Number(None)),
        (kind @ (Type::U64 | Type::U32), Token::Number(bytes, false)) => {
            let n = serde_json::from_slice::<u64>(&bytes).map_err(|_| Error::Protocol)?;
            if matches!(kind, Type::U32) && n > u32::MAX.into() {
                return protocol();
            }
            Ok(Value::Number(Some(n)))
        }
        (Type::Count, Token::Null) => Ok(Value::Count(0)),
        (Type::Strings, Token::Null) => Ok(Value::Strings(Vec::new())),
        (Type::Float, Token::Null) => Ok(Value::Float(None)),
        (Type::Float, Token::Number(bytes, false)) => Ok(Value::Float(Some(serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?))),
        (Type::Scalar, Token::Null) => Ok(Value::Scalar(None)),
        (Type::Scalar, Token::String(s)) if s.len() <= 4096 => Ok(Value::Scalar(Some(s))),
        (Type::Scalar, Token::Number(bytes, false)) => {
            let n: serde_json::Number = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
            if !n.is_i64() && !n.is_u64() { return protocol(); }
            Ok(Value::Scalar(Some(n.to_string())))
        },
        (Type::Audio, Token::Null) => Ok(Value::Present(false)),
        (Type::Audio, _) => Ok(Value::Present(true)),
        (Type::Row(_, true) | Type::Episode(_), Token::Null) => Ok(Value::Row(None)),
        (Type::Episode(_), Token::String(s)) if s.len() <= 4096 => Ok(Value::Scalar(Some(s))),
        (Type::Episode(_), Token::Number(bytes, false)) => {
            let value: serde_json::Number =
                serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
            if !value.is_i64() && !value.is_u64() {
                return protocol();
            }
            Ok(Value::Scalar(Some(value.to_string())))
        }
        _ => protocol(),
    }
}
pub(super) struct Parser {
    root: Type,
    frames: Vec<Frame>,
    skip: Option<(Skip, bool)>,
    lexer: Lexer,
    result: Option<Window>,
    bytes: u64,
}
impl Parser {
    pub(super) fn window(plan: Plan) -> Self {
        Self::new(Type::Window(plan))
    }
    pub(super) fn home(id: &'static str, limit: u32) -> Self {
        Self::new(Type::Home(id, limit))
    }
    fn new(root: Type) -> Self {
        Self {
            root,
            frames: vec![],
            skip: None,
            lexer: Lexer::default(),
            result: None,
            bytes: 0,
        }
    }
    fn expected(&self) -> Result<Type, Error> {
        if let Some(frame) = self.frames.last() {
            frame.child()
        } else if self.result.is_none() {
            Ok(self.root.clone())
        } else {
            protocol()
        }
    }
    fn strict(&self) -> bool {
        self.skip.is_none()
            && (self
                .frames
                .last()
                .is_some_and(|f| matches!(f.state, 0 | 1 | 2 | 4 | 7))
                || !matches!(self.expected(), Ok(Type::Ignore)))
    }
    fn finish_value(&mut self, value: Value) -> Result<(), Error> {
        if let Some(frame) = self.frames.last_mut() {
            frame.finish_value(value)
        } else {
            let Value::Window(w) = value else {
                return protocol();
            };
            self.result = Some(w);
            Ok(())
        }
    }
    fn token(&mut self, token: Token) -> Result<(), Error> {
        if let Some((skip, present)) = &mut self.skip {
            if skip.token(&token)? {
                let value = Value::Present(*present);
                self.skip = None;
                self.finish_value(value)?;
            }
            return Ok(());
        }
        if let Some(frame) = self.frames.last_mut() {
            match &token {
                Token::Colon if frame.state == 2 => {
                    frame.state = 3;
                    return Ok(());
                }
                Token::Comma if frame.state == 4 || frame.state == 7 => {
                    frame.state = if frame.state == 4 { 1 } else { 6 };
                    return Ok(());
                }
                Token::ObjectEnd if matches!(frame.state, 0 | 4) => {
                    let value = self.frames.pop().unwrap().finish();
                    return self.finish_value(value);
                }
                Token::ArrayEnd if matches!(frame.state, 5 | 7) => {
                    let value = self.frames.pop().unwrap().finish();
                    return self.finish_value(value);
                }
                Token::String(key) if matches!(frame.state, 0 | 1) => {
                    frame.key = key.clone();
                    frame.state = 2;
                    return Ok(());
                }
                _ => {}
            }
        }
        let ty = self.expected()?;
        if let Some(frame) = self.frames.last_mut() {
            frame.begin()?;
        }
        if matches!(ty, Type::Ignore) || matches!((&ty, &token), (Type::Audio, Token::Array)) {
            let mut skip = Skip::default();
            let present = !matches!(token, Token::Null);
            if skip.token(&token)? {
                self.finish_value(Value::Present(present))?;
            } else {
                self.skip = Some((skip, present));
            }
            return Ok(());
        }
        match token {
            Token::Object | Token::Array => {
                if self.frames.len() >= 127 {
                    return protocol();
                }
                self.frames
                    .push(Frame::new(ty, matches!(token, Token::Object))?);
                Ok(())
            }
            other => {
                let value = scalar(ty, other)?;
                self.finish_value(value)
            }
        }
    }
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Protocol)?;
        if self.bytes > MAX_RESPONSE {
            return protocol();
        }
        let mut at = 0;
        while at < bytes.len() {
            let strict = self.lexer.idle() && (self.frames.is_empty() || self.strict());
            let (consumed, token) = self.lexer.byte(bytes[at], strict)?;
            if consumed {
                at += 1;
            }
            if let Some(token) = token {
                self.token(token)?;
            }
        }
        Ok(())
    }
    pub(super) fn finish(mut self) -> Result<Window, Error> {
        if let Some(token) = self.lexer.finish()? {
            self.token(token)?;
        }
        if !self.frames.is_empty() || self.skip.is_some() {
            return protocol();
        }
        let w = self.result.ok_or(Error::Protocol)?;
        if !w.found {
            return protocol();
        }
        Ok(w)
    }
}
#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
