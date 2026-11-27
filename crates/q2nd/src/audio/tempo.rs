#![forbid(unsafe_code)]
//! Pinned Apache-2 Sonic speech tempo. Sample rate/pitch remain unchanged.
//! 100% bypass is performed by the player, preserving original PCM precision.
use super::sonic_ffi::Stage;

pub fn valid_speed(percent: u16) -> bool {
    (50..=300).contains(&percent) && percent % 5 == 0
}

/// Above 2x, compose two whole-period tempo stages. A single Sonic >2x
/// stage overlaps fractional pitch periods; low-rate speech then has larger
/// pitch and duration error. Composition retains the PICOLA pitch-period path.
pub struct Tempo<'a> {
    first: Stage<'a>,
    second: Option<Stage<'a>>,
    intermediate: Vec<i32>,
    tail: Vec<i32>,
    pub rate: u32,
    pub channels: u32,
    pub percent: u16,
}
impl<'a> Tempo<'a> {
    pub fn new(blocking: &'a crate::rt::Blocking, rate: u32, channels: u32, percent: u16) -> Result<Self, String> {
        if !valid_speed(percent) {
            return Err("Invalid spoken playback speed".into());
        }
        let first = Stage::new(blocking, rate, channels, f32::from(percent.min(200)) / 100.0)?;
        let second = if percent > 200 {
            Some(Stage::new(blocking, rate, channels, f32::from(percent) / 200.0)?)
        } else {
            None
        };
        Ok(Self {
            first,
            second,
            intermediate: Vec::new(),
            tail: Vec::new(),
            rate,
            channels,
            percent,
        })
    }
    pub fn process(&mut self, blocking: &crate::rt::Blocking, input: &[i32], output: &mut Vec<i32>) -> Result<(), String> {
        if let Some(second) = self.second.as_mut() {
            self.first.process(blocking, input, &mut self.intermediate)?;
            second.process(blocking, &self.intermediate, output)
        } else {
            self.first.process(blocking, input, output)
        }
    }
    pub fn finish(&mut self, blocking: &crate::rt::Blocking, output: &mut Vec<i32>) -> Result<(), String> {
        if let Some(second) = self.second.as_mut() {
            self.first.finish(blocking, &mut self.intermediate)?;
            second.process(blocking, &self.intermediate, output)?;
            second.finish(blocking, &mut self.tail)?;
            output.extend_from_slice(&self.tail);
            Ok(())
        } else {
            self.first.finish(blocking, output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(rate: u32, channels: u32) -> Vec<i32> {
        (0..rate * 2)
            .flat_map(|i| {
                let value = ((i as f64 * 2.0 * std::f64::consts::PI * 220.0 / rate as f64).sin()
                    * 1e9) as i32;
                (0..channels).map(move |_| value)
            })
            .collect()
    }
    fn render_packets(
        input: &[i32],
        rate: u32,
        channels: u32,
        percent: u16,
        packets: &[usize],
    ) -> Vec<i32> {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut tempo = Tempo::new(&blocking, rate, channels, percent).unwrap();
        let mut output = Vec::new();
        let mut block = Vec::new();
        let mut start = 0;
        let mut packet = 0;
        while start < input.len() {
            let end =
                (start + packets[packet % packets.len()] * channels as usize).min(input.len());
            tempo.process(&blocking, &input[start..end], &mut block).unwrap();
            output.extend_from_slice(&block);
            start = end;
            packet += 1;
        }
        tempo.finish(&blocking, &mut block).unwrap();
        output.extend_from_slice(&block);
        output
    }

    fn voiced_unvoiced_fixture(rate: u32, channels: u32) -> Vec<i32> {
        let mut random = 0x71364b2du32;
        (0..rate)
            .flat_map(|frame| {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = (random as i32 as f64) * 0.15;
                let voiced = (1..=4)
                    .map(|harmonic| {
                        (frame as f64 * 2.0 * std::f64::consts::PI * 137.0 * harmonic as f64
                            / rate as f64)
                            .sin()
                            * 500_000_000.0
                            / harmonic as f64
                    })
                    .sum::<f64>();
                let value = if frame / (rate / 10) % 3 == 0 {
                    noise
                } else {
                    voiced
                } as i32;
                (0..channels).map(move |channel| if channel == 0 { value } else { value / 2 })
            })
            .collect()
    }

    #[test]
    fn decoder_packet_boundaries_cannot_change_spoken_pcm() {
        for rate in [8000, 16000, 44100] {
            for channels in [1, 2] {
                let input = voiced_unvoiced_fixture(rate, channels);
                for percent in (50..=300).step_by(5) {
                    let reference = render_packets(&input, rate, channels, percent, &[1024]);
                    for packets in [&[4096][..], &[17, 511, 1, 1152, 4096, 173][..]] {
                        let actual = render_packets(&input, rate, channels, percent, packets);
                        assert!(
                            reference == actual,
                            "packet-dependent rendering: rate{rate} channels{channels} speed{percent} frames {} vs {}",
                            reference.len() / channels as usize,
                            actual.len() / channels as usize
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn all_speed_steps_preserve_pitch_duration_and_stereo() {
        for rate in [8000, 16000, 22050, 44100, 48000, 96000, 192000] {
            let input = tone(rate, 2);
            for percent in (50..=300).step_by(5) {
                let blocking = crate::rt::Blocking::dedicated_thread();
                let mut tempo = Tempo::new(&blocking, rate, 2, percent).unwrap();
                let mut output = Vec::new();
                let mut chunk = Vec::new();
                // Seeded variable packet boundaries exercise streaming state.
                let mut start = 0;
                let mut seed = 0x5132u32;
                while start < input.len() {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let end = (start + ((seed as usize % 4096) + 1) * 2).min(input.len());
                    tempo.process(&blocking, &input[start..end], &mut chunk).unwrap();
                    output.extend_from_slice(&chunk);
                    start = end;
                }
                tempo.finish(&blocking, &mut chunk).unwrap();
                output.extend_from_slice(&chunk);
                let expected = input.len() as f64 * 100.0 / percent as f64;
                assert!(
                    (output.len() as f64 - expected).abs() < rate as f64 * 0.04,
                    "rate{rate} speed{percent} length{} expected{expected}",
                    output.len()
                );
                assert!(output.chunks_exact(2).all(|f| f[0] == f[1]));
                let mono: Vec<_> = output.chunks_exact(2).map(|f| f[0]).collect();
                let trim = (rate as usize / 10).min(mono.len() / 4);
                let middle = &mono[trim..mono.len() - trim];
                // Pitch-period joins can add/remove zero crossings without
                // moving the dominant voice fundamental. Measure spectrum,
                // rather than mistaking those join artifacts for pitch shift.
                let stride = (rate as usize / 4000).max(1);
                let signal: Vec<_> = middle
                    .iter()
                    .step_by(stride)
                    .take(4000)
                    .map(|&v| v as f64 / 2147483648.0)
                    .collect();
                let sample_rate = rate as f64 / stride as f64;
                let pitch = (380..=500)
                    .map(|bin| {
                        let frequency = bin as f64 / 2.0;
                        let coefficient =
                            2.0 * (2.0 * std::f64::consts::PI * frequency / sample_rate).cos();
                        let (mut a, mut b) = (0.0, 0.0);
                        for &sample in &signal {
                            let next = sample + coefficient * a - b;
                            b = a;
                            a = next;
                        }
                        (frequency, a * a + b * b - coefficient * a * b)
                    })
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .unwrap()
                    .0;
                assert!(
                    (pitch - 220.0).abs() < 3.0,
                    "rate{rate} speed{percent} dominant pitch{pitch}"
                );
            }
        }
    }
    #[test]
    fn long_silence_keeps_streaming_state_bounded_and_flush_is_idempotent() {
        for percent in [50, 95, 105, 175, 205, 300] {
            let blocking = crate::rt::Blocking::dedicated_thread();
            let mut tempo = Tempo::new(&blocking, 8000, 1, percent).unwrap();
            let input = [0i32; 1024];
            let mut output = Vec::new();
            let mut count = 0usize;
            // More than four minutes of compressed-book PCM, never retained
            // across calls. Zero/singleton input also exercises drain edges.
            tempo.process(&blocking, &[], &mut output).unwrap();
            tempo.process(&blocking, &[0], &mut output).unwrap();
            count += output.len();
            for _ in 0..2048 {
                tempo.process(&blocking, &input, &mut output).unwrap();
                count += output.len();
                assert!(output.len() < 8192 && output.capacity() < 16384);
                assert!(tempo.intermediate.capacity() < 16384);
                assert!(output.iter().all(|&sample| sample == 0));
            }
            tempo.finish(&blocking, &mut output).unwrap();
            count += output.len();
            let expected = (2048.0 * 1024.0 + 1.0) * 100.0 / f64::from(percent);
            assert!(
                (count as f64 - expected).abs() < 160.0,
                "speed{percent}: {count} expected{expected}"
            );
            tempo.finish(&blocking, &mut output).unwrap();
            assert!(output.is_empty());
        }
    }

    #[test]
    fn invalid_steps_frames_and_format_are_rejected() {
        for percent in [0, 49, 51, 99, 301, 65535] {
            assert!(!valid_speed(percent));
        }
        assert!(Tempo::new(&crate::rt::Blocking::dedicated_thread(), 0, 2, 100).is_err());
        assert!(Tempo::new(&crate::rt::Blocking::dedicated_thread(), 48000, 0, 100).is_err());
        assert!(Tempo::new(&crate::rt::Blocking::dedicated_thread(), 48000, 9, 100).is_err());
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut tempo = Tempo::new(&blocking, 48000, 2, 150).unwrap();
        assert!(tempo.process(&blocking, &[1], &mut Vec::new()).is_err());
    }
}
