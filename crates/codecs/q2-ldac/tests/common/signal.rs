//! Deterministic stereo test signal from integer arithmetic only: a 1 kHz
//! and a 7 kHz tone plus low-level noise, mixed differently per channel.
//! Shared by the tests and `examples/bench.rs`.

pub struct Signal {
    phase_a: u32,
    phase_b: u32,
    step_a: u32,
    step_b: u32,
    lcg: u32,
}

/// Sine of `phase` (2^32 is one cycle) in Q15, by a parabola with one
/// correction step. About 0.1 % harmonic distortion.
fn sine_q15(phase: u32) -> i32 {
    let x = (phase as i32) >> 16; // -32768..=32767 is -pi..pi
    let y = (x * (32768 - x.abs())) >> 13;
    y + ((7373 * (((y * y.abs()) >> 15) - y)) >> 15)
}

impl Signal {
    pub fn new(rate: u32) -> Signal {
        let step = |hz: u64| ((hz << 32) / u64::from(rate)) as u32;
        Signal {
            phase_a: 0,
            phase_b: 0,
            step_a: step(1000),
            step_b: step(7000),
            lcg: 0x1234_5678,
        }
    }

    /// Noise in -256..=255.
    fn noise(&mut self) -> i32 {
        self.lcg = self.lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.lcg >> 23) as i32) - 256
    }

    /// Fills `pcm` (interleaved left, right) with the next samples.
    pub fn fill(&mut self, pcm: &mut [i16]) {
        for frame in pcm.as_chunks_mut::<2>().0 {
            let a = sine_q15(self.phase_a);
            let b = sine_q15(self.phase_b);
            self.phase_a = self.phase_a.wrapping_add(self.step_a);
            self.phase_b = self.phase_b.wrapping_add(self.step_b);
            // 0.4 and 0.2 of full scale.
            let left = (a * 13107 + b * 6554) >> 15;
            let right = (b * 13107 + a * 6554) >> 15;
            frame[0] = (left + self.noise()) as i16;
            frame[1] = (right + self.noise()) as i16;
        }
    }
}
