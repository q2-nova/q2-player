//! MSA span kernels for drawing (Q2 only), called by `gfx/span.rs` once per
//! row, span or glyph. Each entry returns the number of leading pixels it
//! handled (a multiple of four, or zero without MSA) and the caller finishes
//! with the scalar code, which is also the specification: `Color::over` per
//! pixel (alpha 0 keeps the pixel, 255 stores the source with its top byte,
//! anything else stores `(s*a + d*(255-a) + 127) / 255` per channel with a
//! zero top byte).
//!
//! Layout (q2-toolchain docs/x2000-msa.md): four `0x00RRGGBB` pixels per vector, no
//! shuffle for per-channel work. Source and destination bytes interleave
//! (`ilvr.b`/`ilvl.b`) against interleaved weights so one `dotp_u.h` gives
//! `s*a + d*(255-a)` per channel in 16 bits; `x / 255` is
//! `(t + (t >> 8)) >> 8` with `t = x + 128`, whose last shift is the
//! `pckod.b` that packs the result. Constants are splat before the loop and
//! nothing moves between general and vector registers per pixel except the
//! four alpha bytes of a mask, loaded as one word.
//!
//! `--cfg q2_gfx_scalar` builds a Q2 binary that never enters these, for A/B
//! measurement.

#[cfg(all(q2_eq_msa, not(q2_gfx_scalar)))]
mod kernels {
    use core::arch::mips::*;
    use core::mem::transmute as cast;

    /// One span, row block or glyph for the kernels, as borrowed slices.
    pub(crate) enum Job<'a> {
        /// `color` over the pixels at `alpha`, in place.
        BlendColor(&'a mut [u32], u32, u32),
        /// `out[i]` = `color` over `under[i]` at `alpha`.
        BlendColorFrom(&'a mut [u32], &'a [u32], u32, u32),
        /// `src[i]` over `dst[i]` at `alpha`.
        BlendPixels(&'a mut [u32], &'a [u32], u32),
        /// `src[i]` over `dst[i]` at `alpha[i]`.
        BlendPixelsMask(&'a mut [u32], &'a [u32], &'a [u8]),
        /// `color` over `dst[i]` at `mask[i]`.
        BlendColorMask(&'a mut [u32], &'a [u8], u32),
        /// A padded mask `w` x `h` (rows `stride` bytes apart) in `color`
        /// with its first pixel at `at` of a canvas `width` wide.
        BlendColorMaskRect { pixels: &'a mut [u32], width: usize, at: usize, mask: &'a [u8], stride: usize, w: usize, h: usize, color: u32 },
        /// Green times `mg`, blue times `mb` (24-bit fractions), red kept.
        Tint(&'a mut [u32], u32, u32),
    }

    /// Runs `job` on its leading pixels in whole groups of four and returns
    /// how many it handled per row (0: none). Callers must have seen the
    /// kernel publish MSA; everything else is checked in the job's function.
    /// This is the only kernel function called from code without the `msa`
    /// target feature; every function below has the feature, so each one
    /// inlines into its caller.
    #[target_feature(enable = "msa")]
    pub(super) fn run(job: Job<'_>) -> usize {
        match job {
            Job::BlendColor(px, color, alpha) => blend_color(px, color, alpha),
            Job::BlendColorFrom(out, under, color, alpha) => blend_color_from(out, under, color, alpha),
            Job::BlendPixels(dst, src, alpha) => blend_pixels(dst, src, alpha),
            Job::BlendPixelsMask(dst, src, alpha) => blend_pixels_mask(dst, src, alpha),
            Job::BlendColorMask(dst, mask, color) => blend_color_mask(dst, mask, color),
            Job::BlendColorMaskRect { pixels, width, at, mask, stride, w, h, color } => {
                blend_color_mask_rect(pixels, width, at, mask, stride, w, h, color)
            }
            Job::Tint(px, mg, mb) => tint(px, mg, mb),
        }
    }

    // One function per job. Each returns the pixels per row it handled.

    /// `color` over `px` at `alpha`, in place.
    #[target_feature(enable = "msa")]
    #[inline]
    fn blend_color(px: &mut [u32], color: u32, alpha: u32) -> usize {
        let n = px.len() & !3;
        if !(1..255).contains(&alpha) { return 0; }
        let p = px.as_mut_ptr();
        let (s, w, rgb) = (splat_w(color), weights(alpha), splat_w(0x00FF_FFFF));
        let (c128, c8) = rounding();
        let mut i = 0;
        while i < n {
            // SAFETY: `i + 4 <= n <= px.len()`, so the group is inside `px`.
            // MSA comes from this function's target feature.
            unsafe { store(__msa_and_v(blend(s, load(p.add(i)), w, w, c128, c8), rgb), p.add(i)) };
            i += 4;
        }
        n
    }

    /// `out[i]` = `color` over `under[i]` at `alpha`.
    #[target_feature(enable = "msa")]
    #[inline]
    fn blend_color_from(out: &mut [u32], under: &[u32], color: u32, alpha: u32) -> usize {
        let n = out.len().min(under.len()) & !3;
        if !(1..255).contains(&alpha) { return 0; }
        let (o, u) = (out.as_mut_ptr(), under.as_ptr());
        let (s, w, rgb) = (splat_w(color), weights(alpha), splat_w(0x00FF_FFFF));
        let (c128, c8) = rounding();
        let mut i = 0;
        while i < n {
            // SAFETY: `i + 4 <= n`, which is no larger than either slice.
            // MSA comes from this function's target feature.
            unsafe { store(__msa_and_v(blend(s, load(u.add(i)), w, w, c128, c8), rgb), o.add(i)) };
            i += 4;
        }
        n
    }

    /// `src[i]` over `dst[i]` at `alpha`.
    #[target_feature(enable = "msa")]
    #[inline]
    fn blend_pixels(dst: &mut [u32], src: &[u32], alpha: u32) -> usize {
        let n = dst.len().min(src.len()) & !3;
        if !(1..255).contains(&alpha) { return 0; }
        let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
        let (w, rgb) = (weights(alpha), splat_w(0x00FF_FFFF));
        let (c128, c8) = rounding();
        let mut i = 0;
        while i < n {
            // SAFETY: `i + 4 <= n`, which is no larger than either slice.
            // MSA comes from this function's target feature.
            unsafe { store(__msa_and_v(blend(load(s.add(i)), load(d.add(i)), w, w, c128, c8), rgb), d.add(i)) };
            i += 4;
        }
        n
    }

    /// `src[i]` over `dst[i]` at `alpha[i]`.
    #[target_feature(enable = "msa")]
    #[inline]
    fn blend_pixels_mask(dst: &mut [u32], src: &[u32], alpha: &[u8]) -> usize {
        let n = dst.len().min(src.len()).min(alpha.len()) & !3;
        let (d, s, a) = (dst.as_mut_ptr(), src.as_ptr(), alpha.as_ptr());
        let rgb = splat_w(0x00FF_FFFF);
        let (c128, c8) = rounding();
        let mut i = 0;
        while i < n {
            // SAFETY: `i + 4 <= n`, which is no larger than any of the three
            // slices; the alpha word is read unaligned.
            unsafe {
                let word = (a.add(i) as *const u32).read_unaligned();
                if word != 0 {
                    store(blend_alpha(load(s.add(i)), load(d.add(i)), word, c128, c8, rgb), d.add(i));
                }
            }
            i += 4;
        }
        n
    }

    /// `color` over `dst[i]` at `mask[i]`.
    #[target_feature(enable = "msa")]
    #[inline]
    fn blend_color_mask(dst: &mut [u32], mask: &[u8], color: u32) -> usize {
        let n = dst.len().min(mask.len()) & !3;
        // SAFETY: one row of `n` pixels and bytes, a multiple of four no
        // larger than either slice.
        unsafe { mask_rows(dst.as_mut_ptr(), 0, mask.as_ptr(), 0, n, 1, color) };
        n
    }

    /// A padded mask `w` x `h` in `color` (see `Job::BlendColorMaskRect`).
    #[target_feature(enable = "msa")]
    #[inline]
    fn blend_color_mask_rect(pixels: &mut [u32], width: usize, at: usize, mask: &[u8], stride: usize, w: usize, h: usize, color: u32) -> usize {
        // Each row is handled in groups of four pixels, up to three past
        // `w`: there the mask's padding is zero and the pixel is rewritten
        // unchanged.
        let Some(n) = super::mask_rect_groups(pixels.len(), width, at, mask.len(), stride, w, h) else { return 0 };
        // SAFETY: `mask_rect_groups` returns `n` (a multiple of four) only
        // when row `h - 1`'s `n` pixels end inside `pixels` and its `n` mask
        // bytes inside `mask`; earlier rows end before them.
        unsafe { mask_rows(pixels.as_mut_ptr().add(at), width, mask.as_ptr(), stride, n, h, color) };
        n
    }

    /// Green times `mg`, blue times `mb` (24-bit fractions), red kept.
    #[target_feature(enable = "msa")]
    #[inline]
    fn tint(px: &mut [u32], mg: u32, mb: u32) -> usize {
        let n = px.len() & !3;
        let p = px.as_mut_ptr();
        // SAFETY: each group is at `i` with `i + 4 <= n <= px.len()`, inside
        // `px`. MSA comes from this function's target feature; the casts are
        // between 16-byte vector types.
        unsafe {
            let (mg, mb) = (__msa_fill_w(mg as i32), __msa_fill_w(mb as i32));
            let (c8, c16, c24) = (__msa_fill_w(8), __msa_fill_w(16), __msa_fill_w(24));
            let (blue, green, red) = (splat_w(0xFF), splat_w(0xFF00), splat_w(0xFF_0000));
            let mut i = 0;
            while i < n {
                let v = load(p.add(i));
                let g = __msa_and_v(cast(__msa_srl_w(cast(v), c8)), blue);
                let g = __msa_and_v(cast(__msa_srl_w(__msa_mulv_w(cast(g), mg), c16)), green);
                let b: v16u8 = cast(__msa_srl_w(__msa_mulv_w(cast(__msa_and_v(v, blue)), mb), c24));
                store(__msa_or_v(__msa_or_v(__msa_and_v(v, red), g), b), p.add(i));
                i += 4;
            }
        }
        n
    }

    // Helpers. `#[inline(always)]` with the job functions' target feature:
    // no closures, so no intrinsic is compiled without the feature.

    /// Four pixels at `p`.
    ///
    /// # Safety
    /// `p` must be valid for reading four `u32`s; any alignment.
    #[target_feature(enable = "msa")]
    #[inline(always)]
    unsafe fn load(p: *const u32) -> v16u8 {
        // SAFETY: the caller provides four readable words at `p` and `ld.w`
        // accepts any alignment. MSA comes from this function's target feature.
        unsafe { cast::<v4i32, v16u8>(__msa_ld_w::<0>(p as *mut u8)) }
    }

    /// Stores `v` as four pixels at `p`.
    ///
    /// # Safety
    /// `p` must be valid for writing four `u32`s; any alignment.
    #[target_feature(enable = "msa")]
    #[inline(always)]
    unsafe fn store(v: v16u8, p: *mut u32) {
        // SAFETY: the caller provides four writable words at `p` and `st.w`
        // accepts any alignment. MSA comes from this function's target feature.
        unsafe { __msa_st_w::<0>(cast::<v16u8, v4i32>(v), p as *mut u8) }
    }

    /// `x` in each of the four words.
    #[target_feature(enable = "msa")]
    #[inline(always)]
    fn splat_w(x: u32) -> v16u8 {
        // SAFETY: MSA comes from this function's target feature; the cast is
        // between 16-byte vector types.
        unsafe { cast::<v4i32, v16u8>(__msa_fill_w(x as i32)) }
    }

    /// Weights for `blend`: bytes `a, 255 - a` repeated, for `alpha` <= 255.
    #[target_feature(enable = "msa")]
    #[inline(always)]
    fn weights(alpha: u32) -> v16u8 {
        // SAFETY: MSA comes from this function's target feature; the cast is
        // between 16-byte vector types.
        unsafe { cast::<v8i16, v16u8>(__msa_fill_h((alpha | (255 - alpha) << 8) as i32)) }
    }

    /// The rounding constant 128 and the shift 8 of `blend`, per halfword.
    #[target_feature(enable = "msa")]
    #[inline(always)]
    fn rounding() -> (v8i16, v8i16) {
        // SAFETY: MSA comes from this function's target feature.
        unsafe { (__msa_fill_h(128), __msa_fill_h(8)) }
    }

    /// `(s * w_even + d * w_odd + 127) / 255` per byte, for weight pairs in
    /// `w_lo` (pixels 0 and 1) and `w_hi` (pixels 2 and 3).
    #[target_feature(enable = "msa")]
    #[inline(always)]
    fn blend(s: v16u8, d: v16u8, w_lo: v16u8, w_hi: v16u8, c128: v8i16, c8: v8i16) -> v16u8 {
        // SAFETY: MSA comes from this function's target feature; the casts
        // are between 16-byte vector types.
        unsafe {
            let lo: v8i16 = cast(__msa_dotp_u_h(cast(__msa_ilvr_b(cast(d), cast(s))), w_lo));
            let hi: v8i16 = cast(__msa_dotp_u_h(cast(__msa_ilvl_b(cast(d), cast(s))), w_hi));
            let lo = __msa_addv_h(lo, c128);
            let hi = __msa_addv_h(hi, c128);
            let lo = __msa_addv_h(lo, __msa_srl_h(lo, c8));
            let hi = __msa_addv_h(hi, __msa_srl_h(hi, c8));
            cast::<v16i8, v16u8>(__msa_pckod_b(cast(hi), cast(lo)))
        }
    }

    /// `blend` at the per-pixel alphas in the four bytes of `word`, with
    /// `Color::over`'s top byte: kept from the blend at alpha 0 and 255,
    /// zero otherwise.
    #[target_feature(enable = "msa")]
    #[inline(always)]
    fn blend_alpha(s: v16u8, d: v16u8, word: u32, c128: v8i16, c8: v8i16, rgb: v16u8) -> v16u8 {
        // SAFETY: MSA comes from this function's target feature; the casts
        // are between 16-byte vector types.
        unsafe {
            // Each alpha byte four times: the pixel's alpha in every channel.
            let a: v16i8 = cast(__msa_fill_w(word as i32));
            let a: v8i16 = cast(__msa_ilvr_b(a, a));
            let a: v16u8 = cast(__msa_ilvr_h(a, a));
            let inv = __msa_xori_b::<255>(a);
            let w_lo: v16u8 = cast(__msa_ilvr_b(cast(inv), cast(a)));
            let w_hi: v16u8 = cast(__msa_ilvl_b(cast(inv), cast(a)));
            let out = blend(s, d, w_lo, w_hi, c128, c8);
            // a + 1 <= 1 (unsigned bytes) exactly when a is 255 or 0.
            let extreme: v16u8 = cast(__msa_clei_u_b::<1>(cast(__msa_addvi_b::<1>(cast(a)))));
            __msa_and_v(out, __msa_or_v(extreme, rgb))
        }
    }

    /// Rows of a coverage mask in `color`: `h` rows of `n` pixels, rows
    /// `dst_stride` pixels and `mask_stride` bytes apart. Groups without
    /// coverage are skipped.
    ///
    /// # Safety
    /// `n` must be a multiple of four, and for every row `r < h` the `n`
    /// pixels at `dst + r * dst_stride` must be valid for reads and writes
    /// and the `n` bytes at `mask + r * mask_stride` valid for reads.
    #[target_feature(enable = "msa")]
    #[inline(always)]
    unsafe fn mask_rows(dst: *mut u32, dst_stride: usize, mask: *const u8, mask_stride: usize, n: usize, h: usize, color: u32) {
        let (s, rgb) = (splat_w(color), splat_w(0x00FF_FFFF));
        let (c128, c8) = rounding();
        let mut row = 0;
        while row < h {
            // SAFETY: `row < h` and `i + 4 <= n`, so every group is inside a
            // row the caller provides; the mask word is read unaligned.
            unsafe {
                let (d, m) = (dst.add(row * dst_stride), mask.add(row * mask_stride));
                let mut i = 0;
                while i < n {
                    let word = (m.add(i) as *const u32).read_unaligned();
                    if word != 0 {
                        store(blend_alpha(s, load(d.add(i)), word, c128, c8, rgb), d.add(i));
                    }
                    i += 4;
                }
            }
            row += 1;
        }
    }
}

/// The pixels per row the vector code handles for a padded mask `w` wide
/// and `h` high (`w` rounded up to four), or `None` when a row's groups
/// would leave the `pixels`-long canvas (first pixel `at`, rows `width`
/// apart) or the `mask`-long mask (rows `stride` apart, a multiple of four).
#[cfg_attr(not(all(q2_eq_msa, not(q2_gfx_scalar))), allow(dead_code))]
pub(crate) fn mask_rect_groups(pixels: usize, width: usize, at: usize, mask: usize, stride: usize, w: usize, h: usize) -> Option<usize> {
    if w == 0 || h == 0 || stride % 4 != 0 { return None; }
    let n = w.checked_add(3)? & !3;
    let pixel_end = (h - 1).checked_mul(width)?.checked_add(at)?.checked_add(n)?;
    let mask_end = (h - 1).checked_mul(stride)?.checked_add(n)?;
    (n <= stride && pixel_end <= pixels && mask_end <= mask).then_some(n)
}

/// Whether the entries below use MSA in this build on this CPU.
#[inline]
pub(crate) fn available() -> bool {
    #[cfg(all(q2_eq_msa, not(q2_gfx_scalar)))]
    { crate::audio::msa_available() }
    #[cfg(not(all(q2_eq_msa, not(q2_gfx_scalar))))]
    { false }
}

// The entries compile to `0`/`false` where the kernels do not exist.
#[cfg(not(all(q2_eq_msa, not(q2_gfx_scalar))))]
mod portable {
    pub(crate) fn blend_color(_: &mut [u32], _: u32, _: u32) -> usize { 0 }
    pub(crate) fn blend_color_from(_: &mut [u32], _: &[u32], _: u32, _: u32) -> usize { 0 }
    pub(crate) fn blend_pixels(_: &mut [u32], _: &[u32], _: u32) -> usize { 0 }
    pub(crate) fn blend_pixels_mask(_: &mut [u32], _: &[u32], _: &[u8]) -> usize { 0 }
    pub(crate) fn blend_color_mask(_: &mut [u32], _: &[u8], _: u32) -> usize { 0 }
    pub(crate) fn blend_color_mask_rect(_: &mut [u32], _: usize, _: usize, _: &[u8], _: usize, _: usize, _: usize, _: u32) -> bool { false }
    pub(crate) fn tint(_: &mut [u32], _: u32, _: u32) -> usize { 0 }
}
#[cfg(not(all(q2_eq_msa, not(q2_gfx_scalar))))]
pub(crate) use portable::*;

#[cfg(all(q2_eq_msa, not(q2_gfx_scalar)))]
mod entries {
    use super::kernels::{self, Job};

    /// The leading pixels (per row) the kernels handled: a multiple of four,
    /// or zero without MSA.
    fn run(job: Job<'_>) -> usize {
        if !super::available() { return 0; }
        // SAFETY: the kernel published MSA for this process, which is all
        // `kernels::run` requires of its caller.
        unsafe { kernels::run(job) }
    }

    /// `color` over `px` at `alpha` in 1..=254, in place.
    pub(crate) fn blend_color(px: &mut [u32], color: u32, alpha: u32) -> usize { run(Job::BlendColor(px, color, alpha)) }
    pub(crate) fn blend_color_from(out: &mut [u32], under: &[u32], color: u32, alpha: u32) -> usize { run(Job::BlendColorFrom(out, under, color, alpha)) }
    pub(crate) fn blend_pixels(dst: &mut [u32], src: &[u32], alpha: u32) -> usize { run(Job::BlendPixels(dst, src, alpha)) }
    pub(crate) fn blend_pixels_mask(dst: &mut [u32], src: &[u32], alpha: &[u8]) -> usize { run(Job::BlendPixelsMask(dst, src, alpha)) }
    pub(crate) fn blend_color_mask(dst: &mut [u32], mask: &[u8], color: u32) -> usize { run(Job::BlendColorMask(dst, mask, color)) }
    /// A whole padded mask (see `span::blend_color_mask_rect`). False
    /// (nothing drawn) when a row's last group would leave either buffer.
    pub(crate) fn blend_color_mask_rect(pixels: &mut [u32], width: usize, at: usize, mask: &[u8], stride: usize, w: usize, h: usize, color: u32) -> bool {
        run(Job::BlendColorMaskRect { pixels, width, at, mask, stride, w, h, color }) != 0
    }
    pub(crate) fn tint(px: &mut [u32], mg: u32, mb: u32) -> usize { run(Job::Tint(px, mg, mb)) }
}
#[cfg(all(q2_eq_msa, not(q2_gfx_scalar)))]
pub(crate) use entries::*;

#[cfg(test)]
mod tests {
    use super::mask_rect_groups;

    /// Every small case: an accepted rectangle keeps each row's groups of
    /// four inside both buffers, and covers the mask's width.
    #[test]
    fn accepted_mask_rectangles_stay_inside_both_buffers() {
        let mut accepted = 0u32;
        for pixels in 0..40usize {
            for width in 0..9usize {
                for at in [0usize, 1, 5, 17, 39, 40] {
                    for (mask, stride) in [(0usize, 4usize), (8, 4), (12, 4), (16, 8), (33, 8), (40, 12), (40, 6), (40, 0)] {
                        for w in 0..11usize {
                            for h in 0..6usize {
                                let Some(n) = mask_rect_groups(pixels, width, at, mask, stride, w, h) else { continue };
                                accepted += 1;
                                assert!(n % 4 == 0 && n >= w && n < w + 4 && n <= stride && stride % 4 == 0 && w > 0 && h > 0);
                                for row in 0..h {
                                    assert!(at + row * width + n <= pixels, "{pixels} {width} {at} {w} {h}");
                                    assert!(row * stride + n <= mask, "{mask} {stride} {w} {h}");
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(accepted > 1000);
        // A fitting rectangle is accepted; arithmetic that overflows is refused.
        assert_eq!(mask_rect_groups(375 * 320, 375, 375 * 319 + 363, 12, 12, 10, 1), Some(12));
        assert_eq!(mask_rect_groups(375 * 320, 375, 375 * 319 + 364, 12, 12, 10, 1), None);
        for big in [usize::MAX, usize::MAX - 2, usize::MAX / 2 + 1] {
            assert_eq!(mask_rect_groups(usize::MAX, big, 4, usize::MAX, 4, 4, 3), None);
            if big > usize::MAX / 2 + 1 { assert_eq!(mask_rect_groups(usize::MAX, 4, big, usize::MAX, 4, 4, 3), None); }
            assert_eq!(mask_rect_groups(usize::MAX, 4, 4, usize::MAX, 4, big, 3), None);
            assert_eq!(mask_rect_groups(usize::MAX, 4, 4, usize::MAX, big & !3, 4, 3), None);
        }
    }
}
