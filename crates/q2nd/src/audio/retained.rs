#![forbid(unsafe_code)]
//! Shadow only the PCM that has not yet been heard. Device shutdown must not
//! require closing the decoder or refetching the compressed stream.
use std::collections::VecDeque;

/// Shadow samples stay before the device attenuator, so changing that gain
/// affects all unheard frames without modifying their source representation.
/// Ordinary writes remain i32; transformed replay keeps fractional PCM values.
enum Shadow {
    Integer(VecDeque<i32>),
    Precise(VecDeque<f64>),
}
pub(super) struct Tail(Shadow, Option<VolumeCeiling>);

/// Older frames can still be louder after a software-only running decrease.
/// Count accepted steady-gain samples until they cover the complete unheard tail.
struct VolumeCeiling {
    highest: f32,
    target: f32,
    settled_samples: Option<usize>,
}
impl Default for Tail {
    fn default() -> Self { Self(Shadow::Integer(VecDeque::new()), None) }
}
impl Tail {
    #[cfg(test)]
    pub fn append(&mut self, samples: &[i32]) { self.append_output(samples,1.0,None); }
    pub fn append_output(&mut self, samples: &[i32], output_gain: f64, audible: Option<&[f64]>) {
        if let Some(values)=audible { assert_eq!(samples.len(),values.len()); }
        if let Some(settled) = self.1.as_mut().and_then(|ceiling| ceiling.settled_samples.as_mut()) {
            *settled = settled.saturating_add(samples.len());
        }
        if let (Shadow::Integer(values),None)=(&mut self.0,audible) {
            values.extend(samples.iter().copied());
            return;
        }
        if matches!(self.0,Shadow::Integer(_)) {
            let Shadow::Integer(values)=std::mem::replace(&mut self.0,Shadow::Precise(VecDeque::new())) else {unreachable!()};
            self.0=Shadow::Precise(values.into_iter().map(f64::from).collect());
        }
        let Shadow::Precise(values)=&mut self.0 else {unreachable!()};
        if let Some(audible)=audible {
            // One reciprocal for the packet, not a division for each sample.
            let inverse=if output_gain==0.0 {0.0}else{1.0/output_gain};
            values.extend(audible.iter().map(|&sample|sample*inverse));
        } else {values.extend(samples.iter().map(|&sample|f64::from(sample)));}
    }
    fn len(&self)->usize {
        match &self.0 {Shadow::Integer(values)=>values.len(),Shadow::Precise(values)=>values.len()}
    }
    pub fn retain(&mut self, frames:u64, channels:u32) {
        let keep=usize::try_from(frames).unwrap_or(usize::MAX).saturating_mul(channels as usize);
        let remove=self.len().saturating_sub(keep);
        match &mut self.0 {
            Shadow::Integer(values)=>{values.drain(..remove);},
            Shadow::Precise(values)=>{values.drain(..remove);},
        }
        let remaining = self.len();
        if self.1.as_ref().and_then(|ceiling| ceiling.settled_samples).is_some_and(|settled| settled >= remaining) {
            self.1 = None;
        }
        if remaining == 0 && matches!(self.0, Shadow::Precise(_)) {
            self.0 = Shadow::Integer(VecDeque::new());
        }
    }
    #[cfg(test)]
    pub fn snapshot(&self, frames:u64, channels:u32)->Option<Vec<i32>> {
        self.retained(frames,48000,channels,1.0).map(|held|held.samples)
    }
    pub fn retained(&self,frames:u64,rate:u32,channels:u32,output_gain:f64)->Option<RetainedPcm> {
        let count=usize::try_from(frames).ok()?.checked_mul(channels as usize)?;
        if count>self.len() {return None;}
        let skip=self.len()-count;
        match &self.0 {
            Shadow::Integer(values)=>Some(RetainedPcm::at_output_gain(values.iter().skip(skip).copied().collect(),rate,channels,output_gain)),
            Shadow::Precise(values)=>RetainedPcm::from_audible(values.iter().skip(skip).map(|&v|v*output_gain).collect(),rate,channels,output_gain).ok(),
        }
    }
    /// Inspect the unheard suffix in place. Gain planning must not allocate
    /// and render a complete retained queue merely to find its peak.
    pub fn required_output_gain(&self,frames:u64,channels:u32,output_gain:f64)->Option<f64> {
        let count=usize::try_from(frames).ok()?.checked_mul(channels as usize)?;
        if count>self.len() {return None;}
        let skip=self.len()-count;
        let (low,high)=match &self.0 {
            Shadow::Integer(values)=>extremes(values.iter().skip(skip).map(|&v|f64::from(v))),
            Shadow::Precise(values)=>extremes(values.iter().skip(skip).copied()),
        };
        Some(needed_gain(low*output_gain,high*output_gain))
    }
    pub fn frames(&self,channels:u32)->u64 {(self.len()/channels as usize) as u64}
    pub fn defer_running_volume(&mut self, previous: f32, target: f32) {
        let highest = self.1.as_ref().map_or(previous, |ceiling| ceiling.highest.max(previous)).max(target);
        self.1 = Some(VolumeCeiling { highest, target, settled_samples: None });
    }

    pub fn settled_running_volume(&mut self) {
        if let Some(ceiling) = &mut self.1 {
            ceiling.settled_samples.get_or_insert(0);
        }
    }

    pub fn scale_running_volume_ceiling(&mut self, target: f32) {
        if target == 0.0 {
            self.1 = None;
        } else if let Some(ceiling) = &mut self.1 {
            ceiling.highest *= target / ceiling.target;
            ceiling.target = target;
        }
    }

    pub fn running_volume_ceiling(&self) -> Option<(f32, f32)> {
        self.1.as_ref().map(|ceiling| (ceiling.highest, ceiling.target))
    }

    pub fn clear(&mut self) {
        self.1 = None;
        match &mut self.0 {Shadow::Integer(values)=>values.clear(),Shadow::Precise(_)=>*self=Self::default()}
    }
}

/// The lowest and highest value, each 0.0 when no value lies beyond zero.
/// NaN compares false and is passed over, as `f64::max` passes over it.
/// Comparisons, not `f64::min`/`max`: those are library calls on the Q2.
fn extremes(values:impl Iterator<Item=f64>)->(f64,f64) {
    let (mut low,mut high)=(0.0f64,0.0f64);
    for value in values {
        if value<low {low=value;}
        if value>high {high=value;}
    }
    (low,high)
}
/// Smallest attenuator gain that represents audible values from `low` to
/// `high`. The per-sample requirement rises with the sample's magnitude on
/// each side of zero, so the two extremes decide it: two divisions a packet.
fn needed_gain(low:f64,high:f64)->f64 {
    let positive=if high>0.0 {high/f64::from(i32::MAX)}else{0.0};
    let negative=if low<0.0 {low/f64::from(i32::MIN)}else{0.0};
    if negative>positive {negative}else{positive}
}
/// `pcm.round() as i32` for a finite value within the i32 range (which
/// `render_into` has checked): halves round away from zero. `f64::round` is
/// a library call on the Q2; this is a conversion and two comparisons.
#[inline]
fn round_pcm(pcm:f64)->i32 {
    let whole=pcm as i32;
    let rest=pcm-f64::from(whole);
    whole.wrapping_add(i32::from(rest>=0.5)).wrapping_sub(i32::from(rest<=-0.5))
}

#[derive(Clone)]
pub struct RetainedPcm {
    gain_ceiling: Option<f32>,
    /// Exact audible values before the most recent route quantization. None
    /// means the original integer samples at output_gain are authoritative.
    audible: Option<Vec<f64>>,
    output_gain: f64,
    pub samples: Vec<i32>,
    pub format: super::pcm_format::PcmFormat,
    pub rate: u32,
    pub channels: u32,
}
impl RetainedPcm {
    pub fn new(samples: Vec<i32>, rate: u32, channels: u32) -> Self {
        Self::at_output_gain(samples, rate, channels, 1.0)
    }
    pub fn at_output_gain(samples: Vec<i32>, rate: u32, channels: u32, output_gain: f64) -> Self {
        assert!(output_gain.is_finite() && (0.0..=1.0).contains(&output_gain));
        Self { format:Default::default(), samples, rate, channels, gain_ceiling: None, audible: None, output_gain }
    }
    pub fn with_format(mut self,format:super::pcm_format::PcmFormat)->Self {self.format=format;self}
    /// Render `audible` in the `output_gain` domain into `samples`, reusing
    /// its allocation. A refusal leaves `samples` as it was: the values are
    /// checked first (a scan for the extremes), then written with one
    /// multiplication by the gain's reciprocal for each sample.
    fn render_into(audible: &[f64], output_gain: f64, samples: &mut Vec<i32>) -> Result<(), String> {
        if !output_gain.is_finite() || !(0.0..=1.0).contains(&output_gain) {
            return Err("Invalid PCM output gain".into());
        }
        let (mut low,mut high,mut numbers)=(0.0f64,0.0f64,true);
        for &value in audible {
            if value<low {low=value;}
            if value>high {high=value;}
            numbers&=!value.is_nan();
        }
        // At gain 0 only silence is representable.
        let inverse=if output_gain==0.0 {if low==0.0 && high==0.0 {0.0}else{f64::INFINITY}}else{1.0/output_gain};
        // Permit only sub-LSB arithmetic noise at the integer endpoints.
        // A gain-domain change must never silently saturate a louder tail.
        // (Infinite products fail these comparisons.)
        if !numbers || low*inverse < f64::from(i32::MIN)-0.00001 || high*inverse > f64::from(i32::MAX)+0.00001 {
            return Err("Retained PCM exceeds output headroom".into());
        }
        samples.clear();
        samples.extend(audible.iter().map(|&value|round_pcm(value*inverse)));
        Ok(())
    }
    fn render(audible: &[f64], output_gain: f64) -> Result<Vec<i32>, String> {
        let mut samples=Vec::new();
        Self::render_into(audible,output_gain,&mut samples).map(|()|samples)
    }
    pub fn from_audible(audible: Vec<f64>, rate: u32, channels: u32, output_gain: f64) -> Result<Self, String> {
        let samples = Self::render(&audible, output_gain)?;
        Ok(Self { format:Default::default(), samples, rate, channels, gain_ceiling: None, audible: Some(audible), output_gain })
    }
    /// `from_audible_near` that renders into `samples`, whose allocation the
    /// player keeps from the packet before (`into_buffers`).
    pub fn from_audible_near_in(audible: Vec<f64>, mut samples: Vec<i32>, rate: u32, channels: u32, preferred: f64) -> Result<Self, String> {
        let output_gain=if preferred != 1.0 && Self::render_into(&audible,preferred,&mut samples).is_ok() {preferred}
            else {Self::render_into(&audible,1.0,&mut samples)?;1.0};
        Ok(Self { format:Default::default(), samples, rate, channels, gain_ceiling: None, audible: Some(audible), output_gain })
    }
    /// The audible and sample buffers of a block that has been written, for
    /// the next packet.
    pub fn into_buffers(self) -> (Vec<f64>, Vec<i32>) { (self.audible.unwrap_or_default(), self.samples) }
    /// `from_audible`, rendered once in the `preferred` gain domain when the
    /// samples fit it (the open PCM's: a later `rebase_output` to the same
    /// gain is then free), else at unity as `from_audible` does. The audible
    /// values, and the samples after a rebase to any gain, are the same either way.
    pub fn from_audible_near(audible: Vec<f64>, rate: u32, channels: u32, preferred: f64) -> Result<Self, String> {
        Self::from_audible_near_in(audible, Vec::new(), rate, channels, preferred)
    }
    /// Smallest attenuator gain that can represent these audible samples.
    pub fn required_output_gain(&self)->f64 {
        let (low,high)=match &self.audible {
            Some(values)=>extremes(values.iter().copied()),
            None=>{
                let (low,high)=extremes(self.samples.iter().map(|&v|f64::from(v)));
                (low*self.output_gain,high*self.output_gain)
            }
        };
        needed_gain(low,high)
    }
    /// A running queue follows a real volume change, whereas a paused queue
    /// keeps its earlier audible ceiling. Preserve the normalized original and
    /// move only the compensation coefficient; clipping remains after volume.
    pub fn retarget_running(&mut self,old_soft:f32,new_soft:f32,output_gain:f64)->Result<(),String> {
        if old_soft<=0.0 {return self.rebase_output(output_gain);}
        let ratio=f64::from(new_soft)/f64::from(old_soft);
        let mut audible=match &self.audible {
            Some(values) if self.output_gain>0.0=>values.iter().map(|&v|v/self.output_gain).collect(),
            _=>self.samples.iter().map(|&v|f64::from(v)).collect::<Vec<_>>(),
        };
        for value in &mut audible {*value=(*value*ratio*output_gain).clamp(f64::from(i32::MIN),f64::from(i32::MAX));}
        let samples=Self::render(&audible,output_gain)?;
        self.samples=samples;self.audible=Some(audible);self.output_gain=output_gain;self.gain_ceiling=None;
        Ok(())
    }
    pub fn output_gain(&self) -> f64 { self.output_gain }
    pub fn audible(&self) -> Option<&[f64]> { self.audible.as_deref() }
    fn audible_copy(&self) -> Vec<f64> {
        self.audible.clone().unwrap_or_else(||self.samples.iter().map(|&s|f64::from(s)*self.output_gain).collect())
    }
    /// Change the device's gain domain, preserving each frame's audible level,
    /// including older ramp frames. Refusal leaves the whole block unchanged.
    pub fn rebase_output(&mut self, output_gain: f64) -> Result<(), String> {
        if output_gain == self.output_gain { return Ok(()); }
        // Exact audible values are rendered in place, not from a copy.
        if let Some(audible) = &self.audible {
            Self::render_into(audible, output_gain, &mut self.samples)?;
            self.output_gain = output_gain;
            return Ok(());
        }
        let audible = self.audible_copy();
        let samples = Self::render(&audible, output_gain)?;
        self.samples = samples;
        self.audible = Some(audible);
        self.output_gain = output_gain;
        Ok(())
    }
    /// Apply one independent, bounded per-frame envelope in audible space.
    /// Keeping the precise representation preserves codec headroom on rebase.
    pub fn attenuate_linear(&mut self, start: f64, step: f64) {
        // In place: this runs for every packet of a sleep fade.
        let mut audible = self.audible.take().unwrap_or_else(||self.samples.iter().map(|&s|f64::from(s)*self.output_gain).collect());
        let channels = self.channels as usize;
        for (frame, values) in audible.chunks_exact_mut(channels).enumerate() {
            let gain = (start + step * frame as f64).clamp(0.0, 1.0);
            for sample in values { *sample *= gain; }
        }
        Self::render_into(&audible, self.output_gain, &mut self.samples).expect("attenuation fits PCM");
        self.audible = Some(audible);
    }
    pub fn frames(&self) -> u64 { (self.samples.len() / self.channels as usize) as u64 }
    pub fn suffix(&self, samples: usize) -> Self {
        let mut result = self.clone();
        result.discard_prefix(samples);
        result
    }
    pub fn discard_prefix(&mut self, samples: usize) {
        self.samples.drain(..samples);
        if let Some(audible) = &mut self.audible { audible.drain(..samples); }
    }
    /// Never replay a pre-pause tail louder than a newly lowered volume. An
    /// increase takes effect on newly decoded frames, as with hardware pause.
    pub fn lower_gain(&mut self, old: f32, new: f32) {
        let old = self.gain_ceiling.unwrap_or(old);
        self.gain_ceiling = Some(old.min(new));
        if old > 0.0 && new < old {
            let ratio = f64::from((new / old).clamp(0.0,1.0));
            let mut audible = self.audible_copy();
            for sample in &mut audible { *sample *= ratio; }
            // This operation only decreases an already representable level.
            self.samples = Self::render(&audible,self.output_gain).expect("attenuation fits PCM");
            self.audible = Some(audible);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn generated_audible(count:usize,peak:f64)->Vec<f64> {
        let mut x=0x2545_f491u32;
        (0..count).map(|index| {
            x^=x<<13;x^=x>>17;x^=x<<5;
            match index {0=>peak,1=>-peak,2=>0.0,3=>0.5,4=>-0.5,_=>(f64::from(x)/f64::from(u32::MAX)*2.0-1.0)*peak}
        }).collect()
    }
    /// The rendering before the buffers were reused: one division and one
    /// `round` for each sample.
    fn render_by_division(audible:&[f64],output_gain:f64)->Result<Vec<i32>,String> {
        if !output_gain.is_finite() || !(0.0..=1.0).contains(&output_gain) {return Err("Invalid PCM output gain".into());}
        audible.iter().map(|&sample| {
            let pcm=if output_gain==0.0 && sample==0.0 {0.0}else{sample/output_gain};
            if !pcm.is_finite() || pcm<f64::from(i32::MIN)-0.00001 || pcm>f64::from(i32::MAX)+0.00001 {
                return Err("Retained PCM exceeds output headroom".into());
            }
            Ok(pcm.round() as i32)
        }).collect()
    }
    #[test]
    fn rounding_without_the_library_call_equals_round() {
        let mut rng=fastrand::Rng::with_seed(0x524f554e);
        let edges=[0.0,-0.0,0.5,-0.5,0.49999999999999994,-0.49999999999999994,1.5,-1.5,2.5,-2.5,
            2147483646.5,2147483647.0,2147483647.00001,-2147483647.5,-2147483648.0,-2147483648.00001,1e-300,-1e-300];
        for value in edges.into_iter().chain((0..200_000).map(|index| {
            let whole=f64::from(rng.i32(..));
            match index%4 {0=>whole,1=>whole+0.5,2=>whole+rng.f64(),_=>(whole+rng.f64()-0.5)/65536.0}
        })).filter(|value|(-2147483648.00001..=2147483647.00001).contains(value)) {
            assert_eq!(round_pcm(value),value.round() as i32,"{value:?}");
        }
    }
    #[test]
    fn rendering_by_reciprocal_stays_within_one_step_of_division_and_refuses_the_same_blocks() {
        let mut rng=fastrand::Rng::with_seed(0x52454349);
        let mut reused=vec![7i32;5];
        for case in 0..4000 {
            let gain=match case%5 {0=>1.0,1=>0.5,2=>crate::audio::gain::codec_gain(rng.u8(..=254)),3=>rng.f64(),_=>0.0};
            // Peaks below, at and above what the gain represents.
            let peak=f64::from(i32::MAX)*gain*[0.01,0.5,0.999,1.0,1.001,2.0][case/5%6];
            let mut audible:Vec<f64>=(0..rng.usize(0..40)).map(|_|(rng.f64()*2.0-1.0)*peak).collect();
            if case%7==0 && !audible.is_empty() {
                let at=rng.usize(..audible.len());
                audible[at]=[peak,-peak,f64::NAN,f64::INFINITY,f64::NEG_INFINITY,0.0,-0.0][case/7%7];
            }
            let before=reused.clone();
            let reference=render_by_division(&audible,gain);
            match (RetainedPcm::render_into(&audible,gain,&mut reused),&reference) {
                (Ok(()),Ok(reference))=>{
                    assert_eq!(reused.len(),reference.len());
                    for (index,(&new,&old)) in reused.iter().zip(reference).enumerate() {
                        assert!((i64::from(new)-i64::from(old)).abs()<=1,"gain {gain} value {}: {new} against {old}",audible[index]);
                        // An exact reciprocal changes nothing.
                        if gain==1.0 || gain==0.5 {assert_eq!(new,old);}
                    }
                }
                (Err(_),Err(_))=>assert_eq!(reused,before,"a refusal must leave the samples alone"),
                // The two may differ only inside the 0.00001 endpoint tolerance.
                (new,old)=>{
                    let extreme=audible.iter().fold(0.0f64,|most,value|most.max(value.abs()))/gain;
                    assert!((extreme-2147483648.0).abs()<1.5,"gain {gain}: {new:?} against {:?}, extreme {extreme}",old.as_ref().map(Vec::len));
                    if new.is_err() {assert_eq!(reused,before);}
                }
            }
        }
        for gain in [-1.0,1.5,f64::NAN,f64::INFINITY] {assert!(RetainedPcm::render_into(&[0.0],gain,&mut reused).is_err());}
    }
    #[test]
    fn the_extremes_decide_the_required_gain_exactly_as_every_sample_did() {
        let mut rng=fastrand::Rng::with_seed(0x4e454544);
        let needed=|value:f64|if value>=0.0 {value/f64::from(i32::MAX)}else{value/f64::from(i32::MIN)};
        for case in 0..2000 {
            let mut audible:Vec<f64>=(0..rng.usize(0..30)).map(|_|(rng.f64()*2.0-1.0)*f64::from(i32::MAX)).collect();
            match case%6 {
                0=>audible.iter_mut().for_each(|value|*value=value.abs()),
                1=>audible.iter_mut().for_each(|value|*value=-value.abs()),
                2=>audible.push(f64::NAN),
                3=>audible.extend([f64::from(i32::MIN),f64::from(i32::MAX)]),
                _=>{}
            }
            let reference=audible.iter().map(|&value|needed(value)).fold(0.0,f64::max);
            let gain=crate::audio::gain::codec_gain(rng.u8(..=254));
            let Ok(held)=RetainedPcm::from_audible(audible.iter().map(|value|if value.is_nan() {0.0}else{*value}).collect(),48000,1,1.0) else {continue};
            let (low,high)=extremes(audible.iter().copied());
            assert_eq!(needed_gain(low,high).to_bits(),reference.to_bits(),"{audible:?}");
            // Integer samples in a gain domain: the same rule on `sample * gain`.
            let integer=RetainedPcm::at_output_gain(held.samples.clone(),48000,1,gain);
            let reference=held.samples.iter().map(|&sample|needed(f64::from(sample)*gain)).fold(0.0,f64::max);
            assert_eq!(integer.required_output_gain().to_bits(),reference.to_bits());
        }
    }
    #[test]
    fn a_reused_block_keeps_its_allocations_through_render_rebase_and_fade() {
        let audible=generated_audible(2304,f64::from(i32::MAX)*0.2);
        let mut buffers=(Vec::new(),Vec::new());
        for _ in 0..2 {
            let (mut values,samples)=buffers;
            values.clear();values.extend_from_slice(&audible);
            buffers=RetainedPcm::from_audible_near_in(values,samples,44100,2,0.25).unwrap().into_buffers();
        }
        let ((),heap)=crate::test_alloc::measure(|| {
            for _ in 0..50 {
                let (mut values,samples)=std::mem::take(&mut buffers);
                values.clear();values.extend_from_slice(&audible);
                let mut held=RetainedPcm::from_audible_near_in(values,samples,44100,2,0.25).unwrap();
                assert_eq!(held.output_gain(),0.25);
                held.attenuate_linear(0.9,-0.0001);
                held.rebase_output(0.5).unwrap();
                assert!(held.rebase_output(0.01).is_err());
                assert!(held.required_output_gain()<=0.2);
                buffers=held.into_buffers();
            }
        });
        // The refused rebase builds its error text: one small request each.
        assert_eq!((heap.allocations,heap.largest<64),(50,true),"{heap:?}");
    }
    #[test]
    fn rendering_once_at_the_open_gain_equals_unity_then_rebase() {
        // Every codec step the player can select, with audible peaks inside,
        // at and beyond what that gain can represent.
        for steps in [0u8,1,7,40,80,99,150,254] {
            let gain=crate::audio::gain::codec_gain(steps);
            for scale in [0.25,0.999,1.0,1.0001,4.0] {
                let peak=(f64::from(i32::MAX)*gain*scale).min(f64::from(i32::MAX));
                let audible=generated_audible(2304,peak);
                let near=RetainedPcm::from_audible_near(audible.clone(),44100,2,gain).unwrap();
                let mut reference=RetainedPcm::from_audible(audible.clone(),44100,2,1.0).unwrap();
                match reference.rebase_output(gain) {
                    Ok(())=>{
                        assert_eq!(near.output_gain(),gain,"steps={steps} scale={scale}");
                        assert_eq!(near.samples,reference.samples,"steps={steps} scale={scale}");
                    }
                    // Beyond this gain's headroom the unity rendering is kept, as before.
                    Err(_)=>{
                        assert_eq!(near.output_gain(),1.0);
                        assert_eq!(near.samples,reference.samples);
                    }
                }
                assert_eq!(near.audible(),Some(&audible[..]));
                assert_eq!(near.required_output_gain(),reference.required_output_gain());
                // A later headroom move lands on the same samples from either start.
                let mut moved=near.clone();
                let wider=reference.required_output_gain().max(gain).min(1.0);
                moved.rebase_output(wider).unwrap();reference.rebase_output(wider).unwrap();
                assert_eq!((&moved.samples,moved.output_gain()),(&reference.samples,reference.output_gain()));
            }
        }
    }
    #[test]
    fn refused_rebase_of_exact_audible_values_leaves_the_block_unchanged() {
        let audible=generated_audible(64,f64::from(i32::MAX));
        let mut held=RetainedPcm::from_audible(audible.clone(),48000,2,1.0).unwrap();
        let before=held.samples.clone();
        assert!(held.rebase_output(0.5).is_err());
        assert_eq!((held.samples.clone(),held.output_gain(),held.audible()),(before,1.0,Some(&audible[..])));
        // Integer samples without exact audible values still gain them on a rebase.
        let mut integer=RetainedPcm::at_output_gain(vec![1<<20,-(1<<20),0,7],48000,2,0.5);
        integer.rebase_output(1.0).unwrap();
        assert_eq!(integer.samples,[1<<19,-(1<<19),0,4]);
        assert_eq!(integer.audible(),Some(&[524288.0,-524288.0,0.0,3.5][..]));
    }
    #[test]
    fn independent_envelope_survives_codec_rebase_and_partial_tail() {
        let mut held = RetainedPcm::at_output_gain(vec![1000, -1000, 1000, -1000, 1000, -1000], 10, 2, 0.5);
        held.attenuate_linear(1.0, -0.25);
        assert_eq!(held.samples, vec![1000, -1000, 750, -750, 500, -500]);
        let audible = held.audible_copy();
        held.rebase_output(1.0).unwrap();
        assert_eq!(held.audible_copy(), audible);
        let suffix = held.suffix(2);
        assert_eq!(suffix.audible_copy(), audible[2..]);
    }
    #[test]
    fn in_place_peak_matches_retained_audio_for_every_unheard_suffix() {
        let mut rng=fastrand::Rng::with_seed(0x5045414b);
        for channels in [1,2,8] {
            let mut samples:Vec<i32>=(0..64*channels).map(|_|rng.i32(..)).collect();
            samples[..4].copy_from_slice(&[i32::MIN,i32::MAX,-1,0]);
            for gain in [0.0,0.003349654,0.1,1.0] {
                for precise in [false,true] {
                    let mut tail=Tail::default();
                    let audible:Vec<_>=samples.iter().map(|&s|f64::from(s)*gain*0.75).collect();
                    tail.append_output(&samples,gain,precise.then_some(audible.as_slice()));
                    for frames in 0..=64 {
                        let held=tail.retained(frames,48000,channels as u32,gain).unwrap();
                        assert_eq!(tail.required_output_gain(frames,channels as u32,gain),Some(held.required_output_gain()));
                    }
                    assert!(tail.required_output_gain(65,channels as u32,gain).is_none());
                    assert!(tail.required_output_gain(u64::MAX,channels as u32,gain).is_none());
                    tail.retain(17,channels as u32);
                    let held=tail.retained(17,48000,channels as u32,gain).unwrap();
                    assert_eq!(tail.required_output_gain(17,channels as u32,gain),Some(held.required_output_gain()));
                }
            }
        }
    }

    #[test]
    fn repeated_route_quantization_retains_original_integer_detail() {
        let mut rng = fastrand::Rng::with_seed(0x4741494e);
        let mut original: Vec<i32> = (0..1024).map(|_|rng.i32(..)).collect();
        original.extend([i32::MIN, i32::MAX, -1, 0, 1]);
        for gain in [0.003349654, 0.1, 0.316227766, 1.0] {
            let mut pcm = RetainedPcm::at_output_gain(original.clone(),48000,1,gain);
            for _ in 0..100 {
                pcm.rebase_output(1.0).unwrap();
                pcm.rebase_output(gain).unwrap();
                assert_eq!(pcm.samples,original,"return must not invert the rounded USB samples");
            }
        }
    }

    #[test]
    fn insufficient_destination_headroom_refuses_without_changing_ownership() {
        let original = vec![i32::MAX, i32::MIN, 17, -19];
        let mut pcm = RetainedPcm::new(original.clone(),48000,2);
        for gain in [0.1,0.0,-1.0,f64::NAN,f64::INFINITY] {
            assert!(pcm.rebase_output(gain).is_err());
            assert_eq!(pcm.samples,original);
            assert_eq!(pcm.output_gain(),1.0);
            assert!(pcm.audible().is_none());
        }
        pcm.lower_gain(1.0,0.0);
        pcm.rebase_output(0.0).unwrap();
        pcm.rebase_output(0.1).unwrap();
        assert_eq!(pcm.samples,[0;4],"a muted retained tail stays muted");
    }

    #[test]
    fn route_round_trip_keeps_lowered_ceiling_and_frame_alignment() {
        let original = vec![200000000,-200000000,100000001,-100000001,300000001,-300000001];
        let mut pcm = RetainedPcm::new(original,8000,2);
        pcm.lower_gain(1.0,0.25);
        let expected = pcm.samples.clone();
        pcm.rebase_output(0.1).unwrap();
        pcm.lower_gain(0.25,1.0);
        pcm.lower_gain(1.0,0.5);
        let mut suffix = pcm.suffix(2);
        suffix.rebase_output(1.0).unwrap();
        assert_eq!(suffix.samples,expected[2..]);
        assert_eq!(suffix.frames(),2);
    }

    #[test]
    fn seeded_write_consume_pause_traces_keep_exact_unheard_frames() {
        let mut rng = fastrand::Rng::with_seed(0x5041555345);
        for channels in 1..=8 {
            let mut tail = Tail::default();
            let mut reference = VecDeque::new();
            let mut next = 0i32;
            for _ in 0..2000 {
                let n = rng.usize(0..512) * channels;
                let packet: Vec<_> = (0..n)
                    .map(|_| {
                        next = next.wrapping_add(1);
                        next
                    })
                    .collect();
                tail.append(&packet);
                reference.extend(packet);
                let consumed = rng.usize(0..=reference.len() / channels) * channels;
                reference.drain(..consumed);
                let frames = (reference.len() / channels) as u64;
                tail.retain(frames, channels as u32);
                assert_eq!(
                    tail.snapshot(frames, channels as u32).unwrap(),
                    reference.iter().copied().collect::<Vec<_>>()
                );
                assert!(tail.snapshot(frames + 1, channels as u32).is_none());
            }
        }
    }
    #[test]
    fn lower_volume_cannot_replay_louder_tail() {
        let mut p = RetainedPcm::new(vec![i32::MIN, -100, 0, 100, i32::MAX], 48000, 1);
        p.lower_gain(1.0, 0.5);
        assert_eq!(p.samples[1..4], [-50, 0, 50]);
        let held = p.samples.clone();
        p.lower_gain(0.5, 1.0);
        assert_eq!(p.samples, held);
        p.lower_gain(1.0, 0.75);
        assert_eq!(p.samples, held);
        p.lower_gain(0.75, 0.0);
        assert!(p.samples.iter().all(|s| *s == 0));
    }
}
