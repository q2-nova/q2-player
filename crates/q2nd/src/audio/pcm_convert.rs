#![forbid(unsafe_code)]
//! Planar decoder output to interleaved S32 or f32, and software gain.
//!
//! Each function is the scalar code; with `vector` set, the Q2 first converts
//! whole groups of four frames with MSA (`pcm_msa.rs`, a C leaf) and the
//! scalar code finishes the tail. Both produce the bits of the equations in `reference`
//! below (Symphonia's sample conversion, then gain, clamp and cast), which
//! the tests compare on generated input.
//!
//! Why the short forms are exact:
//! - `(s * 2^31) as i32` for f32 `s`: scaling by a power of two is exact and
//!   the cast saturates, so clamping `s` to +/-1 first changes nothing; NaN
//!   casts to 0 either way.
//! - `(x * gain) as i32`: the cast saturates to the bounds the clamp used.
//! - `s as f32 * 2^-31` for i32 `s`: one rounding to 24 bits, as
//!   `(s as f64 / 2^31) as f32`.
//! - `(s as f64 * (gain as f64 * 2^31)) as i32`: a 24-bit by 24-bit product
//!   is exact in f64, so the order of the two multiplications does not matter.
use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Signal};
use symphonia::core::conv::{ConvertibleSample, IntoSample};
use symphonia::core::sample::Sample;

pub(super) const FULL_SCALE: f32 = 2147483648.0;

/// Whether this CPU has the vector kernels. `--cfg q2_pcm_scalar` builds a
/// Q2 player that never uses them, to measure the scalar code on the device.
pub(super) fn vector() -> bool {
    #[cfg(all(q2_eq_msa, not(q2_pcm_scalar)))]
    { super::eq_msa::available() }
    #[cfg(not(all(q2_eq_msa, not(q2_pcm_scalar))))]
    { false }
}

/// Frames the MSA kernel converted; zero elsewhere.
macro_rules! vectorized {
    ($vector:expr, $job:expr) => {{
        #[cfg(q2_eq_msa)]
        let done = if $vector { super::pcm_msa::run($job) } else { 0 };
        #[cfg(not(q2_eq_msa))]
        let done = { let _ = $vector; 0usize };
        done
    }};
}

#[inline(always)]
fn pcm(sample: f32) -> i32 { (sample * FULL_SCALE) as i32 }
#[inline(always)]
fn scaled(sample: i32, gain: f32) -> i32 { (sample as f32 * gain) as i32 }
#[inline(always)]
fn float(sample: i32) -> f32 { sample as f32 * (1.0 / FULL_SCALE) }

/// Two f32 planes to interleaved S32 in `unity`, and that times the gain in
/// `gained`'s slice.
pub(super) fn stereo_f32(vector: bool, l: &[f32], r: &[f32], unity: &mut [i32], gained: Option<(&mut [i32], f32)>) {
    assert!(l.len() == r.len() && unity.len() == l.len() * 2);
    match gained {
        None => {
            let done = vectorized!(vector, super::pcm_msa::Job::StereoF32(l, r, &mut *unity, None));
            for ((u, &a), &b) in unity[done * 2..].chunks_exact_mut(2).zip(&l[done..]).zip(&r[done..]) {
                u[0] = pcm(a); u[1] = pcm(b);
            }
        }
        Some((out, gain)) => {
            assert!(out.len() == unity.len());
            let done = vectorized!(vector, super::pcm_msa::Job::StereoF32(l, r, &mut *unity, Some((&mut *out, gain))));
            for (((u, g), &a), &b) in unity[done * 2..].chunks_exact_mut(2).zip(out[done * 2..].chunks_exact_mut(2)).zip(&l[done..]).zip(&r[done..]) {
                let (a, b) = (pcm(a), pcm(b));
                u[0] = a; u[1] = b;
                g[0] = scaled(a, gain); g[1] = scaled(b, gain);
            }
        }
    }
}

/// As `stereo_f32` for S32 planes.
pub(super) fn stereo_s32(vector: bool, l: &[i32], r: &[i32], unity: &mut [i32], gained: Option<(&mut [i32], f32)>) {
    assert!(l.len() == r.len() && unity.len() == l.len() * 2);
    match gained {
        None => {
            let done = vectorized!(vector, super::pcm_msa::Job::StereoS32(l, r, &mut *unity, None));
            for ((u, &a), &b) in unity[done * 2..].chunks_exact_mut(2).zip(&l[done..]).zip(&r[done..]) {
                u[0] = a; u[1] = b;
            }
        }
        Some((out, gain)) => {
            assert!(out.len() == unity.len());
            let done = vectorized!(vector, super::pcm_msa::Job::StereoS32(l, r, &mut *unity, Some((&mut *out, gain))));
            for (((u, g), &a), &b) in unity[done * 2..].chunks_exact_mut(2).zip(out[done * 2..].chunks_exact_mut(2)).zip(&l[done..]).zip(&r[done..]) {
                u[0] = a; u[1] = b;
                g[0] = scaled(a, gain); g[1] = scaled(b, gain);
            }
        }
    }
}

/// One f32 plane to S32.
pub(super) fn mono_f32(vector: bool, source: &[f32], unity: &mut [i32]) {
    assert!(source.len() == unity.len());
    let done = vectorized!(vector, super::pcm_msa::Job::MonoF32(source, &mut *unity));
    for (u, &s) in unity[done..].iter_mut().zip(&source[done..]) { *u = pcm(s); }
}

/// S32 times the gain.
pub(super) fn gain(vector: bool, source: &[i32], out: &mut [i32], gain: f32) {
    assert!(source.len() == out.len());
    let done = vectorized!(vector, super::pcm_msa::Job::Gain(source, &mut *out, gain));
    for (o, &s) in out[done..].iter_mut().zip(&source[done..]) { *o = scaled(s, gain); }
}

/// Two f32 planes to interleaved f32.
pub(super) fn stereo_f32_floats(vector: bool, l: &[f32], r: &[f32], floats: &mut [f32]) {
    assert!(l.len() == r.len() && floats.len() == l.len() * 2);
    let done = vectorized!(vector, super::pcm_msa::Job::StereoF32Floats(l, r, &mut *floats));
    for ((f, &a), &b) in floats[done * 2..].chunks_exact_mut(2).zip(&l[done..]).zip(&r[done..]) {
        f[0] = a; f[1] = b;
    }
}

/// Two S32 planes to interleaved f32 in +/-1.
pub(super) fn stereo_s32_floats(vector: bool, l: &[i32], r: &[i32], floats: &mut [f32]) {
    assert!(l.len() == r.len() && floats.len() == l.len() * 2);
    let done = vectorized!(vector, super::pcm_msa::Job::StereoS32Floats(l, r, &mut *floats));
    for ((f, &a), &b) in floats[done * 2..].chunks_exact_mut(2).zip(&l[done..]).zip(&r[done..]) {
        f[0] = float(a); f[1] = float(b);
    }
}

/// f32 times an f64 scale (gain times full scale) to S32.
pub(super) fn floats_pcm(vector: bool, floats: &[f32], out: &mut [i32], scale: f64) {
    assert!(floats.len() == out.len());
    let done = vectorized!(vector, super::pcm_msa::Job::FloatsPcm(floats, &mut *out, scale));
    for (o, &s) in out[done..].iter_mut().zip(&floats[done..]) { *o = (s as f64 * scale) as i32; }
}

/// Any sample format and channel count through Symphonia's own conversion.
pub(super) fn interleave<T: ConvertibleSample>(decoded: &AudioBufferRef<'_>, out: &mut [T]) {
    fn typed<S: Sample + IntoSample<T>, T: Sample>(buffer: &AudioBuffer<S>, out: &mut [T]) {
        let channels = buffer.spec().channels.count();
        assert!(out.len() == buffer.frames() * channels);
        // One loop for every layout: these formats are rare, and each
        // specialization would be compiled for ten formats and two outputs.
        for channel in 0..channels {
            for (o, &s) in out.iter_mut().skip(channel).step_by(channels).zip(buffer.chan(channel)) { *o = s.into_sample(); }
        }
    }
    match decoded {
        AudioBufferRef::U8(b) => typed(b, out),
        AudioBufferRef::U16(b) => typed(b, out),
        AudioBufferRef::U24(b) => typed(b, out),
        AudioBufferRef::U32(b) => typed(b, out),
        AudioBufferRef::S8(b) => typed(b, out),
        AudioBufferRef::S16(b) => typed(b, out),
        AudioBufferRef::S24(b) => typed(b, out),
        AudioBufferRef::S32(b) => typed(b, out),
        AudioBufferRef::F32(b) => typed(b, out),
        AudioBufferRef::F64(b) => typed(b, out),
    }
}

/// Every vector kernel against the scalar code on generated input: all
/// lengths to 40, extreme samples and gains. True where there are no kernels.
pub fn vector_matches_scalar() -> bool {
    if !vector() { return true; }
    let mut state = 0x2545_f491u32;
    let mut next = move || { state = state.wrapping_mul(1664525).wrapping_add(1013904223); state };
    let edges = [0u32, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x3f7f_ffff, 0xbf7f_ffff, 0x3f80_0001, 0xbf80_0001,
        0x4000_0000, 0xc000_0000, 0x2f80_0000, 0x3000_0000, 0x0080_0000, 1, 0x8000_0001, 0x7f7f_ffff, 0xff7f_ffff,
        0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0xffc0_0000, 0x7fa0_0000, 0x4f00_0000, 0xcf00_0000, 0x4eff_ffff,
        0xcf00_0001, 0x5f00_0000, 0xdf00_0000, 0x7fff_ffff, 0x7fff_ffc0, 0x8000_0000, 0x8000_0040];
    let mut same = true;
    for round in 0..40usize {
        let frames = round;
        let mut word = |edge: bool| { let r = next(); if edge && r >> 30 == 0 { edges[(r >> 8) as usize % edges.len()] } else { r } };
        let li: Vec<i32> = (0..frames).map(|_| word(true) as i32).collect();
        let ri: Vec<i32> = (0..frames).map(|_| word(true) as i32).collect();
        // Every bit pattern class: mostly audio-range values, then raw bits.
        let lf: Vec<f32> = (0..frames).map(|_| { let r = word(true); if r & 3 == 0 { f32::from_bits(r) } else { ((r >> 8) as f32 / 8_388_608.0 - 1.0) * 1.05 } }).collect();
        let rf: Vec<f32> = (0..frames).map(|_| { let r = word(true); if r & 3 == 0 { f32::from_bits(r) } else { (r as i32) as f32 / 2e9 } }).collect();
        for g in [1.0f32, 0.0, 0.31622776, 2.0, 1e10, 1e-10, f32::INFINITY, f32::NAN, -1.5, f32::from_bits(edges[round % edges.len()])] {
            let (mut a, mut b, mut c, mut d) = (vec![7i32; frames * 2], vec![7i32; frames * 2], vec![7i32; frames * 2], vec![7i32; frames * 2]);
            stereo_f32(false, &lf, &rf, &mut a, Some((&mut b[..], g)));
            stereo_f32(true, &lf, &rf, &mut c, Some((&mut d[..], g)));
            same &= a == c && b == d;
            stereo_f32(false, &lf, &rf, &mut a, None);
            stereo_f32(true, &lf, &rf, &mut c, None);
            same &= a == c;
            stereo_s32(false, &li, &ri, &mut a, Some((&mut b[..], g)));
            stereo_s32(true, &li, &ri, &mut c, Some((&mut d[..], g)));
            same &= a == c && b == d;
            stereo_s32(false, &li, &ri, &mut a, None);
            stereo_s32(true, &li, &ri, &mut c, None);
            same &= a == c;
            mono_f32(false, &lf, &mut a[..frames]);
            mono_f32(true, &lf, &mut c[..frames]);
            same &= a == c;
            gain(false, &li, &mut b[..frames], g);
            gain(true, &li, &mut d[..frames], g);
            same &= b == d;
            for scale in [g as f64 * 2147483648.0, g as f64, 0.5, -3e9] {
                floats_pcm(false, &lf, &mut a[..frames], scale);
                floats_pcm(true, &lf, &mut c[..frames], scale);
                floats_pcm(false, &rf, &mut b[..frames], scale);
                floats_pcm(true, &rf, &mut d[..frames], scale);
                same &= a == c && b == d;
            }
            let (mut x, mut y) = (vec![0f32; frames * 2], vec![0f32; frames * 2]);
            let bits = |v: &[f32]| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
            stereo_f32_floats(false, &lf, &rf, &mut x);
            stereo_f32_floats(true, &lf, &rf, &mut y);
            same &= bits(&x) == bits(&y);
            stereo_s32_floats(false, &li, &ri, &mut x);
            stereo_s32_floats(true, &li, &ri, &mut y);
            same &= bits(&x) == bits(&y);
        }
    }
    same
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The equations of the code this module replaced.
    mod reference {
        use symphonia::core::conv::IntoSample;
        pub fn pcm(s: f32) -> i32 { s.into_sample() }
        pub fn scaled(s: i32, gain: f32) -> i32 { (s as f32 * gain).clamp(i32::MIN as f32, i32::MAX as f32) as i32 }
        pub fn float(s: i32) -> f32 { s.into_sample() }
        pub fn floats_pcm(s: f32, gain: f32) -> i32 {
            (s as f64 * gain as f64 * 2147483648.0).clamp(i32::MIN as f64, i32::MAX as f64) as i32
        }
    }

    fn lcg(state: &mut u32) -> u32 { *state = state.wrapping_mul(1664525).wrapping_add(1013904223); *state }

    /// Every exponent with boundary and random mantissas, both signs.
    fn f32_patterns() -> Vec<f32> {
        let mut state = 1u32;
        let mut out = Vec::new();
        for exponent in 0..=255u32 {
            let mut mantissas = vec![0, 1, 2, 0x7f_ffff, 0x7f_fffe, 0x40_0000, 0x3f_ffff, 0x40_0001, 0x00_0100, 0x00_00ff, 0x7f_ff00, 0x7f_ff80, 0x7f_ffc0];
            mantissas.extend((0..300).map(|_| lcg(&mut state) >> 9));
            for mantissa in mantissas {
                for sign in [0, 1u32 << 31] { out.push(f32::from_bits(sign | exponent << 23 | mantissa)); }
            }
        }
        out
    }
    fn i32_patterns() -> Vec<i32> {
        let mut state = 2u32;
        let mut out = vec![0, 1, -1, i32::MAX, i32::MIN, i32::MAX - 1, i32::MIN + 1];
        for bit in 0..31 { for delta in -130i32..=130 { out.push((1i32 << bit).wrapping_add(delta)); out.push((-1i32 << bit).wrapping_add(delta)); } }
        out.extend((0..200_000).map(|_| lcg(&mut state) as i32));
        out
    }
    fn gains() -> Vec<f32> {
        let mut state = 3u32;
        let mut out = vec![0.0, -0.0, 1.0, 0.31622776, 0.5, 2.0, 1.0001, 0.9999, 7.5, 1e10, 1e-10, 1e38, 1e-38, 1e-45,
            -1.0, -0.3, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, f32::MAX, f32::MIN_POSITIVE];
        out.extend((0..12).map(|_| (lcg(&mut state) >> 8) as f32 / 8_388_608.0));
        out
    }

    #[test]
    fn scalar_forms_equal_the_replaced_equations() {
        let floats = f32_patterns();
        let integers = i32_patterns();
        for &s in &floats { assert_eq!(pcm(s), reference::pcm(s), "{:#x}", s.to_bits()); }
        for &s in &integers { assert_eq!(float(s).to_bits(), reference::float(s).to_bits(), "{s}"); }
        for gain in gains() {
            for &s in &integers { assert_eq!(scaled(s, gain), reference::scaled(s, gain), "{s} {gain}"); }
            for &s in floats.iter().step_by(7) {
                assert_eq!((s as f64 * (gain as f64 * 2147483648.0)) as i32, reference::floats_pcm(s, gain), "{:#x} {gain}", s.to_bits());
                assert_eq!(scaled(pcm(s), gain), reference::scaled(reference::pcm(s), gain));
            }
        }
    }

    #[test]
    fn kernels_interleave_and_scale_every_length() {
        let floats = f32_patterns();
        let integers = i32_patterns();
        for frames in (0..70).chain([1152, 4096]) {
            for (offset, gain) in [(0usize, 0.31622776f32), (977, 1.0), (5003, 2.5), (20011, 0.0)] {
                let (lf, rf) = (&floats[offset..offset + frames], &floats[offset + 4099..offset + 4099 + frames]);
                let (li, ri) = (&integers[offset..offset + frames], &integers[offset + 4099..offset + 4099 + frames]);
                let (mut unity, mut gained) = (vec![7; frames * 2], vec![7; frames * 2]);
                stereo_f32(vector(), lf, rf, &mut unity, Some((&mut gained[..], gain)));
                let expected: Vec<i32> = lf.iter().zip(rf).flat_map(|(&a, &b)| [reference::pcm(a), reference::pcm(b)]).collect();
                assert_eq!(unity, expected);
                assert_eq!(gained, expected.iter().map(|&s| reference::scaled(s, gain)).collect::<Vec<_>>());
                unity.fill(7);
                stereo_f32(vector(), lf, rf, &mut unity, None);
                assert_eq!(unity, expected);
                stereo_s32(vector(), li, ri, &mut unity, Some((&mut gained[..], gain)));
                let expected: Vec<i32> = li.iter().zip(ri).flat_map(|(&a, &b)| [a, b]).collect();
                assert_eq!(unity, expected);
                assert_eq!(gained, expected.iter().map(|&s| reference::scaled(s, gain)).collect::<Vec<_>>());
                unity.fill(7);
                stereo_s32(vector(), li, ri, &mut unity, None);
                assert_eq!(unity, expected);
                let mut mono = vec![7; frames];
                mono_f32(vector(), lf, &mut mono);
                assert_eq!(mono, lf.iter().map(|&s| reference::pcm(s)).collect::<Vec<_>>());
                let mut out = vec![7; frames];
                super::gain(vector(), li, &mut out, gain);
                assert_eq!(out, li.iter().map(|&s| reference::scaled(s, gain)).collect::<Vec<_>>());
                floats_pcm(vector(), lf, &mut out, gain as f64 * 2147483648.0);
                assert_eq!(out, lf.iter().map(|&s| reference::floats_pcm(s, gain)).collect::<Vec<_>>());
                let mut interleaved = vec![0f32; frames * 2];
                stereo_f32_floats(vector(), lf, rf, &mut interleaved);
                assert_eq!(interleaved.iter().map(|f| f.to_bits()).collect::<Vec<_>>(),
                    lf.iter().zip(rf).flat_map(|(a, b)| [a.to_bits(), b.to_bits()]).collect::<Vec<_>>());
                stereo_s32_floats(vector(), li, ri, &mut interleaved);
                assert_eq!(interleaved.iter().map(|f| f.to_bits()).collect::<Vec<_>>(),
                    li.iter().zip(ri).flat_map(|(&a, &b)| [reference::float(a).to_bits(), reference::float(b).to_bits()]).collect::<Vec<_>>());
            }
        }
        assert!(vector_matches_scalar());
    }
}
