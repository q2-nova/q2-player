#![forbid(unsafe_code)]
//! Transport width is independent of the precision used by DSP and retention.
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub enum PcmFormat { S16, #[default] S32 }
impl PcmFormat {
    pub fn for_source(bits:Option<u32>)->Self {
        match bits {Some(1..=16)=>Self::S16,_=>Self::S32}
    }
    pub(super) fn alsa(self)->i32 {match self {Self::S16=>2,Self::S32=>10}}
}

/// Quantize once per submitted block, then reuse those bytes across short writes
/// and ALSA recoveries. Exact S16-grid blocks bypass dither (including silence).
/// Other blocks use independent TPDF dither and nearest rounding; arithmetic
/// stays wide so the positive endpoint cannot wrap into negative full scale.
pub struct Quantizer { random:u32 }
impl Default for Quantizer {fn default()->Self {Self {random:0x5132_5043}}}
impl Quantizer {
    fn uniform(&mut self)->i32 {
        let mut x=self.random;x^=x<<13;x^=x>>17;x^=x<<5;self.random=x;
        (x&65535) as i32
    }
    /// `((sample + dither + 32768) >> 16)` clamped to i16, without 64-bit
    /// arithmetic (the Q2 is a 32-bit CPU). `offset` is dither + 32768, within
    /// -32767..=98303. Where the 32-bit sum saturates, the exact sum's shift
    /// lies outside i16 on the same side and clamps to the same endpoint;
    /// elsewhere the sum is exact and its shift is already an i16.
    fn quantize(sample:i32,offset:i32)->i16 {(sample.saturating_add(offset)>>16) as i16}
    pub fn render(&mut self,samples:&[i32],out:&mut Vec<i16>) {
        out.clear();
        // Skip speculative packing when the first two stereo frames (or four
        // mono samples) already contain a fractional value. Otherwise validate
        // during packing so exact PCM needs only one full source pass.
        if samples.iter().take(4).all(|sample| sample & 65535 == 0) {
            let mut bits = 0;
            out.extend(samples.iter().map(|&sample| {
                bits |= sample;
                (sample >> 16) as i16
            }));
            if bits & 65535 == 0 { return; }
            out.clear();
        }
        self.render_dithered(samples, out);
    }

    // Keep dither independent of packing so each loop can be optimized on its
    // own. The entire block uses dither, including samples on the grid.
    #[inline(never)]
    fn render_dithered(&mut self, samples: &[i32], out: &mut Vec<i16>) {
        out.extend(samples.iter().map(|&sample| {
            let dither=self.uniform()-self.uniform();
            Self::quantize(sample,dither+32768)
        }));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_s16_value_round_trips_exactly_and_unknown_depth_stays_wide() {
        let source:Vec<_>=(i16::MIN as i32..=i16::MAX as i32).map(|v|v<<16).collect();
        let mut out=Vec::new();Quantizer::default().render(&source,&mut out);
        assert!(out.iter().zip(&source).all(|(&a,&b)|(i32::from(a)<<16)==b));
        for bits in [None,Some(0),Some(17),Some(24),Some(32)] {assert_eq!(PcmFormat::for_source(bits),PcmFormat::S32);}
        for bits in 1..=16 {assert_eq!(PcmFormat::for_source(Some(bits)),PcmFormat::S16);}
    }
    #[test]
    fn exact_packing_keeps_the_rng_at_the_start_of_the_next_fractional_block() {
        let exact = [i32::MIN, 0, 1 << 16, -1 << 16, i32::MAX & !65535];
        let mut q = Quantizer::default();
        let mut out = Vec::new();
        let mut reference = Quantizer::default();
        for position in [4, 0, 3, 4] {
            let mut mixed = exact;
            mixed[position] |= 16384;
            q.render(&exact, &mut out);
            assert_eq!(out, [i16::MIN, 0, 1, -1, i16::MAX]);
            q.render(&mixed, &mut out);
            let expected: Vec<_> = mixed.iter().map(|&sample| {
                let offset = reference.uniform() - reference.uniform() + 32768;
                Quantizer::quantize(sample, offset)
            }).collect();
            assert_eq!(out, expected);
            assert_eq!(q.random, reference.random);
        }
    }
    #[test]
    fn narrow_quantizer_equals_the_wide_reference_at_every_boundary() {
        // The arithmetic this replaced: 64-bit sum, shift, clamp.
        let reference=|sample:i32,dither:i64|((i64::from(sample)+dither+32768)>>16)
            .clamp(i64::from(i16::MIN),i64::from(i16::MAX)) as i16;
        let mut samples:Vec<i32>=Vec::new();
        for base in [i32::MIN,i32::MIN+65536,-98304,-65536,-32768,0,32768,65536,98304,i32::MAX-98304,i32::MAX-65536,i32::MAX] {
            for delta in -70000i64..=70000 {
                let value=i64::from(base)+delta;
                if let Ok(value)=i32::try_from(value) {samples.push(value);}
            }
        }
        let mut x=0x1234_5678u32;
        for step in 0..200_000 {x^=x<<13;x^=x>>17;x^=x<<5;samples.push((x as i32).wrapping_add(step));}
        for &sample in &samples {
            for dither in [-65535i64,-65534,-32769,-32768,-32767,-1,0,1,32767,32768,65534,65535] {
                assert_eq!(Quantizer::quantize(sample,dither as i32+32768),reference(sample,dither),"sample={sample} dither={dither}");
            }
        }
        // The same generator sequence gives the same block as the reference.
        let block:Vec<i32>=samples.iter().copied().step_by(97).collect();
        let mut out=Vec::new();Quantizer::default().render(&block,&mut out);
        let mut q=Quantizer::default();
        let expected:Vec<i16>=block.iter().map(|&sample| {
            let dither=i64::from(q.uniform())-i64::from(q.uniform());reference(sample,dither)
        }).collect();
        assert_eq!(out,expected);
    }
    #[test]
    fn dither_preserves_quarter_lsb_mean_and_full_scale_never_wraps() {
        let source=vec![16384;131072];let mut out=Vec::new();
        let mut q=Quantizer::default();q.render(&source,&mut out);
        let mean=out.iter().map(|&v|f64::from(v)).sum::<f64>()/out.len() as f64;
        assert!((mean-0.25).abs()<0.01,"mean {mean}");
        assert!(out.iter().all(|v|(-1..=1).contains(v)));
        q.render(&[i32::MIN,i32::MAX,0,1,-1],&mut out);
        assert!(out[0]<=i16::MIN+1 && out[1]>=i16::MAX-1);
        q.render(&[0;256],&mut out);assert_eq!(out,[0;256]);
    }
}
