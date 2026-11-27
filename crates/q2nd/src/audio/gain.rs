#![forbid(unsafe_code)]
//! Software counterpart of the stock DAC's approximately 1 dB/ms soft ramp.
//! Both stereo samples use one gain, and retargeting starts at the rendered gain.
#[derive(Default)]
pub struct GainRamp {
    current: Option<f64>,
    target: f64,
    remaining: u32,
    ratio: f64,
    rate: u32,
}

impl GainRamp {
    pub fn start(&mut self, target: f32, rate: u32) {
        *self = Self::default();
        self.target(target, rate);
    }

    pub fn target(&mut self, target: f32, rate: u32) {
        let target = target as f64;
        let Some(current) = self.current else {
            // Starting playback must use the configured volume, never unity.
            self.current = Some(target);
            self.target = target;
            self.rate = rate;
            return;
        };
        if target == self.target && rate == self.rate {
            return;
        }
        self.target = target;
        self.rate = rate;
        // -120 dB is the finite endpoint used when fading to/from exact zero.
        let from = current.max(0.000001);
        let to = target.max(0.000001);
        let db = (20.0 * (to / from).log10()).abs();
        self.remaining = (db * rate as f64 / 1000.0).ceil() as u32;
        self.ratio = if self.remaining == 0 {
            1.0
        } else {
            (to / from).powf(1.0 / self.remaining as f64)
        };
        if self.remaining == 0 {
            self.current = Some(target);
        }
    }

    /// Apply audible gain after a quantizing spoken time stretcher. Keeping
    /// volume out of its input preserves quiet samples and pitch analysis.
    pub fn apply_pcm(
        &mut self,
        samples: &mut [i32],
        channels: usize,
        track_gain: f32,
        volume: f32,
        rate: u32,
    ) {
        self.target(volume, rate);
        if !self.active() {
            let gain = track_gain as f64 * volume as f64;
            if gain == 1.0 {
                return;
            }
            for sample in samples {
                *sample = (*sample as f64 * gain).clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            }
            return;
        }
        for frame in samples.chunks_exact_mut(channels) {
            let gain = track_gain as f64 * self.next() as f64;
            for sample in frame {
                *sample = (*sample as f64 * gain).clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            }
        }
    }

    /// Hardware applied the whole listener delta to an existing envelope.
    /// Rescale its current and target levels without consuming any frames.
    pub fn scale(&mut self, ratio: f64) {
        self.current = self.current.map(|gain| gain * ratio);
        self.target *= ratio;
    }

    pub fn active(&self) -> bool {
        self.remaining != 0
    }

    pub fn next(&mut self) -> f32 {
        if self.remaining != 0 {
            self.remaining -= 1;
            self.current = Some(if self.remaining == 0 {
                self.target
            } else {
                self.current.unwrap().max(0.000001) * self.ratio
            });
        }
        self.current.unwrap_or(0.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stretched(input: &[i32], percent: u16) -> Vec<i32> {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut tempo = super::super::tempo::Tempo::new(&blocking, 16000, 2, percent).unwrap();
        let mut output = Vec::new();
        let mut block = Vec::new();
        for packet in input.chunks(1024 * 2) {
            tempo.process(&blocking, packet, &mut block).unwrap();
            output.extend_from_slice(&block);
        }
        tempo.finish(&blocking, &mut block).unwrap();
        output.extend_from_slice(&block);
        output
    }

    fn carrier_snr(samples: &[i32]) -> f64 {
        let frames = samples.len() / 2;
        let count = (frames - 3200) / 64 * 64;
        let mono: Vec<_> = samples
            .chunks_exact(2)
            .skip(1600)
            .take(count)
            .map(|frame| frame[0] as f64)
            .collect();
        let sine: f64 = mono
            .iter()
            .enumerate()
            .map(|(i, &x)| x * (i as f64 * std::f64::consts::TAU / 64.0).sin())
            .sum::<f64>()
            * 2.0
            / count as f64;
        let cosine: f64 = mono
            .iter()
            .enumerate()
            .map(|(i, &x)| x * (i as f64 * std::f64::consts::TAU / 64.0).cos())
            .sum::<f64>()
            * 2.0
            / count as f64;
        let noise: f64 = mono
            .iter()
            .enumerate()
            .map(|(i, &x)| {
                let phase = i as f64 * std::f64::consts::TAU / 64.0;
                (x - sine * phase.sin() - cosine * phase.cos()).powi(2)
            })
            .sum::<f64>()
            / count as f64;
        10.0 * (((sine * sine + cosine * cosine) / 2.0) / noise).log10()
    }

    #[test]
    fn quiet_spoken_detail_survives_volume_and_old_order_is_a_negative_control() {
        let input: Vec<i32> = (0..32000)
            .flat_map(|i| {
                let sample =
                    (64.0 * (i as f64 * std::f64::consts::TAU / 64.0).sin()).round() as i32 * 65536;
                [sample, sample]
            })
            .collect();
        for percent in [125, 150, 200, 250, 300] {
            let volume = super::super::player::soft_gain(50);
            let mut output = stretched(&input, percent);
            GainRamp::default().apply_pcm(&mut output, 2, 1.0, volume, 16000);
            let reduced: Vec<_> = input
                .iter()
                .map(|&sample| (sample as f32 * volume) as i32)
                .collect();
            let negative = stretched(&reduced, percent);
            assert!(
                carrier_snr(&output) > 38.0,
                "speed{percent}: {}dB",
                carrier_snr(&output)
            );
            assert!(
                carrier_snr(&negative) < 20.0,
                "old order must expose quantization loss"
            );
            assert!(output.chunks_exact(2).all(|frame| frame[0] == frame[1]));
        }
    }

    #[test]
    fn post_stretch_gain_ramp_counts_output_frames_and_preserves_mute_and_headroom() {
        let mut ramp = GainRamp::default();
        ramp.start(1.0, 16000);
        ramp.target(0.1, 16000);
        let mut samples = vec![i32::MAX; 160 * 2];
        ramp.apply_pcm(&mut samples, 2, 2.0, 0.1, 16000);
        assert_eq!(ramp.remaining, 160); // Ten of the requested20dB/20ms consumed.
        assert!(samples.chunks_exact(2).all(|frame| frame[0] == frame[1]));
        let mut quiet = vec![1_000_000_000; 16];
        ramp.start(0.0, 16000);
        ramp.apply_pcm(&mut quiet, 2, 4.0, 0.0, 16000);
        assert!(quiet.iter().all(|&sample| sample == 0));
        ramp.start(0.1, 16000);
        ramp.apply_pcm(&mut samples, 2, 2.0, 0.1, 16000);
        assert!(samples.iter().all(|&sample| sample <= 429_496_736));
    }

    #[test]
    fn configured_start_and_one_db_per_ms_at_multiple_rates() {
        for rate in [44100, 48000, 96000] {
            let mut ramp = GainRamp::default();
            ramp.target(0.1, rate);
            assert_eq!(ramp.next(), 0.1);
            ramp.target(1.0, rate);
            for _ in 0..rate / 100 {
                ramp.next();
            }
            assert!((ramp.next() - 0.3162).abs() < 0.001);
            while ramp.active() {
                ramp.next();
            }
            assert_eq!(ramp.next(), 1.0);
        }
    }
    fn volume() -> impl proptest::strategy::Strategy<Value = f32> {
        use proptest::prelude::*;
        prop_oneof![Just(0.0f32), Just(1.0f32), 0.0f32..=1.5f32]
    }
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 96, failure_persistence: None, ..proptest::prelude::ProptestConfig::default() })]
        /// Any sequence of volume changes, each cut short by the next: the
        /// gain never leaves the span between where it was and its target,
        /// never moves faster than 1 dB a millisecond, and lands on the
        /// target exactly within the time 124 dB takes.
        #[test]
        fn any_retarget_sequence_is_monotonic_slew_limited_and_lands_exactly(
            rate in proptest::prelude::prop_oneof![proptest::prelude::Just(8000u32), proptest::prelude::Just(44100u32),
                proptest::prelude::Just(48000u32), proptest::prelude::Just(96000u32), proptest::prelude::Just(192000u32)],
            start in volume(),
            steps in proptest::collection::vec((volume(), 0usize..3000), 1..6),
        ) {
            use proptest::prelude::*;
            let mut ramp = GainRamp::default();
            ramp.start(start, rate);
            prop_assert!(!ramp.active());
            let mut current = start;
            let step = 10f64.powf(1000.0 / rate as f64 / 20.0) * 1.0001;
            let mut advance = |ramp: &mut GainRamp, current: &mut f32, target: f32| -> Result<(), TestCaseError> {
                let next = ramp.next();
                prop_assert!(next.is_finite() && next >= current.min(target) && next <= current.max(target), "{} -> {} toward {}", *current, next, target);
                // Below -120 dB the ramp steps to and from exact zero.
                if *current > 0.000001 && next > 0.000001 {
                    let ratio = (next as f64 / *current as f64).max(*current as f64 / next as f64);
                    prop_assert!(ratio <= step, "{} -> {} is more than 1 dB/ms at {} Hz", *current, next, rate);
                }
                *current = next;
                Ok(())
            };
            for (target, frames) in steps {
                ramp.target(target, rate);
                for _ in 0..frames { advance(&mut ramp, &mut current, target)?; }
                let mut settle = 0u32;
                ramp.target(target, rate);
                while ramp.active() {
                    advance(&mut ramp, &mut current, target)?;
                    settle += 1;
                    prop_assert!(settle <= 124 * rate / 1000 + 2, "still ramping after {} frames", settle);
                }
                prop_assert_eq!(ramp.next().to_bits(), target.to_bits());
                current = target;
            }
        }
    }

    #[test]
    fn rapid_retarget_stays_continuous_and_zero_is_exact() {
        let mut ramp = GainRamp::default();
        ramp.target(1.0, 48000);
        ramp.target(0.01, 48000);
        for _ in 0..240 {
            ramp.next();
        }
        let last = ramp.next();
        ramp.target(1.0, 48000);
        let next = ramp.next();
        assert!(next > last && next / last < 1.003);
        ramp.target(0.0, 48000);
        for _ in 0..5761 {
            ramp.next();
        }
        assert!(!ramp.active());
        assert_eq!(ramp.next(), 0.0);
        ramp.target(0.1, 48000);
        assert!(ramp.next() < 0.000002);
    }
}

/// CS43131 digital attenuation: half-dB steps, 255 is the dedicated mute code.
/// Preserve the slider's f32 gain calculation for its existing 0..49.5 dB range.
pub fn codec_gain(steps:u8)->f64 {
    if steps==255 {0.0}else{f64::from(10.0_f32.powf(-f32::from(steps)/40.0))}
}
pub fn codec_steps(required:f64)->u8 {
    // The grid is monotone. Eight comparisons replace up to255 powf calls
    // per wheel event while retaining the exact conservative rounding rule.
    let (mut low,mut high)=(0u16,255u16);
    while low<high {
        let middle=(low+high)/2;
        if codec_gain(middle as u8)>=required {low=middle+1;}else{high=middle;}
    }
    low.saturating_sub(1) as u8
}

/// Reserve headroom for ReplayGain on the codec's half-dB grid. Round the
/// track multiplier once, then add the integer listener attenuation. Rounding
/// an independently multiplied f32 listener gain can change the residue on
/// adjacent steps even when the track multiplier is exactly on the grid.
pub fn nominal_codec_steps(volume: u8, track_gain: f32) -> u8 {
    if track_gain <= 0.0 { return 254; }
    let lower = (40.0 * f64::from(track_gain).log10()).floor() as i16;
    let boost = lower + i16::from(track_gain > 10.0f32.powf(f32::from(lower) / 40.0));
    (i16::from(100 - volume.min(100)) - boost).clamp(0, 254) as u8
}

/// The two stages must move in the same direction. Efficient can use a
/// different physical codec step from the logical PCM gain domain.
pub fn mixed_gain_steps_fit(pcm_delta: i16, physical_delta: i16) -> bool {
    pcm_delta == 0 || physical_delta == 0 || pcm_delta.signum() == physical_delta.signum()
}

/// Both volume and the codec use the same half-dB grid. Derive compensation
/// from their integer step difference, rather than dividing independently
/// rounded gains. Equivalent domains then have exactly identical coefficients:
/// a wheel detent must not pause/replay PCM because of a floating-point ULP.
pub fn codec_compensation(volume:u8,steps:u8)->f32 {
    let difference=i16::from(steps)-i16::from(100-volume.min(100));
    10.0f32.powf(f32::from(difference)/40.0)
}

#[cfg(test)]
mod codec_grid_tests {
    #[test]
    fn binary_grid_selection_matches_exhaustive_reference_at_every_boundary() {
        for required in (0..=254).flat_map(|steps| {
            let gain=super::codec_gain(steps);
            [gain,gain*(1.0-1e-12),gain*(1.0+1e-12)]
        }).chain([0.0,-1.0,2.0,f64::INFINITY,f64::NAN]) {
            let reference=(0..=254).rev().find(|&step|super::codec_gain(step)>=required).unwrap_or(0);
            assert_eq!(super::codec_steps(required),reference,"required={required}");
        }
    }
}
