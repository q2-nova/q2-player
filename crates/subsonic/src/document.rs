//! Typed parsing of Subsonic documents as they arrive, without a complete
//! text or a `serde_json::Value` tree.
//!
//! A [`Scanner`] is fed the decoded body piece by piece (each piece in its own
//! short job, so no job owns a socket or waits for the network). It follows
//! the JSON structure byte by byte and hands each element of a record list
//! (the songs of `starred2`, the entries of a playlist, …) to a sink as soon
//! as the element is complete; the element is deserialized there into its
//! typed record. Everything outside the record lists (the envelope, status,
//! a playlist's own fields) is kept as a small skeleton text with those
//! lists emptied, and deserialized at the end. So the memory a document
//! needs beyond its typed result is one record and the skeleton, whatever
//! its length.
//!
//! The same scanner reads the response cache's stored copy (`{field: T}`,
//! without the envelope) and old cache entries (the inner body object).
use crate::{Error, Result};
use serde::Deserialize;
use serde::de::{self, DeserializeOwned, DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use std::fmt;

/// One step of a record list's path below the call's field object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seg {
    Key(&'static str),
    /// Any element of an array.
    Each,
}

/// A result type that can be assembled from streamed records. `shape` lists
/// the paths (below the call's field) of the arrays whose elements are
/// records; `push` deserializes one record into an accumulator, and `merge`
/// moves the accumulated records into the skeleton's result.
pub trait Streamed: DeserializeOwned + Default {
    fn shape() -> &'static [&'static [Seg]] { &[] }
    /// `target` indexes `shape`; `parent` is the position of the nearest
    /// enclosing `Each` element (0 when there is none).
    fn push(&mut self, _target: usize, _parent: u32, _record: &[u8]) -> serde_json::Result<()> { Ok(()) }
    fn merge(&mut self, _records: Self) {}
}

impl Streamed for serde_json::Value {}
impl<T: DeserializeOwned> Streamed for Vec<T> {}

/// The most bytes a document may have: a safety bound against a broken
/// server that never ends a document, not a library size limit (about
/// 230,000 Navidrome songs in one list). Memory follows the typed records
/// kept, not this figure (see the module comment).
pub const DOCUMENT_LIMIT: u64 = 256 * 1024 * 1024;
/// The most bytes outside record lists, and the most in one record.
pub const SKELETON_LIMIT: usize = 4 * 1024 * 1024;
pub const RECORD_LIMIT: usize = 1024 * 1024;
const DEPTH_LIMIT: usize = 64;
const KEY_LIMIT: usize = 128;

fn malformed<T>() -> Result<T> { Err(Error::Json("malformed document".into())) }

#[derive(Debug)]
enum Reach {
    Root,
    Key(Option<Box<str>>),
    Each(u32),
}

/// Where a container is in its grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// Object: a key or `}`; array: a value or `]`.
    First,
    /// After a comma: a key (object) or a value (array).
    Item,
    /// Object: `:` after a key.
    Colon,
    /// Object: a value after `:`.
    Value,
    /// After a value: `,` or the close.
    Next,
}

#[derive(Debug)]
struct Frame {
    array: bool,
    at: At,
    reach: Reach,
    /// Arrays: elements begun.
    count: u32,
    /// Objects: the last key.
    key: Option<Box<str>>,
    /// Arrays: which record list this is.
    target: Option<usize>,
}

struct Capture {
    target: usize,
    parent: u32,
    depth: usize,
}

/// A scalar in progress: a key or string, or a number or literal, which the
/// next delimiter ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scalar {
    None,
    Key,
    String,
    Bare,
}

type Sink<'a> = dyn FnMut(usize, u32, &[u8]) -> Result<()> + 'a;

/// The resumable structural scanner. See the module comment.
pub struct Scanner {
    field: Option<&'static str>,
    shape: &'static [&'static [Seg]],
    stack: Vec<Frame>,
    scalar: Scalar,
    escape: bool,
    key: Vec<u8>,
    key_escaped: bool,
    /// Whether the document has the `subsonic-response` envelope: always
    /// for a network document; for a stored copy, whether its first key is
    /// the envelope's (copies up to V0.123 stored the inner body).
    enveloped: Option<bool>,
    capture: Option<Capture>,
    record: Vec<u8>,
    skeleton: Vec<u8>,
    bytes: u64,
    limit: u64,
    done: bool,
    records: u64,
}

impl Scanner {
    pub fn new(field: Option<&'static str>, shape: &'static [&'static [Seg]]) -> Self {
        Self::with_limit(field, shape, DOCUMENT_LIMIT)
    }

    pub fn with_limit(field: Option<&'static str>, shape: &'static [&'static [Seg]], limit: u64) -> Self {
        Scanner { field, shape, stack: Vec::new(), scalar: Scalar::None, escape: false,
                  key: Vec::new(), key_escaped: false, enveloped: None, capture: None, record: Vec::new(),
                  skeleton: Vec::new(), bytes: 0, limit, done: false, records: 0 }
    }

    /// A network document: the envelope is required.
    pub fn network(mut self) -> Self { self.enveloped = Some(true); self }
    /// Bytes fed so far.
    pub fn bytes(&self) -> u64 { self.bytes }
    /// Records handed to the sink so far.
    pub fn records(&self) -> u64 { self.records }

    /// Feeds the next piece. `sink` receives each complete record's text.
    pub fn feed(&mut self, bytes: &[u8], sink: &mut Sink) -> Result<()> {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if self.bytes > self.limit { return Err(Error::Json("response too large".into())); }
        for &b in bytes { self.byte(b, sink)?; }
        Ok(())
    }

    /// The skeleton, once the document is complete.
    pub fn finish(mut self, sink: &mut Sink) -> Result<Skeleton> {
        if self.scalar == Scalar::Bare { self.end_value(sink)?; }
        if !self.done || self.scalar != Scalar::None || !self.stack.is_empty() { return malformed(); }
        Ok(Skeleton { text: self.skeleton, enveloped: self.enveloped == Some(true), field: self.field, bytes: self.bytes })
    }

    fn emit(&mut self, b: u8) -> Result<()> {
        if self.capture.is_some() {
            if self.record.len() >= RECORD_LIMIT { return Err(Error::Json("record too large".into())); }
            self.record.push(b);
        } else if !self.stack.last().is_some_and(|f| f.target.is_some()) {
            if self.skeleton.len() >= SKELETON_LIMIT { return Err(Error::Json("document outside its lists too large".into())); }
            self.skeleton.push(b);
        }
        Ok(())
    }

    /// A value begins at the current position.
    fn begin_value(&mut self) -> Result<()> {
        if self.done { return malformed(); }
        let depth = self.stack.len();
        let Some(top) = self.stack.last_mut() else { return Ok(()) };
        match (top.array, top.at) {
            (true, At::First | At::Item) => {
                top.count = top.count.saturating_add(1);
                if let (Some(target), None) = (top.target, &self.capture) {
                    let parent = self.stack.iter().rev().find_map(|f| match f.reach { Reach::Each(n) => Some(n), _ => None }).unwrap_or(0);
                    self.capture = Some(Capture { target, parent, depth });
                }
            }
            (false, At::Value) => {}
            _ => return malformed(),
        }
        Ok(())
    }

    /// The value at the current position ended (a scalar's last byte, or a
    /// container's close after its pop).
    fn end_value(&mut self, sink: &mut Sink) -> Result<()> {
        self.scalar = Scalar::None;
        match self.stack.last_mut() {
            Some(top) => top.at = At::Next,
            None => self.done = true,
        }
        if self.capture.as_ref().is_some_and(|c| c.depth == self.stack.len()) {
            let capture = self.capture.take().expect("a record in capture");
            self.records += 1;
            let result = sink(capture.target, capture.parent, &self.record);
            self.record.clear();
            // A record of hundreds of KiB need not stay reserved.
            if self.record.capacity() > 64 * 1024 { self.record = Vec::new(); }
            result?;
        }
        Ok(())
    }

    fn push(&mut self, array: bool) -> Result<()> {
        if self.stack.len() >= DEPTH_LIMIT { return Err(Error::Json("document too deep".into())); }
        let reach = match self.stack.last_mut() {
            None => Reach::Root,
            Some(f) if f.array => Reach::Each(f.count - 1),
            Some(f) => Reach::Key(f.key.take()),
        };
        let mut frame = Frame { array, at: At::First, reach, count: 0, key: None, target: None };
        if array && self.capture.is_none() { frame.target = self.target_of(&frame); }
        self.stack.push(frame);
        Ok(())
    }

    /// Whether an array reached as `frame` holds the records of a shape entry.
    fn target_of(&self, frame: &Frame) -> Option<usize> {
        if self.shape.is_empty() { return None; }
        // The path of `frame` below the root, envelope and field excluded
        // (below the root for a document without a field).
        let mut path: Vec<&Reach> = self.stack.iter().skip(1).map(|f| &f.reach).collect();
        path.push(&frame.reach);
        let key = |r: Option<&&Reach>, name: &str| matches!(r, Some(Reach::Key(Some(k))) if &**k == name);
        let mut path = &path[..];
        if self.enveloped == Some(true) {
            if !key(path.first(), "subsonic-response") { return None; }
            path = &path[1..];
        }
        if let Some(field) = self.field {
            if !key(path.first(), field) { return None; }
            path = &path[1..];
        }
        self.shape.iter().position(|shape| shape.len() == path.len() && shape.iter().zip(path).all(|(s, r)| match (s, r) {
            (Seg::Key(name), Reach::Key(Some(k))) => **k == **name,
            (Seg::Each, Reach::Each(_)) => true,
            _ => false,
        }))
    }

    fn key_done(&mut self) {
        let key = if self.key.len() > KEY_LIMIT { None }
            else if self.key_escaped {
                let mut quoted = Vec::with_capacity(self.key.len() + 2);
                quoted.push(b'"'); quoted.extend_from_slice(&self.key); quoted.push(b'"');
                serde_json::from_slice::<String>(&quoted).ok().map(String::into_boxed_str)
            } else { std::str::from_utf8(&self.key).ok().map(Box::from) };
        if self.enveloped.is_none() && self.stack.len() == 1 {
            self.enveloped = Some(key.as_deref() == Some("subsonic-response"));
        }
        if let Some(top) = self.stack.last_mut() { top.key = key; top.at = At::Colon; }
        self.scalar = Scalar::None;
    }

    fn byte(&mut self, b: u8, sink: &mut Sink) -> Result<()> {
        match self.scalar {
            Scalar::Key | Scalar::String => {
                self.emit(b)?;
                let key = self.scalar == Scalar::Key;
                if self.escape {
                    self.escape = false;
                    if key && self.key.len() <= KEY_LIMIT { self.key.push(b); }
                } else if b == b'\\' {
                    self.escape = true;
                    if key { self.key_escaped = true; if self.key.len() <= KEY_LIMIT { self.key.push(b); } }
                } else if b == b'"' {
                    if key { self.key_done(); } else { self.end_value(sink)?; }
                } else if b < 0x20 {
                    return malformed();
                } else if key && self.key.len() <= KEY_LIMIT {
                    self.key.push(b);
                }
                return Ok(());
            }
            Scalar::Bare if is_bare(b) => return self.emit(b),
            Scalar::Bare => self.end_value(sink)?,
            Scalar::None => {}
        }
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => self.emit(b),
            b'"' => {
                match self.stack.last_mut() {
                    Some(f) if !f.array && matches!(f.at, At::First | At::Item) => {
                        self.scalar = Scalar::Key;
                        self.key.clear();
                        self.key_escaped = false;
                    }
                    _ => { self.begin_value()?; self.scalar = Scalar::String; }
                }
                self.emit(b)
            }
            b'{' | b'[' => {
                self.begin_value()?;
                self.emit(b)?;
                self.push(b == b'[')
            }
            b'}' | b']' => {
                let Some(frame) = self.stack.pop() else { return malformed() };
                if frame.array != (b == b']') || !matches!(frame.at, At::First | At::Next) { return malformed(); }
                self.emit(b)?;
                self.end_value(sink)
            }
            b',' => {
                match self.stack.last_mut() {
                    Some(f) if f.at == At::Next => f.at = At::Item,
                    _ => return malformed(),
                }
                self.emit(b)
            }
            b':' => {
                match self.stack.last_mut() {
                    Some(f) if f.at == At::Colon => f.at = At::Value,
                    _ => return malformed(),
                }
                self.emit(b)
            }
            b if is_bare(b) => {
                self.begin_value()?;
                self.scalar = Scalar::Bare;
                self.emit(b)
            }
            _ => malformed(),
        }
    }
}

fn is_bare(b: u8) -> bool { b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.') }

/// The document outside its record lists, complete.
pub struct Skeleton {
    text: Vec<u8>,
    enveloped: bool,
    field: Option<&'static str>,
    bytes: u64,
}

impl Skeleton {
    /// Bytes of the whole document.
    pub fn bytes(&self) -> u64 { self.bytes }

    /// The document with its record lists emptied.
    pub fn text(&self) -> &[u8] { &self.text }

    /// The checked result: for an enveloped document a failed status is the
    /// server's error; a stored copy (no envelope) was checked when fetched.
    pub fn parse<T: DeserializeOwned>(&self, endpoint: &str) -> Result<T> {
        let json = |e: serde_json::Error| Error::Json(format!("{endpoint}: {e}"));
        let mut de = serde_json::Deserializer::from_slice(&self.text);
        let inner = if self.enveloped {
            Envelope::<T> { field: self.field, _t: std::marker::PhantomData }.deserialize(&mut de).map_err(json)?
                .ok_or_else(|| Error::Json("no subsonic-response".into()))?
        } else {
            Inner::<T> { field: self.field, _t: std::marker::PhantomData }.deserialize(&mut de).map_err(json)?
        };
        de.end().map_err(json)?;
        if self.enveloped && inner.status.as_deref() != Some("ok") {
            let error = inner.error.as_ref();
            let code = error.and_then(|e| e.get("code")).and_then(serde_json::Value::as_u64).unwrap_or(0) as u32;
            let message = error.and_then(|e| e.get("message")).and_then(serde_json::Value::as_str)
                .unwrap_or("request failed").to_string();
            return Err(Error::Api { code, message });
        }
        match inner.value {
            Some(value) => Ok(value),
            // An empty list comes back as a missing field or an empty object.
            None => serde_json::from_slice(b"{}").map_err(json),
        }
    }
}

struct Body<T> {
    status: Option<String>,
    error: Option<serde_json::Value>,
    value: Option<T>,
}

struct Envelope<T> { field: Option<&'static str>, _t: std::marker::PhantomData<fn() -> T> }
struct Inner<T> { field: Option<&'static str>, _t: std::marker::PhantomData<fn() -> T> }

impl<'de, T: DeserializeOwned> DeserializeSeed<'de> for Envelope<T> {
    type Value = Option<Body<T>>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> { d.deserialize_map(self) }
}
impl<'de, T: DeserializeOwned> Visitor<'de> for Envelope<T> {
    type Value = Option<Body<T>>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("a Subsonic document") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
        let mut body = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if key == "subsonic-response" && body.is_none() {
                body = Some(map.next_value_seed(Inner::<T> { field: self.field, _t: std::marker::PhantomData })?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(body)
    }
}
impl<'de, T: DeserializeOwned> DeserializeSeed<'de> for Inner<T> {
    type Value = Body<T>;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> {
        match self.field {
            Some(_) => d.deserialize_map(self),
            // The whole body is the result (small action and ping replies);
            // its status is read from the same tree.
            None => {
                let value = serde_json::Value::deserialize(d)?;
                let status = value.get("status").and_then(serde_json::Value::as_str).map(str::to_owned);
                let error = value.get("error").cloned();
                let value = T::deserialize(value).map_err(de::Error::custom)?;
                Ok(Body { status, error, value: Some(value) })
            }
        }
    }
}
impl<'de, T: DeserializeOwned> Visitor<'de> for Inner<T> {
    type Value = Body<T>;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("a Subsonic response body") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
        let mut body = Body { status: None, error: None, value: None };
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            match &*key {
                "status" if body.status.is_none() => body.status = map.next_value::<Option<String>>()?,
                "error" if body.error.is_none() => body.error = Some(map.next_value()?),
                k if Some(k) == self.field && body.value.is_none() => body.value = map.next_value::<Option<T>>()?,
                _ => { map.next_value::<IgnoredAny>()?; }
            }
        }
        Ok(body)
    }
}

/// A scanner and its typed accumulator: the resumable unit a body job owns.
pub struct Parser<T: Streamed> {
    scanner: Scanner,
    records: T,
    endpoint: &'static str,
}

impl<T: Streamed> Parser<T> {
    /// A network document, which must have the envelope and status `ok`.
    pub fn new(field: Option<&'static str>, endpoint: &'static str) -> Self {
        Self::with_limit(field, endpoint, DOCUMENT_LIMIT)
    }
    pub fn with_limit(field: Option<&'static str>, endpoint: &'static str, limit: u64) -> Self {
        Parser { scanner: Scanner::with_limit(field, T::shape(), limit).network(), records: T::default(), endpoint }
    }
    /// The response cache's copy: `{field: value}`, or an earlier version's
    /// stored inner body.
    pub fn stored(field: Option<&'static str>, endpoint: &'static str) -> Self {
        Parser { scanner: Scanner::new(field, T::shape()), records: T::default(), endpoint }
    }
    /// Bytes fed so far.
    pub fn bytes(&self) -> u64 { self.scanner.bytes() }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        let Parser { scanner, records, endpoint } = self;
        scanner.feed(bytes, &mut |target, parent, record| {
            records.push(target, parent, record).map_err(|e| Error::Json(format!("{endpoint}: {e}")))
        })
    }
    pub fn finish(self) -> Result<T> {
        let Parser { scanner, mut records, endpoint } = self;
        let skeleton = scanner.finish(&mut |target, parent, record| {
            records.push(target, parent, record).map_err(|e| Error::Json(format!("{endpoint}: {e}")))
        })?;
        let mut value: T = skeleton.parse(endpoint)?;
        value.merge(records);
        Ok(value)
    }
}

/// Parses a stored copy from a reader, in 16 KiB pieces (tests; the player
/// reads stored copies in the database owner, `Db::response_pieces`).
#[cfg(test)]
pub fn parse_reader<T: Streamed>(mut reader: impl std::io::Read, field: Option<&'static str>, endpoint: &'static str) -> Result<T> {
    let mut parser = Parser::<T>::stored(field, endpoint);
    let mut piece = [0u8; 16 * 1024];
    loop {
        let n = match reader.read(&mut piece) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        };
        parser.feed(&piece[..n])?;
    }
    parser.finish()
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;
