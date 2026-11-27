//! The conversion before `pcm_convert.rs` (Symphonia's `SampleBuffer`, then a
//! second gain pass), kept verbatim as `Old`, against `Samples` on generated
//! packets: every entry point, sample format, odd length and extreme value.
use super::*;
use crate::audio::eq::{Equalizer, Settings};
use crate::audio::gain::GainRamp;
use std::borrow::Cow;
use symphonia::core::audio::{AudioBuffer, Channels, SampleBuffer, SignalSpec};
use symphonia::core::sample::{i24, u24, Sample};

#[derive(Default)]
struct Old {
    floats: Option<SampleBuffer<f32>>,
    eq_output: Vec<i32>,
    spoken_floats: Vec<f32>,
    spoken_original: Vec<i32>,
    source: SourceBuffer,
    source_gain: f32,
    using_eq: bool,
    buffer: Option<SampleBuffer<i32>>,
    spec: Option<SignalSpec>,
}
impl Old {
    pub fn convert(&mut self, decoded: AudioBufferRef<'_>, gain:f32) -> &[i32] {
        self.using_eq = false;
        self.source = SourceBuffer::Integer;
        self.source_gain = gain;
        let spec = *decoded.spec();
        let needed = decoded.capacity() * spec.channels.count();
        if self.spec != Some(spec) { self.floats=None; }
        if self.spec != Some(spec) || self.buffer.as_ref().is_none_or(|b|b.capacity()<needed) {
            self.buffer = Some(SampleBuffer::new(decoded.capacity() as u64,spec));
            self.spec = Some(spec);
        }
        let buffer = self.buffer.as_mut().unwrap();
        buffer.copy_interleaved_ref(decoded);
        if (gain-1.0).abs()>1e-4 {
            self.eq_output.clear();
            self.eq_output.extend(buffer.samples().iter().map(|&sample|
                (sample as f32*gain).clamp(i32::MIN as f32,i32::MAX as f32) as i32));
            self.using_eq = true;
            &self.eq_output
        } else {
            buffer.samples()
        }
    }
    pub fn convert_eq(&mut self, decoded: AudioBufferRef<'_>, gain: f32, eq: &mut crate::audio::eq::Equalizer) {
        let spec = *decoded.spec();
        eq.prepare(spec.rate, spec.channels.count());
        if eq.bypass() { self.convert(decoded,gain); return; }
        self.source=SourceBuffer::Equalized;
        self.source_gain=gain;
        let needed = decoded.capacity()*spec.channels.count();
        if self.spec != Some(spec) || self.floats.as_ref().is_none_or(|b|b.capacity()<needed) {
            self.floats=Some(SampleBuffer::new(decoded.capacity() as u64,spec));
            // Keep integer and floating buffers' format bookkeeping consistent.
            self.buffer=None;
            self.spec=Some(spec);
        }
        let floats=self.floats.as_mut().unwrap();
        floats.copy_interleaved_ref(decoded);
        eq.process(floats.samples_mut());
        self.eq_output.clear();
        self.eq_output.extend(floats.samples().iter().map(|s|
            (*s as f64 * gain as f64 * 2147483648.0).clamp(i32::MIN as f64,i32::MAX as f64) as i32));
        self.using_eq=true;
    }
    pub fn convert_eq_ramped(&mut self, decoded: AudioBufferRef<'_>, track_gain: f32,
        volume: f32, eq: &mut crate::audio::eq::Equalizer,
        ramp: &mut crate::audio::gain::GainRamp, skip: usize) {
        let spec = *decoded.spec();
        let channels = spec.channels.count();
        ramp.target(volume, spec.rate);
        if !ramp.active() {
            self.convert_eq(decoded,track_gain*volume,eq);
            self.source_gain=track_gain;
            return;
        }
        eq.prepare(spec.rate, channels);
        if eq.bypass() {
            self.convert(decoded,1.0);
            self.source_gain=track_gain;
            self.eq_output.clear();
            self.eq_output.extend_from_slice(self.buffer.as_ref().unwrap().samples());
            self.using_eq=true;
            for frame in self.eq_output.chunks_exact_mut(channels).skip(skip) {
                let gain = track_gain as f64 * ramp.next() as f64;
                for sample in frame {
                    *sample = (*sample as f64 * gain).clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                }
            }
            return;
        }
        self.source=SourceBuffer::Equalized;
        self.source_gain=track_gain;
        let needed = decoded.capacity() * channels;
        if self.spec != Some(spec) || self.floats.as_ref().is_none_or(|b|b.capacity()<needed) {
            self.floats = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
            self.buffer = None;
            self.spec = Some(spec);
        }
        let floats = self.floats.as_mut().unwrap();
        floats.copy_interleaved_ref(decoded);
        eq.process(floats.samples_mut());
        self.eq_output.clear();
        for (index, frame) in floats.samples().chunks_exact(channels).enumerate() {
            // Seek-discarded frames must not consume the audible gain transition.
            let gain = track_gain as f64 * if index < skip { volume } else { ramp.next() } as f64;
            self.eq_output.extend(frame.iter().map(|s|
                (*s as f64 * gain * 2147483648.0).clamp(i32::MIN as f64, i32::MAX as f64) as i32));
        }
        self.using_eq = true;
    }
    /// Output-side headphone/EQ/level processing for a spoken time stretcher.
    /// Float EQ retains transient headroom until the final software gain.
    pub fn process_spoken_output(&mut self, pcm: &mut [i32], rate: u32, channels: usize,
        eq: &mut crate::audio::eq::Equalizer, ramp: &mut crate::audio::gain::GainRamp,
        track_gain: f32, volume: f32) {
        eq.prepare(rate,channels);
        self.source_gain=track_gain;
        if eq.bypass() {
            self.source=SourceBuffer::SpokenInteger;
            self.spoken_original.clear();
            self.spoken_original.extend_from_slice(pcm);
            ramp.apply_pcm(pcm,channels,track_gain,volume,rate);
            return;
        }
        self.source=SourceBuffer::SpokenEqualized;
        self.spoken_floats.clear();
        self.spoken_floats.extend(pcm.iter().map(|&sample| sample as f32 / 2147483648.0));
        eq.process(&mut self.spoken_floats);
        ramp.target(volume, rate);
        if !ramp.active() {
            let gain = track_gain as f64 * volume as f64 * 2147483648.0;
            for (sample, &value) in pcm.iter_mut().zip(&self.spoken_floats) {
                *sample = (value as f64 * gain).clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            }
        } else {
            for (output, input) in pcm.chunks_exact_mut(channels).zip(self.spoken_floats.chunks_exact(channels)) {
                let gain = track_gain as f64 * ramp.next() as f64 * 2147483648.0;
                for (sample, &value) in output.iter_mut().zip(input) {
                    *sample = (value as f64 * gain).clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                }
            }
        }
    }

    pub fn samples(&self) -> &[i32] {
        if self.using_eq { &self.eq_output } else { self.buffer.as_ref().map_or(&[],SampleBuffer::samples) }
    }
    pub fn pre_volume(&self)->PreVolume<'_> {
        match self.source {
            SourceBuffer::Integer=>PreVolume::Integer(self.buffer.as_ref().map_or(&[],SampleBuffer::samples),self.source_gain),
            SourceBuffer::Equalized=>PreVolume::Float(self.floats.as_ref().map_or(&[],SampleBuffer::samples),self.source_gain),
            SourceBuffer::SpokenInteger=>PreVolume::Integer(&self.spoken_original,self.source_gain),
            SourceBuffer::SpokenEqualized=>PreVolume::Float(&self.spoken_floats,self.source_gain),
        }
    }
}

fn lcg(state: &mut u32) -> u32 { *state = state.wrapping_mul(1664525).wrapping_add(1013904223); *state }

const F32_EDGES: [u32; 24] = [0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x3f7f_ffff, 0xbf7f_ffff, 0x3f80_0001, 0xbf80_0001,
    0x4000_0000, 0xc000_0000, 0x2f80_0000, 0xaf80_0000, 0x3000_0000, 0x0080_0000, 1, 0x8000_0001, 0x7f7f_ffff, 0xff7f_ffff,
    0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0xffc0_0000, 0x7fa0_0000, 0x3380_0000];

fn packet<S: Sample>(frames: usize, channels: usize, mut value: impl FnMut() -> S) -> AudioBuffer<S> {
    let layout = [Channels::FRONT_LEFT, Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
        Channels::FRONT_LEFT | Channels::FRONT_RIGHT | Channels::FRONT_CENTRE][channels - 1];
    // Decoders trim packets: fewer frames than capacity.
    let mut buffer = AudioBuffer::<S>::new(frames as u64 + 3, SignalSpec::new(44100, layout));
    buffer.render_reserved(Some(frames));
    for channel in 0..channels { for sample in buffer.chan_mut(channel) { *sample = value(); } }
    buffer
}

/// Calls `check` with a generated packet in every sample format.
/// `finite` keeps values an IIR filter would turn into a constant NaN out.
fn packets(frames: usize, channels: usize, seed: u32, finite: bool, mut check: impl FnMut(AudioBufferRef<'_>)) {
    let state = std::cell::Cell::new(seed);
    let word = || { let mut s = state.get(); let r = lcg(&mut s); state.set(s); r };
    let float = || {
        let r = word();
        let edge = f32::from_bits(F32_EDGES[(r >> 8) as usize % F32_EDGES.len()]);
        if r & 3 == 0 && (!finite || edge.abs() <= 2.0) { edge } else { ((word() >> 8) as f32 / 8_388_608.0 - 1.0) * 1.05 }
    };
    let integer = || {
        let r = word();
        if r & 7 == 0 { [0, 1, -1, i32::MAX, i32::MIN, i32::MAX - 1, 0x7fff_ffc0, (1 << 24) + 1][(r >> 8) as usize % 8] } else { word() as i32 }
    };
    check(AudioBufferRef::F32(Cow::Owned(packet(frames, channels, float))));
    check(AudioBufferRef::S32(Cow::Owned(packet(frames, channels, integer))));
    check(AudioBufferRef::S16(Cow::Owned(packet(frames, channels, || (integer() >> 16) as i16))));
    check(AudioBufferRef::S8(Cow::Owned(packet(frames, channels, || (integer() >> 24) as i8))));
    check(AudioBufferRef::S24(Cow::Owned(packet(frames, channels, || i24::from(integer() >> 8)))));
    check(AudioBufferRef::U8(Cow::Owned(packet(frames, channels, || (word() >> 24) as u8))));
    check(AudioBufferRef::U16(Cow::Owned(packet(frames, channels, || (word() >> 16) as u16))));
    check(AudioBufferRef::U24(Cow::Owned(packet(frames, channels, || u24::from(word() >> 8)))));
    check(AudioBufferRef::U32(Cow::Owned(packet(frames, channels, word))));
    check(AudioBufferRef::F64(Cow::Owned(packet(frames, channels, || float() as f64 * 1.000000001))));
}

fn format(decoded: &AudioBufferRef<'_>) -> &'static str {
    match decoded {
        AudioBufferRef::U8(_) => "u8", AudioBufferRef::U16(_) => "u16", AudioBufferRef::U24(_) => "u24",
        AudioBufferRef::U32(_) => "u32", AudioBufferRef::S8(_) => "s8", AudioBufferRef::S16(_) => "s16",
        AudioBufferRef::S24(_) => "s24", AudioBufferRef::S32(_) => "s32", AudioBufferRef::F32(_) => "f32",
        AudioBufferRef::F64(_) => "f64",
    }
}

fn same(new: &Samples, old: &Old, context: &str) {
    assert_eq!(new.samples(), old.samples(), "samples {context}");
    match (new.pre_volume(), old.pre_volume()) {
        (PreVolume::Integer(a, x), PreVolume::Integer(b, y)) => { assert_eq!(a, b, "integer source {context}"); assert_eq!(x.to_bits(), y.to_bits()); }
        (PreVolume::Float(a, x), PreVolume::Float(b, y)) => {
            assert_eq!(a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), b.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), "float source {context}");
            assert_eq!(x.to_bits(), y.to_bits());
        }
        _ => panic!("source kind {context}"),
    }
}

fn equalizer(active: bool) -> Equalizer {
    let mut settings = Settings::default();
    settings.graphic_enabled = active;
    settings.bands[5] = 3;
    Equalizer::new(settings)
}

const FRAMES: [usize; 14] = [0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 17, 33, 100, 1152];
fn gains() -> [f32; 14] {
    [1.0, 0.0, 0.31622776, 0.5, 2.0, 7.5, 1e10, 1e-10, 1.00005, 0.9998, f32::INFINITY, f32::NAN, -1.0, -0.0]
}

#[test]
fn convert_equals_the_replaced_conversion() {
    for (vector, make) in [(true, Samples::default as fn() -> Samples), (false, Samples::scalar_only)] {
        // One state for every packet: formats, channel counts and lengths change under it.
        let (mut new, mut old) = (make(), Old::default());
        let mut seed = 0;
        for channels in [2, 1, 3] {
            for frames in FRAMES {
                for gain in gains() {
                    seed += 1;
                    packets(frames, channels, seed, false, |decoded| {
                        let context = format!("vector {vector} {channels}ch {frames} frames gain {gain} {}", format(&decoded));
                        assert_eq!(new.convert(decoded.clone(), gain), old.convert(decoded, gain), "{context}");
                        same(&new, &old, &context);
                    });
                }
            }
        }
    }
}

#[test]
fn convert_eq_equals_the_replaced_conversion() {
    for active in [true, false] {
        let (mut new, mut old) = (Samples::default(), Old::default());
        let mut seed = 0;
        for channels in [2, 1, 3] {
            // The filters keep history across packets, as in the player.
            let (mut new_eq, mut old_eq) = (equalizer(active), equalizer(active));
            for frames in FRAMES {
                for gain in gains() {
                    if !gain.is_finite() { continue; }
                    seed += 1;
                    packets(frames, channels, seed, true, |decoded| {
                        let context = format!("eq {active} {channels}ch {frames} frames gain {gain} {}", format(&decoded));
                        new.convert_eq(decoded.clone(), gain, &mut new_eq);
                        old.convert_eq(decoded, gain, &mut old_eq);
                        same(&new, &old, &context);
                    });
                }
            }
        }
    }
}

#[test]
fn ramped_and_spoken_paths_equal_the_replaced_conversion() {
    for active in [true, false] {
        let (mut new, mut old) = (Samples::default(), Old::default());
        let (mut new_eq, mut old_eq) = (equalizer(active), equalizer(active));
        let (mut new_ramp, mut old_ramp) = (GainRamp::default(), GainRamp::default());
        new_ramp.start(1.0, 44100);
        old_ramp.start(1.0, 44100);
        let mut seed = 0;
        // Volume steps start ramps that span packets; repeats run the settled path.
        for (step, volume) in [0.1f32, 0.1, 1.0, 0.0, 0.0, 0.5, 2.0, 0.5, 0.5].into_iter().enumerate() {
            for frames in [1152, 33, 100, 7, 1152, 2000] {
                seed += 1;
                let skip = [0, 17, 3][seed as usize % 3].min(frames);
                packets(frames, 2, seed, true, |decoded| {
                    let context = format!("eq {active} step {step} volume {volume} {frames} frames {}", format(&decoded));
                    new.convert_eq_ramped(decoded.clone(), 0.7, volume, &mut new_eq, &mut new_ramp, skip);
                    old.convert_eq_ramped(decoded, 0.7, volume, &mut old_eq, &mut old_ramp, skip);
                    same(&new, &old, &context);
                    let (mut a, mut b) = (new.samples().to_vec(), old.samples().to_vec());
                    new.process_spoken_output(&mut a, 44100, 2, &mut new_eq, &mut new_ramp, 0.7, volume);
                    old.process_spoken_output(&mut b, 44100, 2, &mut old_eq, &mut old_ramp, 0.7, volume);
                    assert_eq!(a, b, "spoken {context}");
                    same(&new, &old, &context);
                });
            }
        }
        assert_eq!(new_ramp.next().to_bits(), old_ramp.next().to_bits());
    }
}
