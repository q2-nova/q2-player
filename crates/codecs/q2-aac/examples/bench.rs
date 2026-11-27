//! Encodes 30 s of generated 44.1 kHz stereo audio at 256 and 320 kbit/s and prints
//! the encode time per second of audio.

#[path = "../tests/common/mod.rs"]
mod common;

use q2_aac::Encoder;
use std::time::Instant;

fn main() {
    const RATE: u32 = 44100;
    const SECONDS: usize = 30;
    let pcm = common::signal(RATE as usize * SECONDS);
    for bitrate in [256_000u32, 320_000] {
        let mut encoder = Encoder::new(RATE, bitrate).expect("encoder");
        let mut out = Vec::with_capacity(2048);
        let mut frames = 0usize;
        let mut bytes = 0usize;
        let start = Instant::now();
        for frame in pcm.as_chunks::<{ Encoder::FRAME_SAMPLES * 2 }>().0 {
            out.clear();
            bytes += encoder.encode(frame, &mut out).expect("encode");
            frames += 1;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let audio_seconds = (frames * Encoder::FRAME_SAMPLES) as f64 / f64::from(RATE);
        println!(
            "aac {bitrate} ms_per_audio_second={:.3} frames={frames} measured_bitrate={:.0}",
            elapsed * 1000.0 / audio_seconds,
            bytes as f64 * 8.0 / audio_seconds
        );
    }
}
