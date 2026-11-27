// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `synthesis` module implements the polyphase synthesis filterbank of the MPEG audio standard.

/// Synthesis window D[i], defined in Table B.3 of ISO/IEC 11172-3.
#[allow(clippy::unreadable_literal)]
#[rustfmt::skip]
const SYNTHESIS_D_UNALIGNED: [f32; 512] = [
     0.000000000, -0.000015259, -0.000015259, -0.000015259,
    -0.000015259, -0.000015259, -0.000015259, -0.000030518,
    -0.000030518, -0.000030518, -0.000030518, -0.000045776,
    -0.000045776, -0.000061035, -0.000061035, -0.000076294,
    -0.000076294, -0.000091553, -0.000106812, -0.000106812,
    -0.000122070, -0.000137329, -0.000152588, -0.000167847,
    -0.000198364, -0.000213623, -0.000244141, -0.000259399,
    -0.000289917, -0.000320435, -0.000366211, -0.000396729,
    -0.000442505, -0.000473022, -0.000534058, -0.000579834,
    -0.000625610, -0.000686646, -0.000747681, -0.000808716,
    -0.000885010, -0.000961304, -0.001037598, -0.001113892,
    -0.001205444, -0.001296997, -0.001388550, -0.001480103,
    -0.001586914, -0.001693726, -0.001785278, -0.001907349,
    -0.002014160, -0.002120972, -0.002243042, -0.002349854,
    -0.002456665, -0.002578735, -0.002685547, -0.002792358,
    -0.002899170, -0.002990723, -0.003082275, -0.003173828,
     0.003250122,  0.003326416,  0.003387451,  0.003433228,
     0.003463745,  0.003479004,  0.003479004,  0.003463745,
     0.003417969,  0.003372192,  0.003280640,  0.003173828,
     0.003051758,  0.002883911,  0.002700806,  0.002487183,
     0.002227783,  0.001937866,  0.001617432,  0.001266479,
     0.000869751,  0.000442505, -0.000030518, -0.000549316,
    -0.001098633, -0.001693726, -0.002334595, -0.003005981,
    -0.003723145, -0.004486084, -0.005294800, -0.006118774,
    -0.007003784, -0.007919312, -0.008865356, -0.009841919,
    -0.010848999, -0.011886597, -0.012939453, -0.014022827,
    -0.015121460, -0.016235352, -0.017349243, -0.018463135,
    -0.019577026, -0.020690918, -0.021789551, -0.022857666,
    -0.023910522, -0.024932861, -0.025909424, -0.026840210,
    -0.027725220, -0.028533936, -0.029281616, -0.029937744,
    -0.030532837, -0.031005859, -0.031387329, -0.031661987,
    -0.031814575, -0.031845093, -0.031738281, -0.031478882,
     0.031082153,  0.030517578,  0.029785156,  0.028884888,
     0.027801514,  0.026535034,  0.025085449,  0.023422241,
     0.021575928,  0.019531250,  0.017257690,  0.014801025,
     0.012115479,  0.009231567,  0.006134033,  0.002822876,
    -0.000686646, -0.004394531, -0.008316040, -0.012420654,
    -0.016708374, -0.021179199, -0.025817871, -0.030609131,
    -0.035552979, -0.040634155, -0.045837402, -0.051132202,
    -0.056533813, -0.061996460, -0.067520142, -0.073059082,
    -0.078628540, -0.084182739, -0.089706421, -0.095169067,
    -0.100540161, -0.105819702, -0.110946655, -0.115921021,
    -0.120697021, -0.125259399, -0.129562378, -0.133590698,
    -0.137298584, -0.140670776, -0.143676758, -0.146255493,
    -0.148422241, -0.150115967, -0.151306152, -0.151962280,
    -0.152069092, -0.151596069, -0.150497437, -0.148773193,
    -0.146362305, -0.143264771, -0.139450073, -0.134887695,
    -0.129577637, -0.123474121, -0.116577148, -0.108856201,
     0.100311279,  0.090927124,  0.080688477,  0.069595337,
     0.057617187,  0.044784546,  0.031082153,  0.016510010,
     0.001068115, -0.015228271, -0.032379150, -0.050354004,
    -0.069168091, -0.088775635, -0.109161377, -0.130310059,
    -0.152206421, -0.174789429, -0.198059082, -0.221984863,
    -0.246505737, -0.271591187, -0.297210693, -0.323318481,
    -0.349868774, -0.376800537, -0.404083252, -0.431655884,
    -0.459472656, -0.487472534, -0.515609741, -0.543823242,
    -0.572036743, -0.600219727, -0.628295898, -0.656219482,
    -0.683914185, -0.711318970, -0.738372803, -0.765029907,
    -0.791213989, -0.816864014, -0.841949463, -0.866363525,
    -0.890090942, -0.913055420, -0.935195923, -0.956481934,
    -0.976852417, -0.996246338, -1.014617920, -1.031936646,
    -1.048156738, -1.063217163, -1.077117920, -1.089782715,
    -1.101211548, -1.111373901, -1.120223999, -1.127746582,
    -1.133926392, -1.138763428, -1.142211914, -1.144287109,
     1.144989014,  1.144287109,  1.142211914,  1.138763428,
     1.133926392,  1.127746582,  1.120223999,  1.111373901,
     1.101211548,  1.089782715,  1.077117920,  1.063217163,
     1.048156738,  1.031936646,  1.014617920,  0.996246338,
     0.976852417,  0.956481934,  0.935195923,  0.913055420,
     0.890090942,  0.866363525,  0.841949463,  0.816864014,
     0.791213989,  0.765029907,  0.738372803,  0.711318970,
     0.683914185,  0.656219482,  0.628295898,  0.600219727,
     0.572036743,  0.543823242,  0.515609741,  0.487472534,
     0.459472656,  0.431655884,  0.404083252,  0.376800537,
     0.349868774,  0.323318481,  0.297210693,  0.271591187,
     0.246505737,  0.221984863,  0.198059082,  0.174789429,
     0.152206421,  0.130310059,  0.109161377,  0.088775635,
     0.069168091,  0.050354004,  0.032379150,  0.015228271,
    -0.001068115, -0.016510010, -0.031082153, -0.044784546,
    -0.057617187, -0.069595337, -0.080688477, -0.090927124,
     0.100311279,  0.108856201,  0.116577148,  0.123474121,
     0.129577637,  0.134887695,  0.139450073,  0.143264771,
     0.146362305,  0.148773193,  0.150497437,  0.151596069,
     0.152069092,  0.151962280,  0.151306152,  0.150115967,
     0.148422241,  0.146255493,  0.143676758,  0.140670776,
     0.137298584,  0.133590698,  0.129562378,  0.125259399,
     0.120697021,  0.115921021,  0.110946655,  0.105819702,
     0.100540161,  0.095169067,  0.089706421,  0.084182739,
     0.078628540,  0.073059082,  0.067520142,  0.061996460,
     0.056533813,  0.051132202,  0.045837402,  0.040634155,
     0.035552979,  0.030609131,  0.025817871,  0.021179199,
     0.016708374,  0.012420654,  0.008316040,  0.004394531,
     0.000686646, -0.002822876, -0.006134033, -0.009231567,
    -0.012115479, -0.014801025, -0.017257690, -0.019531250,
    -0.021575928, -0.023422241, -0.025085449, -0.026535034,
    -0.027801514, -0.028884888, -0.029785156, -0.030517578,
     0.031082153,  0.031478882,  0.031738281,  0.031845093,
     0.031814575,  0.031661987,  0.031387329,  0.031005859,
     0.030532837,  0.029937744,  0.029281616,  0.028533936,
     0.027725220,  0.026840210,  0.025909424,  0.024932861,
     0.023910522,  0.022857666,  0.021789551,  0.020690918,
     0.019577026,  0.018463135,  0.017349243,  0.016235352,
     0.015121460,  0.014022827,  0.012939453,  0.011886597,
     0.010848999,  0.009841919,  0.008865356,  0.007919312,
     0.007003784,  0.006118774,  0.005294800,  0.004486084,
     0.003723145,  0.003005981,  0.002334595,  0.001693726,
     0.001098633,  0.000549316,  0.000030518, -0.000442505,
    -0.000869751, -0.001266479, -0.001617432, -0.001937866,
    -0.002227783, -0.002487183, -0.002700806, -0.002883911,
    -0.003051758, -0.003173828, -0.003280640, -0.003372192,
    -0.003417969, -0.003463745, -0.003479004, -0.003479004,
    -0.003463745, -0.003433228, -0.003387451, -0.003326416,
     0.003250122,  0.003173828,  0.003082275,  0.002990723,
     0.002899170,  0.002792358,  0.002685547,  0.002578735,
     0.002456665,  0.002349854,  0.002243042,  0.002120972,
     0.002014160,  0.001907349,  0.001785278,  0.001693726,
     0.001586914,  0.001480103,  0.001388550,  0.001296997,
     0.001205444,  0.001113892,  0.001037598,  0.000961304,
     0.000885010,  0.000808716,  0.000747681,  0.000686646,
     0.000625610,  0.000579834,  0.000534058,  0.000473022,
     0.000442505,  0.000396729,  0.000366211,  0.000320435,
     0.000289917,  0.000259399,  0.000244141,  0.000213623,
     0.000198364,  0.000167847,  0.000152588,  0.000137329,
     0.000122070,  0.000106812,  0.000106812,  0.000091553,
     0.000076294,  0.000076294,  0.000061035,  0.000061035,
     0.000045776,  0.000045776,  0.000030518,  0.000030518,
     0.000030518,  0.000030518,  0.000015259,  0.000015259,
     0.000015259,  0.000015259,  0.000015259,  0.000015259,
];

/// [q2 patch] The synthesis window, 16-byte aligned for vector loads.
static SYNTHESIS_D: Aligned16<[f32; 512]> = Aligned16(SYNTHESIS_D_UNALIGNED);

/// `SynthesisState` maintains the persistant state of sub-band synthesis.
///
/// [q2 patch] 16-byte aligned, and 32 V vectors instead of upstream's 16:
/// the V vectors of the four time slots of a batch are written before the
/// first of them is windowed, while that window still reads the 15 slots
/// before it.
#[repr(C, align(16))]
pub struct SynthesisState {
    v_vec: [[f32; 64]; 32],
    v_front: usize,
}

impl Default for SynthesisState {
    fn default() -> Self {
        SynthesisState { v_vec: [[0f32; 64]; 32], v_front: 0 }
    }
}

// [q2 patch] From here to the tests the file is rewritten for the Q2
// (README.q2.md). Upstream ran, per time slot, a strided gather of the 32
// sub-band samples, one scalar DCT-32, a scalar build of the V vector and a
// scalar 16-tap window over 32 outputs. This version:
//
// - runs the DCT-32 on four time slots at once: lane t of `x[sb]` is
//   `in_samples[n_frames * sb + b0 + t]`, four adjacent floats, so the input
//   is 32 plain vector loads and each lane is upstream's scalar DCT;
// - transposes the result into one row of 32 per slot, and builds each V
//   vector from its row with forward, negated and reversed four-lane stores;
// - runs the window with four adjacent outputs per vector: every output
//   keeps upstream's 16 products added in upstream's order, starting from 0.
//
// Output bits equal upstream's for every input (tests below; upstream's code
// is kept as the reference in q2ref/synthesis.rs).

#[cfg(target_arch = "mips")]
use crate::q2simd;
use crate::q2simd::{Aligned16, Lanes, S4};

/// Sub-band synthesis transforms 32 sub-band blocks containing 18 time-domain samples each into
/// 18 blocks of 32 PCM audio samples.
pub fn synthesis(state: &mut SynthesisState, n_frames: usize, in_samples: &[f32], out: &mut [f32]) {
    #[cfg(target_arch = "mips")]
    if q2simd::msa() {
        // SAFETY: `msa()` is true only when the kernel reports MSA.
        #[allow(unsafe_code)]
        unsafe {
            synthesis_msa(state, n_frames, in_samples, out)
        };
        return;
    }
    synthesis_lanes::<S4>(state, n_frames, in_samples, out);
}

#[cfg(target_arch = "mips")]
#[target_feature(enable = "msa")]
#[allow(unsafe_code)]
unsafe fn synthesis_msa(
    state: &mut SynthesisState,
    n_frames: usize,
    in_samples: &[f32],
    out: &mut [f32],
) {
    synthesis_lanes::<q2simd::M4>(state, n_frames, in_samples, out);
}

#[inline(always)]
fn synthesis_lanes<V: Lanes>(
    state: &mut SynthesisState,
    n_frames: usize,
    in_samples: &[f32],
    out: &mut [f32],
) {
    assert!(in_samples.len() == 32 * n_frames);
    assert!(out.len() >= 32 * n_frames);

    let zero = V::splat(0.0);
    let mut done = 0;

    while done < n_frames {
        // The batch covers time slots b0..b0 + 4; lanes first..last are new.
        // A final batch of fewer than four slots is moved back to end at
        // n_frames and skips the lanes already done. Fewer than four slots in
        // all (not layer 3) are padded with zeros.
        let (b0, first, last) = if n_frames - done >= 4 {
            (done, 0, 4)
        }
        else if n_frames >= 4 {
            (n_frames - 4, done + 4 - n_frames, 4)
        }
        else {
            (0, 0, n_frames)
        };

        let mut x = [zero; 32];
        if n_frames >= 4 {
            for (i, x) in x.iter_mut().enumerate() {
                *x = V::load(&in_samples[n_frames * i + b0..n_frames * i + b0 + 4]);
            }
        }
        else {
            for (i, x) in x.iter_mut().enumerate() {
                let mut padded = [0f32; 4];
                padded[..n_frames].copy_from_slice(&in_samples[n_frames * i..n_frames * (i + 1)]);
                *x = V::load(&padded);
            }
        }

        #[cfg(q2_mp3_profile_fine)]
        let q2_stage = crate::q2prof::span(crate::q2prof::DCT32);
        let d = dct32(&x);

        // rows[t][g] holds d[4g..4g + 4] of time slot b0 + t.
        let mut rows = [[zero; 8]; 4];
        for g in 0..8 {
            let t = V::transpose([d[4 * g], d[4 * g + 1], d[4 * g + 2], d[4 * g + 3]]);
            rows[0][g] = t[0];
            rows[1][g] = t[1];
            rows[2][g] = t[2];
            rows[3][g] = t[3];
        }

        // The V vectors of the new slots, the first slot's first: a vector
        // loaded right after it was stored waits 12 cycles on the X2000
        // (q2-toolchain docs/x2000-msa.md), and the window of a slot starts with the V
        // vector of that slot.
        for lane in first..last {
            let r = &rows[lane];
            let front = (state.v_front + 32 - (lane - first)) & 0x1f;

            // Build the V vector. Upstream's mapping, with d the DCT output:
            //   v[0] = d[16], v[1..16] = d[17..32], v[16] = 0,
            //   v[16 + k] = -d[32 - k], v[32] = -d[16], v[32 + k] = -d[16 - k],
            //   v[48] = -d[0], v[48 + k] = -d[k]            (k = 1..15)
            // which is: d[16..32] at v[0..16]; -reverse(d[16..32]) at
            // v[17..33]; -reverse(d[0..16]) at v[33..49]; -d[0..16] at
            // v[48..64] (v[48] is written twice with -d[0]).
            {
                let v = &mut state.v_vec[front];
                let n = [-r[0], -r[1], -r[2], -r[3], -r[4], -r[5], -r[6], -r[7]];
                r[4].store(&mut v[0..4]);
                r[5].store(&mut v[4..8]);
                r[6].store(&mut v[8..12]);
                r[7].store(&mut v[12..16]);
                v[16] = 0.0;
                n[7].reverse().store(&mut v[17..21]);
                n[6].reverse().store(&mut v[21..25]);
                n[5].reverse().store(&mut v[25..29]);
                n[4].reverse().store(&mut v[29..33]);
                n[3].reverse().store(&mut v[33..37]);
                n[2].reverse().store(&mut v[37..41]);
                n[1].reverse().store(&mut v[41..45]);
                n[0].reverse().store(&mut v[45..49]);
                n[0].store(&mut v[48..52]);
                n[1].store(&mut v[52..56]);
                n[2].store(&mut v[56..60]);
                n[3].store(&mut v[60..64]);
            }
        }

        #[cfg(q2_mp3_profile_fine)]
        drop(q2_stage);
        for lane in first..last {
            #[cfg(q2_mp3_profile_fine)]
            let _q2_stage = crate::q2prof::span(crate::q2prof::WINDOW);
            let front = state.v_front;

            // Window: eight accumulators of four adjacent outputs. Per tap
            // pair, all eight take the first product, then all eight take
            // the second, so consecutive additions never wait on each other.
            let mut acc = [zero; 8];
            for j in 0..8 {
                let v0: &[f32; 64] = &state.v_vec[(front + (j << 1)) & 0x1f];
                let v1: &[f32; 64] = &state.v_vec[(front + (j << 1) + 1) & 0x1f];
                let k = j << 6;
                for m in 0..8 {
                    let i = 4 * m;
                    acc[m] = acc[m] + V::load(&v0[i..i + 4]) * V::load(&SYNTHESIS_D.0[k + i..k + i + 4]);
                }
                for m in 0..8 {
                    let i = 4 * m;
                    acc[m] = acc[m]
                        + V::load(&v1[32 + i..36 + i]) * V::load(&SYNTHESIS_D.0[k + 32 + i..k + 36 + i]);
                }
            }

            // Clamp output samples and store them in the output buffer.
            let offset = (b0 + lane) << 5;
            let o = &mut out[offset..offset + 32];
            for m in 0..8 {
                acc[m].clamp1().store(&mut o[4 * m..4 * m + 4]);
            }

            // Shift the v_vec deque. The value at the back of the queue is no longer needed.
            state.v_front = (front + 31) & 0x1f;
        }

        done = b0 + last;
    }
}

// The following tables are pre-computed values of the the following equation:
//
// c[i] = 1.0 / [2.0 * cos((PI / N) * (2*i + 1))]    for i = 0..N/2
//
// where N = [32, 16, 8, 4, 2], for COS_16, COS8, COS_4, and COS_2, respectively.
const COS_16: [f32; 16] = [
    0.500_602_998_235_196_3,  // i= 0
    0.505_470_959_897_543_6,  // i= 1
    0.515_447_309_922_624_6,  // i= 2
    0.531_042_591_089_784_1,  // i= 3
    0.553_103_896_034_444_5,  // i= 4
    0.582_934_968_206_133_9,  // i= 5
    0.622_504_123_035_664_8,  // i= 6
    0.674_808_341_455_005_7,  // i= 7
    0.744_536_271_002_298_6,  // i= 8
    0.839_349_645_415_526_8,  // i= 9
    0.972_568_237_861_960_8,  // i=10
    1.169_439_933_432_884_7,  // i=11
    1.484_164_616_314_166_2,  // i=12
    2.057_781_009_953_410_8,  // i=13
    3.407_608_418_468_719_0,  // i=14
    10.190_008_123_548_032_9, // i=15
];

const COS_8: [f32; 8] = [
    0.502_419_286_188_155_7, // i=0
    0.522_498_614_939_688_9, // i=1
    0.566_944_034_816_357_7, // i=2
    0.646_821_783_359_990_1, // i=3
    0.788_154_623_451_250_2, // i=4
    1.060_677_685_990_347_1, // i=5
    1.722_447_098_238_334_2, // i=6
    5.101_148_618_689_155_3, // i=7
];

const COS_4: [f32; 4] = [
    0.509_795_579_104_159_2, // i=0
    0.601_344_886_935_045_3, // i=1
    0.899_976_223_136_415_6, // i=2
    2.562_915_447_741_505_5, // i=3
];

const COS_2: [f32; 2] = [
    0.541_196_100_146_197_0, // i=0
    1.306_562_964_876_376_4, // i=1
];

const COS_1: f32 = 0.707_106_781_186_547_5;

// [q2 patch] Byeong Gi Lee's fast DCT as upstream flattened it into one
// function body, here as the recursion it came from, over four lanes. Each
// function computes upstream's values with upstream's operands; only the
// names of the temporaries differ.

/// 2-point DCT.
#[inline(always)]
fn dct2<V: Lanes>(t: [V; 2]) -> [V; 2] {
    [t[0] + t[1], (t[0] - t[1]) * V::splat(COS_1)]
}

/// 4-point DCT.
#[inline(always)]
fn dct4<V: Lanes>(t: [V; 4]) -> [V; 4] {
    let lo = dct2([t[0] + t[3], t[1] + t[2]]);
    let hi = dct2([(t[0] - t[3]) * V::splat(COS_2[0]), (t[1] - t[2]) * V::splat(COS_2[1])]);
    [lo[0], hi[0] + hi[1], lo[1], hi[1]]
}

/// 8-point DCT.
#[inline(always)]
fn dct8<V: Lanes>(t: [V; 8]) -> [V; 8] {
    let lo = dct4([t[0] + t[7], t[1] + t[6], t[2] + t[5], t[3] + t[4]]);
    let hi = dct4([
        (t[0] - t[7]) * V::splat(COS_4[0]),
        (t[1] - t[6]) * V::splat(COS_4[1]),
        (t[2] - t[5]) * V::splat(COS_4[2]),
        (t[3] - t[4]) * V::splat(COS_4[3]),
    ]);
    [lo[0], hi[0] + hi[1], lo[1], hi[1] + hi[2], lo[2], hi[2] + hi[3], lo[3], hi[3]]
}

/// 16-point DCT.
#[inline(always)]
fn dct16<V: Lanes>(t: [V; 16]) -> [V; 16] {
    let mut sum = [t[0]; 8];
    let mut diff = [t[0]; 8];
    for i in 0..8 {
        sum[i] = t[i] + t[15 - i];
        diff[i] = (t[i] - t[15 - i]) * V::splat(COS_8[i]);
    }
    let lo = dct8(sum);
    let hi = dct8(diff);
    let mut y = [t[0]; 16];
    for i in 0..7 {
        y[2 * i] = lo[i];
        y[2 * i + 1] = hi[i] + hi[i + 1];
    }
    y[14] = lo[7];
    y[15] = hi[7];
    y
}

/// Performs a 32-point Discrete Cosine Transform (DCT) using Byeong Gi Lee's fast algorithm
/// published in article [1] without inverse square-root 2 scaling.
///
/// [1] B.G. Lee, "A new algorithm to compute the discrete cosine transform", IEEE Transactions
/// on Acoustics, Speech, and Signal Processing, vol. 32, no. 6, pp. 1243-1245, 1984.
///
/// https://ieeexplore.ieee.org/document/1164443
#[inline(always)]
fn dct32<V: Lanes>(x: &[V; 32]) -> [V; 32] {
    let mut sum = [x[0]; 16];
    let mut diff = [x[0]; 16];
    for i in 0..16 {
        sum[i] = x[i] + x[31 - i];
        diff[i] = (x[i] - x[31 - i]) * V::splat(COS_16[i]);
    }
    let lo = dct16(sum);
    let hi = dct16(diff);
    let mut y = [x[0]; 32];
    for i in 0..15 {
        y[2 * i] = lo[i];
        y[2 * i + 1] = hi[i] + hi[i + 1];
    }
    y[30] = lo[15];
    y[31] = hi[15];
    y
}

#[cfg(test)]
mod tests {
    use super::{dct32, synthesis, SynthesisState};
    use crate::q2ref_synthesis as upstream;
    use crate::q2simd::S4;
    use std::f64;

    fn dct32_analytical(x: &[f32; 32]) -> [f32; 32] {
        const PI_32: f64 = f64::consts::PI / 32.0;

        let mut result = [0f32; 32];
        for (i, item) in result.iter_mut().enumerate() {
            *item = x
                .iter()
                .enumerate()
                .map(|(j, &jtem)| jtem * (PI_32 * (i as f64) * ((j as f64) + 0.5)).cos() as f32)
                .sum();
        }

        result
    }

    #[test]
    fn verify_dct32() {
        const TEST_VECTOR: [f32; 32] = [
            0.1710, 0.1705, 0.3476, 0.1866, 0.4784, 0.6525, 0.2690, 0.9996, //
            0.1864, 0.7277, 0.1163, 0.6620, 0.0911, 0.3225, 0.1126, 0.5344, //
            0.7839, 0.9741, 0.8757, 0.5763, 0.5926, 0.2756, 0.1757, 0.6531, //
            0.7101, 0.7376, 0.1924, 0.0351, 0.8044, 0.2409, 0.9347, 0.9417, //
        ];

        let x = TEST_VECTOR.map(|v| S4([v, 0.0, -v, 1.0]));
        let test_result = dct32(&x).map(|v| v.0[0]);

        let actual_result = dct32_analytical(&TEST_VECTOR);
        for i in 0..32 {
            assert!((actual_result[i] - test_result[i]).abs() < 0.00001);
        }
    }

    /// [q2 patch] The rewritten synthesis returns upstream's bits: random
    /// sub-band samples (with exact zeros, both signs, and values that
    /// clamp), every batch shape (n_frames 1..=19, 36), state carried across
    /// calls.
    #[test]
    fn q2_synthesis_matches_upstream_bit_for_bit() {
        let mut rng = crate::q2simd::tests::Rng(0x5eed_1234);
        for n_frames in (1..=19).chain([36]) {
            let mut ours = SynthesisState::default();
            let mut theirs = upstream::SynthesisState::default();
            for call in 0..6 {
                let mut input = vec![0f32; 32 * n_frames];
                for v in input.iter_mut() {
                    *v = match (call, rng.next() % 16) {
                        (0, _) | (_, 0) => 0.0,
                        (_, 1) => -0.0,
                        (5, _) => rng.float() * 40.0,
                        _ => rng.float(),
                    };
                }
                let mut a = vec![7f32; 32 * n_frames];
                let mut b = vec![7f32; 32 * n_frames];
                synthesis(&mut ours, n_frames, &input, &mut a);
                upstream::synthesis(&mut theirs, n_frames, &input, &mut b);
                let a: Vec<u32> = a.iter().map(|v| v.to_bits()).collect();
                let b: Vec<u32> = b.iter().map(|v| v.to_bits()).collect();
                assert_eq!(a, b, "n_frames {} call {}", n_frames, call);
            }
        }
    }
}
