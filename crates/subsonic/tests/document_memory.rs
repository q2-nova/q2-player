//! Heap bounds of the streamed document parse, measured with dhat (its own
//! test binary: dhat replaces the global allocator, and one test runs at a
//! time). Documents are generated with proptest's runner from Navidrome
//! 0.64-shaped records (every OpenSubsonic field it sends, most of which the
//! player does not declare), then parsed in 32 KiB pieces as they arrive.
//!
//! Properties, for getStarred2, getPlaylist, getPlayQueue, getArtists and a
//! paged getAlbumList2:
//! - Streamed into a sink that keeps no records (as the album catalog
//!   streams pages into the database), peak heap does not grow with N: it is
//!   one record and the skeleton.
//! - Collected into the typed result, peak heap is the result plus at most
//!   a quarter of it (lists grow by a quarter) and the parser's buffers. It
//!   grows with the records kept, not with document bytes; the earlier
//!   whole-text plus `serde_json::Value` parse peaked at several document
//!   lengths.
//! - Bytes per record of the typed result do not grow past recorded ceilings.
//!
//! N is 1,000 and 10,000 records by default; `Q2_DHAT_LARGE=1` adds 100,000.
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use subsonic::document::Scanner;
use subsonic::{Call, Streamed, calls};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// One Navidrome song as 0.64 sends it, with generated text.
fn song_json(id: &str, title: &str, album: &str, artist: &str, n: u32) -> String {
    format!(r#"{{"id":"{id}","parent":"al{n:06}","isDir":false,"title":"{title}","album":"{album}","artist":"{artist}","track":{t},"year":2015,"genre":"Tidewater","coverArt":"mf-{id}_0","size":35454321,"contentType":"audio/flac","suffix":"flac","starred":"2026-11-21T05:20:08.82848537Z","duration":241,"bitRate":917,"path":"{artist}/{album}/{t:02} - {title}.flac","created":"2026-11-21T05:08:53.337680984Z","albumId":"al{n:06}","artistId":"ar{n:06}","type":"music","bpm":0,"comment":"","sortName":"{title}","mediaType":"song","musicBrainzId":"","isrc":[],"genres":[{{"name":"Tidewater"}}],"replayGain":{{"trackGain":-6.5,"albumGain":-7.1,"trackPeak":0.98}},"channelCount":2,"samplingRate":44100,"bitDepth":16,"moods":[],"artists":[{{"id":"ar{n:06}","name":"{artist}"}}],"displayArtist":"{artist}","albumArtists":[{{"id":"ar{n:06}","name":"{artist}"}}],"displayAlbumArtist":"{artist}","contributors":[],"displayComposer":"","explicitStatus":"","groupings":[],"works":[],"movements":[]}}"#,
        t = n % 20 + 1)
}

fn album_json(id: &str, name: &str, artist: &str, n: u32) -> String {
    format!(r#"{{"id":"{id}","name":"{name}","artist":"{artist}","artistId":"ar{n:06}","coverArt":"al-{id}_0","songCount":12,"duration":2900,"created":"2026-11-21T05:05:45.43296606Z","starred":"2026-11-21T05:29:40.856393728Z","year":1978,"genre":"Coastal","userRating":0,"genres":[{{"name":"Coastal"}}],"musicBrainzId":"","isCompilation":false,"sortName":"{name}","discTitles":[],"originalReleaseDate":{{}},"releaseDate":{{}},"releaseTypes":[],"recordLabels":[],"moods":[],"artists":[{{"id":"ar{n:06}","name":"{artist}"}}],"displayArtist":"{artist}","explicitStatus":"","version":""}}"#)
}

fn artist_json(id: &str, name: &str) -> String {
    format!(r#"{{"id":"{id}","name":"{name}","albumCount":5,"coverArt":"ar-{id}_0","artistImageUrl":"https://example.invalid/{id}.jpg","starred":"2026-11-21T05:29:41.02402794Z","musicBrainzId":"","sortName":"{name}","roles":["albumartist","artist"]}}"#)
}

/// Generated names: printable text of the lengths real tags have.
fn names(count: usize, seed: u64) -> Vec<String> {
    let mut runner = TestRunner::new_with_rng(Default::default(), proptest::test_runner::TestRng::from_seed(
        proptest::test_runner::RngAlgorithm::ChaCha, &{ let mut s = [0u8; 32]; s[..8].copy_from_slice(&seed.to_le_bytes()); s }));
    let strategy = "[A-Za-z][A-Za-z ]{4,30}";
    (0..count).map(|_| strategy.new_tree(&mut runner).unwrap().current().trim().to_string()).collect()
}

fn envelope(field: &str, body: &str) -> String {
    format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,"{field}":{body}}}}}"#)
}

fn songs(n: usize, seed: u64) -> String {
    let text = names(64, seed);
    (0..n).map(|i| song_json(&format!("s{i:08}x{seed:04}"), &text[i % 64], &text[(i / 7) % 64], &text[(i / 13) % 64], i as u32))
        .collect::<Vec<_>>().join(",")
}

fn documents(n: usize) -> Vec<(&'static str, String)> {
    let text = names(64, n as u64);
    let albums = (0..n / 10).map(|i| album_json(&format!("a{i:08}"), &text[i % 64], &text[(i / 3) % 64], i as u32)).collect::<Vec<_>>().join(",");
    let artists = (0..n / 30).map(|i| artist_json(&format!("r{i:08}"), &text[i % 64])).collect::<Vec<_>>().join(",");
    let groups = (0..26).map(|g| {
        let members = (0..n / 10 / 26).map(|i| artist_json(&format!("r{g:02}{i:06}"), &text[(g + i) % 64])).collect::<Vec<_>>().join(",");
        format!(r#"{{"name":"{}","artist":[{members}]}}"#, (b'A' + g as u8) as char)
    }).collect::<Vec<_>>().join(",");
    let page = (0..500).map(|i| album_json(&format!("p{i:08}"), &text[i % 64], &text[(i / 3) % 64], i as u32)).collect::<Vec<_>>().join(",");
    vec![
        ("getStarred2", envelope("starred2", &format!(r#"{{"artist":[{artists}],"album":[{albums}],"song":[{}]}}"#, songs(n, 1)))),
        ("getPlaylist", envelope("playlist", &format!(r#"{{"id":"pl","name":"Generated","owner":"q2","public":false,"songCount":{n},"duration":1,"created":"2026-11-21T05:05:45Z","changed":"2026-11-21T05:05:45Z","entry":[{}]}}"#, songs(n, 2)))),
        ("getPlayQueue", envelope("playQueue", &format!(r#"{{"current":"s00000001x0003","position":1000,"username":"q2","changed":"2026-11-21T05:05:45Z","changedBy":"q2","entry":[{}]}}"#, songs(n, 3)))),
        ("getArtists", envelope("artists", &format!(r#"{{"ignoredArticles":"The El La Los Las Le Les","index":[{groups}]}}"#))),
        ("getAlbumList2", envelope("albumList2", &format!(r#"{{"album":[{page}]}}"#))),
    ]
}

struct Measured { peak: usize, retained: usize, records: u64 }

/// Peak and retained heap while parsing `text` into the call's result.
fn collect<T: Streamed>(call: &Call<T>, text: &[u8]) -> (Measured, usize) {
    let profiler = dhat::Profiler::builder().testing().build();
    let mut parser = call.parser();
    for piece in text.chunks(32 * 1024) { parser.feed(piece).unwrap(); }
    let value = parser.finish().unwrap();
    let stats = dhat::HeapStats::get();
    let measured = Measured { peak: stats.max_bytes, retained: stats.curr_bytes, records: 0 };
    drop(value);
    drop(profiler);
    (measured, text.len())
}

/// Peak heap while scanning `text` into a sink that parses each record and
/// keeps none (the album catalog's page-by-page store).
fn sink<T: Streamed>(call: &Call<T>, text: &[u8]) -> Measured {
    let field = call.field();
    let profiler = dhat::Profiler::builder().testing().build();
    let mut scanner = Scanner::new(field, T::shape()).network();
    let mut kept = T::default();
    let mut feed = |record: &[u8], target: usize| -> subsonic::Result<()> {
        // Parse it as the store would, then drop it.
        let mut one = T::default();
        one.push(target, 0, record).map_err(|e| subsonic::Error::Json(e.to_string()))?;
        drop(one);
        Ok(())
    };
    for piece in text.chunks(32 * 1024) { scanner.feed(piece, &mut |target, _, record| feed(record, target)).unwrap(); }
    let records = scanner.records();
    let skeleton = scanner.finish(&mut |target, _, record| feed(record, target)).unwrap();
    kept.merge(skeleton.parse::<T>("test").unwrap());
    let stats = dhat::HeapStats::get();
    drop(kept);
    drop(profiler);
    Measured { peak: stats.max_bytes, retained: stats.curr_bytes, records }
}

fn run<T: Streamed>(call: &Call<T>, name: &str, n: usize, text: &str, report: &mut Vec<String>) -> (Measured, Measured) {
    let (collected, bytes) = collect(call, text.as_bytes());
    let streamed = sink(call, text.as_bytes());
    report.push(format!("{name} N={n} doc={:.1}MiB records={} | collect: peak {:.2}MiB retained {:.2}MiB transient {:.2}MiB ({:.0}% of doc) | sink: peak {:.0}KiB",
        bytes as f64 / 1048576.0, streamed.records, collected.peak as f64 / 1048576.0, collected.retained as f64 / 1048576.0,
        (collected.peak - collected.retained) as f64 / 1048576.0, 100.0 * (collected.peak - collected.retained) as f64 / bytes as f64,
        streamed.peak as f64 / 1024.0));
    // Collected: beyond the result, at most a quarter of it (list growth)
    // and the parser's buffers (a 32 KiB piece, one record, the skeleton).
    let slack = collected.retained / 4;
    assert!(collected.peak <= collected.retained + slack + 512 * 1024,
            "{name} N={n}: peak {} retained {} slack {slack}", collected.peak, collected.retained);
    (collected, streamed)
}

#[test]
fn streamed_parse_heap_is_bounded() {
    let mut sizes = vec![1_000, 10_000];
    if std::env::var_os("Q2_DHAT_LARGE").is_some() { sizes.push(100_000); }
    let mut report = Vec::new();
    let mut sink_peaks: Vec<(String, usize, usize)> = Vec::new();
    let mut per_record: Vec<(String, f64)> = Vec::new();
    let song = std::mem::size_of::<subsonic::Song>();
    let album = std::mem::size_of::<subsonic::Album>();
    let artist = std::mem::size_of::<subsonic::Artist>();
    for &n in &sizes {
        for (name, text) in documents(n) {
            let (collected, streamed) = match name {
                "getStarred2" => run(&calls::starred(), name, n, &text, &mut report),
                "getPlaylist" => run(&calls::playlist("pl"), name, n, &text, &mut report),
                "getPlayQueue" => run(&calls::play_queue(), name, n, &text, &mut report),
                "getArtists" => run(&calls::artists(), name, n, &text, &mut report),
                _ => run(&calls::album_list(subsonic::AlbumListType::Newest, 500, 0), name, n, &text, &mut report),
            };
            sink_peaks.push((name.to_string(), n, streamed.peak));
            if streamed.records > 0 { per_record.push((format!("{name} N={n}"), collected.retained as f64 / streamed.records as f64)); }
        }
    }
    // Audiobookshelf: a podcast item (`api/items/:id`, the play session's
    // embedded item) with N episodes, scanned as `audiobookshelf::json_lists`
    // does, keeping none of them.
    use subsonic::document::Seg::Key;
    const ITEM: &[&[subsonic::document::Seg]] = &[&[Key("media"), Key("episodes")], &[Key("libraryFiles")]];
    let mut item_peaks = Vec::new();
    for &n in &sizes {
        let episodes = (0..n).map(|i| format!(r#"{{"id":"ep-{i}","title":"Episode {i}","description":"{}","duration":60.5,"audioFile":{{"ino":"{i}","metadata":{{"filename":"e{i}.mp3"}}}},"chapters":[]}}"#, "d".repeat(300)))
            .collect::<Vec<_>>().join(",");
        let text = format!(r#"{{"id":"pod","libraryId":"lib","mediaType":"podcast","media":{{"metadata":{{"title":"Generated"}},"episodes":[{episodes}]}},"libraryFiles":[]}}"#);
        let profiler = dhat::Profiler::builder().testing().build();
        let mut scanner = Scanner::new(None, ITEM);
        for piece in text.as_bytes().chunks(32 * 1024) { scanner.feed(piece, &mut |_, _, _| Ok(())).unwrap(); }
        let records = scanner.records();
        let skeleton = scanner.finish(&mut |_, _, _| Ok(())).unwrap();
        let value: serde_json::Value = serde_json::from_slice(skeleton.text()).unwrap();
        let peak = dhat::HeapStats::get().max_bytes;
        drop((value, skeleton));
        drop(profiler);
        assert_eq!(records, n as u64);
        report.push(format!("ABS podcast item N={n} doc={:.1}MiB | sink: peak {:.0}KiB", text.len() as f64 / 1048576.0, peak as f64 / 1024.0));
        item_peaks.push(peak);
    }
    assert!(item_peaks.iter().all(|p| *p <= item_peaks[0] + 16 * 1024 && *p <= 64 * 1024), "ABS item peaks {item_peaks:?}");
    for line in &report { eprintln!("{line}"); }
    for (name, bytes) in &per_record { eprintln!("bytes per record retained: {name}: {bytes:.0}"); }
    // Sink: no growth with N. At N and 10 N the peak is the same within a
    // record and the parser's buffers.
    for (name, n, peak) in &sink_peaks {
        let first = sink_peaks.iter().find(|(m, _, _)| m == name).unwrap().2;
        assert!(*peak <= first + 64 * 1024, "{name}: sink peak {peak} at N={n} grew from {first}");
        assert!(*peak <= 64 * 1024, "{name}: sink peak {peak} at N={n}");
    }
    // Record sizes (host, 64-bit) must not grow; update these only with a
    // reason. A Q2 record is about half (32-bit pointers and lengths).
    assert!(song <= 392, "Song is {song} bytes (624 before V0.124)");
    assert!(album <= 216, "Album is {album} bytes (232 before)");
    assert!(artist <= 128, "Artist is {artist} bytes (152 before)");
    for (name, bytes) in &per_record {
        // Measured 2026-11-21 (host): songs 522-547, artists 234-254, albums 348.
        let ceiling = if name.starts_with("getArtists") { 270.0 } else if name.starts_with("getAlbumList2") { 370.0 } else { 570.0 };
        assert!(*bytes <= ceiling, "{name}: {bytes:.0} bytes per record retained");
    }
}

