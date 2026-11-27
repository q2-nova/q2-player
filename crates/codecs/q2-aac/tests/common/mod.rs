//! Deterministic test signal, integer arithmetic only: two tones per channel and noise.
//! Shared by the tests and `examples/bench.rs`.

/// Coupled-form oscillator in Q30. `k` is `2 sin(pi f / rate)` in Q30.
struct Osc {
    x: i64,
    y: i64,
    k: i64,
}

impl Osc {
    fn new(amplitude: i64, k: i64) -> Osc {
        Osc {
            x: amplitude << 16,
            y: 0,
            k,
        }
    }

    fn next(&mut self) -> i64 {
        self.x -= (self.k * self.y) >> 30;
        self.y += (self.k * self.x) >> 30;
        self.y >> 16
    }
}

/// `frames` interleaved stereo frames. At 44.1 kHz the tones are 440 Hz and 3 kHz (left)
/// and 660 Hz and 5 kHz (right); the noise is a 32-bit LCG at +-512.
pub fn signal(frames: usize) -> Vec<i16> {
    let mut left = [Osc::new(9000, 67_301_185), Osc::new(4000, 455_461_206)];
    let mut right = [Osc::new(7000, 100_931_113), Osc::new(3000, 748_839_539)];
    let mut lcg: u32 = 0x1234_5678;
    let mut noise = || {
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        i64::from(lcg >> 22) - 512
    };
    let mut pcm = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        let l = left[0].next() + left[1].next() + noise();
        let r = right[0].next() + right[1].next() + noise();
        pcm.push(l.clamp(-32768, 32767) as i16);
        pcm.push(r.clamp(-32768, 32767) as i16);
    }
    pcm
}
