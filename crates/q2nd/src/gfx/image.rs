//! Decoded images (cover art) in the canvas's pixel format.

use std::path::Path;

#[derive(Debug, Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

/// The most a decode's output and coefficient buffers may take together:
/// what the other admitted formats use for a 2048 x 2048 image (WebP,
/// 28 MiB measured by `image_damage_tests`).
const DECODE_BYTES: u64 = 28 << 20;

/// Output plus coefficient bytes of a JPEG that the decoder holds whole
/// before it writes a pixel: a progressive one, or one whose first scan lacks
/// a component. `max_alloc` covers only the output; the coefficients are two
/// more bytes a sample (zune-jpeg `mcu_prog.rs`, `mcu.rs`), and the frame
/// header alone decides them. None for anything else, the decoder's to judge.
fn jpeg_buffered_bytes(bytes: &[u8]) -> Option<u64> {
    if !bytes.starts_with(&[0xFF, 0xD8]) { return None; }
    let (mut at, mut frame) = (2usize, None);
    loop {
        let marker = *bytes.get(at + 1)?;
        if *bytes.get(at)? != 0xFF { return None; }
        // Fill bytes and markers without a length.
        if marker == 0xFF { at += 1; continue; }
        if matches!(marker, 0x01 | 0xD0..=0xD8) { at += 2; continue; }
        let length = u16::from_be_bytes([*bytes.get(at + 2)?, *bytes.get(at + 3)?]) as usize;
        let body = bytes.get(at + 4..at + 2 + length)?;
        match marker {
            // Start of frame, every coding process (not DHT, JPG, DAC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                // 64-bit: 65535 x 65535 samples do not fit the Q2's usize.
                let (height, width) = (u64::from(u16::from_be_bytes([*body.get(1)?, *body.get(2)?])), u64::from(u16::from_be_bytes([*body.get(3)?, *body.get(4)?])));
                let components = body.get(6..6 + 3 * *body.get(5)? as usize)?;
                let factors = || components.chunks_exact(3).map(|c| (u64::from(c[1] >> 4), u64::from(c[1] & 15)));
                let (h_max, v_max) = (factors().map(|f| f.0).max()?.max(1), factors().map(|f| f.1).max()?.max(1));
                let units = width.div_ceil(8 * h_max) * height.div_ceil(8 * v_max);
                let coefficients: u64 = factors().map(|(h, v)| units * h * v * 64 * 2).sum();
                frame = Some((components.len() / 3, matches!(marker, 0xC2 | 0xC6 | 0xCA | 0xCE), width * height * (components.len() / 3) as u64 + coefficients));
            }
            0xDA => {
                let (count, progressive, working) = frame?;
                return (progressive || *body.first()? as usize != count).then_some(working);
            }
            0xD9 => return None,
            _ => {}
        }
        at += 2 + length;
    }
}

impl Image {
    /// Decodes supported image bytes (the first frame for animation) to a
    /// `size` x `size` square (centre-cropped), as album art is shown.
    pub fn decode_square(bytes: &[u8], size: u32) -> Option<Image> {
        if size == 0 || size > 1024 { return None; }
        if jpeg_buffered_bytes(bytes).is_some_and(|working| working > DECODE_BYTES) { return None; }
        let mut reader = ::image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
        let mut limits = ::image::Limits::default();
        limits.max_image_width = Some(2048);
        limits.max_image_height = Some(2048);
        limits.max_alloc = Some(16 * 1024 * 1024);
        reader.limits(limits);
        let img = reader.decode().ok()?;
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }
        if w == size && h == size {
            match &img {
                ::image::DynamicImage::ImageRgb8(rgb) => return Some(Image::from_rgb(size,size,rgb.as_raw())),
                // The existing path drops alpha after the identity resize.
                // Pack the same RGB bytes directly without two image copies.
                ::image::DynamicImage::ImageRgba8(rgba) => return Some(Image {
                    width: size, height: size,
                    pixels: rgba.as_raw().chunks_exact(4).map(|p|
                        (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32).collect(),
                }),
                _ => {},
            }
        }
        let side = w.min(h);
        // 8-bit RGB(A) downscales: the same Triangle resize without the
        // generic per-tap conversions (gfx/resize.rs, bit-identical).
        let (x0, y0) = (((w - side) / 2) as usize, ((h - side) / 2) as usize);
        let fast = match &img {
            ::image::DynamicImage::ImageRgb8(rgb) => super::resize::square_rgb(rgb.as_raw(), w as usize * 3, 3, x0, y0, side, size),
            ::image::DynamicImage::ImageRgba8(rgba) => super::resize::square_rgb(rgba.as_raw(), w as usize * 4, 4, x0, y0, side, size),
            _ => None,
        };
        if let Some(rgb) = fast {
            return Some(Image::from_rgb(size, size, &rgb));
        }
        // Crop through a borrowed view, avoiding a second full-size image.
        // Convert to RGB only after resizing, keeping peak memory bounded.
        macro_rules! resized {
            ($image:expr) => {{
                let cropped = ::image::imageops::crop_imm($image, (w-side)/2, (h-side)/2, side, side);
                ::image::imageops::resize(&*cropped,size,size,::image::imageops::FilterType::Triangle)
            }};
        }
        // JPEG and most server thumbnails are already RGB8. Keeping that
        // typed view avoids dynamic per-pixel conversion and a fourth channel
        // in the floating-point resize workspace.
        let rgb = match &img {
            ::image::DynamicImage::ImageRgb8(rgb) => resized!(rgb),
            ::image::DynamicImage::ImageRgba8(rgba) => ::image::DynamicImage::ImageRgba8(resized!(rgba)).to_rgb8(),
            _ => ::image::DynamicImage::ImageRgba8(resized!(&img)).to_rgb8(),
        };
        Some(Image::from_rgb(size, size, rgb.as_raw()))
    }

    fn rgb_bytes(&self) -> Vec<u8> {
        self.pixels.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect()
    }

    /// Downsample an already normalized square cover using the same Triangle
    /// filter as decode_square, without inflating its encoded PNG again.
    pub(crate) fn thumbnail(&self, size: u32) -> Option<Image> {
        if self.width != self.height || self.width > 1024 || size == 0 || size >= self.width { return None; }
        let rgb = super::resize::square_rgb(&self.rgb_bytes(), self.width as usize * 3, 3, 0, 0, self.width, size)?;
        Some(Image::from_rgb(size, size, &rgb))
    }

    /// Cache a small, normalized PNG, never a provider's full-size original.
    pub fn png(&self) -> Option<Vec<u8>> {
        let image = ::image::RgbImage::from_raw(self.width, self.height, self.rgb_bytes())?;
        let mut output = std::io::Cursor::new(Vec::new());
        image.write_to(&mut output, ::image::ImageFormat::Png).ok()?;
        Some(output.into_inner())
    }

    pub fn load_square(_blocking: &crate::rt::Blocking, path: &Path, size: u32) -> Option<Image> {
        Image::decode_square(&std::fs::read(path).ok()?, size)
    }

    fn from_rgb(width: u32, height: u32, rgb: &[u8]) -> Image {
        let pixels = rgb.chunks_exact(3).map(|p| (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32).collect();
        Image { width, height, pixels }
    }

    /// A flat placeholder with a music note, for songs without art.
    pub fn placeholder(size: u32) -> Image {
        use super::{Canvas, Color, Rect};
        let mut c = Canvas::new(size as i32, size as i32);
        c.vertical_gradient(Rect::new(0, 0, size as i32, size as i32), Color(0x3A3A3C), Color(0x1C1C1E));
        let s = size as f32 / 52.0;
        let note = Color(0x8C8C8C);
        // A beamed pair of eighth notes.
        c.fill_circle(19.0 * s, 35.0 * s, 5.0 * s, 0.0, note);
        c.fill_circle(33.0 * s, 32.0 * s, 5.0 * s, 0.0, note);
        c.fill_polygon(&[(22.0 * s, 35.0 * s), (24.0 * s, 35.0 * s), (24.0 * s, 15.0 * s), (22.0 * s, 15.5 * s)], note);
        c.fill_polygon(&[(36.0 * s, 32.0 * s), (38.0 * s, 32.0 * s), (38.0 * s, 12.0 * s), (36.0 * s, 12.5 * s)], note);
        c.fill_polygon(&[(22.0 * s, 15.5 * s), (38.0 * s, 12.0 * s), (38.0 * s, 16.0 * s), (22.0 * s, 19.5 * s)], note);
        Image { width: size, height: size, pixels: c.pixels }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_thumbnail_matches_the_original_png_roundtrip() {
        let large = Image::placeholder(150);
        let direct = large.thumbnail(50).unwrap();
        let previous = Image::decode_square(&large.png().unwrap(), 50).unwrap();
        assert_eq!(direct.pixels, previous.pixels);
        assert_eq!(direct.png(), previous.png());
    }

    #[test]
    fn server_image_formats_and_normalized_cache() {
        for format in [::image::ImageFormat::Jpeg, ::image::ImageFormat::Png, ::image::ImageFormat::WebP,
                       ::image::ImageFormat::Gif, ::image::ImageFormat::Bmp] {
            let image = ::image::RgbImage::from_pixel(80, 40, ::image::Rgb([230, 20, 10]));
            let mut bytes = std::io::Cursor::new(Vec::new());
            image.write_to(&mut bytes, format).unwrap();
            let out = Image::decode_square(bytes.get_ref(), 52).unwrap();
            assert_eq!((out.width, out.height), (52, 52));
            assert!(out.pixels.iter().all(|p| (p >> 16) > 200), "{format:?}");
            let roundtrip = Image::decode_square(&out.png().unwrap(), 52).unwrap();
            assert_eq!(out.pixels, roundtrip.pixels);
        }
    }

    #[test]
    fn exact_rgba_matches_legacy_pipeline_for_formats_sizes_and_alpha_values() {
        // Deterministic generated pixel properties, not an owner image or a
        // substitute for actual decoder/hardware performance qualification.
        for size in [1, 7, 52, 150] {
            for seed in [0u32, 1, 0x51324f53] {
                for format in [::image::ImageFormat::Png, ::image::ImageFormat::WebP,
                               ::image::ImageFormat::Gif, ::image::ImageFormat::Bmp] {
                    let mut value = seed;
                    let rgba = ::image::RgbaImage::from_fn(size, size, |_, _| {
                        value = value.wrapping_mul(1664525).wrapping_add(1013904223);
                        ::image::Rgba(value.to_le_bytes())
                    });
                    let mut encoded = std::io::Cursor::new(Vec::new());
                    rgba.write_to(&mut encoded, format).unwrap();
                    let decoded = ::image::load_from_memory(encoded.get_ref()).unwrap();
                    if matches!(format, ::image::ImageFormat::Png | ::image::ImageFormat::WebP) {
                        assert!(matches!(decoded, ::image::DynamicImage::ImageRgba8(_)));
                    }
                    let legacy = ::image::DynamicImage::ImageRgba8(::image::imageops::resize(
                        &decoded.to_rgba8(), size, size, ::image::imageops::FilterType::Triangle)).to_rgb8();
                    let expected = Image::from_rgb(size, size, legacy.as_raw());
                    let actual = Image::decode_square(encoded.get_ref(), size).unwrap();
                    assert_eq!(actual.pixels, expected.pixels, "{size} {seed} {format:?}");
                }
            }
        }
    }

    #[test]
    fn exact_rgba_output_limit_preserves_pixels_and_rejects_the_next_size() {
        let rgba = ::image::RgbaImage::from_fn(1024, 1024, |x, y|
            ::image::Rgba([x as u8, y as u8, (x ^ y) as u8, (x + y) as u8]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        rgba.write_to(&mut encoded, ::image::ImageFormat::Png).unwrap();
        let actual = Image::decode_square(encoded.get_ref(), 1024).unwrap();
        let legacy = ::image::DynamicImage::ImageRgba8(::image::imageops::resize(
            &rgba, 1024, 1024, ::image::imageops::FilterType::Triangle)).to_rgb8();
        assert_eq!(actual.pixels, Image::from_rgb(1024, 1024, legacy.as_raw()).pixels);
        assert_eq!(actual.pixels.len(), 1024 * 1024);
        assert!(Image::decode_square(encoded.get_ref(), 1025).is_none());
    }

    #[test]
    fn bounds_decoding_and_output_memory() {
        let image = ::image::RgbImage::new(4096, 1);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, ::image::ImageFormat::Png).unwrap();
        assert!(Image::decode_square(bytes.get_ref(), 52).is_none());
        assert!(Image::decode_square(bytes.get_ref(), u32::MAX).is_none());
        assert!(Image::decode_square(bytes.get_ref(), 0).is_none());
    }

    #[test]
    fn decodes_and_crops_png() {
        let mut png = Vec::new();
        let img = ::image::RgbImage::from_fn(80, 40, |x, _| if x < 40 { ::image::Rgb([255, 0, 0]) } else { ::image::Rgb([0, 0, 255]) });
        img.write_to(&mut std::io::Cursor::new(&mut png), ::image::ImageFormat::Png).unwrap();
        let out = Image::decode_square(&png, 20).unwrap();
        assert_eq!((out.width, out.height), (20, 20));
        // The centre crop keeps both halves.
        assert_eq!(out.pixels[10 * 20 + 1] >> 16, 255);
        assert_eq!(out.pixels[10 * 20 + 18] & 0xFF, 255);
        assert!(Image::decode_square(b"not an image", 20).is_none());
    }
}
