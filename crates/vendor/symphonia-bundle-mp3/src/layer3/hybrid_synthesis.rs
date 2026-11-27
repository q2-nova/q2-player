// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Justification: Some loops are better expressed without a range loop.
#![allow(clippy::needless_range_loop)]

use crate::common::FrameHeader;

use super::{common::*, GranuleChannel};

// [q2 patch]
#[cfg(target_arch = "mips")]
use crate::q2simd;
use crate::q2simd::{Aligned16, Lanes, S4};

use std::{convert::TryInto, f64};

use lazy_static::lazy_static;

lazy_static! {
    /// Hybrid synthesesis IMDCT window coefficients for: Long, Start, Short, and End block, in that
    /// order.
    ///
    /// For long blocks:
    ///
    /// ```text
    /// W[ 0..36] = sin(PI/36.0 * (i + 0.5))
    /// ```
    ///
    /// For start blocks:
    ///
    /// ```text
    /// W[ 0..18] = sin(PI/36.0 * (i + 0.5))
    /// W[18..24] = 1.0
    /// W[24..30] = sin(PI/12.0 * ((i - 18) - 0.5))
    /// W[30..36] = 0.0
    /// ```
    ///
    /// For short blocks (to be applied to each 12 sample window):
    ///
    /// ```text
    /// W[ 0..12] = sin(PI/12.0 * (i + 0.5))
    /// W[12..36] = 0.0
    /// ```
    ///
    /// For end blocks:
    ///
    /// ```text
    /// W[ 0..6 ] = 0.0
    /// W[ 6..12] = sin(PI/12.0 * ((i - 6) + 0.5))
    /// W[12..18] = 1.0
    /// W[18..36] = sin(PI/36.0 * (i + 0.5))
    /// ```
    static ref IMDCT_WINDOWS: [[f32; 36]; 4] = {
        const PI_36: f64 = f64::consts::PI / 36.0;
        const PI_12: f64 = f64::consts::PI / 12.0;

        let mut windows = [[0f32; 36]; 4];

        // Window for Long blocks.
        for i in 0..36 {
            windows[0][i] = (PI_36 * (i as f64 + 0.5)).sin() as f32;
        }

        // Window for Start blocks (indicies 30..36 implictly 0.0).
        for i in 0..18 {
            windows[1][i] = (PI_36 * (i as f64 + 0.5)).sin() as f32;
        }
        for i in 18..24 {
            windows[1][i] = 1.0;
        }
        for i in 24..30 {
            windows[1][i] = (PI_12 * ((i - 18) as f64 + 0.5)).sin() as f32;
        }

        // Window for Short blocks.
        for i in 0..12 {
            windows[2][i] = (PI_12 * (i as f64 + 0.5)).sin() as f32;
        }

        // Window for End blocks (indicies 0..6 implicitly 0.0).
        for i in 6..12 {
            windows[3][i] = (PI_12 * ((i - 6) as f64 + 0.5)).sin() as f32;
        }
        for i in 12..18 {
            windows[3][i] = 1.0;
        }
        for i in 18..36 {
            windows[3][i] = (PI_36 * (i as f64 + 0.5)).sin() as f32;
        }

        windows
   };
}

lazy_static! {
    /// Lookup table of cosine coefficients for half of a 12-point IMDCT.
    ///
    /// This table is derived from the general expression:
    ///
    /// ```text
    /// cos12[i][k] = cos(PI/24.0 * (2*i + 1 + N/2) * (2*k + 1))
    /// ```
    /// where:
    ///     `N=12`, `i=N/4..3N/4`, and `k=0..N/2`.
    static ref IMDCT_HALF_COS_12: [[f32; 6]; 6] = {
        const PI_24: f64 = f64::consts::PI / 24.0;

        let mut cos = [[0f32; 6]; 6];

        for (i, cos_i) in cos.iter_mut().enumerate() {
            for (k, cos_ik) in cos_i.iter_mut().enumerate() {
                // Only compute the middle half of the cosine lookup table (i offset by 3).
                let n = (2 * (i + 3) + (12 / 2) + 1) * (2 * k + 1);
                *cos_ik = (PI_24 * n as f64).cos() as f32;
            }
        }

        cos
    };
}

lazy_static! {
    /// Pair of lookup tables, CS and CA, for alias reduction.
    ///
    /// As per ISO/IEC 11172-3, CS and CA are calculated as follows:
    ///
    /// ```text
    /// cs[i] =  1.0 / sqrt(1.0 + c[i]^2)
    /// ca[i] = c[i] / sqrt(1.0 + c[i]^2)
    /// ```
    ///
    /// where:
    /// ```text
    /// c[i] = [ -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037 ]
    /// ```
    static ref ANTIALIAS_CS_CA: ([f32; 8], [f32; 8]) = {
        const C: [f64; 8] = [ -0.6, -0.535, -0.33, -0.185, -0.095, -0.041, -0.0142, -0.0037 ];

        let mut cs = [0f32; 8];
        let mut ca = [0f32; 8];

        for i in 0..8 {
            let sqrt = f64::sqrt(1.0 + (C[i] * C[i]));
            cs[i] = (1.0 / sqrt) as f32;
            ca[i] = (C[i] / sqrt) as f32;
        }

        (cs, ca)
    };
}

/// Reorder samples that are part of short blocks into sub-band order.
pub(super) fn reorder(header: &FrameHeader, channel: &mut GranuleChannel, buf: &mut [f32; 576]) {
    // Only short blocks are reordered.
    if let BlockType::Short { is_mixed } = channel.block_type {
        // Every short block is split into 3 equally sized windows as illustrated below (e.g. for
        // a short scale factor band with win_len=4):
        //
        //    <- Window #1 ->  <- Window #2 ->  <- Window #3 ->
        //   [ 0 | 1 | 2 | 3 ][ 4 | 5 | 6 | 7 ][ 8 | 9 | a | b ]
        //    <-----  3 * Short Scale Factor Band Width  ----->
        //
        // Reordering interleaves the samples of each window as follows:
        //
        //   [ 0 | 4 | 8 | 1 | 5 | 9 | 2 | 6 | a | 3 | 7 | b ]
        //    <----  3 * Short Scale Factor Band Width  ---->
        //
        // Basically, reordering interleaves the 3 windows the same way that 3 planar audio buffers
        // would be interleaved.
        debug_assert!(channel.rzero <= 576);

        // In mixed blocks, only the short bands can be re-ordered. Determine the applicable bands.
        let bands = if is_mixed {
            let switch = SFB_MIXED_SWITCH_POINT[header.sample_rate_idx];
            &SFB_MIXED_BANDS[header.sample_rate_idx][switch..]
        }
        else {
            &SFB_SHORT_BANDS[header.sample_rate_idx]
        };

        let mut reorder_buf = [0f32; 576];

        let start = bands[0];
        let mut i = start;

        for (((s0, s1), s2), s3) in
            bands.iter().zip(&bands[1..]).zip(&bands[2..]).zip(&bands[3..]).step_by(3)
        {
            // Do not reorder short blocks that begin after the rzero partition boundary since
            // they're zeroed.
            if *s0 >= channel.rzero {
                break;
            }

            // The three short sample windows.
            let win0 = &buf[*s0..*s1];
            let win1 = &buf[*s1..*s2];
            let win2 = &buf[*s2..*s3];

            // Interleave the three short sample windows.
            for ((w0, w1), w2) in win0.iter().zip(win1).zip(win2) {
                reorder_buf[i + 0] = *w0;
                reorder_buf[i + 1] = *w1;
                reorder_buf[i + 2] = *w2;
                i += 3;
            }
        }

        // Copy reordered samples from the reorder buffer to the actual sample buffer.
        buf[start..i].copy_from_slice(&reorder_buf[start..i]);

        // After reordering, the start of the rzero partition may no longer be valid. Update it.
        channel.rzero = channel.rzero.max(i);
    }
}

/// Applies the anti-aliasing filter to sub-bands that are not part of short blocks.
pub(super) fn antialias(channel: &mut GranuleChannel, samples: &mut [f32; 576]) {
    // The maximum number of sub-bands to anti-alias depends on block type.
    let sb_limit = match channel.block_type {
        // Short blocks are never anti-aliased.
        BlockType::Short { is_mixed: false } => return,
        // Mixed blocks have a long block span the first 36 samples (2 sub-bands). Therefore, only
        // anti-alias these two sub-bands.
        BlockType::Short { is_mixed: true } => 2,
        // All other block types require all 32 sub-bands to be anti-aliased.
        _ => 32,
    };

    // Amortize the lazy_static fetch over the entire anti-aliasing operation.
    let (cs, ca): &([f32; 8], [f32; 8]) = &ANTIALIAS_CS_CA;

    // The sub-band that intersects the start of the rzero partition. All sub-bands after this one
    // are zeroed and do-not need anti-aliasing.
    let sb_rzero = channel.rzero / 18;

    // The anti-aliasing filter must be applied up-to the last non-zero sub-band. After
    // anti-aliasing, the first zeroed sub-band may have non-zero values "smeared" into it.
    // Therefore, the rzero must be updated.
    channel.rzero = 18 * sb_limit.min(sb_rzero + 2).min(32);

    // Anti-aliasing is performed using 8 butterfly calculations at the boundaries of ADJACENT
    // sub-bands. For each calculation, there are two samples: lower and upper. For each iteration,
    // the lower sample index advances backwards from the boundary, while the upper sample index
    // advances forward from the boundary.
    //
    // For example, let B(li, ui) represent the butterfly calculation where li and ui are the
    // indicies of the lower and upper samples respectively. If j is the index of the first sample
    // of a sub-band, then the iterations are as follows:
    //
    // B(j-1,j), B(j-2,j+1), B(j-3,j+2), B(j-4,j+3), B(j-5,j+4), B(j-6,j+5), B(j-7,j+6), B(j-8,j+7)
    //
    // The butterfly calculation itself can be illustrated as follows:
    //
    //              * cs[i]
    //   l0 -------o------(-)------> l1
    //               \    /                  l1 = l0 * cs[i] - u0 * ca[i]
    //                \  / * ca[i]           u1 = u0 * cs[i] + l0 * ca[i]
    //                 \
    //               /  \  * ca[i]           where:
    //             /     \                       cs[i], ca[i] are constant values for iteration i,
    //   u0 ------o------(+)-------> u1          derived from table B.9 of ISO/IEC 11172-3.
    //             * cs[i]
    //
    // Note that all butterfly calculations only involve two samples, and all iterations are
    // independant of each other. This lends itself well for SIMD processing.
    //
    // [q2 patch] Four butterflies per vector: lanes are i = 0..4 and 4..8,
    // the lower samples loaded and stored reversed.
    let rzero = channel.rzero;
    #[cfg(target_arch = "mips")]
    if q2simd::msa() {
        // SAFETY: `msa()` is true only when the kernel reports MSA.
        #[allow(unsafe_code)]
        unsafe {
            antialias_msa(rzero, cs, ca, samples)
        };
        return;
    }
    antialias_lanes::<S4>(rzero, cs, ca, samples);
}

#[cfg(target_arch = "mips")]
#[target_feature(enable = "msa")]
#[allow(unsafe_code)]
unsafe fn antialias_msa(rzero: usize, cs: &[f32; 8], ca: &[f32; 8], samples: &mut [f32; 576]) {
    antialias_lanes::<q2simd::M4>(rzero, cs, ca, samples);
}

#[inline(always)]
fn antialias_lanes<V: Lanes>(rzero: usize, cs: &[f32; 8], ca: &[f32; 8], samples: &mut [f32; 576]) {
    let cs = [V::load(&cs[0..4]), V::load(&cs[4..8])];
    let ca = [V::load(&ca[0..4]), V::load(&ca[4..8])];
    for sb in (18..rzero.min(576)).step_by(18) {
        for h in 0..2 {
            let li = sb - 4 - 4 * h;
            let ui = sb + 4 * h;
            let lower = V::load(&samples[li..li + 4]).reverse();
            let upper = V::load(&samples[ui..ui + 4]);
            (lower * cs[h] - upper * ca[h]).reverse().store(&mut samples[li..li + 4]);
            (upper * cs[h] + lower * ca[h]).store(&mut samples[ui..ui + 4]);
        }
    }
}

// [q2 patch] Hybrid synthesis is rewritten for the Q2 (README.q2.md).
// Upstream ran one scalar 36-point IMDCT per sub-band, then a separate
// frequency-inversion pass over the granule. This version:
//
// - runs the long-block IMDCT on four sub-bands at once (lane l is sub-band
//   sb0 + l, each lane upstream's scalar arithmetic);
// - keeps the overlap as `overlap[i][sb]`, so the four lanes of one time
//   index are four adjacent floats;
// - negates the odd samples of odd sub-bands (frequency inversion) as the
//   block is stored, and no longer makes that pass;
// - copies the empty sub-bands above rzero four at a time.
//
// Output bits equal upstream's hybrid_synthesis followed by
// frequency_inversion (tests below; upstream's code is the reference in
// q2ref/hybrid_synthesis.rs).

/// The overlap of one channel: `[i][sb]`, time index first.
pub(super) type Overlap = [[f32; 32]; 18];

lazy_static! {
    /// IMDCT_WINDOWS with every coefficient repeated for four lanes.
    static ref IMDCT_WINDOWS_X4: Aligned16<[[[f32; 4]; 36]; 4]> = {
        let mut windows = Aligned16([[[0f32; 4]; 36]; 4]);
        for (x4, window) in windows.0.iter_mut().zip(IMDCT_WINDOWS.iter()) {
            for (x4, coefficient) in x4.iter_mut().zip(window.iter()) {
                *x4 = [*coefficient; 4];
            }
        }
        windows
    };
}

/// Performs hybrid synthesis (IMDCT and windowing) and the frequency inversion.
pub(super) fn hybrid_synthesis(
    channel: &GranuleChannel,
    overlap: &mut Overlap,
    samples: &mut [f32; 576],
) {
    #[cfg(target_arch = "mips")]
    if q2simd::msa() {
        // SAFETY: `msa()` is true only when the kernel reports MSA.
        #[allow(unsafe_code)]
        unsafe {
            hybrid_synthesis_msa(channel, overlap, samples)
        };
        return;
    }
    hybrid_synthesis_lanes::<S4>(channel, overlap, samples);
}

#[cfg(target_arch = "mips")]
#[target_feature(enable = "msa")]
#[allow(unsafe_code)]
unsafe fn hybrid_synthesis_msa(
    channel: &GranuleChannel,
    overlap: &mut Overlap,
    samples: &mut [f32; 576],
) {
    hybrid_synthesis_lanes::<q2simd::M4>(channel, overlap, samples);
}

/// Time indices at which four-lane groups start to cover 18 values: the last
/// group overlaps the one before by two.
const GROUPS: [usize; 5] = [0, 4, 8, 12, 14];

#[inline(always)]
fn hybrid_synthesis_lanes<V: Lanes>(
    channel: &GranuleChannel,
    overlap: &mut Overlap,
    samples: &mut [f32; 576],
) {
    // The first sub-band without samples (rzero is at most 576).
    let sb_limit = ((channel.rzero + 17) / 18).min(32);

    // The boundary between long (below) and short (above) sub-bands.
    let sb_split = match channel.block_type {
        BlockType::Short { is_mixed: false } => 0,
        BlockType::Short { is_mixed: true } => 2,
        _ => 32,
    };
    let sb_long_end = sb_split.min(sb_limit);

    // Long blocks, four sub-bands per IMDCT. A last group of fewer than four
    // computes all four lanes and stores only its own.
    if sb_long_end > 0 {
        let window: &[[f32; 4]; 36] = match channel.block_type {
            BlockType::Start => &IMDCT_WINDOWS_X4.0[1],
            BlockType::End => &IMDCT_WINDOWS_X4.0[3],
            _ => &IMDCT_WINDOWS_X4.0[0],
        };

        let mut sb = 0;
        while sb < sb_long_end {
            imdct36::imdct36_x4::<V>(samples, sb, (sb_long_end - sb).min(4), window, overlap);
            sb += 4;
        }
    }

    // Short blocks, one sub-band at a time as upstream.
    if sb_long_end < sb_limit {
        let window: &[f32; 36] = &IMDCT_WINDOWS[2];

        for sb in sb_long_end..sb_limit {
            let start = 18 * sb;
            let sub_band: &mut [f32; 18] = (&mut samples[start..(start + 18)]).try_into().unwrap();

            let mut sb_overlap = [0f32; 18];
            for i in 0..18 {
                sb_overlap[i] = overlap[i][sb];
            }

            imdct12_win(sub_band, window, &mut sb_overlap);

            for i in 0..18 {
                overlap[i][sb] = sb_overlap[i];
            }
            if sb & 1 == 1 {
                for i in (1..18).step_by(2) {
                    sub_band[i] = -sub_band[i];
                }
            }
        }
    }

    // The sub-bands above have no samples: their output is the overlap, and
    // the next overlap is zero. One at a time up to a multiple of four, then
    // four per vector.
    let mut sb = sb_limit;
    while sb < 32 && sb & 3 != 0 {
        for i in 0..18 {
            let value = overlap[i][sb];
            samples[18 * sb + i] = if sb & i & 1 == 1 { -value } else { value };
            overlap[i][sb] = 0.0;
        }
        sb += 1;
    }
    let zero = V::splat(0.0);
    while sb < 32 {
        let mut y = [zero; 18];
        for i in 0..18 {
            let value = V::load(&overlap[i][sb..sb + 4]);
            y[i] = if i & 1 == 1 { value.neg_odd() } else { value };
            zero.store(&mut overlap[i][sb..sb + 4]);
        }
        store_sub_bands(&y, samples, sb, 4);
        sb += 4;
    }
}

/// Stores `y[i]` lane `l` to `samples[18 * (sb0 + l) + i]` for the first
/// `active` lanes.
#[inline(always)]
fn store_sub_bands<V: Lanes>(y: &[V; 18], samples: &mut [f32; 576], sb0: usize, active: usize) {
    for &g in &GROUPS {
        let rows = V::transpose([y[g], y[g + 1], y[g + 2], y[g + 3]]);
        for (l, row) in rows.iter().enumerate().take(active) {
            let at = 18 * (sb0 + l) + g;
            row.store(&mut samples[at..at + 4]);
        }
    }
}

/// Performs the 12-point IMDCT, and windowing for each of the 3 short windows of a short block, and
/// then overlap-adds the result.
fn imdct12_win(x: &mut [f32; 18], window: &[f32; 36], overlap: &mut [f32; 18]) {
    let cos12: &[[f32; 6]; 6] = &IMDCT_HALF_COS_12;

    let mut tmp = [0.0; 36];

    for w in 0..3 {
        for i in 0..3 {
            // Compute the 12-point IMDCT for each of the 3 short windows using a half-size IMDCT
            // followed by post-processing.
            //
            // In general, the IMDCT is defined as:
            //
            //        (N/2)-1
            // y[i] =   SUM   { x[k] * cos(PI/2N * (2i + 1 + N/2) * (2k + 1)) }
            //          k=0
            //
            // For N=12, the IMDCT becomes:
            //
            //         5
            // y[i] = SUM { x[k] * cos(PI/24 * (2i + 7) * (2k + 1)) }
            //        k=0
            //
            // The cosine twiddle factors are easily indexable by i and k, and are therefore
            // pre-computed and placed into a look-up table.
            //
            // Further, y[3..0] = -y[3..6], and y[12..9] = y[6..9] which reduces the amount of work
            // by half.
            //
            // Therefore, it is possible to split the half-size IMDCT computation into two halves.
            // In the calculations below, yl is the left-half output of the half-size IMDCT, and yr
            // is the right-half.

            let yl = (x[w] * cos12[i][0])
                + (x[3 * 1 + w] * cos12[i][1])
                + (x[3 * 2 + w] * cos12[i][2])
                + (x[3 * 3 + w] * cos12[i][3])
                + (x[3 * 4 + w] * cos12[i][4])
                + (x[3 * 5 + w] * cos12[i][5]);

            let yr = (x[w] * cos12[i + 3][0])
                + (x[3 * 1 + w] * cos12[i + 3][1])
                + (x[3 * 2 + w] * cos12[i + 3][2])
                + (x[3 * 3 + w] * cos12[i + 3][3])
                + (x[3 * 4 + w] * cos12[i + 3][4])
                + (x[3 * 5 + w] * cos12[i + 3][5]);

            // Each adjacent 12-point IMDCT windows are overlapped and added in the output, with the
            // first and last 6 samples of the output always being 0.
            //
            // Each sample from the 12-point IMDCT is multiplied by the appropriate window function
            // as specified in ISO/IEC 11172-3. The values of the window function are pre-computed
            // and given by window[0..12].
            //
            // Since there are 3 IMDCT windows (indexed by w), y[0..12] is computed 3 times.
            // For the purpose of the diagram below, we label these IMDCT windows as: y0[0..12],
            // y1[0..12], and y2[0..12], for IMDCT windows 0..3 respectively.
            //
            // Therefore, the overlap-and-add operation can be visualized as below:
            //
            // 0             6           12           18           24           30            36
            // +-------------+------------+------------+------------+------------+-------------+
            // |      0      |  y0[..6]   |  y0[..6]   |  y1[6..]   |  y2[6..]   |      0      |
            // |     (6)     |            |  + y1[6..] |  + y2[..6] |            |     (6)     |
            // +-------------+------------+------------+------------+------------+-------------+
            // .             .            .            .            .            .             .
            // .             +-------------------------+            .            .             .
            // .             |      IMDCT #1 (y0)      |            .            .             .
            // .             +-------------------------+            .            .             .
            // .             .            +-------------------------+            .             .
            // .             .            |      IMDCT #2 (y1)      |            .             .
            // .             .            +-------------------------+            .             .
            // .             .            .            +-------------------------+             .
            // .             .            .            |      IMDCT #3 (y2)      |             .
            // .             .            .            +-------------------------+             .
            // .             .            .            .            .            .             .
            //
            // Since the 12-point IMDCT was decomposed into a half-size IMDCT and post-processing
            // operations, and further split into left and right halves, each iteration of this loop
            // produces 4 output samples.

            tmp[6 + 6 * w + 3 - i - 1] += -yl * window[3 - i - 1];
            tmp[6 + 6 * w + i + 3] += yl * window[i + 3];
            tmp[6 + 6 * w + i + 6] += yr * window[i + 6];
            tmp[6 + 6 * w + 12 - i - 1] += yr * window[12 - i - 1];
        }
    }

    // Overlap-add.
    for i in 0..18 {
        x[i] = tmp[i] + overlap[i];
        overlap[i] = tmp[i + 18];
    }
}

#[cfg(test)]
mod tests {
    use super::imdct12_win;
    use super::IMDCT_WINDOWS;
    use std::f64;

    /// [q2 patch] antialias + hybrid_synthesis return the bits of upstream's
    /// antialias + hybrid_synthesis + frequency_inversion: every block type,
    /// every rzero that changes the sub-band limits, zeros of both signs,
    /// overlap carried across 400 granules.
    #[test]
    fn q2_hybrid_synthesis_matches_upstream_bit_for_bit() {
        use super::super::q2ref_hybrid as upstream;
        use super::super::GranuleChannel;
        use super::BlockType;

        let mut rng = crate::q2simd::tests::Rng(0x0bad_cafe);
        let mut ours_overlap = [[0f32; 32]; 18];
        let mut theirs_overlap = [[0f32; 18]; 32];

        for granule in 0..400 {
            let block = |n: u32| match n % 6 {
                0 | 1 => BlockType::Long,
                2 => BlockType::Start,
                3 => BlockType::End,
                4 => BlockType::Short { is_mixed: false },
                _ => BlockType::Short { is_mixed: true },
            };
            let kind = rng.next();
            let rzero = match granule % 5 {
                0 => 576,
                1 => 0,
                _ => (rng.next() % 577) as usize,
            };
            let mut ours = GranuleChannel { block_type: block(kind), rzero, ..Default::default() };
            let mut theirs = GranuleChannel { block_type: block(kind), rzero, ..Default::default() };

            let mut a = [0f32; 576];
            for (i, v) in a.iter_mut().enumerate() {
                *v = if i >= rzero {
                    0.0
                }
                else {
                    match rng.next() % 12 {
                        0 => 0.0,
                        1 => -0.0,
                        _ => rng.float() * 3.0,
                    }
                };
            }
            let mut b = a;

            super::antialias(&mut ours, &mut a);
            super::hybrid_synthesis(&ours, &mut ours_overlap, &mut a);

            upstream::antialias(&mut theirs, &mut b);
            upstream::hybrid_synthesis(&theirs, &mut theirs_overlap, &mut b);
            upstream::frequency_inversion(&mut b);

            assert_eq!(ours.rzero, theirs.rzero);
            assert_eq!(a.map(f32::to_bits), b.map(f32::to_bits), "granule {} rzero {}", granule, rzero);
            for sb in 0..32 {
                for i in 0..18 {
                    assert_eq!(
                        ours_overlap[i][sb].to_bits(),
                        theirs_overlap[sb][i].to_bits(),
                        "granule {} overlap sb {} i {}",
                        granule,
                        sb,
                        i
                    );
                }
            }
        }
    }

    fn imdct12_analytical(x: &[f32; 6]) -> [f32; 12] {
        const PI_24: f64 = f64::consts::PI / 24.0;

        let mut result = [0f32; 12];

        for i in 0..12 {
            let mut sum = 0.0;
            for k in 0..6 {
                sum +=
                    (x[k] as f64) * (PI_24 * ((2 * i + (12 / 2) + 1) * (2 * k + 1)) as f64).cos();
            }
            result[i] = sum as f32;
        }

        result
    }

    #[test]
    fn verify_imdct12_win() {
        const TEST_VECTOR: [f32; 18] = [
            0.0976, 0.9321, 0.6138, 0.0857, 0.0433, 0.4855, 0.2144, 0.8488, //
            0.6889, 0.2983, 0.1957, 0.7037, 0.0052, 0.0197, 0.3188, 0.5123, //
            0.2994, 0.7157,
        ];

        let window = &IMDCT_WINDOWS[2];

        let mut actual = TEST_VECTOR;
        let mut overlap = [0.0; 18];
        imdct12_win(&mut actual, window, &mut overlap);

        // The following block performs 3 analytical 12-point IMDCTs over the test vector, and then
        // windows and overlaps the results to generate the final result.
        let expected = {
            let mut expected = [0f32; 36];

            let mut x0 = [0f32; 6];
            let mut x1 = [0f32; 6];
            let mut x2 = [0f32; 6];

            for i in 0..6 {
                x0[i] = TEST_VECTOR[3 * i + 0];
                x1[i] = TEST_VECTOR[3 * i + 1];
                x2[i] = TEST_VECTOR[3 * i + 2];
            }

            let imdct0 = imdct12_analytical(&x0);
            let imdct1 = imdct12_analytical(&x1);
            let imdct2 = imdct12_analytical(&x2);

            for i in 0..12 {
                expected[6 + i] += imdct0[i] * window[i];
                expected[12 + i] += imdct1[i] * window[i];
                expected[18 + i] += imdct2[i] * window[i];
            }

            expected
        };

        for i in 0..18 {
            assert!((expected[i] - actual[i]).abs() < 0.00001);
            assert!((expected[i + 18] - overlap[i]).abs() < 0.00001);
        }
    }
}

mod imdct36 {
    use super::{store_sub_bands, Overlap, GROUPS};
    use crate::q2simd::Lanes;
    use std::convert::TryInto;

    /// Performs an Inverse Modified Discrete Cosine Transform (IMDCT) transforming 18
    /// frequency-domain input samples, into 36 time-domain output samples.
    ///
    /// This is a straight-forward implementation of the IMDCT using Szu-Wei Lee's algorithm
    /// published in article [1].
    ///
    /// [1] Szu-Wei Lee, "Improved algorithm for efficient computation of the forward and backward
    /// MDCT in MPEG audio coder", IEEE Transactions on Circuits and Systems II: Analog and Digital
    /// Signal Processing, vol. 48, no. 10, pp. 990-994, 2001.
    ///
    /// https://ieeexplore.ieee.org/document/974789
    ///
    /// [q2 patch] Four sub-bands at once: lane l is sub-band `sb0 + l` (`sb0`
    /// a multiple of 4). The samples of the first `active` sub-bands are
    /// replaced by the windowed first half plus the overlap, with the odd
    /// samples of odd sub-bands negated (frequency inversion), and their
    /// overlap by the windowed second half. The other lanes are computed and
    /// discarded. `window` holds each coefficient four times.
    #[inline(always)]
    pub(super) fn imdct36_x4<V: Lanes>(
        samples: &mut [f32; 576],
        sb0: usize,
        active: usize,
        window: &[[f32; 4]; 36],
        overlap: &mut Overlap,
    ) {
        let base = 18 * sb0;
        let four: &[f32; 72] = (&samples[base..base + 72]).try_into().unwrap();

        // x[i] lane l = samples[18 * (sb0 + l) + i].
        let mut x = [V::splat(0.0); 18];
        for &g in &GROUPS {
            let t = V::transpose([
                V::load(&four[g..g + 4]),
                V::load(&four[18 + g..22 + g]),
                V::load(&four[36 + g..40 + g]),
                V::load(&four[54 + g..58 + g]),
            ]);
            x[g] = t[0];
            x[g + 1] = t[1];
            x[g + 2] = t[2];
            x[g + 3] = t[3];
        }

        let dct = dct_iv(&x);

        // Mapping of the IMDCT to the DCT-IV, with n = 18 and y the IMDCT output:
        //
        //  0            n            2n           3n           4n
        //  +------------+------------+------------+------------+
        //  | dct[n..2n] | dct[2n..n] | dct[n..0]  | dct[0..n]  |
        //  +------------+------------+------------+------------+
        //  |     ( + )  |    ( - )   |    ( - )   |    ( - )   |
        //
        // (see upstream's imdct36). The first half is added to the overlap;
        // the second half is the next overlap.
        let mut y = [V::splat(0.0); 18];
        let mut old = [V::splat(0.0); 18];
        for i in 0..9 {
            old[i] = V::load(&overlap[i][sb0..sb0 + 4]);
            y[i] = old[i] + dct[9 + i] * V::load(&window[i]);
        }
        for i in 9..18 {
            old[i] = V::load(&overlap[i][sb0..sb0 + 4]);
            y[i] = old[i] - dct[27 - i - 1] * V::load(&window[i]);
        }
        for i in (1..18).step_by(2) {
            y[i] = y[i].neg_odd();
        }

        for i in 18..36 {
            let d = if i < 27 { dct[27 - i - 1] } else { dct[i - 27] };
            let next = -d * V::load(&window[i]);
            let next = if active < 4 { next.keep_first(active, old[i - 18]) } else { next };
            next.store(&mut overlap[i - 18][sb0..sb0 + 4]);
        }

        store_sub_bands(&y, samples, sb0, active);
    }

    /// Continutation of `imdct36`.
    ///
    /// Step 2: Mapping N/2-point DCT-IV to N/2-point SDCT-II.
    #[inline(always)]
    fn dct_iv<V: Lanes>(x: &[V; 18]) -> [V; 18] {
        // Scale factors for input samples. Computed from (16).
        // 2 * cos(PI * (2*m + 1) / (2*36)
        const SCALE: [f32; 18] = [
            1.998_096_443_163_715_6, // m=0
            1.982_889_722_747_620_8, // m=1
            1.952_592_014_239_866_7, // m=2
            1.907_433_901_496_453_9, // m=3
            1.847_759_065_022_573_5, // m=4
            1.774_021_666_356_443_4, // m=5
            1.686_782_891_625_771_4, // m=6
            1.586_706_680_582_470_6, // m=7
            1.474_554_673_620_247_9, // m=8
            1.351_180_415_231_320_7, // m=9
            1.217_522_858_017_441_3, // m=10
            1.074_599_216_693_647_8, // m=11
            0.923_497_226_470_067_7, // m=12
            0.765_366_864_730_179_7, // m=13
            0.601_411_599_008_546_1, // m=14
            0.432_879_227_876_205_8, // m=15
            0.261_052_384_440_103_0, // m=16
            0.087_238_774_730_672_0, // m=17
        ];

        let mut samples = *x;
        for i in 0..18 {
            samples[i] = V::splat(SCALE[i]) * x[i];
        }

        let mut y = sdct_ii_18(&samples);

        // [q2 patch] Upstream divides by 2.0; multiplying by 0.5 is the same
        // value for every float.
        let half = V::splat(0.5);
        y[0] = y[0] * half;
        for i in 1..18 {
            y[i] = (y[i] * half) - y[i - 1];
        }
        y
    }

    /// Continutation of `imdct36`.
    ///
    /// Step 3: Decompose N/2-point SDCT-II into two N/4-point SDCT-IIs.
    #[inline(always)]
    fn sdct_ii_18<V: Lanes>(x: &[V; 18]) -> [V; 18] {
        // Scale factors for odd input samples. Computed from (23).
        // 2 * cos(PI * (2*m + 1) / 36)
        const SCALE: [f32; 9] = [
            1.992_389_396_183_491_1,  // m=0
            1.931_851_652_578_136_6,  // m=1
            1.812_615_574_073_299_9,  // m=2
            1.638_304_088_577_983_6,  // m=3
            std::f32::consts::SQRT_2, // m=4
            1.147_152_872_702_092_3,  // m=5
            0.845_236_523_481_398_9,  // m=6
            0.517_638_090_205_041_9,  // m=7
            0.174_311_485_495_316_3,  // m=8
        ];

        let mut even = [x[0]; 9];
        let mut odd = [x[0]; 9];
        for i in 0..9 {
            even[i] = x[i] + x[17 - i];
            odd[i] = V::splat(SCALE[i]) * (x[i] - x[17 - i]);
        }

        let even = sdct_ii_9(&even);
        let odd = sdct_ii_9(&odd);

        let mut y = [x[0]; 18];
        for i in 0..9 {
            y[2 * i] = even[i];
            y[2 * i + 1] = odd[i];
        }

        for i in (3..18).step_by(2) {
            y[i] = y[i] - y[i - 2];
        }
        y
    }

    /// Continutation of `imdct36`.
    ///
    /// Step 4: Computation of 9-point (N/4) SDCT-II. Returns upstream's
    /// y[0], y[2], .., y[16].
    #[inline(always)]
    fn sdct_ii_9<V: Lanes>(x: &[V; 9]) -> [V; 9] {
        const D: [f32; 7] = [
            -1.732_050_807_568_877_2, // -sqrt(3.0)
            1.879_385_241_571_816_6,  // -2.0 * cos(8.0 * PI / 9.0)
            -0.347_296_355_333_860_8, // -2.0 * cos(4.0 * PI / 9.0)
            -1.532_088_886_237_956_0, // -2.0 * cos(2.0 * PI / 9.0)
            -0.684_040_286_651_337_8, // -2.0 * sin(8.0 * PI / 9.0)
            -1.969_615_506_024_416_0, // -2.0 * sin(4.0 * PI / 9.0)
            -1.285_575_219_373_078_5, // -2.0 * sin(2.0 * PI / 9.0)
        ];

        let a01 = x[3] + x[5];
        let a02 = x[3] - x[5];
        let a03 = x[6] + x[2];
        let a04 = x[6] - x[2];
        let a05 = x[1] + x[7];
        let a06 = x[1] - x[7];
        let a07 = x[8] + x[0];
        let a08 = x[8] - x[0];

        let a09 = x[4] + a05;
        let a10 = a01 + a03;
        let a11 = a10 + a07;
        let a12 = a03 - a07;
        let a13 = a01 - a07;
        let a14 = a01 - a03;
        let a15 = a02 - a04;
        let a16 = a15 + a08;
        let a17 = a04 + a08;
        let a18 = a02 - a08;
        let a19 = a02 + a04;
        // [q2 patch] Upstream multiplies by 2.0 here and for y[12]; adding a
        // float to itself is the same value.
        let a20 = (x[4] + x[4]) - a05;

        let m1 = V::splat(D[0]) * a06;
        let m2 = V::splat(D[1]) * a12;
        let m3 = V::splat(D[2]) * a13;
        let m4 = V::splat(D[3]) * a14;
        let m5 = V::splat(D[0]) * a16;
        let m6 = V::splat(D[4]) * a17;
        let m7 = V::splat(D[5]) * a18; // Note: the cited paper has an error, a1 should be a18.
        let m8 = V::splat(D[6]) * a19;

        let a21 = a20 + m2;
        let a22 = a20 - m2;
        let a23 = a20 + m3;
        let a24 = m1 + m6;
        let a25 = m1 - m6;
        let a26 = m1 + m7;

        [
            a09 + a11,
            m8 - a26,
            m4 - a21,
            m5,
            a22 - m3,
            a25 - m7,
            a11 - (a09 + a09),
            a24 + m8,
            a23 + m4,
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::imdct36_x4;
        use crate::q2simd::S4;
        use std::f64;

        fn imdct36_analytical(x: &[f32; 18]) -> [f32; 36] {
            let mut result = [0f32; 36];

            const PI_72: f64 = f64::consts::PI / 72.0;

            for i in 0..36 {
                let mut sum = 0.0;
                for j in 0..18 {
                    sum +=
                        (x[j] as f64) * (PI_72 * (((2 * i) + 1 + 18) * ((2 * j) + 1)) as f64).cos();
                }
                result[i] = sum as f32;
            }
            result
        }

        #[test]
        fn verify_imdct36() {
            const TEST_VECTOR: [f32; 18] = [
                0.0976, 0.9321, 0.6138, 0.0857, 0.0433, 0.4855, 0.2144, 0.8488, //
                0.6889, 0.2983, 0.1957, 0.7037, 0.0052, 0.0197, 0.3188, 0.5123, //
                0.2994, 0.7157,
            ];

            const WINDOW: [[f32; 4]; 36] = [[1.0; 4]; 36];

            // Sub-band 0 (no frequency inversion) of a granule.
            let mut samples = [0f32; 576];
            samples[..18].copy_from_slice(&TEST_VECTOR);
            let mut overlap = [[0f32; 32]; 18];
            imdct36_x4::<S4>(&mut samples, 0, 4, &WINDOW, &mut overlap);

            let expected = imdct36_analytical(&TEST_VECTOR);

            for i in 0..18 {
                assert!((expected[i] - samples[i]).abs() < 0.00001);
                assert!((expected[i + 18] - overlap[i][0]).abs() < 0.00001);
            }
        }
    }
}
