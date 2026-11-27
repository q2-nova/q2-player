//! Bounded C boundary for the MSA PCM conversion kernels
//! (`vendor/pcm/convert.c`, Q2 only). MSA is enabled only in that C file; the
//! entry checks the kernel's capability bit. A kernel converts whole groups
//! of four frames; `run` returns the frames done and the caller
//! (`pcm_convert.rs`) finishes with the scalar code.
unsafe extern "C" {
    fn q2_pcm_stereo_f32(l: *const f32, r: *const f32, unity: *mut i32, gained: *mut i32, gain: f32, frames: usize);
    fn q2_pcm_stereo_s32(l: *const i32, r: *const i32, unity: *mut i32, gained: *mut i32, gain: f32, frames: usize);
    fn q2_pcm_mono_f32(source: *const f32, unity: *mut i32, count: usize);
    fn q2_pcm_gain(source: *const i32, out: *mut i32, gain: f32, count: usize);
    fn q2_pcm_stereo_f32_floats(l: *const f32, r: *const f32, floats: *mut f32, frames: usize);
    fn q2_pcm_stereo_s32_floats(l: *const i32, r: *const i32, floats: *mut f32, frames: usize);
    fn q2_pcm_floats_pcm(floats: *const f32, out: *mut i32, scale: f64, count: usize);
}

pub(super) enum Job<'a> {
    /// Planar f32 or S32 to interleaved S32, and optionally that times a gain.
    StereoF32(&'a [f32], &'a [f32], &'a mut [i32], Option<(&'a mut [i32], f32)>),
    StereoS32(&'a [i32], &'a [i32], &'a mut [i32], Option<(&'a mut [i32], f32)>),
    MonoF32(&'a [f32], &'a mut [i32]),
    Gain(&'a [i32], &'a mut [i32], f32),
    /// Planar to interleaved f32 for the equalizer.
    StereoF32Floats(&'a [f32], &'a [f32], &'a mut [f32]),
    StereoS32Floats(&'a [i32], &'a [i32], &'a mut [f32]),
    /// Equalized f32 times an f64 scale to S32.
    FloatsPcm(&'a [f32], &'a mut [i32], f64),
}

/// Frames (samples for the one-plane jobs) converted: a multiple of four, or
/// zero when this CPU has no MSA.
pub(super) fn run(job: Job<'_>) -> usize {
    if !super::eq_msa::available() { return 0; }
    fn stereo(l: usize, r: usize, unity: usize, gained: &Option<(&mut [i32], f32)>) -> usize {
        let n = l.min(r).min(unity / 2);
        gained.as_ref().map_or(n, |g| n.min(g.0.len() / 2)) & !3
    }
    fn split(gained: Option<(&mut [i32], f32)>) -> (*mut i32, f32) {
        gained.map_or((std::ptr::null_mut(), 1.0), |(out, gain)| (out.as_mut_ptr(), gain))
    }
    // SAFETY: the kernel published MSA for this process. Each count is a
    // multiple of four no larger than every plane, and an interleaved buffer
    // holds at least twice that; the buffers are exclusively or immutably
    // borrowed for the call. C retains no pointers and calls nothing.
    unsafe {
        match job {
            Job::StereoF32(l, r, unity, gained) => {
                let n = stereo(l.len(), r.len(), unity.len(), &gained);
                let (out, gain) = split(gained);
                q2_pcm_stereo_f32(l.as_ptr(), r.as_ptr(), unity.as_mut_ptr(), out, gain, n);
                n
            }
            Job::StereoS32(l, r, unity, gained) => {
                let n = stereo(l.len(), r.len(), unity.len(), &gained);
                let (out, gain) = split(gained);
                q2_pcm_stereo_s32(l.as_ptr(), r.as_ptr(), unity.as_mut_ptr(), out, gain, n);
                n
            }
            Job::MonoF32(source, unity) => {
                let n = source.len().min(unity.len()) & !3;
                q2_pcm_mono_f32(source.as_ptr(), unity.as_mut_ptr(), n);
                n
            }
            Job::Gain(source, out, gain) => {
                let n = source.len().min(out.len()) & !3;
                q2_pcm_gain(source.as_ptr(), out.as_mut_ptr(), gain, n);
                n
            }
            Job::StereoF32Floats(l, r, floats) => {
                let n = l.len().min(r.len()).min(floats.len() / 2) & !3;
                q2_pcm_stereo_f32_floats(l.as_ptr(), r.as_ptr(), floats.as_mut_ptr(), n);
                n
            }
            Job::StereoS32Floats(l, r, floats) => {
                let n = l.len().min(r.len()).min(floats.len() / 2) & !3;
                q2_pcm_stereo_s32_floats(l.as_ptr(), r.as_ptr(), floats.as_mut_ptr(), n);
                n
            }
            Job::FloatsPcm(floats, out, scale) => {
                let n = floats.len().min(out.len()) & !3;
                q2_pcm_floats_pcm(floats.as_ptr(), out.as_mut_ptr(), scale, n);
                n
            }
        }
    }
}
