//! The streamed parse (document.rs) against the reference parse of a whole
//! JSON tree (`parse_envelope` and `Call::parse`), on generated documents:
//! optional and missing fields, unknown fields, unicode, escapes, long
//! strings, empty lists, error envelopes, any split into pieces, and the
//! response cache's stored copy. Malformed and truncated input must fail
//! cleanly. Heap bounds are in tests/document_memory.rs (dhat).
use super::{Parser, Streamed};
use crate::client::parse_envelope;
use crate::{Call, Error, Result, calls};
use proptest::prelude::*;
use serde_json::{Value, json};

/// Strings with escapes, controls, multi-byte and astral characters, and
/// occasionally long ones.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => "[a-zA-Z0-9 ]{0,24}",
        3 => "[\\PC]{0,16}",
        2 => prop::collection::vec(prop::sample::select(vec!['"', '\\', '/', '\n', '\t', '\u{1}', 'é', '中', '🎵', '\u{2028}']), 0..12)
            .prop_map(|c| c.into_iter().collect()),
        1 => "[a-z]{2000,6000}",
    ]
}

/// A JSON value no model knows: nested arrays and objects included.
fn unknown() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![Just(Value::Null), any::<bool>().prop_map(Value::from), any::<i64>().prop_map(Value::from),
                           any::<f64>().prop_filter("finite", |f| f.is_finite()).prop_map(Value::from), text().prop_map(Value::from)];
    leaf.prop_recursive(3, 16, 4, |inner| prop_oneof![
        prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
        prop::collection::btree_map("[a-z]{1,6}", inner, 0..4).prop_map(|m| Value::Object(m.into_iter().collect())),
    ])
}

/// An object with `known` fields each present or absent, plus unknown keys.
fn record(known: Vec<(&'static str, BoxedStrategy<Value>)>) -> impl Strategy<Value = Value> {
    let fields: Vec<_> = known.into_iter().map(|(k, s)| prop::option::weighted(0.7, s).prop_map(move |v| (k, v))).collect();
    (fields, prop::collection::btree_map("x[a-z]{1,5}", unknown(), 0..3)).prop_map(|(fields, extra)| {
        let mut map = serde_json::Map::new();
        for (k, v) in fields { if let Some(v) = v { map.insert(k.into(), v); } }
        for (k, v) in extra { map.insert(k, v); }
        Value::Object(map)
    })
}

fn s() -> BoxedStrategy<Value> { text().prop_map(Value::from).boxed() }
fn n() -> BoxedStrategy<Value> { (0u32..100_000).prop_map(Value::from).boxed() }
fn f() -> BoxedStrategy<Value> { (-30.0f32..10.0).prop_map(|x| json!(x)).boxed() }

fn song() -> impl Strategy<Value = Value> {
    let gain = record(vec![("trackGain", f()), ("albumGain", f()), ("trackPeak", f()), ("albumPeak", f())]).boxed();
    record(vec![("id", s()), ("parent", s()), ("title", s()), ("album", s()), ("artist", s()), ("albumArtist", s()),
                ("track", n()), ("discNumber", n()), ("year", n()), ("genre", s()), ("coverArt", s()), ("size", n()),
                ("contentType", s()), ("suffix", s()), ("duration", n()), ("bitRate", n()), ("samplingRate", n()),
                ("bitDepth", n()), ("channelCount", n()), ("albumId", s()), ("artistId", s()), ("starred", s()),
                ("userRating", (0u32..6).prop_map(Value::from).boxed()), ("playCount", n()), ("replayGain", gain)])
}
fn album() -> impl Strategy<Value = Value> {
    record(vec![("id", s()), ("name", s()), ("artist", s()), ("artistId", s()), ("coverArt", s()), ("songCount", n()),
                ("duration", n()), ("year", n()), ("genre", s()), ("created", s()), ("starred", s()), ("playCount", n())])
}
fn artist() -> impl Strategy<Value = Value> {
    record(vec![("id", s()), ("name", s()), ("albumCount", n()), ("coverArt", s()), ("starred", s()),
                ("artistImageUrl", s()), ("sortName", s())])
}
fn list<S: Strategy<Value = Value>>(item: S, max: usize) -> impl Strategy<Value = Option<Value>> {
    prop::option::weighted(0.85, prop::collection::vec(item, 0..max).prop_map(Value::from))
}

/// Writes `value` with its object keys in a shuffled order and whitespace
/// between tokens, so the streamed scan sees layouts serde_json never emits.
fn next(seed: &mut u64) -> usize {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (*seed >> 33) as usize
}

fn space(out: &mut String, n: usize) {
    for _ in 0..n % 3 { out.push([' ', '\n', '\t', '\r'][n % 4]); }
}

fn layout(value: &Value, seed: &mut u64, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            for i in (1..keys.len()).rev() { keys.swap(i, next(seed) % (i + 1)); }
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 { out.push(','); }
                space(out, next(seed));
                out.push_str(&serde_json::to_string(k).unwrap());
                space(out, next(seed));
                out.push(':');
                space(out, next(seed));
                layout(&map[*k], seed, out);
                space(out, next(seed));
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 { out.push(','); }
                space(out, next(seed));
                layout(v, seed, out);
                space(out, next(seed));
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).unwrap()),
    }
}

fn document(field: &str, body: Value, extra: Value, seed: u64) -> String {
    let mut inner = json!({"status": "ok", "version": "1.16.1", "type": "navidrome"});
    inner[field] = body;
    if let (Value::Object(inner), Value::Object(extra)) = (&mut inner, extra) {
        for (k, v) in extra { inner.entry(format!("x{k}")).or_insert(v); }
    }
    let mut text = String::new();
    layout(&json!({"subsonic-response": inner}), &mut seed.clone(), &mut text);
    text
}

/// The reference: the whole tree, then the call's field.
fn reference<T: Streamed>(call: &Call<T>, text: &str) -> Result<T> {
    call.parse(&parse_envelope(text)?)
}

/// Feeds `text` split at `cuts`.
fn streamed<T: Streamed>(call: &Call<T>, text: &[u8], cuts: &[usize]) -> Result<T> {
    feed(call.parser(), text, cuts)
}

fn feed<T: Streamed>(mut parser: Parser<T>, text: &[u8], cuts: &[usize]) -> Result<T> {
    let mut at = 0;
    let mut cuts: Vec<_> = cuts.iter().map(|c| c % (text.len() + 1)).collect();
    cuts.sort();
    for cut in cuts.into_iter().chain([text.len()]) {
        parser.feed(&text[at..cut.max(at)])?;
        at = cut.max(at);
    }
    parser.finish()
}

fn same<T: Streamed + PartialEq + std::fmt::Debug>(call: &Call<T>, text: &str, cuts: &[usize]) -> std::result::Result<(), TestCaseError> {
    let expected = reference(call, text);
    let got = streamed(call, text.as_bytes(), cuts);
    match (&got, &expected) {
        (Ok(a), Ok(b)) => prop_assert_eq!(a, b),
        (Err(Error::Api { code: a, message: m }), Err(Error::Api { code: b, message: n })) => { prop_assert_eq!(a, b); prop_assert_eq!(m, n); }
        (Err(Error::Json(_)), Err(Error::Json(_))) => {}
        _ => prop_assert!(false, "streamed {:?} but reference {:?}", got, expected),
    }
    Ok(())
}

/// The stored copy reads back as the same value, also in pieces.
fn stored<T: Streamed + serde::Serialize + PartialEq + std::fmt::Debug>(call: &Call<T>, text: &str, cuts: &[usize]) -> std::result::Result<(), TestCaseError> {
    if let Ok(value) = reference(call, text) {
        let mut copy = Vec::new();
        call.write_stored(&value, &mut copy).unwrap();
        prop_assert_eq!(&feed(call.stored_parser(), &copy, cuts).unwrap(), &value);
        prop_assert_eq!(call.parse_reader(&copy[..]).unwrap(), value);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn starred_matches_the_reference(artists in list(artist(), 6), albums in list(album(), 6), songs in list(song(), 12),
                                     extra in unknown(), seed in any::<u64>(), cuts in prop::collection::vec(any::<usize>(), 0..8)) {
        let mut body = serde_json::Map::new();
        for (k, v) in [("artist", artists), ("album", albums), ("song", songs)] { if let Some(v) = v { body.insert(k.into(), v); } }
        let text = document("starred2", Value::Object(body), extra, seed);
        same(&calls::starred(), &text, &cuts)?;
        stored(&calls::starred(), &text, &cuts)?;
    }

    #[test]
    fn playlist_matches_the_reference(head in record(vec![("id", s()), ("name", s()), ("owner", s()), ("songCount", n()),
                                          ("duration", n()), ("coverArt", s()), ("changed", s()), ("comment", s())]),
                                      entries in list(song(), 16), seed in any::<u64>(), cuts in prop::collection::vec(any::<usize>(), 0..8)) {
        let mut body = head;
        if let Some(entries) = entries { body["entry"] = entries; }
        let text = document("playlist", body, json!({}), seed);
        same(&calls::playlist("p"), &text, &cuts)?;
        stored(&calls::playlist("p"), &text, &cuts)?;
    }

    #[test]
    fn artists_index_matches_the_reference(groups in prop::collection::vec((text(), list(artist(), 6)), 0..6),
                                           seed in any::<u64>(), cuts in prop::collection::vec(any::<usize>(), 0..8)) {
        let index: Vec<Value> = groups.into_iter().map(|(name, artists)| {
            let mut g = json!({"name": name});
            if let Some(a) = artists { g["artist"] = a; }
            g
        }).collect();
        let text = document("artists", json!({"ignoredArticles": "The", "index": index}), json!({}), seed);
        same(&calls::artists(), &text, &cuts)?;
        stored(&calls::artists(), &text, &cuts)?;
    }

    #[test]
    fn paged_lists_and_play_queue_match_the_reference(albums in list(album(), 10), songs in list(song(), 10),
                                                      seed in any::<u64>(), cuts in prop::collection::vec(any::<usize>(), 0..8)) {
        let mut a = json!({}); if let Some(v) = albums { a["album"] = v; }
        let text = document("albumList2", a, json!({}), seed);
        same(&calls::album_list(crate::AlbumListType::Newest, 10, 0), &text, &cuts)?;
        let mut q = json!({"current": "x", "position": 5}); if let Some(v) = songs.clone() { q["entry"] = v; }
        let text = document("playQueue", q, json!({}), seed);
        same(&calls::play_queue(), &text, &cuts)?;
        let mut r = json!({}); if let Some(v) = songs { r["song"] = v; }
        let text = document("searchResult3", r, json!({}), seed);
        same(&calls::search("q", 1, 1, 1, 0), &text, &cuts)?;
    }

    #[test]
    fn error_envelopes_match_the_reference(code in prop::option::of(0u32..100), message in prop::option::of(text()),
                                           status in prop_oneof![Just("failed"), Just("error"), Just("")], seed in any::<u64>()) {
        let mut error = json!({});
        if let Some(c) = code { error["code"] = c.into(); }
        if let Some(m) = message { error["message"] = m.into(); }
        let mut text = String::new();
        layout(&json!({"subsonic-response": {"status": status, "version": "1.16.1", "error": error, "starred2": {"song": []}}}),
               &mut seed.clone(), &mut text);
        same(&calls::starred(), &text, &[])?;
        same(&calls::ping(), &text, &[])?;
        let refused = matches!(streamed(&calls::starred(), text.as_bytes(), &[]), Err(Error::Api { .. }));
        prop_assert!(refused);
    }

    /// Every proper prefix of a document is an error, never a panic.
    #[test]
    fn truncation_fails_cleanly(songs in list(song(), 6), seed in any::<u64>(), cut in any::<usize>()) {
        let mut body = json!({}); if let Some(v) = songs { body["song"] = v; }
        let text = document("starred2", body, json!({}), seed);
        let cut = cut % text.len();
        prop_assert!(streamed(&calls::starred(), &text.as_bytes()[..cut], &[]).is_err());
    }

    /// Corrupted bytes never panic, and the parse agrees with the reference
    /// on whether the document is acceptable.
    #[test]
    fn corruption_fails_cleanly(songs in list(song(), 6), seed in any::<u64>(),
                                flips in prop::collection::vec((any::<usize>(), any::<u8>()), 1..6)) {
        let mut body = json!({}); if let Some(v) = songs { body["song"] = v; }
        let mut bytes = document("starred2", body, json!({}), seed).into_bytes();
        for (at, b) in flips { let n = bytes.len(); bytes[at % n] = b; }
        let got = streamed(&calls::starred(), &bytes, &[]);
        // Invalid UTF-8 is an error here; the reference replaced it.
        let Ok(text) = std::str::from_utf8(&bytes) else { return Ok(()) };
        if duplicated(text) { return Ok(()); }
        let expected = reference(&calls::starred(), text);
        match (&got, &expected) {
            (Ok(a), Ok(b)) => prop_assert_eq!(a, b),
            (Err(_), Err(_)) => {}
            _ => prop_assert!(false, "streamed {:?} but reference {:?}", got.is_ok(), expected.is_ok()),
        }
    }
}

/// A corrupted byte can duplicate a key; the reference keeps the last value
/// and the streamed parse the first. Such documents are not compared.
fn duplicated(text: &str) -> bool {
    ["\"song\"", "\"starred2\"", "\"subsonic-response\"", "\"status\"", "\"error\""].iter().any(|k| text.matches(k).count() > 1)
}

#[test]
fn stored_bodies_from_earlier_versions_still_parse() {
    // Up to V0.123 the cache held the inner body object.
    let old = r#"{"status":"ok","version":"1.16.1","starred2":{"song":[{"id":"a","title":"T"}]}}"#;
    let s = calls::starred().parse_reader(old.as_bytes()).unwrap();
    assert_eq!(s.song[0].id, "a");
    let ping = calls::ping().parse_reader(&br#"{"generated":"old-account"}"#[..]).unwrap();
    // A network document must have the envelope.
    assert!(matches!(calls::starred().parse_text(old.as_bytes()), Err(Error::Json(m)) if m.contains("subsonic-response")));
    assert_eq!(ping["generated"], "old-account");
}

#[test]
fn malformed_structure_is_refused() {
    for bad in ["", "{", "}", "[1,]", "{\"a\":}", "{\"a\" 1}", "{\"a\":1,}", "{,}", "[1 2]", "{\"subsonic-response\":{\"status\":\"ok\"}} x",
                "{\"subsonic-response\":{\"status\":\"ok\",\"starred2\":{\"song\":[{\"id\":\"a\"} {\"id\":\"b\"}]}}}",
                "{\"subsonic-response\":{\"status\":\"ok\",\"starred2\":{\"song\":[1]}}}", "\"\u{1}\""] {
        assert!(calls::starred().parse_text(bad.as_bytes()).is_err(), "{bad:?}");
    }
    let deep = "[".repeat(100) + &"]".repeat(100);
    assert!(calls::ping().parse_text(deep.as_bytes()).is_err());
}

#[test]
fn limits_bound_records_skeleton_and_document() {
    let long = "a".repeat(super::RECORD_LIMIT + 10);
    let doc = format!(r#"{{"subsonic-response":{{"status":"ok","starred2":{{"song":[{{"id":"{long}"}}]}}}}}}"#);
    assert!(matches!(calls::starred().parse_text(doc.as_bytes()), Err(Error::Json(m)) if m.contains("record too large")));
    let long = "a".repeat(super::SKELETON_LIMIT + 10);
    let doc = format!(r#"{{"subsonic-response":{{"status":"ok","x":"{long}","starred2":{{}}}}}}"#);
    assert!(matches!(calls::starred().parse_text(doc.as_bytes()), Err(Error::Json(m)) if m.contains("outside its lists")));
    let mut parser = Parser::<crate::Starred>::with_limit(Some("starred2"), "getStarred2", 16);
    assert!(parser.feed(&[b' '; 17]).is_err());
}

/// Parse time of a 5,000-song getStarred2 (Navidrome 0.64 fields), streamed
/// in 32 KiB pieces against the earlier whole-text `Value` parse. Host only;
/// run with --release --ignored --nocapture.
#[test]
#[ignore = "a host measurement, run by hand with --release"]
fn streamed_parse_time() {
    let song = |i: usize| format!(r#"{{"id":"s{i:021}","parent":"al{i:020}","isDir":false,"title":"Generated title {i}","album":"Generated album {}","artist":"Generated artist {}","track":3,"year":2015,"genre":"Tidewater","coverArt":"mf-s{i:021}_0","size":35454321,"contentType":"audio/flac","suffix":"flac","starred":"2026-11-21T05:20:08.82848537Z","duration":241,"bitRate":917,"path":"a/b/c.flac","created":"2026-11-21T05:08:53.337680984Z","albumId":"al{i:020}","artistId":"ar{i:020}","type":"music","bpm":0,"comment":"","sortName":"x","mediaType":"song","musicBrainzId":"","isrc":[],"genres":[{{"name":"Tidewater"}}],"replayGain":{{"trackGain":-6.5}},"channelCount":2,"samplingRate":44100,"bitDepth":16,"moods":[],"artists":[{{"id":"ar1","name":"A"}}],"displayArtist":"A","albumArtists":[],"displayAlbumArtist":"A","contributors":[],"displayComposer":"","explicitStatus":""}}"#, i / 12, i / 120);
    let songs = (0..5000).map(song).collect::<Vec<_>>().join(",");
    let text = format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","starred2":{{"song":[{songs}]}}}}}}"#);
    let call = calls::starred();
    let time = |f: &dyn Fn() -> usize| {
        let mut runs: Vec<u128> = (0..15).map(|_| { let t = std::time::Instant::now(); assert_eq!(f(), 5000); t.elapsed().as_micros() }).collect();
        runs.sort();
        runs[7]
    };
    let streamed = time(&|| { let mut p = call.parser(); for piece in text.as_bytes().chunks(32 * 1024) { p.feed(piece).unwrap(); } p.finish().unwrap().song.len() });
    let tree = time(&|| call.parse(&parse_envelope(&text).unwrap()).unwrap().song.len());
    eprintln!("5,000 songs, {:.1} MiB: streamed {streamed} us, whole text and Value {tree} us", text.len() as f64 / 1048576.0);
}
