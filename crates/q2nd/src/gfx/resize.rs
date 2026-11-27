//! Triangle-filter downscale of 8-bit RGB and RGBA, bit-identical to
//! `image::imageops::resize(.., FilterType::Triangle)` (image 0.25.10,
//! `imageops/sample.rs`) for the colour channels.
//!
//! The crate's generic code converts every tap through `NumCast` and reads
//! pixels through bounds-checked `get_pixel`; on the X2000 each `u8 -> f32`
//! is a GPR-to-FPU move (about 7 cycles), and that conversion alone was 28%
//! of `Image::decode_square`'s cycles (q2perf, 2026-11-18). This keeps the
//! crate's arithmetic exactly:
//!
//! - the same weights: `vertical_sample`/`horizontal_sample`'s f32
//!   expressions, normalized by their f32 sum in the same order;
//! - the same per-element sums: `t = 0.0; t += v * w` for taps in ascending
//!   order, as separate f32 multiplies and adds (Rust never contracts them);
//! - the same vertical-then-horizontal order through an f32 intermediate;
//! - the same output: `clamp(t, 0, 255).round()` as `u8`.
//!
//! The vertical pass runs row-wise (for each tap, add `v * w` to every
//! element of the output row) instead of pixel-wise; each element still sees
//! its taps in ascending order, so the sums are the same, and the row form is
//! contiguous for MSA (`resize_msa.rs`), which converts bytes to floats in
//! vector registers. Alpha is not resampled: `decode_square` drops it, and
//! the crate never mixes it into the colour channels.

/// One output position: its first source index and normalized weights.
struct Taps {
    left: usize,
    weights: Vec<f32>,
}

/// `vertical_sample`'s and `horizontal_sample`'s tap selection for a
/// Triangle filter (support 1.0) from `src` to `dst` positions.
fn taps(src: u32, dst: u32) -> Vec<Taps> {
    let ratio = src as f32 / dst as f32;
    let sratio = if ratio < 1.0 { 1.0 } else { ratio };
    let src_support = 1.0 * sratio;
    (0..dst).map(|out| {
        let input = (out as f32 + 0.5) * ratio;
        let left = ((input - src_support).floor() as i64).clamp(0, src as i64 - 1);
        let right = ((input + src_support).ceil() as i64).clamp(left + 1, src as i64);
        let input = input - 0.5;
        let mut weights = Vec::with_capacity((right - left) as usize);
        let mut sum = 0.0f32;
        for i in left..right {
            let x = (i as f32 - input) / sratio;
            let w = if x.abs() < 1.0 { 1.0 - x.abs() } else { 0.0 };
            weights.push(w);
            sum += w;
        }
        for w in weights.iter_mut() {
            *w /= sum;
        }
        Taps { left: left as usize, weights }
    }).collect()
}

/// `u8 -> f32` by table: a load instead of a GPR-to-FPU move.
static BYTE_F32: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = i as f32;
        i += 1;
    }
    t
};

/// Downscales the `side` x `side` square at (`x0`, `y0`) of an image with
/// `channels` (3 or 4) bytes per pixel and rows `stride` bytes apart to
/// `size` x `size` RGB bytes. None when the square leaves `pixels`.
pub(crate) fn square_rgb(pixels: &[u8], stride: usize, channels: usize, x0: usize, y0: usize, side: u32, size: u32) -> Option<Vec<u8>> {
    if !(channels == 3 || channels == 4) || side == 0 || size == 0 || size >= side {
        return None;
    }
    let (side_us, size_us) = (side as usize, size as usize);
    let last = (y0 + side_us - 1).checked_mul(stride)?.checked_add((x0 + side_us).checked_mul(channels)?)?;
    if last > pixels.len() || (x0 + side_us) * channels > stride {
        return None;
    }
    // Vertical: `size` rows of `side` RGB floats. Rows are padded to a
    // multiple of four floats so a vector row never spills into the next.
    let row_floats = (side_us * 3 + 3) & !3;
    let mut tmp = vec![0.0f32; row_floats * size_us];
    for (outy, t) in taps(side, size).iter().enumerate() {
        let acc = &mut tmp[outy * row_floats..outy * row_floats + side_us * 3];
        for (i, &w) in t.weights.iter().enumerate() {
            let at = (y0 + t.left + i) * stride + x0 * channels;
            let row = &pixels[at..at + side_us * channels];
            accumulate_row(acc, row, channels, w);
        }
    }
    // Horizontal, then clamp and round to bytes.
    let columns = taps(side, size);
    let mut out = vec![0u8; size_us * size_us * 3];
    for y in 0..size_us {
        let row = &tmp[y * row_floats..y * row_floats + side_us * 3];
        let dst = &mut out[y * size_us * 3..(y + 1) * size_us * 3];
        for (outx, t) in columns.iter().enumerate() {
            let (mut r, mut g, mut b) = (0.0f32, 0.0f32, 0.0f32);
            for (i, &w) in t.weights.iter().enumerate() {
                let p = &row[(t.left + i) * 3..(t.left + i) * 3 + 3];
                r += p[0] * w;
                g += p[1] * w;
                b += p[2] * w;
            }
            dst[outx * 3] = byte(r);
            dst[outx * 3 + 1] = byte(g);
            dst[outx * 3 + 2] = byte(b);
        }
    }
    Some(out)
}

/// `NumCast::from(FloatNearest(clamp(t, 0.0, 255.0)))` for `u8`.
#[inline]
fn byte(t: f32) -> u8 {
    let t = if t < 0.0 { 0.0 } else if t > 255.0 { 255.0 } else { t };
    t.round() as u8
}

/// `acc[k] += f32(row byte) * w` for the colour bytes of one source row.
fn accumulate_row(acc: &mut [f32], row: &[u8], channels: usize, w: f32) {
    let done = if channels == 3 { crate::resize_msa::accumulate_rgb(acc, row, w) } else { 0 };
    if channels == 3 {
        for (a, &v) in acc[done..].iter_mut().zip(&row[done..]) {
            *a += BYTE_F32[v as usize] * w;
        }
    } else {
        for (a, p) in acc.chunks_exact_mut(3).zip(row.chunks_exact(4)) {
            a[0] += BYTE_F32[p[0] as usize] * w;
            a[1] += BYTE_F32[p[1] as usize] * w;
            a[2] += BYTE_F32[p[2] as usize] * w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::square_rgb;

    fn reference(pixels: &[u8], w: u32, h: u32, channels: usize, side: u32, size: u32) -> Vec<u8> {
        let (x0, y0) = ((w - side) / 2, (h - side) / 2);
        if channels == 3 {
            let img = image::RgbImage::from_raw(w, h, pixels.to_vec()).unwrap();
            let c = image::imageops::crop_imm(&img, x0, y0, side, side);
            image::imageops::resize(&*c, size, size, image::imageops::FilterType::Triangle).into_raw()
        } else {
            let img = image::RgbaImage::from_raw(w, h, pixels.to_vec()).unwrap();
            let c = image::imageops::crop_imm(&img, x0, y0, side, side);
            let r = image::imageops::resize(&*c, size, size, image::imageops::FilterType::Triangle);
            image::DynamicImage::ImageRgba8(r).to_rgb8().into_raw()
        }
    }

    /// Random images of many shapes and scales against the crate's resize.
    #[test]
    fn matches_image_crate_triangle_resize() {
        let mut seed = 0x1234_5678u32;
        let mut next = move || { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; seed };
        let mut cases = 0;
        for &(w, h) in &[(17u32, 17u32), (64, 48), (100, 133), (257, 255), (320, 320), (641, 480), (1000, 1000)] {
            for channels in [3usize, 4] {
                let pixels: Vec<u8> = (0..w * h * channels as u32).map(|_| next() as u8).collect();
                let side = w.min(h);
                for size in [1u32, 2, 3, 7, 16, 50, 150, 151, 320, side - 1] {
                    if size == 0 || size >= side { continue; }
                    let (x0, y0) = (((w - side) / 2) as usize, ((h - side) / 2) as usize);
                    let got = square_rgb(&pixels, w as usize * channels, channels, x0, y0, side, size).unwrap();
                    assert_eq!(got, reference(&pixels, w, h, channels, side, size), "{w}x{h}x{channels} -> {size}");
                    cases += 1;
                }
            }
        }
        assert!(cases > 80);
    }

    #[test]
    fn refuses_squares_outside_the_buffer() {
        let pixels = vec![0u8; 10 * 10 * 3];
        assert!(square_rgb(&pixels, 30, 3, 0, 0, 10, 5).is_some());
        assert!(square_rgb(&pixels, 30, 3, 1, 0, 10, 5).is_none());
        assert!(square_rgb(&pixels, 30, 3, 0, 1, 10, 5).is_none());
        assert!(square_rgb(&pixels, 30, 3, 0, 0, 10, 10).is_none());
        assert!(square_rgb(&pixels, 30, 5, 0, 0, 10, 5).is_none());
    }
}
