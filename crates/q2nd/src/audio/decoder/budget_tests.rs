//! The player's per-packet PCM conversion must not use the heap once its
//! buffers have reached the packet size: it runs about 40 times a second for
//! every track, and the Q2's allocator is one arena shared with the UI.
//! `test_alloc` counts this thread's allocations; a regression that
//! allocates per packet fails here.
use super::*;
use crate::audio::eq::{Equalizer, Settings};
use crate::audio::gain::GainRamp;
use std::borrow::Cow;
use symphonia::core::audio::{AudioBuffer, Channels, SignalSpec};

/// MP3 packet length and FLAC's usual block length.
const FRAMES: [usize; 2] = [1152, 4096];

fn spec(channels: usize) -> SignalSpec {
    SignalSpec::new(44100, if channels == 2 { Channels::FRONT_LEFT | Channels::FRONT_RIGHT } else { Channels::FRONT_LEFT })
}
fn floats(frames: usize, channels: usize) -> AudioBuffer<f32> {
    let mut buffer = AudioBuffer::<f32>::new(frames as u64, spec(channels));
    buffer.render_silence(Some(frames));
    for channel in 0..channels {
        for (i, sample) in buffer.chan_mut(channel).iter_mut().enumerate() { *sample = ((i + channel * 17) as f32 * 0.031).sin() * 0.8; }
    }
    buffer
}
fn integers(frames: usize, channels: usize) -> AudioBuffer<i32> {
    let mut buffer = AudioBuffer::<i32>::new(frames as u64, spec(channels));
    buffer.render_silence(Some(frames));
    for channel in 0..channels {
        for (i, sample) in buffer.chan_mut(channel).iter_mut().enumerate() { *sample = (((i + channel * 17) as f32 * 0.031).sin() * 1.6e9) as i32; }
    }
    buffer
}
fn equalizer(active: bool) -> Equalizer {
    let mut settings = Settings::default();
    settings.graphic_enabled = active;
    settings.bands = [3, 2, 1, 0, -1, 0, 1, 2, 3, 2];
    Equalizer::new(settings)
}

#[test]
fn steady_state_conversion_does_not_allocate() {
    for channels in [2usize, 1] {
        let packets: Vec<(AudioBuffer<f32>, AudioBuffer<i32>)> = FRAMES.iter().map(|&frames| (floats(frames, channels), integers(frames, channels))).collect();
        for eq_active in [false, true] {
            let mut samples = Samples::default();
            let mut eq = equalizer(eq_active);
            let mut ramp = GainRamp::default();
            ramp.start(0.5, 44100);
            // One pass sizes every buffer; a volume step starts a ramp.
            let mut pass = |samples: &mut Samples, eq: &mut Equalizer, ramp: &mut GainRamp, volume: f32| {
                let mut produced = 0;
                for (float, integer) in packets.iter().rev() {
                    for decoded in [AudioBufferRef::F32(Cow::Borrowed(float)), AudioBufferRef::S32(Cow::Borrowed(integer))] {
                        produced += samples.convert(decoded.clone(), 1.0).len();
                        produced += samples.convert(decoded.clone(), 0.5).len();
                        samples.convert_eq(decoded.clone(), 0.7, eq);
                        produced += samples.samples().len();
                        samples.convert_eq_ramped(decoded, 0.7, volume, eq, ramp, 3);
                        produced += samples.samples().len();
                    }
                }
                produced
            };
            let warm = pass(&mut samples, &mut eq, &mut ramp, 0.5);
            assert_eq!(pass(&mut samples, &mut eq, &mut ramp, 0.1), warm);
            let (produced, heap) = crate::test_alloc::measure(|| {
                let mut produced = 0;
                for volume in [0.1f32, 0.9, 0.9, 0.0, 1.0] { produced += pass(&mut samples, &mut eq, &mut ramp, volume); }
                produced
            });
            assert_eq!(produced, warm * 5);
            assert_eq!(heap.allocations, 0, "{channels} channels, eq {eq_active}: {heap:?}");
        }
    }
}
