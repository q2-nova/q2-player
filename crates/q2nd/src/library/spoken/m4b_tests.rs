//! The M4B metadata readers (`read_m4b_info`, `parse_atoms`, `quicktime`)
//! on generated files: what a well-formed chapter track yields, and that a
//! damaged or hostile file ends in an error or a bounded result. The heap a
//! read may use is checked with `test_alloc`: a book is inspected whenever
//! Home opens, on a device with about 50 MB free.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [(body.len() as u32 + 8).to_be_bytes().as_slice(), kind, body].concat()
}
fn words(values: &[u32]) -> Vec<u8> { values.iter().flat_map(|value| value.to_be_bytes()).collect() }
fn mvhd(scale: u32, ticks: u32) -> Vec<u8> { atom(b"mvhd", &[&[0u8; 12][..], &words(&[scale, ticks])].concat()) }

/// A text sample: a 16-bit length and the title bytes.
fn sample(title: &[u8]) -> Vec<u8> { [&(title.len() as u16).to_be_bytes()[..], title].concat() }

struct Track<'a> {
    /// Size of each sample; `fixed` writes one size for all of them.
    sizes: &'a [u32],
    fixed: Option<u32>,
    /// Ticks from each chapter to the next, at 1000 ticks a second.
    delta: u32,
    offsets: &'a [u64],
    wide: bool,
}
/// `moov` with a movie header, an audio track that names track 2 as its
/// chapters (`tref/chap`), and that text track, one sample a chunk.
fn moov(duration_ms: u32, track: &Track<'_>, extra: &[u8]) -> Vec<u8> {
    let count = track.sizes.len() as u32;
    let tkhd = |id: u32| atom(b"tkhd", &[&[0u8; 12][..], &words(&[id])].concat());
    let stsz = atom(b"stsz", &[words(&[0, track.fixed.unwrap_or(0), count]), if track.fixed.is_some() { Vec::new() } else { words(track.sizes) }].concat());
    let stts = atom(b"stts", &words(&[0, 1, count, track.delta]));
    let stsc = atom(b"stsc", &words(&[0, 1, 1, 1, 1]));
    let chunks = if track.wide {
        atom(b"co64", &[words(&[0, track.offsets.len() as u32]), track.offsets.iter().flat_map(|offset| offset.to_be_bytes()).collect()].concat())
    } else {
        atom(b"stco", &[words(&[0, track.offsets.len() as u32]), words(&track.offsets.iter().map(|&offset| offset as u32).collect::<Vec<_>>())].concat())
    };
    let stbl = atom(b"stbl", &[stsz, stts, stsc, chunks].concat());
    let mdia = atom(b"mdia", &[atom(b"mdhd", &[&[0u8; 12][..], &words(&[1000])].concat()), atom(b"minf", &stbl)].concat());
    let audio = atom(b"trak", &[tkhd(1), atom(b"tref", &atom(b"chap", &words(&[2])))].concat());
    let text = atom(b"trak", &[tkhd(2), mdia].concat());
    atom(b"moov", &[mvhd(1000, duration_ms), audio, text, extra.to_vec()].concat())
}
/// A whole file: `ftyp`, the `moov` that `build` makes for the offset at
/// which the samples will start, then the samples.
fn book(samples: &[Vec<u8>], extra: &[u8], wide: bool) -> Vec<u8> {
    let ftyp = atom(b"ftyp", b"M4B \0\0\0\0");
    let sizes: Vec<u32> = samples.iter().map(|sample| sample.len() as u32).collect();
    let layout = |start: u64| {
        let mut at = start;
        let offsets: Vec<u64> = sizes.iter().map(|&size| { let offset = at; at += u64::from(size); offset }).collect();
        moov(600_000, &Track { sizes: &sizes, fixed: None, delta: 60_000, offsets: &offsets, wide }, extra)
    };
    let start = (ftyp.len() + layout(0).len() + 8) as u64;
    [ftyp, layout(start), atom(b"mdat", &samples.concat())].concat()
}

struct Generated(std::path::PathBuf);
impl Generated {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"))
            .join("m4b-tests").join(format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&root).unwrap();
        Self(root.join("generated.m4b"))
    }
    /// Reads `bytes` as a file; the heap the read used on this thread.
    fn read(&self, bytes: &[u8]) -> (io::Result<M4bInfo>, crate::test_alloc::Stats) {
        std::fs::write(&self.0, bytes).unwrap();
        let blocking = crate::rt::Blocking::dedicated_thread();
        crate::test_alloc::measure(|| read_m4b_info(&blocking, &self.0))
    }
}
impl Drop for Generated {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(self.0.parent().unwrap()); }
}

fn titles(info: &M4bInfo) -> Vec<(u64, &str)> { info.chapters.iter().map(|chapter| (chapter.start_ms, chapter.title.as_str())).collect() }

#[test]
fn a_chapter_text_track_yields_its_titles_and_start_times() {
    let utf16: Vec<u8> = [0xFE, 0xFF].into_iter().chain("Zweites Kapitel – ü".encode_utf16().flat_map(u16::to_be_bytes)).collect();
    // QuickTime writes an encoding atom after the text; the length excludes it.
    let with_tail = [sample("Third".as_bytes()), atom(b"encd", &[0, 0, 1, 0])].concat();
    let samples = [sample(b"Opening"), sample(&utf16), with_tail, sample(b"")];
    let generated = Generated::new();
    for wide in [false, true] {
        let (info, _) = generated.read(&book(&samples, &[], wide));
        let info = info.unwrap();
        assert_eq!(info.duration_ms, 600_000);
        assert_eq!(titles(&info), [(0, "Opening"), (60_000, "Zweites Kapitel – ü"), (120_000, "Third"), (180_000, "")], "co64 {wide}");
    }
    // A Nero list is used when the file has no text track, and yields to one.
    let chpl = |entries: &[(u64, &str)]| atom(b"udta", &atom(b"chpl", &[&[0u8; 8][..], &[entries.len() as u8],
        &entries.iter().flat_map(|(start, title)| [&(start * 10_000).to_be_bytes()[..], &[title.len() as u8], title.as_bytes()].concat()).collect::<Vec<_>>()].concat()));
    let nero = chpl(&[(0, "Nero one"), (300_000, "Nero two"), (900_000, "Beyond the end")]);
    let (info, _) = generated.read(&book(&samples, &nero, false));
    assert_eq!(titles(&info.unwrap())[0], (0, "Opening"));
    let (info, _) = generated.read(&[atom(b"ftyp", b"M4B "), atom(b"moov", &[mvhd(1000, 600_000), nero].concat())].concat());
    assert_eq!(titles(&info.unwrap()), [(0, "Nero one"), (300_000, "Nero two")]);
}

#[test]
fn a_chapter_title_is_kept_to_a_row_and_its_sample_is_not_read_whole() {
    // 255 chunks that all point at one 65538-byte sample of bytes that are
    // not UTF-8: 70 KiB of file. Unbounded, each title becomes 196,608 bytes
    // of replacement characters, 50 MB in all.
    let ftyp = atom(b"ftyp", b"M4B ");
    let sizes = [65538u32; 255];
    let layout = |start: u64| moov(600_000, &Track { sizes: &sizes, fixed: Some(65538), delta: 1000, offsets: &[start; 255], wide: false }, &[]);
    let start = (ftyp.len() + layout(0).len()) as u64;
    for fill in [0x80u8, b'a'] {
        let file = [ftyp.clone(), layout(start), vec![0xFF, 0xFF], vec![fill; 65536]].concat();
        let generated = Generated::new();
        let (info, heap) = generated.read(&file);
        let info = info.unwrap();
        assert_eq!(info.chapters.len(), 255);
        assert!(heap.peak < 512 * 1024 && heap.largest <= 64 * 1024, "{heap:?} for a {} byte file", file.len());
        assert!(info.chapters.iter().all(|chapter| chapter.title.chars().count() == quicktime::MAX_TITLE_CHARS), "{:?}", info.chapters[0].title.len());
    }
    // UTF-16 the same.
    let long: Vec<u8> = [0xFE, 0xFF].into_iter().chain(std::iter::repeat_n([0x26, 0x3A], 30_000).flatten()).collect();
    let generated = Generated::new();
    let (info, heap) = generated.read(&book(&[sample(&long)], &[], false));
    assert_eq!(info.unwrap().chapters[0].title.chars().count(), quicktime::MAX_TITLE_CHARS);
    assert!(heap.peak < 128 * 1024, "{heap:?}");
}

#[test]
fn repeated_nero_chapter_atoms_do_not_add_up_without_limit() {
    // 2312 bytes of atom for 255 chapters; a 16 MiB `moov` holds 1.8 million.
    let one = atom(b"chpl", &[&[0u8; 8][..], &[255], &(0..255u64).flat_map(|n| [&(n * 10_000).to_be_bytes()[..], &[0]].concat()).collect::<Vec<_>>()].concat());
    let (mut duration, mut chapters) = (0, Vec::new());
    assert!(parse_atoms(&one.repeat(64), 0, &mut duration, &mut chapters).is_ok());
    assert_eq!(chapters.len(), 64 * 255);
    let many = one.repeat(1000);
    let (result, heap) = crate::test_alloc::measure(|| {
        let mut chapters = Vec::new();
        parse_atoms(&many, 0, &mut 0, &mut chapters).map(|()| chapters.len())
    });
    assert!(result.is_err(), "{result:?} chapters from {} bytes", many.len());
    assert!(heap.peak < 2 << 20, "{heap:?}");
}

/// One seeded edit of `bytes`, weighted towards the atom tree at the front.
fn damage(rng: &mut fastrand::Rng, bytes: &mut Vec<u8>, front: usize) {
    if bytes.is_empty() { return; }
    let at = if rng.bool() { rng.usize(..front.min(bytes.len())) } else { rng.usize(..bytes.len()) };
    match rng.u8(..8) {
        0 => bytes[at] ^= 1 << rng.u8(..8),
        1 => bytes[at] = [0, 1, 0x7F, 0x80, 0xFF][rng.usize(..5)],
        // A 32-bit field: counts, sizes and offsets are big-endian words.
        2 if at + 4 <= bytes.len() => {
            let value = [0u32, 1, 2, 255, 16384, 16385, 65538, 0x00FF_FFFF, 0x7FFF_FFFF, 0xFFFF_FFFF, bytes.len() as u32][rng.usize(..11)];
            bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
        }
        3 => bytes.truncate(at),
        4 => { let end = (at + rng.usize(1..64)).min(bytes.len()); bytes.drain(at..end); }
        5 => { let end = (at + rng.usize(1..64)).min(bytes.len()); let copy = bytes[at..end].to_vec(); bytes.splice(at..at, copy); }
        6 => { let end = (at + rng.usize(1..32)).min(bytes.len()); rng.fill(&mut bytes[at..end]); }
        _ => bytes[at] = bytes[at].wrapping_add(1),
    }
}

#[test]
fn damaged_books_end_in_an_error_or_a_bounded_result() {
    let cases: usize = std::env::var("Q2_M4B_CASES").ok().and_then(|value| value.parse().ok()).unwrap_or(1500);
    let tags = atom(b"udta", &[
        atom(b"meta", &[&[0u8; 4][..], &atom(b"ilst", &[
            atom(b"\xA9nam", &atom(b"data", &[&[0u8; 8][..], b"Generated book"].concat())),
            atom(b"----", &[atom(b"name", &[&[0u8; 4][..], b"narrator"].concat()), atom(b"data", &[&[0u8; 8][..], b"Generated narrator"].concat())].concat()),
            atom(b"covr", &atom(b"data", &[0u8; 8 + 600])),
        ].concat())].concat()),
        atom(b"chpl", &[&[0u8; 8][..], &[1], &0u64.to_be_bytes(), &[4], b"Nero"].concat()),
    ].concat());
    let samples: Vec<Vec<u8>> = (0..6).map(|n| sample(format!("Chapter {n}").as_bytes())).collect();
    let generated = Generated::new();
    let mut rng = fastrand::Rng::with_seed(0x4d34_4221);
    let (mut read, mut refused) = (0, 0);
    for wide in [false, true] {
        let original = book(&samples, &tags, wide);
        let front = original.len() - samples.concat().len();
        let (info, _) = generated.read(&original);
        assert_eq!(info.unwrap().chapters.len(), 6);
        for case in 0..cases {
            let mut bytes = original.clone();
            for _ in 0..rng.usize(1..4) { damage(&mut rng, &mut bytes, front); }
            // A panic fails the test; the release build would abort on it.
            let (info, heap) = generated.read(&bytes);
            // The reader holds the `moov` once; everything else is small.
            assert!(heap.peak <= bytes.len() + 256 * 1024 && heap.largest <= bytes.len().max(64 * 1024),
                "co64 {wide} case {case}: {heap:?} for {} bytes", bytes.len());
            match info {
                Ok(info) => {
                    read += 1;
                    assert!(info.duration_ms > 0 && info.chapters.len() <= 16384);
                    assert!(info.chapters.iter().all(|chapter| chapter.start_ms < info.duration_ms && chapter.title.chars().count() <= quicktime::MAX_TITLE_CHARS));
                    assert!(info.chapters.windows(2).all(|pair| pair[0].start_ms < pair[1].start_ms));
                }
                Err(_) => refused += 1,
            }
        }
    }
    // The edits reach both outcomes; neither count is a requirement.
    assert!(read > cases / 20 && refused > cases / 20, "{read} read, {refused} refused");
}

#[test]
fn every_truncation_of_a_book_is_an_error_or_a_bounded_result() {
    let samples: Vec<Vec<u8>> = (0..3).map(|n| sample(format!("Chapter {n}").as_bytes())).collect();
    let original = book(&samples, &[], false);
    let generated = Generated::new();
    for length in 0..original.len() {
        let (info, heap) = generated.read(&original[..length]);
        assert!(info.is_err(), "{length} of {} bytes read as a book", original.len());
        assert!(heap.peak <= original.len() + 64 * 1024, "{length}: {heap:?}");
    }
}
