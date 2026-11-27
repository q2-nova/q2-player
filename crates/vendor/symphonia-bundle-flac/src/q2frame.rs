// [q2 patch] Subframe decoding restructured for SIMD (README.q2.md).
//
// Upstream decodes one channel at a time: Rice codes through a 64-bit bit
// cache, a 64-bit scalar predictor, then one pass over the planes per
// decorrelation, wasted-bits shift and output shift.
//
// Here a frame's channels are decoded two at a time, in their output planes:
//   1. both subframes' warm-up samples and residuals are read with a 32-bit
//      bit window (q2bits.rs; `rice_window` for the Rice codes);
//   2. prediction runs over both channels at once: a vector's two 64-bit
//      lanes are the two channels (`lpc_pair`), or per channel in scalar code;
//   3. one pass applies the wasted-bits shifts, the inter-channel
//      decorrelation and the output shift (`output_range`).
// Steps 2 and 3 alternate over runs of `CHUNK` frames, so the output pass
// works on samples that are still in the 32 KiB L1 cache.
//
// `decode_frame` returns `None` for anything it does not handle (every error,
// reserved values, a frame whose channel count is not the stream's); the
// caller then decodes the frame with upstream's code, which stays in
// decoder.rs. So this file only has to be exact for the frames it accepts.
// All sample arithmetic wraps, as upstream's does when compiled without
// overflow checks (the release profile).

use std::convert::TryInto;

use symphonia_core::audio::{AudioBuffer, Signal};

use crate::frame::ChannelAssignment;
use crate::q2bits::Bits;
use crate::q2simd::V4;

/// Longest predictor.
pub(crate) const MAX_TAPS: usize = 32;
/// Frames predicted between output passes: a multiple of every kernel length
/// and of 4; two planes of it are 3.75 KiB.
const CHUNK: usize = 480;

/// A second plane for the last of an odd number of channels. The only
/// allocation; `decode_frame` makes none.
pub(crate) struct Work {
    spare: Vec<i32>,
}

impl Work {
    /// For blocks of up to `frames` frames.
    pub(crate) fn new(frames: usize, channels: usize) -> Work {
        Work { spare: vec![0i32; if channels % 2 == 1 { frames } else { 0 }] }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Predictor {
    /// Constant and verbatim subframes: the plane holds the samples.
    None,
    Fixed(usize),
    /// Order; `taps` and `shift` describe it.
    Lpc(usize),
}

#[derive(Clone, Copy)]
struct Sub {
    predictor: Predictor,
    /// `taps[j]` multiplies the sample `j + 1` frames back; zero past the order.
    taps: [i32; MAX_TAPS],
    shift: u32,
    wasted: u32,
}

impl Sub {
    fn plain() -> Sub {
        Sub { predictor: Predictor::None, taps: [0; MAX_TAPS], shift: 0, wasted: 0 }
    }
    fn lpc_order(&self) -> usize {
        match self.predictor {
            Predictor::Lpc(order) => order,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Mix {
    Independent,
    LeftSide,
    RightSide,
    MidSide,
}

/// Decodes the subframes in `data` (a frame's bytes after its header) into
/// `buf`, whose frame count the caller has set to `n`. `None`: decode the
/// frame with upstream's code instead; `buf`'s samples are then unspecified.
pub(crate) fn decode_frame(
    data: &[u8],
    assignment: &ChannelAssignment,
    bps: u32,
    n: usize,
    buf: &mut AudioBuffer<i32>,
    work: &mut Work,
    msa: bool,
) -> Option<()> {
    let (count, mix) = match assignment {
        ChannelAssignment::Independant(count) => (*count as usize, Mix::Independent),
        ChannelAssignment::LeftSide => (2, Mix::LeftSide),
        ChannelAssignment::RightSide => (2, Mix::RightSide),
        ChannelAssignment::MidSide => (2, Mix::MidSide),
    };
    // Upstream's output shift runs over every plane of the buffer, so a frame
    // with fewer channels than the stream is left to it. A difference channel
    // has one more bit than the frame.
    let side = u32::from(mix != Mix::Independent);
    if count != buf.spec().channels.count() || n == 0 || n != buf.frames() || bps == 0 || bps + side > 32 {
        return None;
    }
    let out_shift = if bps < 32 { 32 - bps } else { 0 };
    let kernels = (pair_kernel(msa), output_kernel(msa));
    let mut bs = Bits::new(data);
    let mut channel = 0;
    while channel < count {
        if count - channel >= 2 {
            let (bps0, bps1) = match mix {
                Mix::Independent => (bps, bps),
                Mix::LeftSide | Mix::MidSide => (bps, bps + 1),
                Mix::RightSide => (bps + 1, bps),
            };
            let (p0, p1) = buf.chan_pair_mut(channel, channel + 1);
            let subs = [read_subframe(&mut bs, bps0, p0)?, read_subframe(&mut bs, bps1, p1)?];
            finish_pair(p0, p1, &subs, mix, out_shift, kernels);
            channel += 2;
        }
        else {
            // The last of an odd number of channels, with the spare plane as
            // an unused second lane.
            let p0 = buf.chan_mut(channel);
            let p1 = work.spare.get_mut(..n)?;
            let subs = [read_subframe(&mut bs, bps, p0)?, Sub::plain()];
            finish_pair(p0, p1, &subs, Mix::Independent, out_shift, kernels);
            channel += 1;
        }
    }
    Some(())
}

fn read_subframe(bs: &mut Bits<'_>, frame_bps: u32, plane: &mut [i32]) -> Option<Sub> {
    let n = plane.len();
    // The padding bit must be 0.
    if bs.bit() {
        return None;
    }
    let kind = bs.bits(6);
    let wasted = if bs.bit() { bs.unary_zeros()?.checked_add(1)? } else { 0 };
    if wasted >= frame_bps {
        return None;
    }
    let bps = frame_bps - wasted;
    let mut sub = Sub::plain();
    sub.wasted = wasted;
    match kind {
        0x00 => {
            let value = bs.signed(bps);
            for sample in plane.iter_mut() {
                *sample = value;
            }
        }
        0x01 => read_verbatim(bs, bps, plane),
        0x08..=0x0c => {
            let order = (kind & 0x07) as usize;
            if order > n {
                return None;
            }
            read_verbatim(bs, bps, &mut plane[..order]);
            read_residual(bs, order, plane)?;
            sub.predictor = Predictor::Fixed(order);
        }
        0x20..=0x3f => {
            let order = (kind & 0x1f) as usize + 1;
            if order > n {
                return None;
            }
            read_verbatim(bs, bps, &mut plane[..order]);
            let precision = bs.bits(4) + 1;
            if precision > 15 {
                return None;
            }
            let shift = bs.signed(5);
            if shift < 0 {
                return None;
            }
            for tap in sub.taps[..order].iter_mut() {
                *tap = bs.signed(precision);
            }
            read_residual(bs, order, plane)?;
            sub.predictor = Predictor::Lpc(order);
            sub.shift = shift as u32;
        }
        _ => return None,
    }
    if bs.overrun() {
        return None;
    }
    Some(sub)
}

fn read_verbatim(bs: &mut Bits<'_>, bps: u32, out: &mut [i32]) {
    for sample in out.iter_mut() {
        *sample = bs.signed(bps);
    }
}

fn read_residual(bs: &mut Bits<'_>, prelude: usize, plane: &mut [i32]) -> Option<()> {
    q2_span!(RESIDUAL);
    let n = plane.len();
    let parameter_bits = match bs.bits(2) {
        0 => 4,
        1 => 5,
        _ => return None,
    };
    let partition_order = bs.bits(4);
    let partition = n >> partition_order;
    if prelude > partition || partition << partition_order != n {
        return None;
    }
    // The first partition is short by the warm-up samples.
    let (mut from, mut to) = (prelude, partition);
    loop {
        read_partition(bs, parameter_bits, &mut plane[from..to])?;
        if to >= n {
            return Some(());
        }
        from = to;
        to += partition;
    }
}

/// Largest Rice parameter of `rice_window`.
const RICE_WINDOW_MAX: u32 = 20;

fn read_partition(bs: &mut Bits<'_>, parameter_bits: u32, out: &mut [i32]) -> Option<()> {
    let parameter = bs.bits(parameter_bits);
    if parameter < (1 << parameter_bits) - 1 {
        if parameter <= RICE_WINDOW_MAX {
            rice_window(bs, parameter, out)?;
        }
        else {
            for sample in out.iter_mut() {
                *sample = rice_one(bs, parameter)?;
            }
        }
    }
    else {
        // Escape: residuals as plain two's complement numbers.
        let width = bs.bits(5);
        for sample in out.iter_mut() {
            *sample = if width == 0 { 0 } else { bs.signed(width) };
        }
    }
    if bs.overrun() {
        return None;
    }
    Some(())
}

/// Folded residual to signed: 0, -1, 1, -2, 2, ...
#[inline(always)]
fn unfold(word: u32) -> i32 {
    ((word >> 1) as i32) ^ -((word & 1) as i32)
}

/// One Rice code with any parameter (up to 30): quotient and remainder read separately.
#[inline]
fn rice_one(bs: &mut Bits<'_>, k: u32) -> Option<i32> {
    let q = bs.unary_zeros()?;
    let r = bs.bits(k);
    Some(unfold((q << k) | r))
}

/// The big-endian word at byte `4 * j`.
#[inline(always)]
fn word(buf: &[u8], j: usize) -> Option<u32> {
    let b = buf.get(4 * j..4 * j + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Rice codes with parameter `k <= RICE_WINDOW_MAX`.
///
/// The stream is taken as big-endian 32-bit words. Two consecutive words
/// stay in registers; a code's 32-bit window is two shifts and an OR of
/// them, so the path from one code to the next is shift, OR,
/// count-leading-zeros and two adds, with no load on it. The next word is
/// loaded every iteration and moved in with conditional moves when the
/// position crosses a word, so there is no data-dependent branch either: the
/// one branch per code tests that the quotient's one bit and the remainder
/// are inside the window (`q + 1 + k <= 32`), which fails only for
/// quotients above `31 - k`.
#[inline(always)]
fn rice_window(bs: &mut Bits<'_>, k: u32, out: &mut [i32]) -> Option<()> {
    let buf = bs.buf;
    let n = out.len();
    let mut done = 0;
    while done < n {
        let mut j = bs.pos >> 5;
        let mut offset = (bs.pos & 31) as u32;
        if let (Some(mut w0), Some(mut w1)) = (word(buf, j), word(buf, j + 1)) {
            for sample in out[done..].iter_mut() {
                let next = match word(buf, j + 2) {
                    Some(next) => next,
                    None => break,
                };
                let w = (w0 << offset) | ((w1 >> 1) >> (31 - offset));
                if w >> k == 0 {
                    break;
                }
                let q = w.leading_zeros();
                // The k bits after the one bit (two shifts: q + 1 may be 32).
                let r = (((w << q) << 1) >> 1) >> (31 - k);
                *sample = unfold((q << k) | r);
                done += 1;
                offset += q + 1 + k;
                let crossed = offset >> 5 != 0;
                offset &= 31;
                j += usize::from(crossed);
                w0 = if crossed { w1 } else { w0 };
                w1 = if crossed { next } else { w1 };
            }
            bs.pos = 32 * j + offset as usize;
        }
        // A long quotient, or the last 12 bytes of the buffer.
        if done < n {
            out[done] = rice_one(bs, k)?;
            done += 1;
        }
    }
    Some(())
}

/// A vector predictor over two planes: frames `from..to`, taps, shifts and the
/// larger order; returns the first frame it did not predict.
pub(crate) type PairKernel = fn(&mut [i32], &mut [i32], usize, usize, &[[i32; MAX_TAPS]; 2], [u32; 2], usize) -> usize;
/// A vector output pass over frames `from..to`; returns the first frame it did not write.
pub(crate) type OutputKernel = fn(&mut [i32], &mut [i32], usize, usize, Mix, [u32; 2], u32) -> usize;

/// The MSA predictor when `msa` is set (and this is a MIPS build).
#[inline]
fn pair_kernel(msa: bool) -> Option<PairKernel> {
    #[cfg(target_arch = "mips")]
    if msa {
        return Some(msa_entry::lpc_pair_checked);
    }
    let _ = msa;
    None
}

#[inline]
fn output_kernel(msa: bool) -> Option<OutputKernel> {
    #[cfg(target_arch = "mips")]
    if msa {
        return Some(msa_entry::output_checked);
    }
    let _ = msa;
    None
}

/// Prediction and output of two planes holding warm-up samples and residuals.
fn finish_pair(
    p0: &mut [i32],
    p1: &mut [i32],
    subs: &[Sub; 2],
    mix: Mix,
    out_shift: u32,
    kernels: (Option<PairKernel>, Option<OutputKernel>),
) {
    let n = p0.len();
    let wasted = [subs[0].wasted, subs[1].wasted];
    {
        q2_span!(PREDICT);
        if let Predictor::Fixed(order) = subs[0].predictor {
            fixed_predict(p0, order);
        }
        if let Predictor::Fixed(order) = subs[1].predictor {
            fixed_predict(p1, order);
        }
    }
    let orders = [subs[0].lpc_order(), subs[1].lpc_order()];
    let order = orders[0].max(orders[1]);
    // Frames below `predicted` hold samples; frames below `written` hold output.
    let mut predicted = order;
    let mut written = 0;
    if order > 0 {
        {
            q2_span!(PREDICT);
            // The lane with the lower order up to the larger order.
            lpc_predict(p0, &subs[0].taps, orders[0], subs[0].shift, orders[0], order);
            lpc_predict(p1, &subs[1].taps, orders[1], subs[1].shift, orders[1], order);
        }
        if let Some(kernel) = kernels.0 {
            // A lane without linear prediction has zero taps and shift 0: the
            // kernel leaves it as it is.
            let taps = [subs[0].taps, subs[1].taps];
            let shift = [subs[0].shift, subs[1].shift];
            loop {
                let reached = {
                    q2_span!(PREDICT);
                    kernel(p0, p1, predicted, n.min(predicted + CHUNK), &taps, shift, order)
                };
                if reached == predicted {
                    break;
                }
                predicted = reached;
                // The next kernel call and the scalar tail read up to
                // MAX_TAPS samples back: the output pass stays behind them.
                let ready = predicted.saturating_sub(MAX_TAPS) & !3;
                if ready > written {
                    q2_span!(OUTPUT);
                    written = output_range(p0, p1, written, ready, mix, wasted, out_shift, kernels.1);
                }
            }
        }
        q2_span!(PREDICT);
        lpc_predict(p0, &subs[0].taps, orders[0], subs[0].shift, predicted, n);
        lpc_predict(p1, &subs[1].taps, orders[1], subs[1].shift, predicted, n);
    }
    q2_span!(OUTPUT);
    output_range(p0, p1, written, n, mix, wasted, out_shift, kernels.1);
}

/// The fixed predictors, order 0 to 4. Their coefficients are integers and
/// there is no shift, so the sums are exact in wrapping 32-bit arithmetic
/// (upstream sums in 64 bits and keeps the low 32).
fn fixed_predict(plane: &mut [i32], order: usize) {
    match order {
        1 => fixed_loop(plane, order, |r, a, _, _, _| r.wrapping_add(a)),
        2 => fixed_loop(plane, order, |r, a, b, _, _| r.wrapping_add(a.wrapping_mul(2)).wrapping_sub(b)),
        3 => fixed_loop(plane, order, |r, a, b, c, _| r.wrapping_add(a.wrapping_sub(b).wrapping_mul(3)).wrapping_add(c)),
        4 => fixed_loop(plane, order, |r, a, b, c, d| {
            r.wrapping_add(a.wrapping_add(c).wrapping_mul(4)).wrapping_sub(b.wrapping_mul(6)).wrapping_sub(d)
        }),
        _ => {}
    }
}

/// `sample = f(residual, a, b, c, d)` for frames `order..`; a, b, c, d are
/// the samples 1, 2, 3 and 4 frames back, kept in registers.
#[inline(always)]
fn fixed_loop(plane: &mut [i32], order: usize, f: impl Fn(i32, i32, i32, i32, i32) -> i32) {
    if order >= plane.len() {
        return;
    }
    let back = |frames: usize| if frames <= order { plane[order - frames] } else { 0 };
    let (mut a, mut b, mut c, mut d) = (back(1), back(2), back(3), back(4));
    for sample in plane[order..].iter_mut() {
        let s = f(*sample, a, b, c, d);
        *sample = s;
        d = c;
        c = b;
        b = a;
        a = s;
    }
}

/// Scalar linear prediction of frames `from..to` of one plane (`order <= from`):
/// `sample += (sum of taps[j] * sample[j + 1 frames back]) >> shift`, the sum
/// in 64 bits as upstream's. Nothing for order 0.
fn lpc_predict(plane: &mut [i32], taps: &[i32; MAX_TAPS], order: usize, shift: u32, from: usize, to: usize) {
    if from >= to || order == 0 {
        return;
    }
    // As upstream: a few loop lengths, each fully unrolled, the taps past the
    // order being zero.
    match order {
        0..=4 => lpc_run::<4>(plane, taps, order, shift, from, to),
        5..=6 => lpc_run::<6>(plane, taps, order, shift, from, to),
        7..=8 => lpc_run::<8>(plane, taps, order, shift, from, to),
        9..=10 => lpc_run::<10>(plane, taps, order, shift, from, to),
        11..=12 => lpc_run::<12>(plane, taps, order, shift, from, to),
        _ => lpc_run::<32>(plane, taps, order, shift, from, to),
    }
}

#[inline(always)]
fn lpc_run<const N: usize>(plane: &mut [i32], taps: &[i32; MAX_TAPS], order: usize, shift: u32, from: usize, to: usize) {
    // Frames with fewer than N frames of history: a loop over the order.
    let unrolled = from.max(N).min(to);
    for frame in from..unrolled {
        let mut sum = 0i64;
        for (j, &tap) in taps[..order].iter().enumerate() {
            sum = sum.wrapping_add(i64::from(tap) * i64::from(plane[frame - 1 - j]));
        }
        plane[frame] = plane[frame].wrapping_add((sum >> shift) as i32);
    }
    let taps: &[i32; N] = taps[..N].try_into().unwrap();
    for frame in unrolled..to {
        // history[N - 1 - j] is the sample j + 1 frames back; history[N] this frame's.
        let history = &mut plane[frame - N..=frame];
        let mut sum = 0i64;
        for j in 0..N {
            sum = sum.wrapping_add(i64::from(taps[j]) * i64::from(history[N - 1 - j]));
        }
        history[N] = history[N].wrapping_add((sum >> shift) as i32);
    }
}

/// `lpc_pair` with the shortest kernel that covers `order`.
#[inline(always)]
#[allow(dead_code)]
fn lpc_pair_any<V: V4>(
    p0: &mut [i32],
    p1: &mut [i32],
    from: usize,
    to: usize,
    taps: &[[i32; MAX_TAPS]; 2],
    shift: [u32; 2],
    order: usize,
) -> usize {
    match order {
        0..=4 => lpc_pair::<V, 4>(p0, p1, from, to, taps, shift),
        5..=8 => lpc_pair::<V, 8>(p0, p1, from, to, taps, shift),
        9..=12 => lpc_pair::<V, 12>(p0, p1, from, to, taps, shift),
        _ => lpc_pair::<V, 32>(p0, p1, from, to, taps, shift),
    }
}

/// Linear prediction of both planes at once, frames `from..` in whole groups
/// of N; returns the first frame not predicted (at most `to`).
///
/// Transposed form. `s[j]` holds, for each plane in one 64-bit vector lane,
/// the part of the prediction of frame `i + j` that is known before frame
/// `i`: the taps `j..N` times the samples up to frame `i - 1`, plus that
/// frame's residual shifted left by the lane's shift. A step is then
///     y    = s[0] >> shift                     (the new samples, both planes)
///     s[j] = s[j + 1] + taps[j] * y            (j < N - 1)
///     s[N - 1] = (residual of frame i + N << shift) + taps[N - 1] * y
/// which is N independent multiply-accumulates of one new vector: no
/// multiply waits for another. The loop-carried path is one `dpadd_s.d` and
/// one `sra.d`. Per lane this is the sum upstream computes, in 64 bits:
/// adding `residual << shift` before the arithmetic shift equals adding the
/// residual after it, and the low 32 bits are the wrapping sum upstream
/// stores. Sums cannot overflow 64 bits: |tap| <= 2^14 and |sample| <= 2^31,
/// 32 of them.
///
/// Needs: N a multiple of 4, taps past N zero, each lane's order `<= from`.
/// Reads residuals up to N frames past the group it predicts, so it stops
/// 2 N frames before the end of the planes.
#[inline(always)]
#[allow(dead_code)]
fn lpc_pair<V: V4, const N: usize>(
    p0: &mut [i32],
    p1: &mut [i32],
    from: usize,
    to: usize,
    taps: &[[i32; MAX_TAPS]; 2],
    shift: [u32; 2],
) -> usize {
    let n = p0.len().min(p1.len());
    let to = to.min(n.saturating_sub(N));
    if from + N > to {
        return from;
    }
    let c: [V; N] = std::array::from_fn(|j| V::new([taps[0][j], 0, taps[1][j], 0]));
    // Residuals arrive as [plane 0 frame f, plane 0 frame f + 1, plane 1
    // frame f, plane 1 frame f + 1]: one multiplier vector per frame parity.
    let (one0, one1) = (1 << shift[0], 1 << shift[1]);
    let scale_even = V::new([one0, 0, one1, 0]);
    let scale_odd = V::new([0, one0, 0, one1]);
    let shift_by = V::new([shift[0] as i32, 0, shift[1] as i32, 0]);
    let mut s: [V; N] = std::array::from_fn(|j| {
        let known = |plane: &[i32], taps: &[i32; MAX_TAPS], shift: u32| {
            let mut sum = i64::from(plane[from + j]) << shift;
            for m in j..N {
                // Tap m multiplies frame from + j - 1 - m; a non-zero tap is
                // below the order, which is at most `from`.
                if taps[m] != 0 {
                    sum = sum.wrapping_add(i64::from(taps[m]) * i64::from(plane[from + j - 1 - m]));
                }
            }
            sum
        };
        let (a, b) = (known(p0, &taps[0], shift[0]), known(p1, &taps[1], shift[1]));
        V::new([a as i32, (a >> 32) as i32, b as i32, (b >> 32) as i32])
    });
    let mut i = from;
    while i + N <= to {
        // N frames to write, then N frames of residuals to read.
        let (b0, b1) = (&mut p0[i..i + 2 * N], &mut p1[i..i + 2 * N]);
        for t in (0..N).step_by(4) {
            let (r0, r1) = (V::load(&b0[t + N..]), V::load(&b1[t + N..]));
            let (ra, rb) = (V::low_halves(r0, r1), V::high_halves(r0, r1));
            let y0 = lpc_step(&mut s, &c, shift_by, ra, scale_even);
            let y1 = lpc_step(&mut s, &c, shift_by, ra, scale_odd);
            let y2 = lpc_step(&mut s, &c, shift_by, rb, scale_even);
            let y3 = lpc_step(&mut s, &c, shift_by, rb, scale_odd);
            // [plane 0, plane 1] of two frames each, then the planes apart.
            let (ya, yb) = (V::evens(y0, y1), V::evens(y2, y3));
            V::evens(ya, yb).store(&mut b0[t..]);
            V::odds(ya, yb).store(&mut b1[t..]);
        }
        i += N;
    }
    i
}

#[inline(always)]
#[allow(dead_code)]
fn lpc_step<V: V4, const N: usize>(s: &mut [V; N], c: &[V; N], shift_by: V, residuals: V, scale: V) -> V {
    let y = s[0].sra64(shift_by);
    for j in 0..N - 1 {
        s[j] = s[j + 1].dot_add(y, c[j]);
    }
    s[N - 1] = V::dot(residuals, scale).dot_add(y, c[N - 1]);
    y
}

/// Frames `from..to` of two planes of samples to output, in place: wasted-bits
/// shift, inter-channel decorrelation, output shift. Returns `to`.
#[allow(clippy::too_many_arguments)]
fn output_range(
    p0: &mut [i32],
    p1: &mut [i32],
    from: usize,
    to: usize,
    mix: Mix,
    wasted: [u32; 2],
    out_shift: u32,
    kernel: Option<OutputKernel>,
) -> usize {
    let from = match kernel {
        Some(kernel) => kernel(p0, p1, from, to, mix, wasted, out_shift),
        None => from,
    };
    let (p0, p1) = (&mut p0[from..to], &mut p1[from..to]);
    match mix {
        Mix::Independent => output_scalar(p0, p1, wasted, out_shift, |a, b| (a, b)),
        Mix::LeftSide => output_scalar(p0, p1, wasted, out_shift, |a, b| (a, a.wrapping_sub(b))),
        Mix::RightSide => output_scalar(p0, p1, wasted, out_shift, |a, b| (a.wrapping_add(b), b)),
        Mix::MidSide => output_scalar(p0, p1, wasted, out_shift, |a, b| {
            // Upstream's decorrelate_mid_side: mid doubled, with the side's low bit.
            let mid = (a << 1) | (b & 1);
            (mid.wrapping_add(b) >> 1, mid.wrapping_sub(b) >> 1)
        }),
    }
    to
}

#[inline(always)]
fn output_scalar(p0: &mut [i32], p1: &mut [i32], wasted: [u32; 2], out_shift: u32, mix: impl Fn(i32, i32) -> (i32, i32)) {
    for (l, r) in p0.iter_mut().zip(p1.iter_mut()) {
        let (a, b) = mix(l.wrapping_shl(wasted[0]), r.wrapping_shl(wasted[1]));
        *l = a.wrapping_shl(out_shift);
        *r = b.wrapping_shl(out_shift);
    }
}

/// `output_range` four frames at a time; returns the first frame not written.
#[inline(always)]
#[allow(dead_code)]
fn output_vector<V: V4>(
    p0: &mut [i32],
    p1: &mut [i32],
    from: usize,
    to: usize,
    mix: Mix,
    wasted: [u32; 2],
    out_shift: u32,
) -> usize {
    let whole = from + ((to - from) & !3);
    let (p0, p1) = (&mut p0[from..whole], &mut p1[from..whole]);
    match mix {
        Mix::Independent => output_quads::<V>(p0, p1, wasted, out_shift, |a, b| (a, b)),
        Mix::LeftSide => output_quads::<V>(p0, p1, wasted, out_shift, |a, b| (a, a.sub(b))),
        Mix::RightSide => output_quads::<V>(p0, p1, wasted, out_shift, |a, b| (a.add(b), b)),
        Mix::MidSide => {
            let one = V::new([1; 4]);
            output_quads::<V>(p0, p1, wasted, out_shift, move |a, b| {
                let mid = a.shl(one).or(b.and(one));
                (mid.add(b).sra1(), mid.sub(b).sra1())
            })
        }
    }
    whole
}

#[inline(always)]
#[allow(dead_code)]
fn output_quads<V: V4>(p0: &mut [i32], p1: &mut [i32], wasted: [u32; 2], out_shift: u32, mix: impl Fn(V, V) -> (V, V)) {
    let wasted0 = V::new([wasted[0] as i32; 4]);
    let wasted1 = V::new([wasted[1] as i32; 4]);
    let out = V::new([out_shift as i32; 4]);
    for (l, r) in p0.chunks_exact_mut(4).zip(p1.chunks_exact_mut(4)) {
        let (a, b) = mix(V::load(l).shl(wasted0), V::load(r).shl(wasted1));
        a.shl(out).store(l);
        b.shl(out).store(r);
    }
}

#[cfg(target_arch = "mips")]
#[allow(unsafe_code)]
mod msa_entry {
    //! The MSA instances of the vector kernels: one call per run of frames
    //! (at most `CHUNK`), the vector operations inlined into these functions.
    use super::{Mix, MAX_TAPS};
    use crate::q2simd::{msa, M4};

    #[target_feature(enable = "msa")]
    unsafe fn lpc_pair(
        p0: &mut [i32],
        p1: &mut [i32],
        from: usize,
        to: usize,
        taps: &[[i32; MAX_TAPS]; 2],
        shift: [u32; 2],
        order: usize,
    ) -> usize {
        super::lpc_pair_any::<M4>(p0, p1, from, to, taps, shift, order)
    }

    /// Predicts nothing (returns `from`) when the kernel does not report MSA.
    pub(super) fn lpc_pair_checked(
        p0: &mut [i32],
        p1: &mut [i32],
        from: usize,
        to: usize,
        taps: &[[i32; MAX_TAPS]; 2],
        shift: [u32; 2],
        order: usize,
    ) -> usize {
        if !msa() {
            return from;
        }
        // SAFETY: the kernel reports MSA.
        unsafe { lpc_pair(p0, p1, from, to, taps, shift, order) }
    }

    #[target_feature(enable = "msa")]
    unsafe fn output(
        p0: &mut [i32],
        p1: &mut [i32],
        from: usize,
        to: usize,
        mix: Mix,
        wasted: [u32; 2],
        out_shift: u32,
    ) -> usize {
        super::output_vector::<M4>(p0, p1, from, to, mix, wasted, out_shift)
    }

    /// Writes nothing (returns `from`) when the kernel does not report MSA.
    pub(super) fn output_checked(
        p0: &mut [i32],
        p1: &mut [i32],
        from: usize,
        to: usize,
        mix: Mix,
        wasted: [u32; 2],
        out_shift: u32,
    ) -> usize {
        if !msa() {
            return from;
        }
        // SAFETY: the kernel reports MSA.
        unsafe { output(p0, p1, from, to, mix, wasted, out_shift) }
    }
}

/// `q2::selftest`: the scalar code against the current kernels on generated input.
pub(crate) fn selftest() -> Result<u32, String> {
    let msa = crate::q2simd::msa();
    compare_kernels(pair_kernel(msa), output_kernel(msa), 0x51ed_270b, 600)
}

/// Random subframe pairs through `finish_pair`, scalar against the given kernels.
pub(crate) fn compare_kernels(
    pair: Option<PairKernel>,
    output: Option<OutputKernel>,
    seed: u32,
    rounds: u32,
) -> Result<u32, String> {
    use crate::q2simd::Rng;
    let mut rng = Rng(seed);
    let mut checks = 0;
    let random_sub = |rng: &mut Rng, n: usize| {
        let mut sub = Sub::plain();
        sub.wasted = if rng.below(4) == 0 { rng.below(8) } else { 0 };
        match rng.below(8) {
            0 => {}
            1 => sub.predictor = Predictor::Fixed((rng.below(5) as usize).min(n)),
            _ => {
                let order = match rng.below(4) {
                    0 => 1 + rng.below(32),
                    1 => 1 + rng.below(4),
                    _ => 1 + rng.below(12),
                } as usize;
                let order = order.min(n);
                let precision = 1 + rng.below(15);
                for tap in sub.taps[..order].iter_mut() {
                    *tap = rng.signed(precision);
                }
                sub.shift = rng.below(16);
                sub.predictor = Predictor::Lpc(order);
            }
        }
        sub
    };
    for round in 0..rounds {
        let n = 1 + rng.below(if round % 8 == 0 { 1500 } else { 90 }) as usize;
        let bits = 1 + rng.below(32);
        let a: Vec<i32> = (0..n).map(|_| rng.signed(bits)).collect();
        let b: Vec<i32> = (0..n).map(|_| rng.signed(bits)).collect();
        let subs = [random_sub(&mut rng, n), random_sub(&mut rng, n)];
        let mix = [Mix::Independent, Mix::LeftSide, Mix::RightSide, Mix::MidSide][rng.below(4) as usize];
        let out_shift = rng.below(32);
        let (mut a0, mut b0, mut a1, mut b1) = (a.clone(), b.clone(), a, b);
        finish_pair(&mut a0, &mut b0, &subs, mix, out_shift, (None, None));
        // Each kernel alone, then both.
        let kernels = [(pair, None), (None, output), (pair, output)][round as usize % 3];
        finish_pair(&mut a1, &mut b1, &subs, mix, out_shift, kernels);
        if a0 != a1 || b0 != b1 {
            return Err(format!(
                "round {}: {} frames, {:?} shift {} and {:?} shift {}, {:?}, wasted {} {}, output shift {}",
                round, n, subs[0].predictor, subs[0].shift, subs[1].predictor, subs[1].shift, mix, subs[0].wasted,
                subs[1].wasted, out_shift
            ));
        }
        checks += 2 * n as u32;
    }
    Ok(checks)
}

#[cfg(test)]
#[path = "q2tests.rs"]
mod tests;
