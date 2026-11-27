#![forbid(unsafe_code)]
//! Shared decoder setup and bounded reusable PCM conversion.
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use symphonia::core::audio::{AudioBufferRef, Signal};
use super::pcm_convert;
use symphonia::core::errors::Result;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::{Hint, ProbeResult};

struct ProbeSource {
    source: Box<dyn MediaSource>,
    probing: Arc<AtomicBool>,
    read_error: Arc<Mutex<Option<std::io::Error>>>,
    capture_error: Arc<AtomicBool>,
}
impl Read for ProbeSource {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        super::callback::require()?;
        self.source.read(b).map_err(|error| {
            if !self.capture_error.load(Ordering::Relaxed) { return error; }
            // Symphonia's marker scan treats read errors as EOF, then reports
            // "no suitable format". Preserve the real transport/cancel reason.
            let copy = std::io::Error::new(error.kind(), error.to_string());
            *self.read_error.lock().unwrap() = Some(error);
            copy
        })
    }
}
impl Seek for ProbeSource {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> { super::callback::require()?; self.source.seek(p) }
}
impl MediaSource for ProbeSource {
    fn is_seekable(&self) -> bool { super::callback::assert_allowed(); !self.probing.load(Ordering::Relaxed) && self.source.is_seekable() }
    fn byte_len(&self) -> Option<u64> { super::callback::assert_allowed(); self.source.byte_len() }
}

pub fn probe(blocking: &crate::rt::Blocking, source: Box<dyn MediaSource>, ext: Option<&str>, streaming: bool) -> Result<ProbeResult> {
    if crate::rt::on_executor() { return Err(symphonia::core::errors::Error::IoError(std::io::Error::new(std::io::ErrorKind::WouldBlock,"decoder probe on executor"))); }
    super::callback::with(blocking, || probe_owned(blocking, source, ext, streaming))
}
fn probe_owned(_blocking: &crate::rt::Blocking, source: Box<dyn MediaSource>, ext: Option<&str>, streaming: bool) -> Result<ProbeResult> {
    let mut hint = Hint::new();
    if let Some(ext) = ext { hint.with_extension(ext); }
    let probing = Arc::new(AtomicBool::new(streaming));
    let read_error = Arc::new(Mutex::new(None));
    let capture_error = Arc::new(AtomicBool::new(true));
    let source = ProbeSource { source, probing:probing.clone(), read_error:read_error.clone(), capture_error:capture_error.clone() };
    // An unfinished download must not be scanned to its end just to obtain
    // duration (Ogg) or search for optional atoms (MP4). Navidrome already
    // supplies duration. Restore random access after the initial probe.
    let result = symphonia::default::get_probe().format(&hint,MediaSourceStream::new(Box::new(source),Default::default()),
        &FormatOptions { enable_gapless:true,..Default::default() },&MetadataOptions::default());
    // Ogg keeps seek bounds computed at construction. Switching it to
    // seekable without those bounds makes Symphonia 0.5.5 unwrap None.
    // The player reopens a completed Ogg when the user requests a seek.
    let sequential_ogg = streaming && result.as_ref().is_ok_and(|r| r.format.tracks().iter()
        .any(|t| t.codec_params.codec == symphonia::core::codecs::CODEC_TYPE_VORBIS));
    probing.store(sequential_ogg,Ordering::Relaxed);
    capture_error.store(false,Ordering::Relaxed);
    result.map_err(|error| read_error.lock().unwrap().take()
        .map(symphonia::core::errors::Error::IoError).unwrap_or(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct CountedSource(Arc<std::sync::atomic::AtomicUsize>);
    impl Read for CountedSource {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::Relaxed);
            if out.is_empty() { return Ok(0); }
            out[0]=42; Ok(1)
        }
    }
    impl Seek for CountedSource {
        fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
            self.0.fetch_add(1, Ordering::Relaxed); Ok(0)
        }
    }
    impl MediaSource for CountedSource {
        fn is_seekable(&self) -> bool { self.0.fetch_add(1, Ordering::Relaxed); true }
        fn byte_len(&self) -> Option<u64> { self.0.fetch_add(1, Ordering::Relaxed); Some(1) }
    }
    fn counted_source() -> (ProbeSource, Arc<std::sync::atomic::AtomicUsize>) {
        let count=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (ProbeSource { source:Box::new(CountedSource(count.clone())), probing:Arc::new(AtomicBool::new(false)),
            read_error:Arc::new(Mutex::new(None)),capture_error:Arc::new(AtomicBool::new(false)) },count)
    }
    #[test]
    fn callback_scope_refuses_unowned_effects_and_restores_nested_calls() {
        let (mut source,count)=counted_source();
        assert_eq!(source.read(&mut [0]).unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
        assert_eq!(source.seek(SeekFrom::Start(0)).unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
        assert_eq!(count.load(Ordering::Relaxed),0);
        let blocking=crate::rt::Blocking::dedicated_thread();
        crate::audio::callback::with(&blocking, || {
            assert_eq!(source.read(&mut [0]).unwrap(),1);
            crate::audio::callback::with(&blocking, || { assert_eq!(source.seek(SeekFrom::Start(0)).unwrap(),0); });
            assert!(source.is_seekable()); assert_eq!(source.byte_len(),Some(1));
            let ui=crate::rt::Ui::new().unwrap();
            assert_eq!(ui.block_on(async {source.read(&mut [0]).unwrap_err()}).kind(),std::io::ErrorKind::WouldBlock);
            assert_eq!(source.read(&mut [0]).unwrap(),1);
        });
        assert_eq!(source.read(&mut [0]).unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
        assert_eq!(count.load(Ordering::Relaxed),5);
    }
    #[test]
    fn callback_permission_cannot_escape_in_future_or_send_reader() {
        use std::future::Future;
        let blocking=crate::rt::Blocking::dedicated_thread();
        let (mut source,count)=counted_source();
        let future=crate::audio::callback::with(&blocking, || async move {source.read(&mut [0])});
        let mut future=std::pin::pin!(future);
        let mut context=std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(future.as_mut().poll(&mut context),std::task::Poll::Ready(Err(error)) if error.kind()==std::io::ErrorKind::WouldBlock));
        assert_eq!(count.load(Ordering::Relaxed),0);
        let (mut source,count)=counted_source();
        // Spawn while authorized: the other thread must not inherit this scope.
        let mut returned=crate::audio::callback::with(&blocking, || std::thread::spawn(move || {
            assert_eq!(source.read(&mut [0]).unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
            let owner=crate::rt::Blocking::dedicated_thread();
            crate::audio::callback::with(&owner, || assert_eq!(source.read(&mut [0]).unwrap(),1));
            source
        }).join().unwrap());
        assert_eq!(returned.read(&mut [0]).unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
        assert_eq!(count.load(Ordering::Relaxed),1);
    }
    #[cfg(panic = "unwind")]
    #[test]
    fn callback_metadata_misuse_and_unwind_restore_admission() {
        let (mut source,count)=counted_source();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||source.byte_len())).is_err());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(||source.is_seekable())).is_err());
        assert_eq!(count.load(Ordering::Relaxed),0);
        let blocking=crate::rt::Blocking::dedicated_thread();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::audio::callback::with(&blocking, || {
                crate::audio::callback::with(&blocking, || {source.read(&mut [0]).unwrap(); panic!("generated callback unwind");});
            });
        })).is_err());
        assert_eq!(source.read(&mut [0]).unwrap_err().kind(),std::io::ErrorKind::WouldBlock);
        assert_eq!(count.load(Ordering::Relaxed),1);
    }

    struct Failed(bool);
    impl Read for Failed {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(if self.0 { std::io::Error::other(super::super::source::Cut) }
                else { std::io::Error::other("Radio connection failed (ProxyConnect)") })
        }
    }
    impl Seek for Failed {
        fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> { unreachable!() }
    }
    impl MediaSource for Failed {
        fn is_seekable(&self) -> bool { false }
        fn byte_len(&self) -> Option<u64> { None }
    }
    #[test]
    fn probe_preserves_transport_failure_and_typed_interrupt() {
        for interrupted in [false, true] {
            let error = match probe(&crate::rt::Blocking::dedicated_thread(), Box::new(Failed(interrupted)), None, true) {
                Err(symphonia::core::errors::Error::IoError(error)) => error,
                _ => panic!("probe must preserve the read error"),
            };
            assert_eq!(super::super::source::is_cut(&error), interrupted);
            if !interrupted { assert_eq!(error.to_string(), "Radio connection failed (ProxyConnect)"); }
        }
    }
    #[test]
    fn probe_rejects_executor_before_read_but_allows_finite_worker() {
        struct Counted(Arc<std::sync::atomic::AtomicUsize>);
        impl Read for Counted {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Err(std::io::Error::other("generated read reached"))
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> { unreachable!() }
        }
        impl MediaSource for Counted {
            fn is_seekable(&self) -> bool { false }
            fn byte_len(&self) -> Option<u64> { None }
        }
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let blocking = crate::rt::Blocking::dedicated_thread();
        let ui = crate::rt::Ui::new().unwrap();
        let result = ui.block_on(async { probe(&blocking, Box::new(Counted(reads.clone())), None, true) });
        assert!(matches!(result, Err(symphonia::core::errors::Error::IoError(ref error)) if error.kind() == std::io::ErrorKind::WouldBlock));
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        let observed = reads.clone();
        let error = ui.block_on(crate::rt::jobs().run_isolated(move |blocking| {
            match probe(blocking, Box::new(Counted(observed)), None, true) {
                Err(symphonia::core::errors::Error::IoError(error)) => error.to_string(),
                _ => panic!("probe must preserve generated read error"),
            }
        })).unwrap();
        assert_eq!(error, "generated read reached");
        assert!(reads.load(Ordering::Relaxed) > 0);
    }

}

pub fn needs_complete_file(error: &symphonia::core::errors::Error) -> bool {
    matches!(error,symphonia::core::errors::Error::Unsupported("isomp4: missing moov atom"))
}

/// Borrowed DSP output before listener volume or PCM saturation. Integer
/// sources stay exact; an EQ buffer keeps its existing floating precision.
/// The gain is ReplayGain (or the explicit conversion gain), not listener volume.
#[derive(Clone,Copy)]
pub enum PreVolume<'a> {
    Integer(&'a[i32],f32),
    Float(&'a[f32],f32),
}
impl<'a> PreVolume<'a> {
    /// The values after the first `skip`, at listener `volume` and limited
    /// to the i32 range, appended to `audible`: `values()` as two plain
    /// loops that fill the caller's buffer.
    pub fn extend_audible(self,skip:usize,volume:f64,audible:&mut Vec<f64>) {
        let limit=|value:f64|value.clamp(f64::from(i32::MIN),f64::from(i32::MAX));
        match self {
            Self::Integer(values,gain)=>{
                let gain=f64::from(gain);
                audible.extend(values.iter().skip(skip).map(|&v|limit(f64::from(v)*gain*volume)));
            }
            Self::Float(values,gain)=>{
                let gain=f64::from(gain);
                audible.extend(values.iter().skip(skip).map(|&v|limit(f64::from(v)*2147483648.0*gain*volume)));
            }
        }
    }
    /// The wide codec path must apply a running volume change before clipping.
    pub fn extend_audible_ramped(
        self, skip: usize, channels: usize, ramp: &mut super::gain::GainRamp, audible: &mut Vec<f64>,
    ) {
        let limit = |value: f64| value.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        match self {
            Self::Integer(values, gain) => {
                for frame in values[skip..].chunks_exact(channels) {
                    let scale = f64::from(gain) * f64::from(ramp.next());
                    audible.extend(frame.iter().map(|&value| limit(f64::from(value) * scale)));
                }
            }
            Self::Float(values, gain) => {
                for frame in values[skip..].chunks_exact(channels) {
                    let scale = f64::from(gain) * f64::from(ramp.next()) * 2147483648.0;
                    audible.extend(frame.iter().map(|&value| limit(f64::from(value) * scale)));
                }
            }
        }
    }
    #[cfg(test)]
    pub fn values(self)->impl Iterator<Item=f64> + 'a {
        let (integers,floats,gain):(&'a[i32],&'a[f32],f32)=match self {
            Self::Integer(values,gain)=>(values,&[],gain),
            Self::Float(values,gain)=>(&[],values,gain),
        };
        integers.iter().map(|&v|f64::from(v)).chain(floats.iter().map(|&v|f64::from(v)*2147483648.0))
            .map(move |v|v*f64::from(gain))
    }
    pub fn integer(self)->Option<&'a[i32]> {
        match self {Self::Integer(values,1.0)=>Some(values),_=>None}
    }
}

#[derive(Default)]
enum SourceBuffer { #[default] Integer, Equalized, SpokenInteger, SpokenEqualized }

/// Reused interleaved buffers; they grow to the largest packet and are not
/// reallocated per packet. The arithmetic is in `pcm_convert.rs`.
#[derive(Default)]
pub struct Samples {
    /// Interleaved f32 after the equalizer.
    floats: Vec<f32>,
    eq_output: Vec<i32>,
    spoken_floats: Vec<f32>,
    spoken_original: Vec<i32>,
    source: SourceBuffer,
    source_gain: f32,
    using_eq: bool,
    /// Interleaved S32 at unity gain.
    buffer: Vec<i32>,
    scalar_only: bool,
}
impl Samples {
    /// Conversion that never uses the vector kernels (comparison runs).
    pub fn scalar_only() -> Self { Self { scalar_only: true, ..Self::default() } }
    fn vector(&self) -> bool { !self.scalar_only && pcm_convert::vector() }
    /// Whether this CPU runs the vector kernels, and whether each of them
    /// equals the scalar code on generated input (benchmark self-check).
    pub fn vector_check() -> (bool, bool) { (pcm_convert::vector(), pcm_convert::vector_matches_scalar()) }
    pub fn convert(&mut self, decoded: AudioBufferRef<'_>, gain:f32) -> &[i32] {
        self.using_eq = false;
        self.source = SourceBuffer::Integer;
        self.source_gain = gain;
        let channels = decoded.spec().channels.count();
        let count = decoded.frames() * channels;
        let vector = self.vector();
        let scaled = (gain-1.0).abs()>1e-4;
        self.buffer.resize(count, 0);
        if scaled { self.eq_output.resize(count, 0); }
        let unity = &mut self.buffer[..];
        let gained = scaled.then(|| (&mut self.eq_output[..], gain));
        // Stereo f32 (MP3, AAC, Vorbis) and S32 (FLAC, ALAC) convert, interleave
        // and scale in one pass; the rest scale in a second.
        let gained = match (&decoded, channels) {
            (AudioBufferRef::F32(planar), 2) => { pcm_convert::stereo_f32(vector, planar.chan(0), planar.chan(1), unity, gained); None }
            (AudioBufferRef::S32(planar), 2) => { pcm_convert::stereo_s32(vector, planar.chan(0), planar.chan(1), unity, gained); None }
            (AudioBufferRef::F32(planar), 1) => { pcm_convert::mono_f32(vector, planar.chan(0), unity); gained }
            (AudioBufferRef::S32(planar), 1) => { unity.copy_from_slice(planar.chan(0)); gained }
            _ => { pcm_convert::interleave(&decoded, unity); gained }
        };
        if let Some((out, gain)) = gained { pcm_convert::gain(vector, &self.buffer, out, gain); }
        if scaled {
            self.using_eq = true;
            &self.eq_output
        } else {
            &self.buffer
        }
    }
    /// Interleaved f32 in +/-1 for the equalizer.
    fn fill_floats(&mut self, decoded: &AudioBufferRef<'_>) {
        let channels = decoded.spec().channels.count();
        let vector = self.vector();
        self.floats.resize(decoded.frames() * channels, 0.0);
        let floats = &mut self.floats[..];
        match (decoded, channels) {
            (AudioBufferRef::F32(planar), 2) => pcm_convert::stereo_f32_floats(vector, planar.chan(0), planar.chan(1), floats),
            (AudioBufferRef::S32(planar), 2) => pcm_convert::stereo_s32_floats(vector, planar.chan(0), planar.chan(1), floats),
            _ => pcm_convert::interleave(decoded, floats),
        }
    }
    pub fn convert_eq(&mut self, decoded: AudioBufferRef<'_>, gain: f32, eq: &mut super::eq::Equalizer) {
        let spec = *decoded.spec();
        eq.prepare(spec.rate, spec.channels.count());
        if eq.bypass() { self.convert(decoded,gain); return; }
        self.source=SourceBuffer::Equalized;
        self.source_gain=gain;
        self.fill_floats(&decoded);
        eq.process(&mut self.floats);
        self.eq_output.resize(self.floats.len(), 0);
        pcm_convert::floats_pcm(self.vector(), &self.floats, &mut self.eq_output, gain as f64 * 2147483648.0);
        self.using_eq=true;
    }
    pub fn convert_eq_ramped(&mut self, decoded: AudioBufferRef<'_>, track_gain: f32,
        volume: f32, eq: &mut super::eq::Equalizer,
        ramp: &mut super::gain::GainRamp, skip: usize) {
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
            self.eq_output.extend_from_slice(&self.buffer);
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
        self.fill_floats(&decoded);
        eq.process(&mut self.floats);
        self.eq_output.clear();
        for (index, frame) in self.floats.chunks_exact(channels).enumerate() {
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
        eq: &mut super::eq::Equalizer, ramp: &mut super::gain::GainRamp,
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
        if self.using_eq { &self.eq_output } else { &self.buffer }
    }
    pub fn pre_volume(&self)->PreVolume<'_> {
        match self.source {
            SourceBuffer::Integer=>PreVolume::Integer(&self.buffer,self.source_gain),
            SourceBuffer::Equalized=>PreVolume::Float(&self.floats,self.source_gain),
            SourceBuffer::SpokenInteger=>PreVolume::Integer(&self.spoken_original,self.source_gain),
            SourceBuffer::SpokenEqualized=>PreVolume::Float(&self.spoken_floats,self.source_gain),
        }
    }
}

#[cfg(test)]
mod reference_tests;
#[cfg(test)]
mod corrupt_tests;
#[cfg(test)]
mod budget_tests;

#[cfg(test)]
mod eq_tests {
    use super::*;
    use symphonia::core::audio::{AudioBuffer,Channels,SignalSpec};
    use std::borrow::Cow;
    #[test]
    fn audible_values_fill_a_reused_buffer_exactly_as_the_iterator_did() {
        let integers:Vec<i32>=(0..97).map(|n|(n as i32-48).wrapping_mul(44_739_243)).chain([i32::MIN,i32::MAX,0]).collect();
        let floats:Vec<f32>=(0..97).map(|n|((n as f32)*0.37).sin()*1.3).chain([1.0,-1.0,0.0,4.0]).collect();
        let mut audible=Vec::new();
        for gain in [1.0f32,0.5,0.31622776,2.0,0.0] {
            for volume in [1.0f64,0.251188643,0.003349654] {
                for source in [PreVolume::Integer(&integers,gain),PreVolume::Float(&floats,gain)] {
                    for skip in [0,3,100,1000] {
                        let reference:Vec<u64>=source.values().skip(skip).map(|value|
                            (value*volume).clamp(f64::from(i32::MIN),f64::from(i32::MAX)).to_bits()).collect();
                        audible.clear();
                        source.extend_audible(skip,volume,&mut audible);
                        assert_eq!(audible.iter().map(|value|value.to_bits()).collect::<Vec<_>>(),reference,"gain {gain} volume {volume} skip {skip}");
                    }
                }
            }
        }
    }
    #[test]
    fn pre_volume_preserves_integer_detail_through_mute_and_boost() {
        let mut pcm=AudioBuffer::<i32>::new(4,SignalSpec::new(48000,Channels::FRONT_LEFT));
        pcm.render_silence(None);
        pcm.chan_mut(0).copy_from_slice(&[i32::MIN,i32::MAX,1,-1]);
        let mut samples=Samples::default();
        let mut eq=super::super::eq::Equalizer::new(Default::default());
        for volume in [0.0,0.1,1.0] {
            let mut ramp=super::super::gain::GainRamp::default();ramp.start(volume,48000);
            samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&pcm)),2.0,volume,&mut eq,&mut ramp,0);
            assert_eq!(samples.pre_volume().values().collect::<Vec<_>>(),
                pcm.chan(0).iter().map(|&v|f64::from(v)*2.0).collect::<Vec<_>>());
        }
        let mut spoken=vec![i32::MIN,i32::MAX,1,-1];
        samples.process_spoken_output(&mut spoken,48000,1,&mut eq,
            &mut super::super::gain::GainRamp::default(),1.0,0.0);
        assert_eq!(samples.pre_volume().integer().unwrap(),pcm.chan(0));
        assert_eq!(spoken,[0;4]);
    }

    #[test]
    fn spoken_output_eq_keeps_transient_headroom_until_the_final_gain() {
        let mut settings = super::super::eq::Settings::default();
        settings.graphic_enabled = true; settings.bands[5] = 12;
        let mut impulse = vec![0.0f32; 4096]; impulse[0] = 1.0;
        let mut response = super::super::eq::Equalizer::new(settings.clone());
        response.prepare(8000,1); response.process(&mut impulse);
        // Reverse-sign excitation attains the filter's bounded L1 peak. A
        // frequency-response headroom reserve is not a transient hard limit.
        let pcm: Vec<i32> = impulse.iter().rev().map(|&sample| if sample >= 0.0 { 32767i32 << 16 } else { i32::MIN }).collect();
        let mut expected_float: Vec<f32> = pcm.iter().map(|&sample|sample as f32 / 2147483648.0).collect();
        let mut reference = super::super::eq::Equalizer::new(settings.clone());
        reference.prepare(8000,1); reference.process(&mut expected_float);
        let peak = expected_float.iter().map(|value|value.abs()).fold(0.0f32,f32::max);
        assert!(peak > 1.0, "fixture must expose intermediate clipping: {peak}");
        let volume = 0.1f32;
        let expected: Vec<_> = expected_float.iter().map(|&sample|
            (sample as f64 * volume as f64 * 2147483648.0) as i32).collect();
        let mut actual = pcm.clone(); let mut eq = super::super::eq::Equalizer::new(settings);
        Samples::default().process_spoken_output(&mut actual,8000,1,&mut eq,
            &mut super::super::gain::GainRamp::default(),1.0,volume);
        assert_eq!(actual, expected);
        let mut negative: Vec<_> = expected_float.iter().map(|&sample|
            (sample as f64 * 2147483648.0).clamp(i32::MIN as f64,i32::MAX as f64) as i32).collect();
        super::super::gain::GainRamp::default().apply_pcm(&mut negative,1,1.0,volume,8000);
        assert_ne!(negative, expected, "clamping before final attenuation must fail this fixture");
    }

    #[test]
    fn reopening_output_at_zero_never_emits_the_old_loud_gain() {
        let mut pcm = AudioBuffer::<i32>::new(256,
            SignalSpec::new(48000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT));
        pcm.render_silence(None);
        pcm.chan_mut(0).fill(i32::MAX);
        pcm.chan_mut(1).fill(i32::MIN);
        let mut samples = Samples::default();
        let mut eq = super::super::eq::Equalizer::new(Default::default());
        let mut ramp = super::super::gain::GainRamp::default();
        ramp.start(1.0, 48000);
        samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&pcm)),
            1.0, 1.0, &mut eq, &mut ramp, 0);
        assert_eq!(&samples.samples()[..2], &[i32::MAX, i32::MIN]);
        // PCM was released; the listener selected mute before starting again.
        ramp.start(0.0, 48000);
        samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&pcm)),
            1.0, 0.0, &mut eq, &mut ramp, 0);
        assert!(samples.samples().iter().all(|&sample| sample == 0));
    }
    #[test]
    fn volume_ramp_links_channels_survives_packets_and_ignores_seek_discard() {
        let spec = SignalSpec::new(48000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        let mut pcm = AudioBuffer::<i32>::new(256, spec);
        pcm.render_silence(None);
        pcm.chan_mut(0).fill(1_000_000_000);
        pcm.chan_mut(1).fill(-1_000_000_000);
        for equalized in [false, true] {
            let mut settings = super::super::eq::Settings::default();
            settings.graphic_enabled = equalized;
            settings.bands[5] = 1;
            let mut eq = super::super::eq::Equalizer::new(settings);
            let mut samples = Samples::default();
            let mut ramp = super::super::gain::GainRamp::default();
            ramp.target(1.0, 48000);
            let mut expected = super::super::gain::GainRamp::default();
            expected.target(1.0, 48000);
            expected.target(0.1, 48000);
            for skip in [17, 0] {
                samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&pcm)),
                    1.0, 0.1, &mut eq, &mut ramp, skip);
                for frame in samples.samples()[skip*2..].chunks_exact(2) {
                    assert!((frame[0] as i64 + frame[1] as i64).abs() <= 1);
                    let gain = expected.next();
                    if !equalized {
                        assert!((frame[0] as f64 / 1e9 - gain as f64).abs() < 1e-8);
                    }
                }
                assert_eq!(ramp.next(), expected.next());
            }
        }
    }
    #[test]
    fn bypass_preserves_pcm_and_eq_saturates_after_replaygain() {
        let mut pcm=AudioBuffer::<i32>::new(8,SignalSpec::new(48000,Channels::FRONT_LEFT));
        pcm.render_silence(None);
        pcm.chan_mut(0).copy_from_slice(&[i32::MIN,i32::MAX,0,1,-1,12345678,-12345678,42]);
        let mut samples=Samples::default();
        let mut eq=super::super::eq::Equalizer::new(Default::default());
        samples.convert_eq(AudioBufferRef::S32(Cow::Borrowed(&pcm)),1.0,&mut eq);
        assert_eq!(samples.samples(),pcm.chan(0));
        let mut settings=super::super::eq::Settings::default();
        settings.graphic_enabled=true;settings.bands[5]=6;
        eq.set(settings);eq.reset();
        let mut loud=AudioBuffer::<f32>::new(1024,SignalSpec::new(48000,Channels::FRONT_LEFT));
        loud.render_silence(None);loud.chan_mut(0).fill(4.0);
        samples.convert_eq(AudioBufferRef::F32(Cow::Borrowed(&loud)),2.0,&mut eq);
        assert_eq!(*samples.samples().last().unwrap(),i32::MAX);
        eq.set(Default::default());eq.reset();
        // Returning to bypass after a channel/rate change must recreate buffers.
        let mut stereo=AudioBuffer::<i32>::new(8,SignalSpec::new(8000,Channels::FRONT_LEFT|Channels::FRONT_RIGHT));
        stereo.render_silence(None);stereo.chan_mut(0).fill(17);stereo.chan_mut(1).fill(-29);
        samples.convert_eq(AudioBufferRef::S32(Cow::Borrowed(&stereo)),1.0,&mut eq);
        assert_eq!(samples.samples(),[17,-29].repeat(8));
    }
}
