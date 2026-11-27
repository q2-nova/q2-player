// [q2 patch] Differential tests of the restructured decoder against
// upstream's code (which stays in decoder.rs): generated frames of every
// subframe type, mutated and truncated frames, and the vector kernels on the
// portable vector type.

use std::panic::{catch_unwind, AssertUnwindSafe};

use symphonia_core::audio::{AudioBufferRef, Signal};
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_FLAC};
use symphonia_core::formats::Packet;

use super::{compare_kernels, lpc_pair_any, output_vector, Mix, MAX_TAPS};
use crate::decoder::FlacDecoder;
use crate::q2simd::{Rng, P4};

fn pair_p4(
    p0: &mut [i32],
    p1: &mut [i32],
    from: usize,
    to: usize,
    taps: &[[i32; MAX_TAPS]; 2],
    shift: [u32; 2],
    order: usize,
) -> usize {
    lpc_pair_any::<P4>(p0, p1, from, to, taps, shift, order)
}

fn output_p4(p0: &mut [i32], p1: &mut [i32], from: usize, to: usize, mix: Mix, wasted: [u32; 2], out_shift: u32) -> usize {
    output_vector::<P4>(p0, p1, from, to, mix, wasted, out_shift)
}

/// The vector kernels (on the portable vectors) against the scalar code.
#[test]
fn vector_kernels_match_scalar() {
    for seed in 1..=6u32 {
        let checks = compare_kernels(Some(pair_p4), Some(output_p4), seed.wrapping_mul(0x9e37_79b9), 500).unwrap();
        assert!(checks > 50_000);
    }
}

/// `q2::selftest` passes (scalar against scalar off the Q2).
#[test]
fn selftest_passes() {
    assert!(crate::q2::selftest().unwrap() > 10_000);
}

// A FLAC frame writer, enough for the decoder's whole syntax.

struct Writer {
    bytes: Vec<u8>,
    acc: u64,
    filled: u32,
}

impl Writer {
    fn new() -> Self {
        Writer { bytes: Vec::new(), acc: 0, filled: 0 }
    }
    fn put(&mut self, value: u32, bits: u32) {
        assert!(bits <= 32);
        if bits == 0 {
            return;
        }
        let value = if bits == 32 { value } else { value & ((1 << bits) - 1) };
        self.acc = (self.acc << bits) | u64::from(value);
        self.filled += bits;
        while self.filled >= 8 {
            self.bytes.push((self.acc >> (self.filled - 8)) as u8);
            self.filled -= 8;
        }
    }
    fn unary(&mut self, zeros: u32) {
        for _ in 0..zeros {
            self.put(0, 1);
        }
        self.put(1, 1);
    }
    fn align(&mut self) {
        if self.filled > 0 {
            self.put(0, 8 - self.filled);
        }
    }
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

fn stream_info(channels: u32, bps: u32, block_max: u16) -> Box<[u8]> {
    let mut w = Writer::new();
    w.put(16, 16);
    w.put(u32::from(block_max.max(16)), 16);
    w.put(0, 24);
    w.put(0, 24);
    w.put(44_100, 20);
    w.put(channels - 1, 3);
    w.put(bps - 1, 5);
    w.put(0, 4);
    w.put(0, 32);
    for _ in 0..4 {
        w.put(0, 32);
    }
    w.bytes.into_boxed_slice()
}

fn decoder(channels: u32, bps: u32, block_max: u16, restructured: bool) -> FlacDecoder {
    let mut params = CodecParameters::new();
    params.for_codec(CODEC_TYPE_FLAC).with_extra_data(stream_info(channels, bps, block_max));
    let mut decoder = FlacDecoder::try_new(&params, &DecoderOptions::default()).unwrap();
    decoder.q2_set_restructured(restructured);
    decoder
}

fn fits(value: i64) -> bool {
    value >= i64::from(i32::MIN) && value <= i64::from(i32::MAX)
}

fn fold(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

/// A test signal of `bits`-bit samples.
fn signal(rng: &mut Rng, n: usize, bits: u32) -> Vec<i32> {
    let (min, max) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
    let clamp = |v: i64| v.clamp(min, max) as i32;
    match rng.below(6) {
        0 => (0..n).map(|_| rng.signed(bits)).collect(),
        1 => (0..n).map(|_| rng.signed(bits.min(4))).collect(),
        2 => {
            let step = 1 + rng.below(bits);
            let mut v = 0i64;
            (0..n)
                .map(|_| {
                    v = (v + i64::from(rng.signed(step))).clamp(min, max);
                    v as i32
                })
                .collect()
        }
        3 => vec![rng.signed(bits); n],
        4 => {
            // A full-scale triangle wave.
            let period = 4 + rng.below(200) as i64;
            (0..n as i64)
                .map(|i| {
                    let phase = i % period;
                    let up = if phase * 2 < period { phase * 2 } else { (period - phase) * 2 };
                    clamp(min + (max - min) * up / period)
                })
                .collect()
        }
        _ => (0..n).map(|i| if (i / (1 + rng.below(7) as usize)) % 2 == 0 { max as i32 } else { min as i32 }).collect(),
    }
}

/// Writes residuals `res[prelude..]` in Rice partitions.
fn write_residual(w: &mut Writer, rng: &mut Rng, res: &[i32], prelude: usize) {
    let n = res.len();
    // Partition orders that divide the block and leave room for the warm-up.
    let mut orders = vec![0u32];
    for order in 1..=15u32 {
        if n % (1 << order) == 0 && (n >> order) >= prelude && (n >> order) > 0 {
            orders.push(order);
        }
    }
    let order = orders[rng.below(orders.len() as u32) as usize];
    let partition = n >> order;
    let widest = res[prelude..].iter().map(|&v| 32 - fold(v).leading_zeros()).max().unwrap_or(0);
    // The 4-bit method reaches parameter 14; keep quotients below about 2^9.
    let method = if widest > 14 + 9 || rng.below(3) == 0 { 1 } else { 0 };
    w.put(method, 2);
    w.put(order, 4);
    let (parameter_bits, escape) = if method == 0 { (4, 15) } else { (5, 31) };
    let mut from = prelude;
    let mut to = partition;
    loop {
        let part = &res[from.min(to)..to];
        let widest = part.iter().map(|&v| 32 - fold(v).leading_zeros()).max().unwrap_or(0);
        let width = part
            .iter()
            .map(|&v| if v == 0 { 0 } else { 33 - (if v < 0 { !v } else { v }).leading_zeros() })
            .max()
            .unwrap_or(0);
        if rng.below(10) == 0 && width <= 31 {
            w.put(escape, parameter_bits);
            let width = if width < 31 && rng.below(2) == 0 { width + 1 } else { width };
            w.put(width, 5);
            for &v in part {
                w.put(v as u32, width);
            }
        }
        else {
            let lowest = widest.saturating_sub(9);
            let k = (lowest + rng.below(6)).min(escape - 1).max(lowest.min(escape - 1));
            assert!(widest.saturating_sub(k) <= 12);
            w.put(k, parameter_bits);
            for &v in part {
                let folded = fold(v);
                let quotient = if k >= 32 { 0 } else { folded >> k };
                w.unary(quotient);
                w.put(folded, k);
            }
        }
        if to >= n {
            break;
        }
        from = to;
        to += partition;
    }
}

/// One subframe coding `bps`-bit samples; returns nothing, the reference
/// decoder defines the expected output.
fn write_subframe(w: &mut Writer, rng: &mut Rng, n: usize, bps: u32) {
    let wasted = if bps > 1 && rng.below(5) == 0 { 1 + rng.below((bps - 1).min(5)) } else { 0 };
    let bits = bps - wasted;
    let samples = signal(rng, n, bits);
    let mut kind = rng.below(10);
    // Residuals and coefficients, when the predictor's numbers fit.
    let mut residual = samples.clone();
    let mut order = 0usize;
    let mut taps = [0i32; 32];
    let (mut precision, mut shift) = (0u32, 0u32);
    match kind {
        0 => {}
        1 => {}
        2..=4 => {
            order = (rng.below(5) as usize).min(n);
            let table: [&[i64]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];
            let weights = table[order];
            for i in order..n {
                let predicted: i64 = weights.iter().enumerate().map(|(j, &c)| c * i64::from(samples[i - 1 - j])).sum();
                let r = i64::from(samples[i]) - predicted;
                if !fits(r) || !fits(predicted) {
                    kind = 1;
                    break;
                }
                residual[i] = r as i32;
            }
        }
        _ => {
            order = match rng.below(4) {
                0 => 1 + rng.below(32),
                1 => 1 + rng.below(4),
                _ => 1 + rng.below(12),
            } as usize;
            order = order.min(n);
            precision = 1 + rng.below(15);
            shift = if rng.below(4) == 0 { rng.below(16) } else { (precision + rng.below(3)).min(15) };
            for tap in taps[..order].iter_mut() {
                *tap = rng.signed(precision);
            }
            if rng.below(3) == 0 {
                // Close to a real predictor: mostly the previous sample.
                taps[0] = ((1i64 << shift) - 1).min((1i64 << (precision - 1)) - 1) as i32;
            }
            for i in order..n {
                let sum: i64 = taps[..order].iter().enumerate().map(|(j, &c)| i64::from(c) * i64::from(samples[i - 1 - j])).sum();
                let predicted = sum >> shift;
                let r = i64::from(samples[i]) - predicted;
                if !fits(r) || !fits(predicted) {
                    kind = 1;
                    break;
                }
                residual[i] = r as i32;
            }
        }
    }
    w.put(0, 1);
    let code = match kind {
        0 => 0x00,
        1 => 0x01,
        2..=4 => 0x08 | order as u32,
        _ => 0x20 | (order as u32 - 1),
    };
    w.put(code, 6);
    if wasted > 0 {
        w.put(1, 1);
        w.unary(wasted - 1);
    }
    else {
        w.put(0, 1);
    }
    match kind {
        0 => w.put(samples[0] as u32, bits),
        1 => {
            for &s in &samples {
                w.put(s as u32, bits);
            }
        }
        _ => {
            for &s in &samples[..order] {
                w.put(s as u32, bits);
            }
            if kind >= 5 {
                w.put(precision - 1, 4);
                w.put(shift, 5);
                for &tap in &taps[..order] {
                    w.put(tap as u32, precision);
                }
            }
            write_residual(w, rng, &residual, order);
        }
    }
}

struct Stream {
    channels: u32,
    bps: u32,
    block_max: u16,
}

/// A frame of `n` samples; returns the bytes and the length of the header.
fn write_frame(rng: &mut Rng, stream: &Stream, n: usize) -> (Vec<u8>, usize) {
    let mut w = Writer::new();
    w.put(0xfff8, 16);
    let block_code = if n <= 256 && rng.below(2) == 0 { 6 } else { 7 };
    w.put(block_code, 4);
    w.put(0x9, 4);
    let assignment = if stream.channels == 2 && stream.bps <= 30 { [1, 8, 9, 10][rng.below(4) as usize] } else { stream.channels - 1 };
    w.put(assignment, 4);
    let bps_code = match stream.bps {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        _ => 0,
    };
    w.put(if rng.below(3) == 0 { 0 } else { bps_code }, 3);
    w.put(0, 1);
    w.put(rng.below(100), 8);
    w.put(n as u32 - 1, if block_code == 6 { 8 } else { 16 });
    let crc = crc8(&w.bytes);
    w.put(u32::from(crc), 8);
    let header = w.bytes.len();
    for channel in 0..stream.channels {
        let side = match assignment {
            8 | 10 => channel == 1,
            9 => channel == 0,
            _ => false,
        };
        write_subframe(&mut w, rng, n, stream.bps + u32::from(side));
    }
    w.align();
    w.put(0, 16);
    (w.bytes, header)
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Samples(Vec<Vec<i32>>),
    Error(String),
}

/// `None`: the decoder panicked (upstream does on arithmetic overflow and
/// some malformed frames when built with debug assertions).
fn decode(decoder: &mut FlacDecoder, bytes: &[u8]) -> Option<Outcome> {
    let packet = Packet::new_from_slice(0, 0, 0, bytes);
    catch_unwind(AssertUnwindSafe(|| match decoder.decode(&packet) {
        Ok(AudioBufferRef::S32(buf)) => {
            Outcome::Samples((0..buf.spec().channels.count()).map(|c| buf.chan(c).to_vec()).collect())
        }
        Ok(_) => unreachable!(),
        Err(error) => Outcome::Error(error.to_string()),
    }))
    .ok()
}

fn random_stream(rng: &mut Rng) -> Stream {
    let channels = match rng.below(6) {
        0 => 1,
        1 => 3 + rng.below(6),
        _ => 2,
    };
    let bps = match rng.below(8) {
        0 => 8,
        1 => 12,
        2 | 3 => 16,
        4 => 20,
        5 | 6 => 24,
        _ => 4 + rng.below(29),
    };
    Stream { channels, bps, block_max: 16 + rng.below(1200) as u16 }
}

fn random_block(rng: &mut Rng, stream: &Stream) -> usize {
    match rng.below(4) {
        0 => usize::from(stream.block_max),
        1 => (16usize << rng.below(6)).min(usize::from(stream.block_max)),
        _ => 1 + rng.below(u32::from(stream.block_max)) as usize,
    }
}

/// Generated valid frames: the restructured decoder accepts every one and
/// returns upstream's samples.
#[test]
fn valid_frames_match_upstream() {
    let mut rng = Rng(0x0f1a_c0de);
    let mut frames = 0;
    for _ in 0..400 {
        let stream = random_stream(&mut rng);
        let mut ours = decoder(stream.channels, stream.bps, stream.block_max, true);
        let mut theirs = decoder(stream.channels, stream.bps, stream.block_max, false);
        // Several frames through one decoder pair: state carried between packets.
        for _ in 0..4 {
            let n = random_block(&mut rng, &stream);
            let (bytes, _) = write_frame(&mut rng, &stream, n);
            let expected = decode(&mut theirs, &bytes).expect("upstream panicked on a valid frame");
            assert!(matches!(expected, Outcome::Samples(_)), "upstream rejected a valid frame: {:?}", expected);
            let accepted = ours.q2_accepted();
            let got = decode(&mut ours, &bytes).expect("restructured decoder panicked");
            assert!(got == expected, "channels {} bps {} frames {}", stream.channels, stream.bps, n);
            assert_eq!(ours.q2_accepted(), accepted + 1, "frame went to upstream's code");
            frames += 1;
        }
    }
    assert_eq!(frames, 1600);
}

/// Mutated and truncated frames: the same samples or the same error.
#[test]
fn damaged_frames_match_upstream() {
    let mut rng = Rng(0xdead_10cc);
    let (mut compared, mut errors, mut panics) = (0, 0, 0);
    for round in 0..6000 {
        let stream = random_stream(&mut rng);
        let n = random_block(&mut rng, &stream);
        let (mut bytes, header) = write_frame(&mut rng, &stream, n);
        match round % 4 {
            0 => {
                let keep = header + rng.below((bytes.len() - header) as u32 + 1) as usize;
                bytes.truncate(keep);
            }
            1 => {
                for _ in 0..1 + rng.below(4) {
                    let at = header + rng.below((bytes.len() - header) as u32) as usize;
                    bytes[at] ^= 1 << rng.below(8);
                }
            }
            2 => {
                // Damage near the start: subframe headers and coefficients.
                let span = (bytes.len() - header).min(12) as u32;
                let at = header + rng.below(span) as usize;
                bytes[at] = rng.next() as u8;
            }
            _ => {
                for byte in bytes[header..].iter_mut() {
                    *byte = rng.next() as u8;
                }
            }
        }
        let mut theirs = decoder(stream.channels, stream.bps, stream.block_max, false);
        let Some(expected) = decode(&mut theirs, &bytes)
        else {
            panics += 1;
            continue;
        };
        let mut ours = decoder(stream.channels, stream.bps, stream.block_max, true);
        let got = decode(&mut ours, &bytes).expect("restructured decoder panicked where upstream did not");
        if got != expected {
            let describe = |o: &Outcome| match o {
                Outcome::Error(e) => e.clone(),
                Outcome::Samples(s) => format!("{} channels of {} samples", s.len(), s[0].len()),
            };
            if let (Outcome::Samples(a), Outcome::Samples(b)) = (&got, &expected) {
                for (c, (a, b)) in a.iter().zip(b).enumerate() {
                    if let Some(i) = (0..a.len()).find(|&i| a[i] != b[i]) {
                        eprintln!("channel {} first difference at {}: {:#x} vs {:#x}", c, i, a[i], b[i]);
                    }
                }
            }
            panic!(
                "round {}: channels {} bps {} frames {}: ours {}, upstream {}",
                round, stream.channels, stream.bps, n, describe(&got), describe(&expected)
            );
        }
        compared += 1;
        errors += u32::from(matches!(expected, Outcome::Error(_)));
    }
    // Both outcomes are exercised.
    assert!(compared > 3000, "{} compared, {} upstream panics", compared, panics);
    assert!(errors > 500 && compared - errors > 500, "{} errors of {}", errors, compared);
}
