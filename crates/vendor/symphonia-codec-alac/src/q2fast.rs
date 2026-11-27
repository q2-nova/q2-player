// [q2 patch] Packet decoding restructured (README.q2.md).
//
// Upstream reads every code through symphonia-core's 64-bit bit cache, runs
// the adaptive predictor through iterator chains over a slice of
// coefficients, and makes one pass over the output per step (mixing, tail
// bits, output shift).
//
// Here:
//   1. the adaptive Rice codes are read with the 32-bit-window reader
//      (q2bits.rs): one window holds a code's prefix and suffix;
//   2. the predictor is instantiated per order with its coefficients and the
//      history differences in registers;
//   3. one pass per element applies the mixing, the tail bits and the output
//      shift, four frames per vector with MSA.
//
// `decode_packet` returns `None` for anything it does not handle (every
// error, elements other than SCE, LFE, CPE and END, Rice parameters above
// 16, short elements); the caller then decodes the packet with upstream's
// code, which stays in lib.rs. So this file only has to be exact for the
// packets it accepts. All sample arithmetic wraps, as upstream's does when
// compiled without overflow checks (the release profile).

use symphonia_core::audio::{AudioBuffer, Signal};

use crate::q2bits::{window, Bits};
use crate::q2simd::V4;
use crate::{MagicCookie, ALAC_ELEM_TAG_CPE, ALAC_ELEM_TAG_END, ALAC_ELEM_TAG_LFE, ALAC_ELEM_TAG_SCE};

/// The tail bits of an element, one plane per channel. Allocated once per decoder.
pub(crate) struct Work {
    tail: [Vec<i32>; 2],
}

impl Work {
    pub(crate) fn new(frames: usize, channels: usize) -> Work {
        Work { tail: [vec![0; frames], vec![0; if channels > 1 { frames } else { 0 }]] }
    }
}

struct Channel {
    mode: u32,
    shift: u32,
    pb_factor: u32,
    order: usize,
    coeffs: [i32; 32],
}

impl Channel {
    fn read(bs: &mut Bits<'_>, config: &MagicCookie) -> Channel {
        let mode = bs.bits(4);
        let shift = bs.bits(4);
        let pb_factor = (bs.bits(3) * u32::from(config.pb)) >> 2;
        let order = bs.bits(5) as usize;
        let mut coeffs = [0; 32];
        for coeff in &mut coeffs[..order] {
            *coeff = bs.signed(16);
        }
        Channel { mode, shift, pb_factor, order, coeffs }
    }
}

/// Decodes `packet` into `buf` as upstream's `decode_inner` does. `None`:
/// decode the packet with upstream's code instead; `buf` is then unspecified.
pub(crate) fn decode_packet(
    packet: &[u8],
    config: &MagicCookie,
    buf: &mut AudioBuffer<i32>,
    work: &mut Work,
    msa: bool,
) -> Option<()> {
    if config.bit_depth == 0 || config.bit_depth > 32 {
        return None;
    }
    let out_shift = 32 - u32::from(config.bit_depth);
    let channel_map = config.channel_layout.channel_map();
    let num_channels = config.num_channels as usize;
    let mut bs = Bits::new(packet);
    let mut next_channel = 0;
    let mut num_frames = 0;

    // Channels and frames that no element writes stay silent.
    buf.clear();
    {
        q2_span!(SILENCE);
        buf.render_silence(None);
    }

    loop {
        match bs.bits(3) {
            ALAC_ELEM_TAG_SCE | ALAC_ELEM_TAG_LFE => {
                let out0 = buf.chan_mut(channel_map[next_channel] as usize);
                num_frames = decode_element(config, &mut bs, work, out0, None, out_shift, msa)?;
                next_channel += 1;
            }
            ALAC_ELEM_TAG_CPE => {
                if next_channel + 2 > num_channels {
                    break;
                }
                let (out0, out1) = buf
                    .chan_pair_mut(channel_map[next_channel] as usize, channel_map[next_channel + 1] as usize);
                num_frames = decode_element(config, &mut bs, work, out0, Some(out1), out_shift, msa)?;
                next_channel += 2;
            }
            ALAC_ELEM_TAG_END => break,
            _ => return None,
        }
        if bs.overrun() {
            return None;
        }
        if next_channel >= num_channels {
            break;
        }
    }
    if bs.overrun() {
        return None;
    }
    buf.truncate(num_frames);
    Some(())
}

/// One SCE or CPE; returns its number of frames. The planes hold the final
/// samples (output shift applied) in `..frames` and are untouched after.
fn decode_element(
    config: &MagicCookie,
    bs: &mut Bits<'_>,
    work: &mut Work,
    out0: &mut [i32],
    mut out1: Option<&mut [i32]>,
    out_shift: u32,
    msa: bool,
) -> Option<usize> {
    let is_cpe = out1.is_some();
    let _instance = bs.bits(4);
    if bs.bits(12) != 0 {
        return None;
    }
    let is_partial = bs.bit();
    let shift = 8 * bs.bits(2);
    let is_uncompressed = bs.bit();
    let bit_depth = u32::from(config.bit_depth);
    if shift >= 24 || shift >= bit_depth {
        return None;
    }
    let n = if is_partial { bs.bits(32) } else { config.frame_length } as usize;
    if n > out0.len() || n > work.tail[0].len() {
        return None;
    }
    let out0 = &mut out0[..n];

    if is_uncompressed {
        q2_span!(OUTPUT);
        match out1.as_mut() {
            Some(out1) => {
                for (s0, s1) in out0.iter_mut().zip(out1[..n].iter_mut()) {
                    *s0 = bs.signed(bit_depth).wrapping_shl(out_shift);
                    *s1 = bs.signed(bit_depth).wrapping_shl(out_shift);
                }
            }
            None => {
                for s0 in out0.iter_mut() {
                    *s0 = bs.signed(bit_depth).wrapping_shl(out_shift);
                }
            }
        }
        return if bs.overrun() { None } else { Some(n) };
    }

    let pred_bits = bit_depth - shift + u32::from(is_cpe);
    if pred_bits > 32 {
        return None;
    }
    let mix_shift = bs.bits(8);
    let mix_weight = bs.signed(8);
    if !is_cpe && (mix_shift != 0 || mix_weight != 0) {
        return None;
    }
    // Upstream shifts an i32 by this.
    if mix_shift > 31 {
        return None;
    }
    let chan0 = Channel::read(bs, config);
    let chan1 = if is_cpe { Some(Channel::read(bs, config)) } else { None };

    if shift > 0 {
        // The tail bits are interleaved in the stream; kept as two planes so
        // the output pass loads them as vectors.
        let [tail0, tail1] = &mut work.tail;
        // A local reader: its position stays in a register.
        let mut tails = Bits { buf: bs.buf, pos: bs.pos };
        if is_cpe {
            for (t0, t1) in tail0[..n].iter_mut().zip(tail1[..n].iter_mut()) {
                *t0 = tails.bits(shift) as i32;
                *t1 = tails.bits(shift) as i32;
            }
        }
        else {
            for t0 in tail0[..n].iter_mut() {
                *t0 = tails.bits(shift) as i32;
            }
        }
        bs.pos = tails.pos;
    }
    if bs.overrun() {
        return None;
    }

    let kb = u32::from(config.kb);
    let mb = u32::from(config.mb);
    read_residuals(bs, &chan0, kb, mb, pred_bits, out0)?;
    predict(&chan0, pred_bits, out0)?;
    match (out1.as_mut(), chan1) {
        (Some(out1), Some(chan1)) => {
            let out1 = &mut out1[..n];
            read_residuals(bs, &chan1, kb, mb, pred_bits, out1)?;
            predict(&chan1, pred_bits, out1)?;
            q2_span!(OUTPUT);
            output_pair(out0, out1, mix_weight, mix_shift, &work.tail, shift, out_shift, msa);
        }
        _ => {
            q2_span!(OUTPUT);
            if shift > 0 {
                for (s0, &t0) in out0.iter_mut().zip(work.tail[0].iter()) {
                    *s0 = ((*s0 << shift) | t0).wrapping_shl(out_shift);
                }
            }
            else if out_shift > 0 {
                for s0 in out0.iter_mut() {
                    *s0 = s0.wrapping_shl(out_shift);
                }
            }
        }
    }
    Some(n)
}

/// Largest Rice parameter read here: nine prefix bits and the suffix must fit
/// in the 25 stream bits a window always has.
const RICE_MAX: u32 = 16;

/// Upstream's `read_rice_code` on one window; `k <= RICE_MAX`. The position
/// is the caller's local, so it stays in a register.
#[inline(always)]
fn rice(buf: &[u8], pos: &mut usize, k: u32, escape_bits: u32) -> u32 {
    let w = window(buf, *pos);
    let prefix = (!w).leading_zeros();
    if prefix > 8 {
        let mut escape = Bits { buf, pos: *pos + 9 };
        let value = escape.bits(escape_bits);
        *pos = escape.pos;
        return value;
    }
    if k > 1 {
        let value = (prefix << k) - prefix;
        // The k bits after the prefix's zero bit. When all but the last are
        // zero the code is k - 1 bits long and adds nothing. No branch: the
        // two cases alternate unpredictably.
        let suffix = (w << (prefix + 1)) >> (32 - k);
        let long = u32::from(suffix > 1);
        *pos += (prefix + k + long) as usize;
        value + (suffix.wrapping_sub(1) & long.wrapping_neg())
    }
    else {
        *pos += prefix as usize + 1;
        if k == 1 {
            prefix
        }
        else {
            0
        }
    }
}

#[inline(always)]
fn lg3a(val: u32) -> u32 {
    31 - ((val >> 9) + 3).leading_zeros()
}

/// The residuals of one channel into `out`, which holds zeros (zero runs are
/// skipped, as upstream skips them).
fn read_residuals(bs: &mut Bits<'_>, chan: &Channel, kb: u32, start_mb: u32, pred_bits: u32, out: &mut [i32]) -> Option<()> {
    q2_span!(RESIDUAL);
    let n = out.len();
    let pb_factor = chan.pb_factor;
    let buf = bs.buf;
    let end = buf.len().saturating_mul(8);
    let mut pos = bs.pos;
    let mut mb = start_mb;
    let mut sign_toggle = 0;
    let mut i = 0;
    while i < n {
        let k = lg3a(mb).min(kb);
        if k > RICE_MAX {
            return None;
        }
        let val = rice(buf, &mut pos, k, pred_bits).wrapping_add(sign_toggle);
        out[i] = ((val >> 1) as i32) ^ -((val & 1) as i32);
        if val > 0xffff {
            mb = 0xffff;
        }
        else {
            mb = mb.wrapping_sub(pb_factor.wrapping_mul(mb) >> 9);
            mb = mb.wrapping_add(pb_factor.wrapping_mul(val));
        }
        sign_toggle = 0;
        i += 1;
        if mb < 128 && i < n {
            // A run of zeros.
            let k = mb.leading_zeros() - 24 + ((mb + 16) >> 6);
            let zeros = rice(buf, &mut pos, k.min(kb), 16);
            if zeros < 0xffff {
                sign_toggle = 1;
            }
            mb = 0;
            i = i.saturating_add(zeros as usize);
        }
        if pos > end {
            return None;
        }
    }
    bs.pos = pos;
    Some(())
}

#[inline(always)]
fn clip_msbs(val: i32, num: u32) -> i32 {
    (val << num) >> num
}

/// Upstream's `ElementChannel::predict`.
fn predict(chan: &Channel, pred_bits: u32, out: &mut [i32]) -> Option<()> {
    q2_span!(PREDICT);
    if chan.mode > 0 && chan.mode < 15 {
        return None;
    }
    let order = chan.order;
    if order == 0 {
        return Some(());
    }
    // Upstream indexes `out[1..1 + order]`.
    if out.len() <= order {
        return None;
    }
    let clip = 32 - pred_bits;
    if order == 31 || chan.mode == 15 {
        let mut prev = out[0];
        for sample in out[1..].iter_mut() {
            prev = clip_msbs(sample.wrapping_add(prev), clip);
            *sample = prev;
        }
    }
    let mut prev = out[0];
    for sample in out[1..1 + order].iter_mut() {
        prev = clip_msbs(sample.wrapping_add(prev), clip);
        *sample = prev;
    }
    // The orders encoders use get their own fully unrolled loop.
    match order {
        4 => predict_taps(out, &chan.coeffs, 4, chan.shift, clip),
        5 => predict_taps(out, &chan.coeffs, 5, chan.shift, clip),
        6 => predict_taps(out, &chan.coeffs, 6, chan.shift, clip),
        8 => predict_taps(out, &chan.coeffs, 8, chan.shift, clip),
        _ => predict_taps(out, &chan.coeffs, order, chan.shift, clip),
    }
    Some(())
}

/// The adaptive FIR predictor over `out[order + 1..]`. Inlined with a
/// constant `order` the tap loops unroll and `c` and `d` live in registers.
///
/// Per sample, as upstream: the prediction is the sum of coefficient times
/// (sample - past0) over the `order` previous samples, past0 being the
/// sample before those; then, when the residual is not zero, each
/// coefficient moves one step toward the sign of its difference, oldest
/// sample first, until the residual's remainder changes sign.
#[inline(always)]
fn predict_taps(out: &mut [i32], coeffs: &[i32; 32], order: usize, shift: u32, clip: u32) {
    // c[j] multiplies the sample order - j frames back (upstream walks its
    // coefficients in reverse).
    let mut c = [0i32; 32];
    for j in 0..order {
        c[j] = coeffs[order - 1 - j];
    }
    let round = (1i32 << shift) >> 1;
    for i in order + 1..out.len() {
        let w = &mut out[i - order - 1..=i];
        let past0 = w[0];
        let mut d = [0i32; 32];
        let mut sum = 0i32;
        for j in 0..order {
            d[j] = w[1 + j].wrapping_sub(past0);
            sum = sum.wrapping_add(c[j].wrapping_mul(d[j]));
        }
        let mut res = w[order + 1];
        let val = sum.wrapping_add(round) >> shift;
        w[order + 1] = clip_msbs(res.wrapping_add(past0).wrapping_add(val), clip);
        // Upstream's `val` is past0 - sample = -d and `sign` its signum, so
        // `sign * val` is |d|. For a positive residual the coefficient moves
        // by signum(d) and the remainder falls by (1 + j) * (|d| >> shift)
        // until it is not positive; for a negative one the coefficient moves
        // by -signum(d) and the remainder falls by (1 + j) * (-|d| >> shift)
        // until it is not negative. One loop for both: `s` is 0 or -1 and
        // `(x ^ s) - s` is x or -x. The residual's sign is unpredictable, so
        // it must not be a branch.
        if res != 0 {
            let s = res >> 31;
            for j in 0..order {
                let sign = i32::from(d[j] > 0) - i32::from(d[j] < 0);
                c[j] = c[j].wrapping_add((sign ^ s).wrapping_sub(s));
                let magnitude = d[j].wrapping_neg().wrapping_abs();
                let step = (magnitude ^ s).wrapping_sub(s) >> shift;
                res = res.wrapping_sub((1 + j as i32).wrapping_mul(step));
                if (s == 0 && res <= 0) || (s != 0 && res >= 0) {
                    break;
                }
            }
        }
    }
}

/// A vector output pass; returns the frames it wrote.
type OutputKernel = fn(&mut [i32], &mut [i32], i32, u32, &[Vec<i32>; 2], u32, u32) -> usize;

#[inline]
fn output_kernel(msa: bool) -> Option<OutputKernel> {
    #[cfg(target_arch = "mips")]
    if msa {
        return Some(msa_entry::output_checked);
    }
    let _ = msa;
    None
}

/// A channel pair's mixing, tail bits and output shift, in place.
fn output_pair(
    out0: &mut [i32],
    out1: &mut [i32],
    mix_weight: i32,
    mix_shift: u32,
    tail: &[Vec<i32>; 2],
    shift: u32,
    out_shift: u32,
    msa: bool,
) {
    output_pair_with(out0, out1, mix_weight, mix_shift, tail, shift, out_shift, output_kernel(msa))
}

fn output_pair_with(
    out0: &mut [i32],
    out1: &mut [i32],
    mix_weight: i32,
    mix_shift: u32,
    tail: &[Vec<i32>; 2],
    shift: u32,
    out_shift: u32,
    kernel: Option<OutputKernel>,
) {
    let done = match kernel {
        Some(kernel) => kernel(out0, out1, mix_weight, mix_shift, tail, shift, out_shift),
        None => 0,
    };
    for i in done..out0.len() {
        let (mut a, mut b) = (out0[i], out1[i]);
        if mix_weight != 0 {
            // Upstream's decorrelate_mid_side.
            a = a.wrapping_add(b).wrapping_sub(b.wrapping_mul(mix_weight) >> mix_shift);
            b = a.wrapping_sub(b);
        }
        if shift > 0 {
            a = (a << shift) | tail[0][i];
            b = (b << shift) | tail[1][i];
        }
        out0[i] = a.wrapping_shl(out_shift);
        out1[i] = b.wrapping_shl(out_shift);
    }
}

/// `output_pair` four frames at a time; returns the frames written.
#[inline(always)]
#[allow(dead_code)]
fn output_vector<V: V4>(
    out0: &mut [i32],
    out1: &mut [i32],
    mix_weight: i32,
    mix_shift: u32,
    tail: &[Vec<i32>; 2],
    shift: u32,
    out_shift: u32,
) -> usize {
    let weight = V::splat(mix_weight);
    let mix_by = V::splat(mix_shift as i32);
    let tail_by = V::splat(shift as i32);
    let out_by = V::splat(out_shift as i32);
    let frames = out0.len() & !3;
    let chunks = out0.chunks_exact_mut(4).zip(out1.chunks_exact_mut(4));
    if shift > 0 {
        let tails = tail[0][..frames].chunks_exact(4).zip(tail[1][..frames].chunks_exact(4));
        for ((s0, s1), (t0, t1)) in chunks.zip(tails) {
            let (mut a, mut b) = (V::load(s0), V::load(s1));
            if mix_weight != 0 {
                a = a.add(b).sub(b.mul(weight).sra(mix_by));
                b = a.sub(b);
            }
            a.shl(tail_by).or(V::load(t0)).shl(out_by).store(s0);
            b.shl(tail_by).or(V::load(t1)).shl(out_by).store(s1);
        }
    }
    else {
        for (s0, s1) in chunks {
            let (mut a, mut b) = (V::load(s0), V::load(s1));
            if mix_weight != 0 {
                a = a.add(b).sub(b.mul(weight).sra(mix_by));
                b = a.sub(b);
            }
            a.shl(out_by).store(s0);
            b.shl(out_by).store(s1);
        }
    }
    frames
}

#[cfg(target_arch = "mips")]
#[allow(unsafe_code)]
mod msa_entry {
    //! The MSA instance of the output kernel: one call per element, the
    //! vector operations inlined into it.
    use crate::q2simd::{msa, M4};

    #[target_feature(enable = "msa")]
    unsafe fn output(
        out0: &mut [i32],
        out1: &mut [i32],
        mix_weight: i32,
        mix_shift: u32,
        tail: &[Vec<i32>; 2],
        shift: u32,
        out_shift: u32,
    ) -> usize {
        super::output_vector::<M4>(out0, out1, mix_weight, mix_shift, tail, shift, out_shift)
    }

    /// Writes nothing when the kernel does not report MSA.
    pub(super) fn output_checked(
        out0: &mut [i32],
        out1: &mut [i32],
        mix_weight: i32,
        mix_shift: u32,
        tail: &[Vec<i32>; 2],
        shift: u32,
        out_shift: u32,
    ) -> usize {
        if !msa() {
            return 0;
        }
        // SAFETY: the kernel reports MSA.
        unsafe { output(out0, out1, mix_weight, mix_shift, tail, shift, out_shift) }
    }
}

/// `q2::selftest`: the scalar output pass against the current kernel on generated input.
pub(crate) fn selftest() -> Result<u32, String> {
    compare_output(output_kernel(crate::q2simd::msa()), 0x2f6e_2b1, 400)
}

pub(crate) fn compare_output(kernel: Option<OutputKernel>, seed: u32, rounds: u32) -> Result<u32, String> {
    use crate::q2simd::Rng;
    let mut rng = Rng(seed);
    let mut checks = 0;
    for round in 0..rounds {
        let n = rng.below(70) as usize;
        let bits = 1 + rng.below(32);
        let plane = |rng: &mut Rng| (0..n).map(|_| rng.signed(bits)).collect::<Vec<i32>>();
        let (a, b) = (plane(&mut rng), plane(&mut rng));
        let shift = [0, 0, 8, 16][rng.below(4) as usize];
        let tail = [
            (0..n).map(|_| (rng.next() & ((1 << shift) - 1)) as i32).collect::<Vec<i32>>(),
            (0..n).map(|_| (rng.next() & ((1 << shift) - 1)) as i32).collect::<Vec<i32>>(),
        ];
        let mix_weight = if rng.below(3) == 0 { 0 } else { rng.signed(8) };
        let mix_shift = rng.below(32);
        let out_shift = rng.below(32);
        let (mut a0, mut b0, mut a1, mut b1) = (a.clone(), b.clone(), a, b);
        output_pair_with(&mut a0, &mut b0, mix_weight, mix_shift, &tail, shift, out_shift, None);
        output_pair_with(&mut a1, &mut b1, mix_weight, mix_shift, &tail, shift, out_shift, kernel);
        if a0 != a1 || b0 != b1 {
            return Err(format!(
                "output differs: round {}, {} frames, weight {}, mix shift {}, tail {}, shift {}",
                round, n, mix_weight, mix_shift, shift, out_shift
            ));
        }
        checks += 2 * n as u32;
    }
    Ok(checks)
}

#[cfg(test)]
#[path = "q2tests.rs"]
mod tests;
