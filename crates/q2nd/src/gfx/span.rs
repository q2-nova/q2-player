//! Span kernels: the blends of `Color::over` over a run of pixels, and the
//! night tint. The drawing code calls these per row or block; on the Q2 runs
//! of eight pixels or more go to the MSA kernels (`crate::gfx_msa`) when the
//! CPU has MSA, and the scalar code here does
//! the rest. Both give `Color::over` exactly:
//!
//! - alpha 0 leaves the pixel, alpha >= 255 stores the source with its top
//!   byte, anything else stores `(s*a + d*(255-a) + 127) / 255` per channel
//!   with a zero top byte.
//!
//! `x / 255` for `x = v + 127`, `v <= 255 * 255`, is `(t + (t >> 8)) >> 8`
//! with `t = v + 128` (checked for every `v` in the tests).

/// Whether this build and CPU use the MSA kernels (for the benchmark's report).
pub fn vector_active() -> bool { crate::gfx_msa::available() }

/// Shorter runs stay scalar: the call and the vector constants cost more.
const MIN_VECTOR: usize = 8;

/// `s` over `d` at `a` in 1..=254.
#[inline(always)]
fn mix(s: u32, d: u32, a: u32) -> u32 {
    let inv = 255 - a;
    let rb = (s & 0xFF00FF) * a + (d & 0xFF00FF) * inv + 0x80_0080;
    let rb = ((rb + ((rb >> 8) & 0xFF00FF)) >> 8) & 0xFF00FF;
    let g = ((s >> 8) & 0xFF) * a + ((d >> 8) & 0xFF) * inv + 128;
    rb | ((g + (g >> 8)) & 0xFF00)
}

/// `color` over every pixel at one alpha, in place.
pub fn blend_color(px: &mut [u32], color: u32, alpha: u32) {
    if alpha == 0 { return; }
    if alpha >= 255 { return px.fill(color); }
    let mut done = 0;
    if px.len() >= MIN_VECTOR { done = crate::gfx_msa::blend_color(px, color, alpha); }
    scalar_blend_color(&mut px[done..], color, alpha);
}

pub fn scalar_blend_color(px: &mut [u32], color: u32, alpha: u32) {
    if alpha == 0 { return; }
    if alpha >= 255 { return px.fill(color); }
    let inv = 255 - alpha;
    let rb = (color & 0xFF00FF) * alpha + 0x80_0080;
    let g = ((color >> 8) & 0xFF) * alpha + 128;
    for p in px {
        let d = *p;
        let x = rb + (d & 0xFF00FF) * inv;
        let y = g + ((d >> 8) & 0xFF) * inv;
        *p = (((x + ((x >> 8) & 0xFF00FF)) >> 8) & 0xFF00FF) | ((y + (y >> 8)) & 0xFF00);
    }
}

/// `out[i]` = `color` over `under[i]` at one alpha.
pub fn blend_color_from(out: &mut [u32], under: &[u32], color: u32, alpha: u32) {
    let n = out.len().min(under.len());
    let (out, under) = (&mut out[..n], &under[..n]);
    if alpha == 0 { return out.copy_from_slice(under); }
    if alpha >= 255 { return out.fill(color); }
    let mut done = 0;
    if n >= MIN_VECTOR { done = crate::gfx_msa::blend_color_from(out, under, color, alpha); }
    scalar_blend_color_from(&mut out[done..], &under[done..], color, alpha);
}

pub fn scalar_blend_color_from(out: &mut [u32], under: &[u32], color: u32, alpha: u32) {
    if alpha == 0 { return out.copy_from_slice(&under[..out.len()]); }
    if alpha >= 255 { return out.fill(color); }
    let inv = 255 - alpha;
    let rb = (color & 0xFF00FF) * alpha + 0x80_0080;
    let g = ((color >> 8) & 0xFF) * alpha + 128;
    for (p, d) in out.iter_mut().zip(under) {
        let x = rb + (d & 0xFF00FF) * inv;
        let y = g + ((d >> 8) & 0xFF) * inv;
        *p = (((x + ((x >> 8) & 0xFF00FF)) >> 8) & 0xFF00FF) | ((y + (y >> 8)) & 0xFF00);
    }
}

/// `src[i]` over `dst[i]` at one alpha.
pub fn blend_pixels(dst: &mut [u32], src: &[u32], alpha: u32) {
    let n = dst.len().min(src.len());
    let (dst, src) = (&mut dst[..n], &src[..n]);
    if alpha == 0 { return; }
    if alpha >= 255 { return dst.copy_from_slice(src); }
    let mut done = 0;
    if n >= MIN_VECTOR { done = crate::gfx_msa::blend_pixels(dst, src, alpha); }
    scalar_blend_pixels(&mut dst[done..], &src[done..], alpha);
}

pub fn scalar_blend_pixels(dst: &mut [u32], src: &[u32], alpha: u32) {
    if alpha == 0 { return; }
    if alpha >= 255 { return dst.copy_from_slice(&src[..dst.len()]); }
    for (d, s) in dst.iter_mut().zip(src) { *d = mix(*s, *d, alpha); }
}

/// `src[i]` over `dst[i]` at `alpha[i]`.
pub fn blend_pixels_mask(dst: &mut [u32], src: &[u32], alpha: &[u8]) {
    let n = dst.len().min(src.len()).min(alpha.len());
    let (dst, src, alpha) = (&mut dst[..n], &src[..n], &alpha[..n]);
    let mut done = 0;
    if n >= MIN_VECTOR { done = crate::gfx_msa::blend_pixels_mask(dst, src, alpha); }
    scalar_blend_pixels_mask(&mut dst[done..], &src[done..], &alpha[done..]);
}

pub fn scalar_blend_pixels_mask(dst: &mut [u32], src: &[u32], alpha: &[u8]) {
    for ((d, s), a) in dst.iter_mut().zip(src).zip(alpha) {
        match *a {
            0 => {}
            255 => *d = *s,
            a => *d = mix(*s, *d, a as u32),
        }
    }
}

/// `color` over `dst[i]` at `mask[i]`: one row of a coverage mask.
pub fn blend_color_mask(dst: &mut [u32], mask: &[u8], color: u32) {
    let n = dst.len().min(mask.len());
    let (dst, mask) = (&mut dst[..n], &mask[..n]);
    let mut done = 0;
    if n >= MIN_VECTOR { done = crate::gfx_msa::blend_color_mask(dst, mask, color); }
    scalar_blend_color_mask(&mut dst[done..], &mask[done..], color);
}

pub fn scalar_blend_color_mask(dst: &mut [u32], mask: &[u8], color: u32) {
    let (crb, cg) = (color & 0xFF00FF, (color >> 8) & 0xFF);
    for (d, a) in dst.iter_mut().zip(mask) {
        match *a {
            0 => {}
            255 => *d = color,
            a => {
                let (a, inv, under) = (a as u32, 255 - a as u32, *d);
                let x = crb * a + (under & 0xFF00FF) * inv + 0x80_0080;
                let y = cg * a + ((under >> 8) & 0xFF) * inv + 128;
                *d = (((x + ((x >> 8) & 0xFF00FF)) >> 8) & 0xFF00FF) | ((y + (y >> 8)) & 0xFF00);
            }
        }
    }
}

/// A coverage mask (a glyph) `w` x `h` with rows `stride` bytes apart,
/// drawn in `color` with its first pixel at `at` of a canvas `width` wide.
/// `padded` promises that `stride` is a multiple of four and that the bytes
/// of each mask row from `w` up to it are zero, which lets the vector code
/// work in whole groups of four pixels.
pub fn blend_color_mask_rect(pixels: &mut [u32], width: usize, at: usize, mask: &[u8], stride: usize, w: usize, h: usize, color: u32, padded: bool) {
    if w == 0 || h == 0 { return; }
    if padded && w >= 4 && crate::gfx_msa::blend_color_mask_rect(pixels, width, at, mask, stride, w, h, color) { return; }
    for row in 0..h {
        let start = at + row * width;
        scalar_blend_color_mask(&mut pixels[start..start + w], &mask[row * stride..row * stride + w], color);
    }
}

/// The multiplier for a channel scaled by `k / 25500`: for `i` in 0..=255,
/// `i * k / 25500 == (i * m) >> 24` (checked for every warmth in the tests).
pub fn tint_multiplier(k: u32) -> u32 {
    (((k as u64) << 24).div_ceil(25500)) as u32
}

/// The night tint: green times `mg`, blue times `mb` (24-bit fractions from
/// `tint_multiplier`), red kept, top byte cleared.
pub fn tint(px: &mut [u32], mg: u32, mb: u32) {
    let mut done = 0;
    if px.len() >= MIN_VECTOR { done = crate::gfx_msa::tint(px, mg, mb); }
    scalar_tint(&mut px[done..], mg, mb);
}

pub fn scalar_tint(px: &mut [u32], mg: u32, mb: u32) {
    for p in px {
        let v = *p;
        *p = (v & 0xFF0000) | (((((v >> 8) & 0xFF) * mg) >> 16) & 0xFF00) | (((v & 0xFF) * mb) >> 24);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::Color;

    fn noise(count: usize, seed: u32) -> Vec<u32> {
        let mut value = seed;
        (0..count).map(|_| { value = value.wrapping_mul(1664525).wrapping_add(1013904223); value }).collect()
    }

    #[test]
    fn division_by_255_is_exact_for_every_blend_sum() {
        for v in 0..=255u32 * 255 {
            let t = v + 128;
            assert_eq!((t + (t >> 8)) >> 8, (v + 127) / 255, "{v}");
        }
    }

    #[test]
    fn mix_equals_color_over_for_every_channel_pair_and_alpha() {
        for a in 1..255u32 {
            for s in 0..256u32 {
                for d in (0..256u32).step_by(3).chain([254, 255]) {
                    // Different values per channel, and top bytes that must not leak.
                    let (sp, dp) = (0xA5000000 | s << 16 | (255 - s) << 8 | (s ^ 0x55), 0x5A000000 | d << 16 | (d ^ 0xAA) << 8 | (255 - d));
                    assert_eq!(mix(sp, dp, a), Color(sp).over(Color(dp), a).0, "{sp:x} {dp:x} {a}");
                }
            }
        }
    }

    #[test]
    fn spans_equal_color_over_including_top_bytes_and_alpha_extremes() {
        for n in [0usize, 1, 3, 4, 7, 8, 13, 64, 375] {
            let src = noise(n, 1);
            let under = noise(n, 2);
            let mut alphas: Vec<u8> = noise(n, 3).iter().map(|v| (v >> 13) as u8).collect();
            for (i, a) in alphas.iter_mut().enumerate() { if i % 5 == 0 { *a = 0 } else if i % 7 == 0 { *a = 255 } }
            for alpha in [0u32, 1, 70, 127, 128, 254, 255, 300] {
                let color = 0x7F123456;
                let expect: Vec<u32> = under.iter().map(|d| Color(color).over(Color(*d), alpha).0).collect();
                let mut out = under.clone();
                blend_color(&mut out, color, alpha);
                assert_eq!(out, expect);
                let mut out = vec![0; n];
                blend_color_from(&mut out, &under, color, alpha);
                assert_eq!(out, expect);
                let expect: Vec<u32> = under.iter().zip(&src).map(|(d, s)| Color(*s).over(Color(*d), alpha).0).collect();
                let mut out = under.clone();
                blend_pixels(&mut out, &src, alpha);
                assert_eq!(out, expect);
            }
            let expect: Vec<u32> = (0..n).map(|i| Color(src[i]).over(Color(under[i]), alphas[i] as u32).0).collect();
            let mut out = under.clone();
            blend_pixels_mask(&mut out, &src, &alphas);
            assert_eq!(out, expect);
            let expect: Vec<u32> = (0..n).map(|i| Color(0x80ABCDEF).over(Color(under[i]), alphas[i] as u32).0).collect();
            let mut out = under.clone();
            blend_color_mask(&mut out, &alphas, 0x80ABCDEF);
            assert_eq!(out, expect);
        }
    }

    #[test]
    fn tint_multiplier_equals_the_division_for_every_warmth_and_value() {
        for q in 0..=255u32 {
            for (step, shift) in [(18, 8), (45, 0)] {
                let k = 25500 - q * step;
                let m = tint_multiplier(k);
                for i in 0..256u32 {
                    assert_eq!((i * m) >> 24, i * k / 25500, "{q} {i}");
                    let mut p = [0xFF000000 | i << shift | 0x330000];
                    if shift == 8 { scalar_tint(&mut p, m, 1 << 24) } else { scalar_tint(&mut p, 1 << 24, m) }
                    assert_eq!(p[0], 0x330000 | (i * k / 25500) << shift);
                }
            }
        }
    }
}
