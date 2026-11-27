//! Bounded C packet boundary. MSA is enabled only in the C leaf on MIPS;
//! the normal Rust target stays scalar and checks the kernel's capability bit.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Filter {
    pub b: [f64; 3],
    pub a: [f64; 2],
    pub state: [[f64; 2]; 2],
}
pub(super) const MAX_FILTERS: usize = 30;
unsafe extern "C" {
    fn q2_eq_stereo(filters: *mut Filter, count: usize, pcm: *mut f32, frames: usize, gain: f64);
}
pub(super) fn available() -> bool {
    #[cfg(q2_eq_msa)]
    {
        use std::sync::atomic::{AtomicU8, Ordering};
        // The capability never changes within this process. Concurrent first
        // callers may repeat the cheap lookup; nobody waits for initialization.
        static AVAILABLE: AtomicU8 = AtomicU8::new(0);
        // Linux MIPS UAPI: HWCAP_MIPS_MSA = 1 << 1. The qualified kernel
        // publishes it only when cpu_has_msa includes kernel context support.
        match AVAILABLE.load(Ordering::Relaxed) {
            0 => {
                let present = unsafe { libc::getauxval(libc::AT_HWCAP) & 2 != 0 };
                AVAILABLE.store(if present { 2 } else { 1 }, Ordering::Relaxed);
                present
            }
            value => value == 2,
        }
    }
    #[cfg(not(q2_eq_msa))]
    { true } // Native test build only: portable vector equations, not MSA.
}
pub(super) fn process(filters: &mut [Filter], pcm: &mut [f32], gain: f64) {
    assert!(filters.len() <= MAX_FILTERS);
    assert!(available());
    // SAFETY: repr(C) matches stereo.c; exclusively borrowed complete arrays,
    // bounded filter count, frames excludes any incomplete trailing sample.
    // C retains no pointers and performs no allocation or external calls.
    unsafe { q2_eq_stereo(filters.as_mut_ptr(), filters.len(), pcm.as_mut_ptr(), pcm.len()/2, gain); }
}
