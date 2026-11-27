//! Encodes 30 s of generated 44.1 kHz stereo audio in each mode and prints
//! the CPU cost per second of audio.
//!
//! cargo run --release --offline --example bench

use std::time::Instant;

use q2_aptx::Encoder;

const RATE: usize = 44100;
const SECONDS: usize = 30;
/// Stereo frames per `encode` call; a multiple of 4.
const BLOCK_FRAMES: usize = 1024;

/// Two integer oscillator tones plus LCG noise; the same construction as the
/// crate's test signal.
fn signal(frames: usize) -> Vec<i16> {
    let (mut x1, mut y1) = (9000i32, 0i32);
    let (mut x2, mut y2) = (5000i32, 0i32);
    let mut lcg = 0x1234_5678u32;
    let mut pcm = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        x1 -= (y1 * 128) >> 10;
        y1 += (x1 * 128) >> 10;
        x2 -= (y2 * 700) >> 10;
        y2 += (x2 * 700) >> 10;
        let mut noise = || {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((lcg >> 23) as i32) - 256
        };
        let left = x1 + x2 / 2 + noise();
        let right = y1 / 2 + y2 + noise();
        pcm.push(left.clamp(-32768, 32767) as i16);
        pcm.push(right.clamp(-32768, 32767) as i16);
    }
    pcm
}

fn main() {
    let pcm = signal(RATE * SECONDS);
    for (name, hd) in [("aptx", false), ("aptx_hd", true)] {
        let mut enc = Encoder::new(hd).expect("encoder");
        let mut out = Vec::with_capacity(BLOCK_FRAMES * 3 / 2);
        let mut bytes = 0usize;
        let mut check = 0u32;
        let start = Instant::now();
        for block in pcm.chunks(BLOCK_FRAMES * 2) {
            out.clear();
            bytes += enc.encode(block, &mut out);
            // Uses the output so the encode cannot be optimised away.
            check = check.wrapping_mul(31).wrapping_add(u32::from(out[0]));
        }
        let elapsed = start.elapsed();
        let ms_per_s = elapsed.as_secs_f64() * 1000.0 / SECONDS as f64;
        println!(
            "{name} ms_per_audio_second={ms_per_s:.3} audio_s={SECONDS} bytes={bytes} bitrate_kbps={} check={check:08x}",
            bytes * 8 / SECONDS / 1000
        );
    }
}
