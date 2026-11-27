#![forbid(unsafe_code)]
//! `--bench` kinds for the player's PCM conversion on generated planar packets.
//! "convert" is the timed one: each case reports wall microseconds (the
//! process `cpu_us` covers all of them) and an MD5 of its output.
//! "convert-check" is correctness only: an MD5 over short and odd lengths,
//! extreme samples and gains, for comparing two builds.
use crate::audio::decoder::{PreVolume, Samples};
use crate::audio::eq::{Equalizer, Settings};
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::time::Instant;
use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        self.0
    }
}

const F32_EDGES: [u32; 32] = [
    0x0000_0000, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, // 0, -0, 1, -1
    0x3f7f_ffff, 0xbf7f_ffff, 0x3f80_0001, 0xbf80_0001, // either side of full scale
    0x4000_0000, 0xc000_0000, 0x3f00_0000, 0xbf00_0000, // 2, -2, 0.5, -0.5
    0x2f80_0000, 0xaf80_0000, 0x3000_0000, 0x2fc0_0000, // 2^-32, -2^-32, 2^-31, 1.5 * 2^-32
    0x0080_0000, 0x0000_0001, 0x8000_0001, 0x7f7f_ffff, // least normal, denormals, greatest
    0xff7f_ffff, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, // least, inf, -inf, NaN
    0xffc0_0000, 0x7fa0_0000, 0x3ea1_e89b, 0x3dfc_d6ea, // -NaN, signalling NaN, 0.316, 0.123
    0x3f7f_ff00, 0xbf7f_ff00, 0x3380_0000, 0xb380_0000, // near full scale, 2^-24
];
const S32_EDGES: [i32; 12] = [0, 1, -1, i32::MAX, i32::MIN, i32::MAX - 1, i32::MIN + 1,
    (1 << 24) + 1, -(1 << 24) - 1, 0x7fff_ff80, 0x7fff_ffbf, 0x7fff_ffc0];
const S16_EDGES: [i16; 6] = [0, 1, -1, i16::MAX, i16::MIN, 256];
const GAINS: [u32; 13] = [
    0x3f80_0000, 0x0000_0000, 0x3ea1_e89b, 0x3f00_0000, 0x4000_0000, 0x40f0_0000, // 1, 0, 0.316, 0.5, 2, 7.5
    0x5015_02f9, 0x2edb_e6ff, 0x3f80_01a3, 0x3f7f_f2e5, // 1e10, 1e-10, 1.00005, 0.9998
    0x7f80_0000, 0x7fc0_0000, 0xbf80_0000, // inf, NaN, -1
];

enum Packet {
    F32(AudioBuffer<f32>),
    S32(AudioBuffer<i32>),
    S16(AudioBuffer<i16>),
}
impl Packet {
    /// `frames` rendered in a buffer of `capacity`; `edges` > 0 mixes the edge
    /// values in at that period, and `finite` leaves out values an IIR filter
    /// would turn into a constant NaN.
    fn new(kind: &str, channels: usize, frames: usize, capacity: usize, seed: u32, edges: u32, finite: bool) -> Self {
        let spec = SignalSpec::new(44100, if channels == 1 { Channels::FRONT_LEFT } else { Channels::FRONT_LEFT | Channels::FRONT_RIGHT });
        let mut lcg = Lcg(seed);
        let edge = |lcg: &mut Lcg| edges != 0 && (lcg.next() >> 16) % edges == 0;
        match kind {
            "f32" => {
                let mut b = AudioBuffer::<f32>::new(capacity as u64, spec);
                b.render_reserved(Some(frames));
                for ch in 0..channels {
                    for x in b.chan_mut(ch) {
                        let r = lcg.next();
                        // Music-like: uniform in about +/-1.05, so some samples clip.
                        *x = ((r >> 8) as f32 / 8_388_608.0 - 1.0) * 1.05;
                        if edge(&mut lcg) {
                            let e = f32::from_bits(F32_EDGES[(lcg.next() >> 8) as usize % F32_EDGES.len()]);
                            if !finite || e.abs() <= 2.0 { *x = e; }
                        }
                    }
                }
                Self::F32(b)
            }
            "s32" => {
                let mut b = AudioBuffer::<i32>::new(capacity as u64, spec);
                b.render_reserved(Some(frames));
                for ch in 0..channels {
                    for x in b.chan_mut(ch) {
                        *x = lcg.next() as i32;
                        if edge(&mut lcg) { *x = S32_EDGES[(lcg.next() >> 8) as usize % S32_EDGES.len()]; }
                    }
                }
                Self::S32(b)
            }
            _ => {
                let mut b = AudioBuffer::<i16>::new(capacity as u64, spec);
                b.render_reserved(Some(frames));
                for ch in 0..channels {
                    for x in b.chan_mut(ch) {
                        *x = (lcg.next() >> 16) as i16;
                        if edge(&mut lcg) { *x = S16_EDGES[(lcg.next() >> 8) as usize % S16_EDGES.len()]; }
                    }
                }
                Self::S16(b)
            }
        }
    }
    fn get(&self) -> AudioBufferRef<'_> {
        match self {
            Self::F32(b) => AudioBufferRef::F32(Cow::Borrowed(b)),
            Self::S32(b) => AudioBufferRef::S32(Cow::Borrowed(b)),
            Self::S16(b) => AudioBufferRef::S16(Cow::Borrowed(b)),
        }
    }
}

fn one_band() -> Equalizer {
    let mut settings = Settings::default();
    settings.graphic_enabled = true;
    settings.bands[5] = 3;
    Equalizer::new(settings)
}

/// Everything a caller can observe after a conversion.
fn observe(hash: &mut Md5, samples: &Samples) {
    hash.update((samples.samples().len() as u32).to_le_bytes());
    for s in samples.samples() { hash.update(s.to_le_bytes()); }
    match samples.pre_volume() {
        PreVolume::Integer(values, gain) => {
            hash.update([1]); hash.update(gain.to_bits().to_le_bytes());
            for v in values { hash.update(v.to_le_bytes()); }
        }
        PreVolume::Float(values, gain) => {
            hash.update([2]); hash.update(gain.to_bits().to_le_bytes());
            for v in values { hash.update(v.to_bits().to_le_bytes()); }
        }
    }
}

/// Short and odd lengths, extreme samples and gains, through `make()`'s
/// conversion state. One state converts every packet, as in the player.
fn edge_md5(make: fn() -> Samples) -> String {
    let mut hash = Md5::new();
    let mut seed = 1u32;
    for kind in ["f32", "s32", "s16"] {
        for channels in [1usize, 2] {
            let mut samples = make();
            let mut eq_samples = make();
            for frames in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 31, 33, 100, 1152] {
                for gain in GAINS {
                    let gain = f32::from_bits(gain);
                    seed += 1;
                    let packet = Packet::new(kind, channels, frames, frames + 3, seed, 3, false);
                    samples.convert(packet.get(), gain);
                    observe(&mut hash, &samples);
                    if gain.is_finite() {
                        let packet = Packet::new(kind, channels, frames, frames + 3, seed, 3, true);
                        let mut eq = one_band();
                        eq_samples.convert_eq(packet.get(), gain, &mut eq);
                        observe(&mut hash, &eq_samples);
                    }
                }
            }
        }
    }
    format!("{:x}", hash.finalize())
}

pub(super) fn run(iterations: usize) -> Result<Value, String> {
    let mut cases = Vec::new();
    let mut seed = 1000u32;
    for mode in ["convert", "eq"] {
        for kind in ["f32", "s32", "s16"] {
            for channels in [2usize, 1] {
                for frames in [1152usize, 1024, 4096] {
                    for gain in [0.31622776f32, 1.0] {
                        // The equalized path has one final gain loop; time it at one size and gain.
                        if mode == "eq" && (frames != 1152 || gain == 1.0 || kind == "s16") { continue; }
                        seed += 1;
                        let packet = Packet::new(kind, channels, frames, frames, seed, 0, true);
                        let mut samples = Samples::default();
                        let mut eq = one_band();
                        let rounds = (iterations * 4096 / frames).max(1);
                        let mut checksum = 0i32;
                        let start = Instant::now();
                        for _ in 0..rounds {
                            if mode == "eq" { samples.convert_eq(packet.get(), gain, &mut eq); }
                            else { samples.convert(packet.get(), gain); }
                            checksum = checksum.wrapping_add(std::hint::black_box(samples.samples())[0]);
                        }
                        let us = start.elapsed().as_micros() as u64;
                        std::hint::black_box(checksum);
                        let mut hash = Md5::new();
                        observe(&mut hash, &samples);
                        cases.push(json!({"name": format!("{mode}/{kind}/{channels}ch/{frames}/{}", if gain == 1.0 { "unity" } else { "gain" }),
                            "us": us, "ns_per_sample": us as f64 * 1000.0 / (rounds * frames * channels) as f64,
                            "sample_md5": format!("{:x}", hash.finalize())}));
                    }
                }
            }
        }
    }
    Ok(json!({"cases": cases}))
}

pub(super) fn check() -> Result<Value, String> {
    let edge = edge_md5(Samples::default);
    // The same sweep without the vector kernels, and each kernel against the
    // scalar code: on the Q2 and in the emulator this is MSA against scalar.
    let (vector, kernels_match) = Samples::vector_check();
    if edge != edge_md5(Samples::scalar_only) || !kernels_match {
        return Err("vector conversion differs from scalar".into());
    }
    if vector { return Ok(json!({"sample_md5": edge, "vector": true, "vector_equals_scalar": true})); }
    Ok(json!({"sample_md5": edge, "vector": false}))
}
