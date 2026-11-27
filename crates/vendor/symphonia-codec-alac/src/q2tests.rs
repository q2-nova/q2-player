// [q2 patch] Differential tests of the restructured decoder against
// upstream's code (which stays in lib.rs). ALAC has few error conditions, so
// generated element headers followed by random bits of several densities
// decode as valid packets and exercise every code length, zero runs, escapes,
// predictor order and mixing mode.

use std::panic::{catch_unwind, AssertUnwindSafe};

use symphonia_core::audio::{AudioBufferRef, Signal};
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_ALAC};
use symphonia_core::formats::Packet;

use super::{compare_output, output_vector};
use crate::q2simd::{Rng, P4};
use crate::AlacDecoder;

fn output_p4(
    out0: &mut [i32],
    out1: &mut [i32],
    mix_weight: i32,
    mix_shift: u32,
    tail: &[Vec<i32>; 2],
    shift: u32,
    out_shift: u32,
) -> usize {
    output_vector::<P4>(out0, out1, mix_weight, mix_shift, tail, shift, out_shift)
}

#[test]
fn vector_output_matches_scalar() {
    for seed in 1..=5u32 {
        assert!(compare_output(Some(output_p4), seed.wrapping_mul(0x9e37_79b9), 600).unwrap() > 10_000);
    }
}

#[test]
fn selftest_passes() {
    assert!(crate::q2::selftest().unwrap() > 5_000);
}

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
}

struct Stream {
    frame_length: u32,
    bit_depth: u8,
    pb: u8,
    mb: u8,
    kb: u8,
    channels: u8,
}

fn decoder(stream: &Stream, restructured: bool) -> AlacDecoder {
    let mut cookie = Vec::new();
    cookie.extend_from_slice(&stream.frame_length.to_be_bytes());
    cookie.extend_from_slice(&[0, stream.bit_depth, stream.pb, stream.mb, stream.kb, stream.channels]);
    cookie.extend_from_slice(&255u16.to_be_bytes());
    cookie.extend_from_slice(&0u32.to_be_bytes());
    cookie.extend_from_slice(&0u32.to_be_bytes());
    cookie.extend_from_slice(&44_100u32.to_be_bytes());
    let mut params = CodecParameters::new();
    params.for_codec(CODEC_TYPE_ALAC).with_extra_data(cookie.into_boxed_slice());
    let mut decoder = AlacDecoder::try_new(&params, &DecoderOptions::default()).unwrap();
    decoder.q2_restructured = restructured;
    decoder
}

fn random_stream(rng: &mut Rng) -> Stream {
    let longest = if rng.below(4) == 0 { 700 } else { 90 };
    Stream {
        frame_length: 1 + rng.below(longest),
        bit_depth: match rng.below(6) {
            0 => 1 + rng.below(32) as u8,
            1 => 24,
            2 => 20,
            3 => 32,
            _ => 16,
        },
        pb: if rng.below(4) == 0 { rng.next() as u8 } else { 40 },
        mb: if rng.below(4) == 0 { rng.next() as u8 } else { 10 },
        kb: if rng.below(4) == 0 { rng.below(40) as u8 } else { 14 },
        channels: match rng.below(5) {
            0 => 1,
            1 => 1 + rng.below(8) as u8,
            _ => 2,
        },
    }
}

fn write_channel(w: &mut Writer, rng: &mut Rng) {
    // Mode: 0, 15 (an extra first-order pass), rarely an invalid one.
    w.put([0, 0, 0, 0, 0, 15, 15, 3][rng.below(8) as usize], 4);
    w.put(if rng.below(3) == 0 { rng.below(16) } else { 9 }, 4);
    w.put(if rng.below(3) == 0 { rng.below(8) } else { 4 }, 3);
    let order = match rng.below(8) {
        0 => rng.below(32),
        1 => 0,
        2 => 31,
        3 => 8,
        4 => 5,
        5 => 6,
        _ => 4,
    };
    w.put(order, 5);
    let scale = 1 + rng.below(16);
    for _ in 0..order {
        w.put(rng.signed(scale) as u32, 16);
    }
}

/// A packet for the stream; `density` selects how the entropy-coded part is filled.
fn write_packet(rng: &mut Rng, stream: &Stream, density: u32) -> Vec<u8> {
    let mut w = Writer::new();
    let mut channel = 0;
    while channel < stream.channels {
        let cpe = stream.channels - channel >= 2 && rng.below(5) != 0;
        match rng.below(40) {
            0 => w.put(7, 3),
            1 => w.put(rng.below(8), 3),
            _ => w.put(if cpe { 1 } else if rng.below(4) == 0 { 3 } else { 0 }, 3),
        }
        w.put(rng.below(16), 4);
        w.put(if rng.below(60) == 0 { 1 } else { 0 }, 12);
        let partial = rng.below(6) == 0;
        w.put(u32::from(partial), 1);
        let depth = u32::from(stream.bit_depth);
        let shift = match rng.below(4) {
            0 if depth > 8 => 1,
            1 if depth > 16 => 2,
            2 if rng.below(30) == 0 => 3,
            _ => 0,
        };
        w.put(shift, 2);
        let uncompressed = rng.below(12) == 0;
        w.put(u32::from(uncompressed), 1);
        let frames = if partial {
            let frames = if rng.below(20) == 0 { rng.next() } else { rng.below(stream.frame_length + 1) };
            w.put(frames, 32);
            frames.min(stream.frame_length)
        }
        else {
            stream.frame_length
        };
        if !uncompressed {
            if cpe {
                w.put(if rng.below(20) == 0 { rng.below(256) } else { rng.below(4) }, 8);
                w.put(if rng.below(3) == 0 { 0 } else { rng.next() }, 8);
            }
            else {
                w.put(if rng.below(30) == 0 { 1 } else { 0 }, 16);
            }
            write_channel(&mut w, rng);
            if cpe {
                write_channel(&mut w, rng);
            }
        }
        // Tail bits, residuals or raw samples: random bits. Sometimes too few.
        let per_sample = 4 + rng.below(depth + 8);
        let mut bits = frames * if cpe { 2 } else { 1 } * (per_sample + 8 * shift);
        if rng.below(10) == 0 {
            bits = rng.below(bits + 1);
        }
        for _ in 0..bits / 8 + 1 {
            let byte = match density {
                0 => rng.next(),
                1 => rng.next() & rng.next(),
                2 => rng.next() & rng.next() & rng.next() & rng.next(),
                3 => rng.next() | rng.next(),
                _ => {
                    if rng.below(8) == 0 {
                        rng.next()
                    }
                    else {
                        0
                    }
                }
            };
            w.put(byte, 8);
        }
        channel += if cpe { 2 } else { 1 };
    }
    w.put(7, 3);
    w.put(0, 7);
    w.bytes
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Samples(Vec<Vec<i32>>),
    Error(String),
}

/// `None`: the decoder panicked (upstream does on arithmetic overflow and on
/// some malformed packets when built with debug assertions).
fn decode(decoder: &mut AlacDecoder, bytes: &[u8]) -> Option<Outcome> {
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

/// Generated packets: the same samples or the same error as upstream, and
/// most packets decoded by the restructured code.
#[test]
fn packets_match_upstream() {
    let mut rng = Rng(0xa1ac_0001);
    let (mut compared, mut errors, mut accepted, mut panics) = (0u32, 0u32, 0u32, 0u32);
    for round in 0..12_000u32 {
        let stream = random_stream(&mut rng);
        let bytes = write_packet(&mut rng, &stream, round % 5);
        let mut theirs = decoder(&stream, false);
        let expected = match decode(&mut theirs, &bytes) {
            Some(expected) => expected,
            None => {
                panics += 1;
                continue;
            }
        };
        let mut ours = decoder(&stream, true);
        let got = decode(&mut ours, &bytes).expect("restructured decoder panicked where upstream did not");
        if got != expected {
            if let (Outcome::Samples(a), Outcome::Samples(b)) = (&got, &expected) {
                for (c, (a, b)) in a.iter().zip(b).enumerate() {
                    if let Some(i) = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]) {
                        eprintln!("channel {} of {} samples: first difference at {}: {:#x} vs {:#x}", c, a.len(), i, a[i], b[i]);
                    }
                }
            }
            let describe = |o: &Outcome| match o {
                Outcome::Error(e) => e.clone(),
                Outcome::Samples(s) => format!("{} channels of {} samples", s.len(), s[0].len()),
            };
            panic!(
                "round {}: {} channels, depth {}, frame {}, pb {} mb {} kb {}: ours {}, upstream {}",
                round, stream.channels, stream.bit_depth, stream.frame_length, stream.pb, stream.mb, stream.kb,
                describe(&got), describe(&expected)
            );
        }
        compared += 1;
        errors += u32::from(matches!(expected, Outcome::Error(_)));
        accepted += ours.q2_accepted;
    }
    assert!(compared > 6000, "{} compared, {} upstream panics", compared, panics);
    assert!(errors > 300, "{} errors of {}", errors, compared);
    assert!(accepted > 3000, "{} of {} decoded by the restructured code", accepted, compared);
}

/// State carried between packets (the buffer's previous contents) does not leak.
#[test]
fn packet_sequences_match_upstream() {
    let mut rng = Rng(0xa1ac_0002);
    let mut compared = 0;
    for round in 0..600u32 {
        let stream = random_stream(&mut rng);
        let mut ours = decoder(&stream, true);
        let mut theirs = decoder(&stream, false);
        for _ in 0..5 {
            let density = rng.below(5);
            let bytes = write_packet(&mut rng, &stream, density);
            let expected = match decode(&mut theirs, &bytes) {
                Some(expected) => expected,
                None => break,
            };
            let got = decode(&mut ours, &bytes).expect("restructured decoder panicked where upstream did not");
            assert!(got == expected, "round {}", round);
            compared += 1;
        }
    }
    assert!(compared > 1000, "{} compared", compared);
}
