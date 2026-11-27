//! Verified codec volume for the internal headphone route.
//! Startup leaves every DAC muted; the player sets the requested 0.5 dB
//! attenuation before opening PCM. EQ and ReplayGain remain in software.
//! DAPM continues to own amplifier mute and power sequencing.
#![forbid(unsafe_code)]
use std::ffi::{c_long, c_uint};
use super::mixer_ffi::AlsaControls;
use crate::config::HeadphonePower;

const DAC_COUNT: usize = 4;
const UNITY: c_long = 255;

/// The DAC Filter setting's four stock choices (0 Sharp/fast, 1 Slow,
/// 2 Short Sharp, 3 Short Slow: the stock driver's "low-delay" pair) as the
/// CS43131's controls: "PCM Filter Speed" item (0 Fast, 1 Slow) and "PCM Phase
/// Compensation". Reading low delay as phase compensation on follows the stock
/// labels; it is not checked against the datasheet.
pub fn filter_controls(filter: u8) -> (c_uint, bool) {
    let f = filter.min(3);
    (c_uint::from(f & 1), f >= 2)
}

/// One owner-facing codec setting written to every DAC and read back.
trait Setting {
    fn write_enum(&mut self, blocking: &crate::rt::Blocking, name: &str, item: c_uint) -> Result<(), String>;
    fn write_bool(&mut self, blocking: &crate::rt::Blocking, name: &str, on: bool) -> Result<(), String>;
    /// Whether the card has a control of this name.
    fn has(&mut self, blocking: &crate::rt::Blocking, name: &str) -> bool;
}

fn apply_filter(blocking: &crate::rt::Blocking, controls: &mut impl Setting, filter: u8) -> Result<(), String> {
    let (speed, phase) = filter_controls(filter);
    for dac in 0..DAC_COUNT {
        controls.write_enum(blocking, &format!("DAC{dac} PCM Filter Speed"), speed)?;
        controls.write_bool(blocking, &format!("DAC{dac} PCM Phase Compensation"), phase)?;
    }
    Ok(())
}

/// Kernel patch 0187's per-codec controls. HP Full Scale items: 0.5, 1,
/// 1.41, 1.73 and 2 Vrms. HP Class H items: Adapt to Signal, Fixed VCP,
/// Fixed VP_LDO. Both refuse (EBUSY) while the codec's HP stage is powered,
/// so the player writes them only with the PCM closed.
const FULL_SCALE: &str = "HP Full Scale";
const CLASS_H: &str = "HP Class H";
/// Patch 0122's control on a driver without 0187: Low (1 Vrms, adapting)
/// or High (1.73 Vrms).
const LEVEL: &str = "HP Output Level";
const ADAPT: c_uint = 0;
const FIXED_VCP: c_uint = 1;

/// Each Headphone Power level as (HP Full Scale, HP Class H) items.
/// Efficient holds the amplifiers on the fixed ±VCP rails (VCP 1.66–1.94 V),
/// which DS1155F2 table 4-2 rates for signals below about −11 dB of the
/// 1.73 Vrms full scale (0.49 Vrms, nominal at 16 Ω): 0.5 Vrms full scale
/// is at that edge, so the codec keeps [`EFFICIENT_FLOOR`] of attenuation.
/// Whether a peak then clips into 16 Ω is a device check (docs/device.md).
pub fn operating_point(level: HeadphonePower) -> (c_uint, c_uint) {
    match level {
        HeadphonePower::Efficient => (0, FIXED_VCP),
        HeadphonePower::Standard => (1, ADAPT),
        HeadphonePower::High => (3, ADAPT),
        HeadphonePower::Maximum => (4, ADAPT),
    }
}

/// Half-dB codec steps that make up a level's lower full scale: Efficient's
/// 0.5 Vrms is 6.02 dB below Standard's 1 Vrms, so with 12 fewer steps of
/// digital attenuation each volume step plays at Standard's level up to
/// Efficient's maximum. High and Maximum are not compensated: they raise
/// every step by 4.8 and 6.0 dB, as High gain did.
pub fn level_boost(level: HeadphonePower) -> u8 {
    if level == HeadphonePower::Efficient { 12 } else { 0 }
}

/// Efficient's least codec attenuation, 1 dB: margin for the fixed ±VCP
/// rails (see [`operating_point`]). Volume 86 and above, at slider gain,
/// all play at this maximum, 7 dB below Standard's.
pub const EFFICIENT_FLOOR: u8 = 2;

/// The codec attenuation for the player's `steps` (0 unity, 255 mute) with
/// the level the codecs hold; an unknown level gets no make-up gain.
pub fn hardware_steps(steps: u8, applied: Option<HeadphonePower>) -> u8 {
    match applied {
        _ if steps == u8::MAX => steps,
        Some(HeadphonePower::Efficient) => steps.saturating_sub(level_boost(HeadphonePower::Efficient)).max(EFFICIENT_FLOOR),
        _ => steps,
    }
}

/// Keep Efficient's installed analogue level while moving the PCM's gain
/// domain. Both attenuation registers are in half-dB steps, so separating
/// physical and logical gain preserves their exact ratio. In particular, the
/// installed early floor/plateau on boosted tracks is retained by request.
pub fn preserve_efficient_gain(requested: u8, installed: u8) -> (u8, u8) {
    let physical = hardware_steps(installed, Some(HeadphonePower::Efficient));
    let offset = i16::from(physical) - i16::from(installed);
    let logical = i16::from(requested).clamp(
        (i16::from(EFFICIENT_FLOOR) - offset).max(0),
        (254 - offset).min(254),
    );
    (logical as u8, (logical + offset) as u8)
}

fn apply_power(blocking: &crate::rt::Blocking, controls: &mut impl Setting, level: HeadphonePower) -> Result<HeadphonePower, String> {
    if !controls.has(blocking, &format!("DAC0 {FULL_SCALE}")) {
        let high = matches!(level, HeadphonePower::High | HeadphonePower::Maximum);
        for dac in 0..DAC_COUNT {
            controls.write_enum(blocking, &format!("DAC{dac} {LEVEL}"), c_uint::from(high))?;
        }
        let applied = if high { HeadphonePower::High } else { HeadphonePower::Standard };
        if applied != level {
            log::warn!("Headphone Power: the codec driver lacks kernel patch 0187; {} instead of {}", applied.label(), level.label());
        }
        return Ok(applied);
    }
    let (full_scale, class_h) = operating_point(level);
    for dac in 0..DAC_COUNT {
        let (fs, ch) = (format!("DAC{dac} {FULL_SCALE}"), format!("DAC{dac} {CLASS_H}"));
        // Leaving the fixed rail adapts first; entering it lowers the full
        // scale first. The HP stage is down for both writes either way.
        if class_h == ADAPT {
            controls.write_enum(blocking, &ch, class_h)?;
            controls.write_enum(blocking, &fs, full_scale)?;
        } else {
            controls.write_enum(blocking, &fs, full_scale)?;
            controls.write_enum(blocking, &ch, class_h)?;
        }
    }
    Ok(level)
}

/// The Headphone Power setting on the mainline image, written to every
/// codec and read back. Call only with the headphone PCM closed. Returns
/// the level the codecs now hold (an older codec driver has two).
pub fn set_headphone_power(blocking: &crate::rt::Blocking, level: HeadphonePower) -> Result<HeadphonePower, String> {
    let mut controls = AlsaControls::open(blocking)?;
    apply_power(blocking, &mut controls, level).map_err(|e| format!("Headphone Power: {e}"))
}

/// The DAC Filter setting on the mainline image (the stock ioctl is absent).
pub fn set_codec_filter(blocking: &crate::rt::Blocking, filter: u8) -> Result<(), String> {
    apply_filter(blocking, &mut AlsaControls::open(blocking)?, filter).map_err(|e| format!("DAC filter: {e}"))
}

trait Controls {
    fn validate(&mut self, blocking: &crate::rt::Blocking, dac: usize) -> Result<(), String>;
    fn write(&mut self, blocking: &crate::rt::Blocking, dac: usize, value: c_long) -> Result<(), String>;
    fn read(&mut self, blocking: &crate::rt::Blocking, dac: usize) -> Result<[c_long; 2], String>;
}

/// The qualified cs43130 driver exposes -127.5 dB + 0.5 dB per raw
/// step, with raw zero as mute (pcm_vol_tlv). Preserve the existing player
/// curve: volume1=-49.5 dB, volume100=0 dB, and volume0 is mute.
#[cfg(test)]
fn codec_volume(volume: u8) -> c_long {
    if volume == 0 { 0 } else { UNITY - c_long::from(100 - volume.min(100)) }
}

pub fn set_codec_attenuation(blocking: &crate::rt::Blocking, steps: u8) -> Result<(), String> {
    let mut controls = AlsaControls::open(blocking)?;
    configure(blocking, &mut controls, UNITY-c_long::from(steps))
        .map_err(|e| format!("Headphone volume: {e}"))
}

fn configure(blocking: &crate::rt::Blocking, controls: &mut impl Controls, value: c_long) -> Result<(), String> {
    let result = (|| {
        // Check every control before changing any DAC gain.
        for dac in 0..DAC_COUNT { controls.validate(blocking, dac)?; }
        for dac in 0..DAC_COUNT {
            controls.write(blocking, dac, value)?;
            if controls.read(blocking, dac)? != [value; 2] {
                return Err(format!("DAC{dac} playback gain readback mismatch"));
            }
        }
        Ok(())
    })();
    if result.is_err() {
        // The player refuses or closes PCM after failure. Also mute any DAC whose gain
        // was already changed, attempting every control even if one fails.
        for dac in 0..DAC_COUNT {
            if let Err(e) = controls.write(blocking, dac, 0) {
                log::error!("mainline mixer cleanup: {e}");
            }
        }
    }
    result
}

/// Call during platform startup, before constructing the player/audio thread.
pub fn prepare_q2_playback() -> Result<(), String> {
    assert!(crate::rt::may_wait_for_tasks(), "initial mixer setup belongs to ordinary platform startup");
    let blocking = &crate::rt::Blocking::dedicated_thread();
    let mut controls = AlsaControls::open(blocking)?;
    configure(blocking, &mut controls, 0).map_err(|e| format!("mainline mixer: {e}"))?;
    log::info!("Mainline DACs verified muted; player owns codec volume");
    Ok(())
}

impl Controls for AlsaControls<'_> {
    fn validate(&mut self, blocking: &crate::rt::Blocking, dac: usize) -> Result<(), String> { AlsaControls::validate(self, blocking, dac) }
    fn write(&mut self, blocking: &crate::rt::Blocking, dac: usize, value: c_long) -> Result<(), String> { AlsaControls::write(self, blocking, dac, value) }
    fn read(&mut self, blocking: &crate::rt::Blocking, dac: usize) -> Result<[c_long; 2], String> { AlsaControls::read(self, blocking, dac) }
}
impl Setting for AlsaControls<'_> {
    fn write_enum(&mut self, blocking: &crate::rt::Blocking, name: &str, item: c_uint) -> Result<(), String> { AlsaControls::write_enum(self, blocking, name, item) }
    fn write_bool(&mut self, blocking: &crate::rt::Blocking, name: &str, on: bool) -> Result<(), String> { AlsaControls::write_bool(self, blocking, name, on) }
    fn has(&mut self, blocking: &crate::rt::Blocking, name: &str) -> bool { AlsaControls::has(self, blocking, name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeControls {
        values: [[c_long; 2]; DAC_COUNT],
        invalid: Option<usize>,
        write_failure: Option<usize>,
        stale_channel: Option<usize>,
        writes: Vec<(usize, c_long)>,
    }
    impl Controls for FakeControls {
        fn validate(&mut self, _blocking: &crate::rt::Blocking, dac: usize) -> Result<(), String> {
            if self.invalid == Some(dac) { Err("missing control".into()) } else { Ok(()) }
        }
        fn write(&mut self, _blocking: &crate::rt::Blocking, dac: usize, value: c_long) -> Result<(), String> {
            self.writes.push((dac, value));
            if self.write_failure == Some(dac) { return Err("write failed".into()); }
            self.values[dac] = [value; 2];
            Ok(())
        }
        fn read(&mut self, _blocking: &crate::rt::Blocking, dac: usize) -> Result<[c_long; 2], String> {
            let mut value = self.values[dac];
            if self.stale_channel == Some(dac) { value[1] = 135; }
            Ok(value)
        }
    }

    #[derive(Default)]
    struct FakeSetting { writes: Vec<String>, fail_on: Option<&'static str>, old_driver: bool }
    impl Setting for FakeSetting {
        fn write_enum(&mut self, _blocking: &crate::rt::Blocking, name: &str, item: c_uint) -> Result<(), String> {
            if self.fail_on == Some("enum") || self.fail_on == Some(name) { return Err("missing".into()); }
            self.writes.push(format!("{name}={item}")); Ok(())
        }
        fn write_bool(&mut self, _blocking: &crate::rt::Blocking, name: &str, on: bool) -> Result<(), String> {
            self.writes.push(format!("{name}={on}")); Ok(())
        }
        fn has(&mut self, _blocking: &crate::rt::Blocking, name: &str) -> bool {
            !(self.old_driver && (name.ends_with(FULL_SCALE) || name.ends_with(CLASS_H)))
        }
    }

    #[test]
    fn headphone_power_levels_write_full_scale_and_class_h_on_every_codec() {
        use HeadphonePower::*;
        let blocking = crate::rt::Blocking::dedicated_thread();
        for (level, first, second) in [
            (Efficient, "HP Full Scale=0", "HP Class H=1"),
            (Standard, "HP Class H=0", "HP Full Scale=1"),
            (High, "HP Class H=0", "HP Full Scale=3"),
            (Maximum, "HP Class H=0", "HP Full Scale=4"),
        ] {
            let mut s = FakeSetting::default();
            assert_eq!(apply_power(&blocking, &mut s, level).unwrap(), level);
            let expected: Vec<String> = (0..DAC_COUNT).flat_map(|dac| [format!("DAC{dac} {first}"), format!("DAC{dac} {second}")]).collect();
            assert_eq!(s.writes, expected, "{level:?}");
        }
        // The fixed rail is only ever paired with the lowest full scale.
        for level in HeadphonePower::ALL {
            let (full_scale, class_h) = operating_point(level);
            assert!(class_h == ADAPT || full_scale == 0, "{level:?}");
        }
        // A refused write (EBUSY with the HP stage powered) is the result.
        let mut busy = FakeSetting { fail_on: Some("DAC0 HP Full Scale"), ..Default::default() };
        assert!(apply_power(&blocking, &mut busy, Efficient).is_err());
        assert!(busy.writes.is_empty());
    }

    #[test]
    fn a_codec_driver_without_patch_0187_gets_the_nearest_of_its_two_levels() {
        use HeadphonePower::*;
        let blocking = crate::rt::Blocking::dedicated_thread();
        for (level, item, applied) in [(Efficient, 0, Standard), (Standard, 0, Standard), (High, 1, High), (Maximum, 1, High)] {
            let mut s = FakeSetting { old_driver: true, ..Default::default() };
            assert_eq!(apply_power(&blocking, &mut s, level).unwrap(), applied, "{level:?}");
            assert_eq!(s.writes, (0..DAC_COUNT).map(|dac| format!("DAC{dac} HP Output Level={item}")).collect::<Vec<_>>());
        }
    }

    #[test]
    fn efficient_makes_up_its_lower_full_scale_until_the_codec_is_at_unity() {
        use HeadphonePower::*;
        // 0.5 Vrms against 1 Vrms is 6.02 dB: 12 half-dB steps, 0.02 dB off.
        assert!((20.0 * (0.5f64).log10() + 0.5 * f64::from(level_boost(Efficient))).abs() < 0.03);
        for steps in [0u8, 1, 11, 12, 13, 14, 15, 98, 254] {
            assert_eq!(hardware_steps(steps, Some(Efficient)), steps.saturating_sub(12).max(EFFICIENT_FLOOR));
            for level in [Standard, High, Maximum] { assert_eq!(hardware_steps(steps, Some(level)), steps); }
            assert_eq!(hardware_steps(steps, None), steps, "an unknown level is never made louder");
        }
        assert_eq!(hardware_steps(u8::MAX, Some(Efficient)), u8::MAX, "mute stays mute");
        // Every volume step up to 86 is the same total level on Efficient and
        // Standard; above it Efficient holds its maximum, 1 dB under its full scale.
        for volume in 1..=100u8 {
            let steps = 100 - volume;
            let standard = -0.5 * f64::from(steps);
            let efficient = 20.0 * (0.5f64).log10() - 0.5 * f64::from(hardware_steps(steps, Some(Efficient)));
            if volume <= 86 { assert!((efficient - standard).abs() < 0.03, "{volume}"); }
            else { assert!((efficient + 7.02).abs() < 0.01, "{volume}"); }
        }
    }

    #[test]
    fn dac_filter_choices_map_to_speed_and_phase_compensation() {
        assert_eq!(filter_controls(0), (0, false));
        assert_eq!(filter_controls(1), (1, false));
        assert_eq!(filter_controls(2), (0, true));
        assert_eq!(filter_controls(3), (1, true));
        assert_eq!(filter_controls(9), (1, true), "out of range clamps to the last choice");
        let mut s = FakeSetting::default();
        apply_filter(&crate::rt::Blocking::dedicated_thread(), &mut s, 3).unwrap();
        assert_eq!(s.writes.len(), 8, "every DAC, both controls");
        assert_eq!(s.writes[0], "DAC0 PCM Filter Speed=1");
        assert_eq!(s.writes[7], "DAC3 PCM Phase Compensation=true");
        let mut failing = FakeSetting { fail_on: Some("enum"), ..Default::default() };
        assert!(apply_filter(&crate::rt::Blocking::dedicated_thread(), &mut failing, 1).is_err());
    }

    #[test]
    fn prepares_every_dac_and_both_channels_muted() {
        let mut ctl = FakeControls { values: [[135; 2]; DAC_COUNT], ..Default::default() };
        configure(&crate::rt::Blocking::dedicated_thread(), &mut ctl, 0).unwrap();
        assert_eq!(ctl.values, [[0; 2]; DAC_COUNT]);
    }


    #[test]
    fn codec_steps_preserve_the_slider_curve_on_all_eight_channels() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut controls = FakeControls::default();
        for volume in 0..=100u8 {
            let value = codec_volume(volume);
            configure(&blocking, &mut controls, value).unwrap();
            assert_eq!(controls.values, [[value; 2]; DAC_COUNT]);
            if volume == 0 { assert_eq!(value, 0); }
            else {
                let db = -127.5 + value as f32 * 0.5;
                assert_eq!(db, -0.5 * (100 - volume) as f32);
                assert!((10f32.powf(db / 20.0) - crate::audio::player::soft_gain(volume)).abs() < 1e-6);
                if volume > 1 { assert_eq!(value, codec_volume(volume - 1) + 1); }
            }
        }
        assert_eq!(codec_volume(255), UNITY);
    }

    #[test]
    fn missing_control_never_raises_any_gain() {
        let mut ctl = FakeControls { invalid: Some(3), ..Default::default() };
        assert!(configure(&crate::rt::Blocking::dedicated_thread(), &mut ctl, 0).is_err());
        assert!(ctl.writes.iter().all(|&(_, value)| value == 0));
    }

    #[test]
    fn stereo_readback_failure_mutes_every_dac() {
        let mut ctl = FakeControls { stale_channel: Some(2), ..Default::default() };
        assert!(configure(&crate::rt::Blocking::dedicated_thread(), &mut ctl, codec_volume(60)).unwrap_err().contains("readback"));
        assert_eq!(ctl.values, [[0; 2]; DAC_COUNT]);
    }

    #[test]
    fn failed_write_still_attempts_all_cleanup_controls() {
        let mut ctl = FakeControls { write_failure: Some(1), ..Default::default() };
        assert!(configure(&crate::rt::Blocking::dedicated_thread(), &mut ctl, codec_volume(60)).is_err());
        assert_eq!(&ctl.writes[ctl.writes.len()-4..], &[(0, 0), (1, 0), (2, 0), (3, 0)]);
    }
}
