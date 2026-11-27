//! Cover art arrives from servers and from files on the card. Damaged or
//! hostile image bytes must end in `None` or a square image, without a panic
//! (the release build aborts on one) and within a heap bound: the Q2 has
//! about 50 MB free, and a refused allocation also aborts. `test_alloc`
//! counts the heap; the fixtures are generated here.
use super::Image;
use crate::test_alloc::{measure, Stats};

/// The most heap one decode may hold. The limits admit 2048 x 2048: RGB
/// output is 12 MiB of that, RGBA 16 MiB, and the decoders and the resize
/// add working memory. Measured here at that size and a 320 square: baseline
/// JPEG, PNG and BMP 19.8 MiB, GIF and RGBA 23.8 MiB, WebP 28.0 MiB,
/// progressive 4:2:0 JPEG 24.3 MiB.
const PEAK_LIMIT: usize = 30 << 20;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2).map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap()).collect()
}
/// 16 x 16 progressive JPEGs written by Pillow 12.3 (`quality=20`,
/// `progressive=True`; `subsampling=0` and `2`) from generated pixels: the
/// image crate has no progressive encoder. The frame header (FFC2) is at
/// byte 158 of each: length, precision, height, width.
const PROGRESSIVE_444: &str = "ffd8ffe000104a46494600010100000100010000ffdb004300281c1e231e19282321232d2b28303c64413c37373c7b585d4964918099968f808c8aa0b4e6c3a0aadaad8a8cc8ffcbdaeef5ffffff9bc1fffffffaffe6fdfff8ffdb0043012b2d2d3c353c76414176f8a58ca5f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8ffc20011080010001003011100021101031101ffc4001500010100000000000000000000000000000304ffc40017010100030000000000000000000000000003010204ffda000c0301000210031000000128bb990ea4a0cbffc4001510010100000000000000000000000000000001ffda00080101000105028888ffc400161101010100000000000000000000000000000151ffda0008010301013f01ab8aafffc4001511010100000000000000000000000000000100ffda0008010201013f01492492ffc40014100100000000000000000000000000000020ffda0008010100063f021fffc40017100100030000000000000000000000000001001041ffda0008010100013f2130e5dfffda000c03010002000300000010161fffc400191100030003000000000000000000000000000111214161ffda0008010301013f10552ec90e9bc9d9ffc400161100030000000000000000000000000000001041ffda0008010201013f101ab5ffc4001b100002030003000000000000000000000001110021413191a1ffda0008010100013f1002077c80c25700121f506c3de29cffd9";
const PROGRESSIVE_420: &str = "ffd8ffe000104a46494600010100000100010000ffdb004300281c1e231e19282321232d2b28303c64413c37373c7b585d4964918099968f808c8aa0b4e6c3a0aadaad8a8cc8ffcbdaeef5ffffff9bc1fffffffaffe6fdfff8ffdb0043012b2d2d3c353c76414176f8a58ca5f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8ffc20011080010001003012200021101031101ffc4001500010100000000000000000000000000000304ffc40014010100000000000000000000000000000000ffda000c03010002100310000001271a0fffc4001510010100000000000000000000000000000001ffda00080101000105028888ffc4001511010100000000000000000000000000000002ffda0008010301013f01a7ffc4001511010100000000000000000000000000000200ffda0008010201013f0142ffc40014100100000000000000000000000000000020ffda0008010100063f021fffc40017100100030000000000000000000000000001001041ffda0008010100013f2130e5dfffda000c0301000200030000001067ffc4001511010100000000000000000000000000000001ffda0008010301013f10b7ffc40014110100000000000000000000000000000000ffda0008010201013f101fffc4001b100002030003000000000000000000000001110021413191a1ffda0008010100013f1002077c80c25700121f506c3de29cffd9";
const FRAME_HEADER: usize = 158;

fn encoded(format: ::image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let mut value = 0x51324f53u32;
    let image = ::image::RgbImage::from_fn(width, height, |x, y| {
        value = value.wrapping_mul(1664525).wrapping_add(1013904223);
        ::image::Rgb([(x * 5) as u8, (y * 7) as u8, (value >> 24) as u8 / 4])
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    use ::image::ImageFormat::*;
    let mut all: Vec<_> = [("jpeg", Jpeg), ("png", Png), ("webp", WebP), ("gif", Gif), ("bmp", Bmp)].into_iter()
        .map(|(name, format)| (name, encoded(format, 96, 64))).collect();
    all.push(("progressive jpeg 4:4:4", hex(PROGRESSIVE_444)));
    all.push(("progressive jpeg 4:2:0", hex(PROGRESSIVE_420)));
    all
}
fn decode(bytes: &[u8], size: u32) -> (Option<Image>, Stats) { measure(|| Image::decode_square(bytes, size)) }

fn damage(rng: &mut fastrand::Rng, bytes: &mut Vec<u8>) {
    if bytes.is_empty() { return; }
    // Headers decide dimensions and table sizes: half of the edits go there.
    let at = if rng.bool() { rng.usize(..bytes.len().min(192)) } else { rng.usize(..bytes.len()) };
    match rng.u8(..8) {
        0 => bytes[at] ^= 1 << rng.u8(..8),
        1 => bytes[at] = [0, 1, 0x7F, 0x80, 0xFF][rng.usize(..5)],
        // 16-bit fields (JPEG and GIF dimensions) in both byte orders.
        2 if at + 2 <= bytes.len() => {
            let value = [0u16, 1, 2047, 2048, 2049, 0x7FFF, 0xFFFF][rng.usize(..7)];
            bytes[at..at + 2].copy_from_slice(&if rng.bool() { value.to_be_bytes() } else { value.to_le_bytes() });
        }
        // 32-bit fields (PNG, BMP and WebP dimensions and lengths).
        3 if at + 4 <= bytes.len() => {
            let value = [0u32, 1, 2048, 2049, 65535, 0x00FF_FFFF, 0x7FFF_FFFF, 0xFFFF_FFFF][rng.usize(..8)];
            bytes[at..at + 4].copy_from_slice(&if rng.bool() { value.to_be_bytes() } else { value.to_le_bytes() });
        }
        4 => bytes.truncate(at),
        5 => { let end = (at + rng.usize(1..48)).min(bytes.len()); bytes.drain(at..end); }
        6 => { let end = (at + rng.usize(1..48)).min(bytes.len()); let copy = bytes[at..end].to_vec(); bytes.splice(at..at, copy); }
        _ => { let end = (at + rng.usize(1..24)).min(bytes.len()); rng.fill(&mut bytes[at..end]); }
    }
}

#[test]
fn undamaged_fixtures_decode() {
    for (name, bytes) in fixtures() {
        let (image, heap) = decode(&bytes, 52);
        let image = image.unwrap_or_else(|| panic!("{name} did not decode"));
        assert_eq!((image.width, image.height, image.pixels.len()), (52, 52, 52 * 52), "{name}");
        assert!(heap.peak < 1 << 20, "{name}: {heap:?}");
    }
}

#[test]
fn damaged_images_end_in_none_or_a_square_within_the_heap_bound() {
    let cases: usize = std::env::var("Q2_IMAGE_CASES").ok().and_then(|value| value.parse().ok()).unwrap_or(400);
    let mut rng = fastrand::Rng::with_seed(0x494d_4721);
    let mut worst = Vec::new();
    for (name, original) in fixtures() {
        let (mut decoded, mut most) = (0, Stats::default());
        for case in 0..cases {
            let mut bytes = original.clone();
            for _ in 0..rng.usize(1..4) { damage(&mut rng, &mut bytes); }
            let size = [1, 52, 150, 320][case % 4];
            // A panic fails the test.
            let (image, heap) = decode(&bytes, size);
            if let Some(image) = image {
                decoded += 1;
                assert_eq!((image.width, image.height, image.pixels.len()), (size, size, (size * size) as usize), "{name} case {case}");
            }
            if heap.peak > most.peak { most = heap; }
            assert!(heap.peak <= PEAK_LIMIT, "{name} case {case}: {heap:?} from {} bytes", bytes.len());
        }
        worst.push(format!("{name}: {decoded} of {cases} decoded, largest peak {} KiB", most.peak / 1024));
    }
    println!("{}", worst.join("\n"));
}

#[test]
fn every_truncation_of_the_fixtures_stays_within_the_heap_bound() {
    for (name, original) in fixtures() {
        for length in 0..original.len() {
            let (_, heap) = decode(&original[..length], 52);
            assert!(heap.peak < 1 << 20, "{name} cut to {length}: {heap:?}");
        }
    }
}

#[test]
fn a_small_file_that_declares_the_largest_frame_stays_within_the_heap_bound() {
    // The decoders size their working memory from the header. 576 bytes that
    // declare 2048 x 2048 progressive 4:4:4 make the JPEG decoder allocate
    // its coefficient buffers (two bytes a sample) before it reads a scan.
    let (mut report, mut over) = (Vec::new(), false);
    for (name, original) in [("progressive jpeg 4:4:4", hex(PROGRESSIVE_444)), ("progressive jpeg 4:2:0", hex(PROGRESSIVE_420))] {
        for (width, height) in [(2048u32, 2048u32), (2048, 16), (1024, 1024), (1448, 1448), (2049, 16), (65535, 65535)] {
            let mut bytes = original.clone();
            assert_eq!(bytes[FRAME_HEADER..FRAME_HEADER + 2], [0xFF, 0xC2]);
            bytes[FRAME_HEADER + 5..FRAME_HEADER + 7].copy_from_slice(&(height as u16).to_be_bytes());
            bytes[FRAME_HEADER + 7..FRAME_HEADER + 9].copy_from_slice(&(width as u16).to_be_bytes());
            let (image, heap) = decode(&bytes, 150);
            report.push(format!("{name} {width}x{height}: {} KiB peak, decoded {}", heap.peak / 1024, image.is_some()));
            over |= heap.peak > PEAK_LIMIT;
            if width > 2048 { assert!(image.is_none()); }
            // Smaller progressive images still decode.
            if width <= 2048 && width * height <= 1024 * 1024 { assert!(image.is_some(), "{name} {width}x{height}"); }
        }
    }
    assert!(!over, "{}", report.join("\n"));
    // The same for the formats the image crate encodes: a real 2048 x 2048
    // image of each (flat colour compresses to little), at the largest size
    // the player asks for. Grey and RGBA take the generic resize.
    use ::image::{DynamicImage, ImageFormat::*};
    let rgb = || DynamicImage::ImageRgb8(::image::RgbImage::from_pixel(2048, 2048, ::image::Rgb([200, 40, 30])));
    let grey = || DynamicImage::ImageLuma8(::image::GrayImage::from_pixel(2048, 2048, ::image::Luma([90])));
    let rgba = || DynamicImage::ImageRgba8(::image::RgbaImage::from_pixel(2048, 2048, ::image::Rgba([200, 40, 30, 128])));
    let deep = || DynamicImage::ImageRgb16(::image::ImageBuffer::from_pixel(2048, 2048, ::image::Rgb([51400u16, 10280, 7710])));
    let mut failed = false;
    for (name, format, image) in [("jpeg", Jpeg, rgb()), ("png", Png, rgb()), ("webp", WebP, rgb()), ("gif", Gif, rgb()), ("bmp", Bmp, rgb()),
        ("grey jpeg", Jpeg, grey()), ("grey png", Png, grey()), ("rgba png", Png, rgba()), ("rgba webp", WebP, rgba()), ("16-bit png", Png, deep())] {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        drop(image);
        let bytes = bytes.into_inner();
        let (image, heap) = decode(&bytes, 320);
        report.push(format!("{name} 2048x2048 ({} KiB of file): {} KiB peak, decoded {}", bytes.len() / 1024, heap.peak / 1024, image.is_some()));
        failed |= heap.peak > PEAK_LIMIT;
    }
    assert!(!failed, "{}", report.join("\n"));
    println!("{}", report.join("\n"));
}
