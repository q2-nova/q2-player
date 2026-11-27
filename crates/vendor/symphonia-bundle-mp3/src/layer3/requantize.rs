// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::common::FrameHeader;

use super::{common::*, GranuleChannel};

// [q2 patch]
#[cfg(target_arch = "mips")]
use crate::q2simd;
use crate::q2simd::{Lanes, S4};

use std::cmp::min;
use std::f64;

use lazy_static::lazy_static;

// [q2 patch] The (4/3)-power table and read_huffman_samples moved to
// q2huffman.rs.

/// [q2 patch] Lowest exponent of POW2_QUARTER: global_gain - 210 is at least
/// -210, the sub-block gain takes up to 56 more, and B is at most 255.
const POW2_QUARTER_MIN: i32 = -210 - 56 - 255;
/// Highest exponent: global_gain 255.
const POW2_QUARTER_MAX: i32 = 255 - 210;

lazy_static! {
    /// [q2 patch] 2^(0.25 * e) for every exponent e a scale-factor band can
    /// have, each computed by upstream's expression. Upstream called `powf`
    /// for every band of every granule.
    static ref POW2_QUARTER: [f32; (POW2_QUARTER_MAX - POW2_QUARTER_MIN + 1) as usize] = {
        let mut pow2 = [0f32; (POW2_QUARTER_MAX - POW2_QUARTER_MIN + 1) as usize];
        for (i, pow2) in pow2.iter_mut().enumerate() {
            *pow2 = f64::powf(2.0, 0.25 * f64::from(i as i32 + POW2_QUARTER_MIN)) as f32;
        }
        pow2
    };
}

/// [q2 patch] Multiplies every sample by `factor`, four per step.
#[inline(always)]
fn scale<V: Lanes>(samples: &mut [f32], factor: f32) {
    let vector = V::splat(factor);
    let mut chunks = samples.chunks_exact_mut(4);
    for chunk in &mut chunks {
        (V::load(chunk) * vector).store(chunk);
    }
    for sample in chunks.into_remainder() {
        *sample *= factor;
    }
}

/// Zero a sample buffer.
#[inline(always)]
pub(super) fn zero(buf: &mut [f32; 576]) {
    buf.fill(0.0);
}

/// Requantize long block samples in `buf`.
#[inline(always)]
fn requantize_long<V: Lanes>(channel: &GranuleChannel, bands: &[usize], buf: &mut [f32; 576]) {
    // For long blocks dequantization and scaling is governed by the following equation:
    //
    //                     xr(i) = s(i)^(4/3) * 2^(0.25*A) * 2^(-B)
    // where:
    //       s(i) is the decoded Huffman sample
    //      xr(i) is the dequantized sample
    // and:
    //      A = global_gain[gr] - 210
    //      B = scalefac_multiplier * (scalefacs[gr][ch][sfb] + (preflag[gr] * pretab[sfb]))
    //
    // Note: The samples in buf are the result of s(i)^(4/3) for each sample i.
    debug_assert!(bands.len() <= 23);

    // The preemphasis table is from table B.6 in ISO/IEC 11172-3.
    const PRE_EMPHASIS: [u8; 22] =
        [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 3, 2, 0];

    // Calculate A, it is constant for the entire requantization.
    let a = i32::from(channel.global_gain) - 210;

    let scalefac_shift = if channel.scalefac_scale { 2 } else { 1 };

    // Requantize each scale-factor band in buf.
    for (i, (start, end)) in bands.iter().zip(&bands[1..]).enumerate() {
        // Do not requantize bands starting after the rzero sample since all samples from there on
        // are 0.
        if *start >= channel.rzero {
            break;
        }

        // Lookup the pre-emphasis amount if required.
        let pre_emphasis = if channel.preflag { PRE_EMPHASIS[i] } else { 0 };

        // Calculate B.
        let b = i32::from((channel.scalefacs[i] + pre_emphasis) << scalefac_shift);

        // Calculate 2^(0.25*A) * 2^(-B). This can be rewritten as 2^{ 0.25 * (A - 4 * B) }.
        // Since scalefac_shift was multiplies by 4 above, the final equation becomes
        // 2^{ 0.25 * (A - B) }.
        // [q2 patch] From the table (the same expression).
        let pow2ab = POW2_QUARTER[(a - b - POW2_QUARTER_MIN) as usize];

        // Calculate the ending sample index for the scale-factor band, clamping it to the length of
        // the sample buffer.
        let band_end = min(*end, channel.rzero);

        // The sample buffer contains s(i)^(4/3), now multiply in 2^(0.25*A) * 2^(-B) to get xr(i).
        scale::<V>(&mut buf[*start..band_end], pow2ab);
    }
}

/// Requantize short block samples in `buf` starting at scale-factor band `sfb_init`.
#[inline(always)]
fn requantize_short<V: Lanes>(
    channel: &GranuleChannel,
    bands: &[usize],
    switch: usize,
    buf: &mut [f32; 576],
) {
    // For short blocks dequantization and scaling is governed by the following equation:
    //
    //                     xr(i) = s(i)^(4/3) * 2^(0.25*A) * 2^(-B)
    // where:
    //       s(i) is the decoded Huffman sample
    //      xr(i) is the dequantized sample
    // and:
    //      A = global_gain[gr] - 210 - (8 * subblock_gain[gr][win])
    //      B = scalefac_multiplier * scalefacs[gr][ch][sfb][win]
    //
    // Note: The samples in buf are the result of s(i)^(4/3) for each sample i.
    debug_assert!(bands.len() <= 40);

    // Calculate the window-independant part of A: global_gain[gr] - 210.
    let gain = i32::from(channel.global_gain) - 210;

    // Calculate A for each window.
    let a = [
        gain - 8 * i32::from(channel.subblock_gain[0]),
        gain - 8 * i32::from(channel.subblock_gain[1]),
        gain - 8 * i32::from(channel.subblock_gain[2]),
    ];

    // Likweise, the scalefac_multiplier is constant for the granule. The actual scale is multiplied
    // by 4 to combine the two pow2 operations into one by adding the exponents. The sum of the
    // exponent is multiplied by 0.25 so B must be multiplied by 4 to counter the quartering. A
    // bitshift operation is used for the actual multiplication, so scalefac_multiplier is named
    // scalefac_shift in this case.
    let scalefac_shift = if channel.scalefac_scale { 2 } else { 1 };

    for (i, (start, end)) in bands.iter().zip(&bands[1..]).enumerate() {
        // Do not requantize bands starting after the rzero sample since all samples from there on
        // are 0.
        if *start >= channel.rzero {
            break;
        }

        // Calculate B.
        let b = i32::from(channel.scalefacs[switch + i] << scalefac_shift);

        // Calculate 2^(0.25*A) * 2^(-B). This can be rewritten as 2^{ 0.25 * (A - 4 * B) }.
        // Since scalefac_shift multiplies by 4 above, the final equation becomes
        // 2^{ 0.25 * (A - B) }.
        // [q2 patch] From the table (the same expression).
        let pow2ab = POW2_QUARTER[(a[i % 3] - b - POW2_QUARTER_MIN) as usize];

        // Clamp the ending sample index to the rzero sample index. Since samples starting from
        // rzero are 0, there is no point in requantizing them.
        let win_end = min(*end, channel.rzero);

        // The sample buffer contains s(i)^(4/3), now multiply in 2^(0.25*A) * 2^(-B) to get
        // xr(i).
        scale::<V>(&mut buf[*start..win_end], pow2ab);
    }
}

/// Requantize samples in `buf` regardless of block type.
pub(super) fn requantize(header: &FrameHeader, channel: &GranuleChannel, buf: &mut [f32; 576]) {
    #[cfg(target_arch = "mips")]
    if q2simd::msa() {
        // SAFETY: `msa()` is true only when the kernel reports MSA.
        #[allow(unsafe_code)]
        unsafe {
            requantize_msa(header, channel, buf)
        };
        return;
    }
    requantize_lanes::<S4>(header, channel, buf);
}

#[cfg(target_arch = "mips")]
#[target_feature(enable = "msa")]
#[allow(unsafe_code)]
unsafe fn requantize_msa(header: &FrameHeader, channel: &GranuleChannel, buf: &mut [f32; 576]) {
    requantize_lanes::<q2simd::M4>(header, channel, buf);
}

#[inline(always)]
fn requantize_lanes<V: Lanes>(header: &FrameHeader, channel: &GranuleChannel, buf: &mut [f32; 576]) {
    match channel.block_type {
        BlockType::Short { is_mixed: false } => {
            requantize_short::<V>(channel, &SFB_SHORT_BANDS[header.sample_rate_idx], 0, buf);
        }
        BlockType::Short { is_mixed: true } => {
            // A mixed block is a combination of a long block and short blocks. The first few scale
            // factor bands, and thus samples, belong to a single long block, while the remaining
            // bands and samples belong to short blocks. Therefore, requantization for mixed blocks
            // can be decomposed into short and long block requantizations.
            //
            // As per ISO/IEC 11172-3, the short scale factor band at which the long block ends and
            // the short blocks begin is denoted by switch_point_s.
            let bands = SFB_MIXED_BANDS[header.sample_rate_idx];
            let switch = SFB_MIXED_SWITCH_POINT[header.sample_rate_idx];

            requantize_long::<V>(channel, &bands[..switch], buf);
            requantize_short::<V>(channel, &bands[switch..], switch, buf);
        }
        _ => {
            requantize_long::<V>(channel, &SFB_LONG_BANDS[header.sample_rate_idx], buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::BlockType;
    use super::super::q2ref_requantize as upstream;
    use super::super::GranuleChannel;
    use crate::common::FrameHeader;

    /// [q2 patch] requantize returns upstream's bits: every block type and
    /// sample rate table, the whole range of gains and scale factors.
    #[test]
    fn q2_requantize_matches_upstream_bit_for_bit() {
        let mut rng = crate::q2simd::tests::Rng(0x2468_ace1);
        for case in 0..3000 {
            let mut header: FrameHeader = crate::common::tests_header();
            header.sample_rate_idx = (rng.next() % 9) as usize;
            let block = match rng.next() % 5 {
                0 => BlockType::Short { is_mixed: false },
                1 => BlockType::Short { is_mixed: true },
                2 => BlockType::Start,
                _ => BlockType::Long,
            };
            let mut scalefacs = [0u8; 39];
            for s in scalefacs.iter_mut() {
                *s = (rng.next() % 32) as u8;
            }
            let make = |block_type| GranuleChannel {
                block_type,
                global_gain: (case % 256) as u8,
                subblock_gain: [(case % 8) as u8, (case / 8 % 8) as u8, (case / 64 % 8) as u8],
                scalefac_scale: case % 2 == 1,
                preflag: case % 3 == 1,
                scalefacs,
                rzero: if case % 7 == 0 { 576 } else { (case * 37) % 577 },
                ..Default::default()
            };
            let (ours, theirs) = match block {
                BlockType::Short { is_mixed } => {
                    (make(BlockType::Short { is_mixed }), make(BlockType::Short { is_mixed }))
                }
                BlockType::Start => (make(BlockType::Start), make(BlockType::Start)),
                _ => (make(BlockType::Long), make(BlockType::Long)),
            };
            let mut a = [0f32; 576];
            for v in a.iter_mut() {
                *v = if rng.next() % 9 == 0 { 0.0 } else { rng.float() * 300.0 };
            }
            let mut b = a;
            super::requantize(&header, &ours, &mut a);
            upstream::requantize(&header, &theirs, &mut b);
            assert_eq!(a.map(f32::to_bits), b.map(f32::to_bits), "case {}", case);
        }
    }
}
