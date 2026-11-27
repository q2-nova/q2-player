//! Damaged files through the player's own probe, demux, decode and PCM
//! conversion. The release player is built with `panic = "abort"`, so a
//! panic anywhere under `Engine::step` ends the process: a damaged cached
//! file or a cut transfer must come back as an error or as skipped packets.
//!
//! Each fixture (`fixtures/`, generated with ffmpeg, 0.25 s or 0.35 s
//! of stereo at 44.1 kHz; `fixtures/README.md`) is damaged by a fixed-seed generator: bit flips, runs
//! of 0x00/0xff/random bytes, truncation, deleted and repeated ranges, with
//! half of the edits aimed at the first 512 bytes. A case fails when it
//! panics, decodes a packet larger than `MAX_PACKET_FRAMES`, reads more
//! packets than `MAX_PACKETS`, or asks the heap for more than
//! `MAX_ALLOCATION` in one request (`test_alloc`): a count field read from a
//! damaged file must not size an allocation. The Q2 has 128 MB, and a
//! refused allocation aborts the player as a panic does.
use super::*;
type Outcome<T> = std::result::Result<T, String>;
use std::io::Cursor;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error;
use symphonia::core::formats::{SeekMode, SeekTo};
use symphonia::core::units::Time;

/// Name, extension, bytes and the frames ffmpeg encoded.
const FIXTURES: &[(&str, &str, &[u8], usize)] = &[
    ("mp3", "mp3", include_bytes!("fixtures/tone.mp3"), 15435),
    ("flac16", "flac", include_bytes!("fixtures/tone.flac"), 11025),
    ("flac24", "flac", include_bytes!("fixtures/tone24.flac"), 11025),
    ("aac", "m4a", include_bytes!("fixtures/tone-aac.m4a"), 15435),
    ("alac", "m4a", include_bytes!("fixtures/tone-alac.m4a"), 11025),
    ("vorbis", "ogg", include_bytes!("fixtures/tone.ogg"), 15435),
];
/// No packet of the fixtures is near this.
const MAX_PACKET_FRAMES: usize = 1 << 18;
const MAX_PACKETS: usize = 20_000;
/// The fixtures are under 32 KiB and their decoders' largest buffer is
/// under 256 KiB (`undamaged_fixtures_decode_every_frame` checks that).
const MAX_ALLOCATION: usize = 4 << 20;

/// The MP3 fixture behind an ID3v2.3 tag with a title and an attached
/// picture of `picture` bytes, as tagged music has; the fixture has no tag.
fn tagged_mp3(picture: usize) -> Vec<u8> {
    fn frame(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend((body.len() as u32).to_be_bytes());
        out.extend([0, 0]);
        out.extend(body);
        out
    }
    let mut apic = b"\0image/png\0\x03cover\0".to_vec();
    apic.extend(image(picture));
    let mut frames = frame(b"TIT2", b"\0Generated tone");
    frames.extend(frame(b"TPE1", b"\0Fixture"));
    frames.extend(frame(b"APIC", &apic));
    frames.extend([0; 64]);
    let size = frames.len() as u32;
    let mut out = b"ID3\x03\0\0".to_vec();
    out.extend([(size >> 21) as u8 & 0x7f, (size >> 14) as u8 & 0x7f, (size >> 7) as u8 & 0x7f, size as u8 & 0x7f]);
    out.extend(frames);
    out.extend(FIXTURES[0].2);
    out
}

/// Bytes standing in for an embedded cover; nothing decodes them.
fn image(length: usize) -> Vec<u8> { (0..length).map(|i| (i * 7 + i / 251) as u8).collect() }

/// The 16-bit FLAC fixture with a PICTURE block after STREAMINFO.
fn pictured_flac(picture: usize) -> Vec<u8> {
    let original = FIXTURES[1].2;
    assert_eq!((&original[..4], original[4] & 0x7f, &original[5..8]), (&b"fLaC"[..], 0, &[0, 0, 34][..]));
    let mut block = Vec::new();
    block.extend(3u32.to_be_bytes());
    block.extend(9u32.to_be_bytes()); block.extend(b"image/png");
    block.extend(5u32.to_be_bytes()); block.extend(b"cover");
    for field in [600u32, 600, 24, 0, picture as u32] { block.extend(field.to_be_bytes()); }
    block.extend(image(picture));
    assert!(block.len() < 1 << 24);
    let last = original[4] & 0x80;
    let mut out = original[..42].to_vec();
    out[4] &= 0x7f;
    out.push(last | 6);
    out.extend(&(block.len() as u32).to_be_bytes()[1..]);
    out.extend(block);
    out.extend(&original[42..]);
    out
}

/// The AAC fixture with `udta/meta/ilst/covr` appended to its `moov`, which
/// ffmpeg wrote last, so no chunk offset moves.
fn covered_m4a(picture: usize) -> Vec<u8> {
    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend(kind); out.extend(body); out
    }
    let original = FIXTURES[3].2;
    let mut at = 0;
    let moov = loop {
        let size = u32::from_be_bytes(original[at..at + 4].try_into().unwrap()) as usize;
        if &original[at + 4..at + 8] == b"moov" { break at; }
        at += size;
    };
    let moov_len = u32::from_be_bytes(original[moov..moov + 4].try_into().unwrap()) as usize;
    assert_eq!(moov + moov_len, original.len(), "moov is the last atom");
    let mut data = vec![0, 0, 0, 13, 0, 0, 0, 0];
    data.extend(image(picture));
    let ilst = atom(b"ilst", &atom(b"covr", &atom(b"data", &data)));
    let mut handler = vec![0; 8];
    handler.extend(b"mdir"); handler.extend([0; 12]);
    let mut meta = vec![0; 4];
    meta.extend(atom(b"hdlr", &handler)); meta.extend(ilst);
    let udta = atom(b"udta", &atom(b"meta", &meta));
    let mut out = original.to_vec();
    out.extend(&udta);
    out[moov..moov + 4].copy_from_slice(&((moov_len + udta.len()) as u32).to_be_bytes());
    out
}

/// 0.1 s of 16-bit stereo PCM with a LIST/INFO title, built here.
const WAV_FRAMES: usize = 4410;
fn wav() -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..WAV_FRAMES {
        let sample = ((i as f32 * 0.0627).sin() * 12000.0) as i16;
        data.extend(sample.to_le_bytes()); data.extend((-sample / 2).to_le_bytes());
    }
    let mut out = b"RIFF".to_vec();
    out.extend((4 + 24 + 24 + 8 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    for field in [1u16, 2] { out.extend(field.to_le_bytes()); }
    for field in [44100u32, 44100 * 4] { out.extend(field.to_le_bytes()); }
    for field in [4u16, 16] { out.extend(field.to_le_bytes()); }
    out.extend(b"LIST"); out.extend(16u32.to_le_bytes()); out.extend(b"INFOINAM"); out.extend(4u32.to_le_bytes()); out.extend(b"tone");
    out.extend(b"data"); out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    out
}

/// The files plus tagged variants: an ID3v2 tag, a FLAC picture block and an
/// MP4 cover of 600 bytes each, and a WAV with an INFO chunk.
fn fixtures() -> Vec<(&'static str, &'static str, Vec<u8>, usize)> {
    let mut all: Vec<_> = FIXTURES.iter().map(|&(name, ext, bytes, frames)| (name, ext, bytes.to_vec(), frames)).collect();
    all.push(("mp3-id3", "mp3", tagged_mp3(600), FIXTURES[0].3));
    all.push(("flac-picture", "flac", pictured_flac(600), FIXTURES[1].3));
    all.push(("aac-cover", "m4a", covered_m4a(600), FIXTURES[3].3));
    all.push(("wav", "wav", wav(), WAV_FRAMES));
    all
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
}

fn damage(rng: &mut Rng, original: &[u8]) -> Vec<u8> {
    let mut bytes = original.to_vec();
    for _ in 0..1 + rng.below(3) {
        if bytes.is_empty() { break; }
        let at = if rng.below(2) == 0 { rng.below(bytes.len().min(512)) } else { rng.below(bytes.len()) };
        let run = (1 + rng.below(32)).min(bytes.len() - at);
        match rng.below(8) {
            0 | 1 => for _ in 0..1 + rng.below(8) { let i = rng.below(bytes.len()); bytes[i] ^= 1 << rng.below(8); },
            2 => bytes[at..at + run].fill(0),
            3 => bytes[at..at + run].fill(0xff),
            4 => for b in &mut bytes[at..at + run] { *b = rng.next() as u8; },
            5 => bytes.truncate(at),
            6 => { bytes.drain(at..at + run); }
            _ => { let copy = bytes[at..at + run].to_vec(); bytes.splice(at..at, copy); }
        }
    }
    bytes
}

#[derive(Default, Debug, Clone, Copy, PartialEq)]
struct Played { packets: usize, frames: usize, bad_packets: usize, non_finite: usize }

/// What `Engine::open_current_inner` and `Engine::step` do with a file,
/// without the queue and the PCM.
fn play(bytes: Vec<u8>, ext: &str, seek_ms: Option<u64>) -> Outcome<Played> {
    let blocking = crate::rt::Blocking::dedicated_thread();
    let mut format = probe(&blocking, Box::new(Cursor::new(bytes)), Some(ext), false).map_err(|e| e.to_string())?.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio track")?;
    let id = track.id;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|e| e.to_string())?;
    let mut samples = Samples::default();
    let mut played = Played::default();
    let mut seek_ms = seek_ms;
    loop {
        if played.packets == 3 {
            if let Some(ms) = seek_ms.take() {
                let to = SeekTo::Time { time: Time::new(ms / 1000, (ms % 1000) as f64 / 1000.0), track_id: Some(id) };
                if crate::audio::callback::with(&blocking, || format.seek(SeekMode::Accurate, to)).is_ok() { decoder.reset(); }
            }
        }
        let packet = match crate::audio::callback::with(&blocking, || format.next_packet()) {
            Ok(packet) => packet,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(e.to_string()),
        };
        played.packets += 1;
        if played.packets > MAX_PACKETS { panic!("more than {MAX_PACKETS} packets"); }
        if packet.track_id() != id { continue; }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(Error::DecodeError(_)) => { played.bad_packets += 1; continue; }
            Err(e) => return Err(e.to_string()),
        };
        let frames = decoded.frames();
        assert!(frames <= MAX_PACKET_FRAMES, "a packet of {frames} frames");
        if decoded.spec().rate == 0 || decoded.spec().channels.count() == 0 { return Err("invalid format".into()); }
        if frames == 0 { continue; }
        let channels = decoded.spec().channels.count();
        if let AudioBufferRef::F32(planar) = &decoded {
            played.non_finite += (0..channels).map(|channel| planar.chan(channel).iter().filter(|sample| !sample.is_finite()).count()).sum::<usize>();
        }
        assert_eq!(samples.convert(decoded, 0.5).len(), frames * channels);
        played.frames += frames;
    }
    Ok(played)
}

/// The played result, or what failed: a panic's message or an oversized allocation.
fn caught(bytes: Vec<u8>, ext: &'static str, seek_ms: Option<u64>) -> Outcome<Outcome<Played>> {
    let (result, heap) = std::panic::catch_unwind(move || crate::test_alloc::measure(|| play(bytes, ext, seek_ms))).map_err(|panic| {
        panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "panic".into())
    })?;
    if heap.largest > MAX_ALLOCATION { return Err(format!("one allocation of {} bytes", heap.largest)); }
    Ok(result)
}

#[test]
fn undamaged_fixtures_decode_every_frame() {
    for (name, ext, bytes, frames) in fixtures() {
        let copy = bytes.clone();
        let (played, heap) = crate::test_alloc::measure(|| play(copy, ext, None));
        let played = played.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(heap.largest <= 256 << 10, "{name}: {heap:?}");
        assert_eq!(played.bad_packets, 0, "{name}");
        // The lossy decoders trim encoder delay and padding to within two frames of the source.
        assert!((frames..=frames + 2304).contains(&played.frames), "{name}: {} frames", played.frames);
        let seeked = play(bytes.to_vec(), ext, Some(120)).unwrap_or_else(|e| panic!("{name} seek: {e}"));
        assert!(seeked.frames > 0 && seeked.frames <= 2 * played.frames, "{name}: {} frames after a seek", seeked.frames);
    }
}

/// The player shows the server's artwork and never reads a file's own. An
/// embedded cover must not be loaded at track start and held while the track
/// plays: with a megabyte in each container's picture field, opening and
/// playing the file stays within the heap it uses without one.
#[test]
fn an_embedded_megabyte_cover_is_not_read_into_memory() {
    const PICTURE: usize = 1 << 20;
    for (name, ext, with, without, frames) in [
        ("mp3", "mp3", tagged_mp3(PICTURE), tagged_mp3(600), FIXTURES[0].3),
        ("flac", "flac", pictured_flac(PICTURE), pictured_flac(600), FIXTURES[1].3),
        ("m4a", "m4a", covered_m4a(PICTURE), covered_m4a(600), FIXTURES[3].3),
    ] {
        let (small, plain) = crate::test_alloc::measure(|| play(without, ext, None));
        let (large, heap) = crate::test_alloc::measure(|| play(with, ext, None));
        let (small, large) = (small.unwrap_or_else(|e| panic!("{name}: {e}")), large.unwrap_or_else(|e| panic!("{name}: {e}")));
        assert_eq!((large.frames, large.bad_packets), (small.frames, 0), "{name}");
        assert!((frames..=frames + 2304).contains(&large.frames), "{name}: {} frames", large.frames);
        // The file's own megabyte (moved into the source) is freed inside the
        // measurement and is not part of the peak.
        assert!(heap.peak <= plain.peak + (64 << 10) && heap.largest <= 256 << 10,
            "{name}: {heap:?} with the cover, {plain:?} without");
    }
}

/// A WAV header: `fmt ` with the given block alignment, an optional
/// LIST/INFO title that declares `title` bytes, and a `data` chunk that
/// declares `data` bytes. Every enclosing length agrees with the fields.
fn wav_header(block_align: u16, title: Option<u32>, data: u32) -> Vec<u8> {
    let list = title.map_or(0, |title| 8 + 4 + 8 + title);
    let mut out = b"RIFF".to_vec();
    out.extend((4 + 24 + list + 8 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    for field in [1u16, 2] { out.extend(field.to_le_bytes()); }
    for field in [44100u32, 44100 * u32::from(block_align)] { out.extend(field.to_le_bytes()); }
    for field in [block_align, 16] { out.extend(field.to_le_bytes()); }
    if let Some(title) = title {
        out.extend(b"LIST"); out.extend((4 + 8 + title).to_le_bytes()); out.extend(b"INFOINAM"); out.extend(title.to_le_bytes());
    }
    out
}

/// Round 1 damaged length fields at random, and a field that disagrees with
/// its enclosing chunk is refused early. These files are consistent: every
/// length agrees, so only a limit in the reader bounds what it allocates.
/// The Ogg reader has one (16 MiB a packet, 64 KiB a page); the RIFF reader
/// had none for an INFO value or for a packet.
#[test]
fn riff_lengths_that_the_file_agrees_on_do_not_size_an_allocation() {
    // An INFO title that declares 1 GiB, cut after its first bytes: on the
    // Q2 a zeroed gigabyte is refused, which aborts.
    let mut cut = wav_header(4, Some(1 << 30), 0);
    cut.extend(b"tone");
    let copy = cut.clone();
    let (played, heap) = crate::test_alloc::measure(|| play(copy, "wav", None));
    assert!(played.is_err());
    assert!(heap.largest <= MAX_ALLOCATION, "an INFO value declared 1 GiB: {heap:?}");
    // The same title present in full (2 MiB): skipped, and the audio plays.
    let mut tagged = wav_header(4, Some(2 << 20), 4 * WAV_FRAMES as u32);
    tagged.extend(image(2 << 20));
    tagged.extend(b"data"); tagged.extend((4 * WAV_FRAMES as u32).to_le_bytes());
    tagged.extend(&wav()[wav().len() - 4 * WAV_FRAMES..]);
    let (played, heap) = crate::test_alloc::measure(|| play(tagged, "wav", None));
    assert_eq!(played.unwrap().frames, WAV_FRAMES);
    assert!(heap.largest <= 256 << 10, "a 2 MiB INFO value: {heap:?}");
    // A block alignment of 65535 bytes makes one packet 1152 blocks: 72 MiB,
    // read into memory when the file has them.
    let mut wide = wav_header(65535, None, 80 << 20);
    wide.extend(b"data"); wide.extend((80u32 << 20).to_le_bytes());
    wide.resize(wide.len() + (80 << 20), 0);
    let (played, heap) = crate::test_alloc::measure(|| play(wide, "wav", None));
    assert!(played.is_err(), "{:?}", played.map(|played| played.frames));
    assert!(heap.largest <= MAX_ALLOCATION, "72 MiB packets: {heap:?}");
}

/// Cases per fixture; `Q2_DECODER_CASES` runs more.
fn cases() -> usize {
    std::env::var("Q2_DECODER_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(300)
}

#[cfg(panic = "unwind")]
#[test]
fn damaged_files_never_panic_the_decode_path() {
    let mut failures = Vec::new();
    let (mut errors, mut played, mut non_finite) = (0usize, 0usize, 0usize);
    for (name, ext, bytes, _) in fixtures() {
        let bytes = &bytes[..];
        let mut rng = Rng(0x5132_4e44 ^ bytes.len() as u64);
        for case in 0..cases() {
            let damaged = damage(&mut rng, bytes);
            let seek = (case % 3 == 0).then(|| rng.below(300) as u64);
            match caught(damaged, ext, seek) {
                Ok(Ok(result)) => { played += 1; non_finite += result.non_finite; }
                Ok(Err(_)) => errors += 1,
                Err(panic) => failures.push(format!("{name} case {case} seek {seek:?}: {panic}")),
            }
        }
    }
    assert!(played > 0 && errors > 0, "the generator must produce both playable and refused files ({played}, {errors})");
    assert!(failures.is_empty(), "{} of {} damaged files failed:\n{}", failures.len(), fixtures().len() * cases(), failures.join("\n"));
    // The equalizer's filter state would keep a NaN or an infinity until the
    // track changes (`Equalizer::reset`); no decoder here produces one.
    assert_eq!(non_finite, 0, "decoded samples that are not finite");
}

/// Every prefix length of the smallest fixtures: a transfer cut at any byte.
#[cfg(panic = "unwind")]
#[test]
fn every_truncation_of_the_small_fixtures_ends_without_a_panic() {
    let mut failures = Vec::new();
    for (name, ext, bytes, _) in fixtures().into_iter().filter(|f| f.2.len() < 8192) {
        for length in 0..bytes.len() {
            if let Err(panic) = caught(bytes[..length].to_vec(), ext, None) { failures.push(format!("{name} cut at {length}: {panic}")); }
        }
    }
    assert!(failures.is_empty(), "{} truncations panicked:\n{}", failures.len(), failures.join("\n"));
}
