//! Rational polyphase resampler for interleaved stereo: brings any source
//! rate to the stream's 44.1 or 48 kHz with a Kaiser-windowed sinc.

pub struct Resampler {
    up: usize,
    down: usize,
    taps: usize,
    /// `up` phases of `taps` coefficients.
    table: Vec<f32>,
    /// Interleaved input not yet consumed, preceded by `taps` frames of history.
    buffer: Vec<f32>,
    /// Position of the next output, in units of 1/`up` input frames past the history.
    position: usize,
}

fn gcd(a: u32, b: u32) -> u32 { if b == 0 { a } else { gcd(b, a % b) } }
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, half) = (1.0, 1.0, x / 2.0);
    for k in 1..40 { term *= (half / k as f64) * (half / k as f64); sum += term; }
    sum
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        assert!(from > 0 && to > 0);
        let g = gcd(from, to);
        let (up, down) = ((to / g) as usize, (from / g) as usize);
        // Longer filters for decimation keep the transition band inside the output's top 6%.
        let taps = (24 * down.div_ceil(up).max(1)).min(128);
        let cutoff = (to as f64 / from as f64).min(1.0) * 0.94;
        let beta = 9.0;
        let mut table = vec![0f32; up * taps];
        for phase in 0..up {
            let fraction = phase as f64 / up as f64;
            let mut row: Vec<f64> = (0..taps).map(|k| {
                let u = k as f64 - (taps / 2 - 1) as f64 - fraction;
                let window = { let r = u / (taps / 2) as f64; if r.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta) } };
                let x = std::f64::consts::PI * cutoff * u;
                cutoff * if x.abs() < 1e-9 { 1.0 } else { x.sin() / x } * window
            }).collect();
            let gain: f64 = row.iter().sum();
            for c in &mut row { *c /= gain; }
            for (k, c) in row.iter().enumerate() { table[phase * taps + k] = *c as f32; }
        }
        Self { up, down, taps, table, buffer: vec![0.0; taps * 2], position: 0 }
    }
    pub fn passthrough(&self) -> bool { self.up == self.down }
    /// Discards filter history (after a flush).
    pub fn reset(&mut self) { self.buffer.clear(); self.buffer.resize(self.taps * 2, 0.0); self.position = 0; }

    /// Consumes all of `input` (interleaved stereo) and appends the output frames it completes.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.passthrough() { out.extend_from_slice(input); return; }
        self.buffer.extend(input.iter().map(|&s| f32::from(s)));
        let frames = self.buffer.len() / 2;
        loop {
            let (index, phase) = (self.position / self.up, self.position % self.up);
            if index + self.taps > frames { break }
            let coefficients = &self.table[phase * self.taps..(phase + 1) * self.taps];
            let window = &self.buffer[index * 2..(index + self.taps) * 2];
            let (mut left, mut right) = (0f32, 0f32);
            for (k, c) in coefficients.iter().enumerate() { left += c * window[k * 2]; right += c * window[k * 2 + 1]; }
            out.push(left.round().clamp(-32768.0, 32767.0) as i16);
            out.push(right.round().clamp(-32768.0, 32767.0) as i16);
            self.position += self.down;
        }
        let consumed = (self.position / self.up).min(frames);
        self.buffer.drain(..consumed * 2);
        self.position -= consumed * self.up;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, frames: usize) -> Vec<i16> {
        (0..frames).flat_map(|n| {
            let v = (20000.0 * (2.0 * std::f64::consts::PI * hz * n as f64 / rate as f64).sin()).round() as i16;
            [v, -v]
        }).collect()
    }
    /// Amplitude of `hz` in the left channel by correlation, skipping the filter's start.
    fn amplitude(pcm: &[i16], rate: u32, hz: f64) -> f64 {
        let (mut re, mut im, mut n) = (0.0, 0.0, 0.0);
        for (i, frame) in pcm.chunks_exact(2).enumerate().skip(2000) {
            let w = 2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64;
            re += f64::from(frame[0]) * w.cos(); im += f64::from(frame[0]) * w.sin(); n += 1.0;
        }
        2.0 * (re * re + im * im).sqrt() / n
    }

    #[test]
    fn tones_pass_and_aliases_are_rejected_for_every_supported_conversion() {
        for (from, to) in [(48000, 44100), (96000, 48000), (88200, 44100), (192000, 48000), (176400, 44100), (96000, 44100),
            (22050, 44100), (32000, 44100), (24000, 48000), (16000, 44100), (8000, 44100), (11025, 44100), (44100, 48000)] {
            let mut r = Resampler::new(from, to);
            let hz = 1000.0;
            let input = sine(from, hz, from as usize);
            let mut out = Vec::new();
            for chunk in input.chunks(2 * 1111) { r.process(chunk, &mut out); }
            let frames = out.len() / 2;
            assert!((frames as i64 - to as i64).abs() <= r.taps as i64 + 2, "{from}->{to}: {frames} frames");
            let level = amplitude(&out, to, hz);
            assert!((level - 20000.0).abs() < 200.0, "{from}->{to}: 1 kHz at {level:.0}");
            assert!(out.chunks_exact(2).all(|f| (i32::from(f[0]) + i32::from(f[1])).abs() <= 1), "channels stay separate");
            if from > to {
                // A tone above the output's Nyquist frequency must not fold into the band.
                let above = to as f64 * 0.62;
                let mut r = Resampler::new(from, to);
                let mut out = Vec::new();
                r.process(&sine(from, above, from as usize), &mut out);
                let alias = amplitude(&out, to, to as f64 - above);
                assert!(alias < 20.0, "{from}->{to}: alias at {alias:.1}");
            }
        }
    }

    #[test]
    fn equal_rates_copy_and_chunking_does_not_change_the_output() {
        let mut r = Resampler::new(44100, 44100);
        let mut out = Vec::new();
        r.process(&[1, 2, 3, 4], &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
        let input = sine(48000, 440.0, 9000);
        let (mut a, mut b) = (Resampler::new(48000, 44100), Resampler::new(48000, 44100));
        let (mut whole, mut pieces) = (Vec::new(), Vec::new());
        a.process(&input, &mut whole);
        for chunk in input.chunks(2 * 7) { b.process(chunk, &mut pieces); }
        assert_eq!(whole, pieces);
    }
}
