//! Encodes 30 s of the generated test signal per configuration and prints
//! the encode time per second of audio. Signal generation is not timed.

#[path = "../tests/common/signal.rs"]
mod signal;

use q2_ldac::{Encoder, Quality};
use std::time::{Duration, Instant};

const SECONDS: usize = 30;

fn bench(quality: Quality, rate: u32) {
    let mut enc = Encoder::new(rate, quality, 679).expect("encoder");
    let mut signal = signal::Signal::new(rate);
    let mut block = vec![0i16; enc.frame_samples() * 2];
    let calls = rate as usize * SECONDS / enc.frame_samples();
    let mut spent = Duration::ZERO;
    let mut bytes = 0usize;
    for _ in 0..calls {
        signal.fill(&mut block);
        let start = Instant::now();
        let packet = enc.encode(&block).expect("encode");
        spent += start.elapsed();
        bytes += packet.map_or(0, |p| p.data.len());
    }
    let audio_s = (calls * enc.frame_samples()) as f64 / f64::from(rate);
    println!(
        "ldac {} {rate} ms_per_audio_second={:.3} kbit_per_s={} bytes={bytes}",
        format!("{quality:?}").to_lowercase(),
        spent.as_secs_f64() * 1000.0 / audio_s,
        enc.bitrate(),
    );
}

fn main() {
    for quality in [Quality::High, Quality::Standard, Quality::Mobile] {
        bench(quality, 44_100);
    }
    bench(Quality::High, 96_000);
}
