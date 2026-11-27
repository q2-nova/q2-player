// [q2 patch] Four-word vectors for the restructured kernels, and the MSA
// capability check (README.q2.md).
//
// The kernels in q2frame.rs are written once over `V4`. `M4` is one MSA
// register and exists on MIPS only, inside `#[target_feature(enable = "msa")]`
// entry points reached only when the kernel reports MSA (`msa()`). `P4` is the
// same arithmetic on an array; the unit tests run the kernels on it, so the
// host tests cover the kernels' structure and the emulator and the Q2 cover
// the instructions (`q2::selftest`).

use std::sync::atomic::{AtomicU8, Ordering};

/// Four 32-bit words; some operations treat them as two 64-bit lanes (words
/// 0-1 and 2-3, low word first, as MSA does on a little-endian CPU).
pub(crate) trait V4: Copy {
    fn new(w: [i32; 4]) -> Self;
    /// `s[0..4]`.
    fn load(s: &[i32]) -> Self;
    /// To `d[0..4]`.
    fn store(self, d: &mut [i32]);
    /// Per word, wrapping.
    fn add(self, o: Self) -> Self;
    fn sub(self, o: Self) -> Self;
    /// Per word, by the low five bits of `n`'s word.
    fn shl(self, n: Self) -> Self;
    /// Per word, arithmetic shift right by one.
    fn sra1(self) -> Self;
    fn or(self, o: Self) -> Self;
    fn and(self, o: Self) -> Self;
    /// `[lo0, lo2, hi0, hi2]`.
    fn evens(lo: Self, hi: Self) -> Self;
    /// `[lo1, lo3, hi1, hi3]`.
    fn odds(lo: Self, hi: Self) -> Self;
    /// `[a0, a1, b0, b1]`.
    fn low_halves(a: Self, b: Self) -> Self;
    /// `[a2, a3, b2, b3]`.
    fn high_halves(a: Self, b: Self) -> Self;
    /// Two 64-bit lanes: `a0*b0 + a1*b1` and `a2*b2 + a3*b3`, signed 32x32 to
    /// 64-bit products, wrapping sum.
    fn dot(a: Self, b: Self) -> Self;
    /// `self + dot(a, b)` per 64-bit lane, wrapping.
    fn dot_add(self, a: Self, b: Self) -> Self;
    /// Per 64-bit lane, arithmetic shift right by the low six bits of `n`'s lane.
    fn sra64(self, n: Self) -> Self;
}

/// Portable vectors: the reference semantics of every `V4` operation.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct P4(pub [i32; 4]);

#[cfg(test)]
impl P4 {
    fn lanes(self) -> [i64; 2] {
        let w = self.0;
        [
            (w[0] as u32 as u64 | (w[1] as u32 as u64) << 32) as i64,
            (w[2] as u32 as u64 | (w[3] as u32 as u64) << 32) as i64,
        ]
    }
    fn from_lanes(l: [i64; 2]) -> Self {
        P4([l[0] as i32, (l[0] >> 32) as i32, l[1] as i32, (l[1] >> 32) as i32])
    }
    fn map2(self, o: Self, f: impl Fn(i32, i32) -> i32) -> Self {
        P4([f(self.0[0], o.0[0]), f(self.0[1], o.0[1]), f(self.0[2], o.0[2]), f(self.0[3], o.0[3])])
    }
}

#[cfg(test)]
impl V4 for P4 {
    fn new(w: [i32; 4]) -> Self {
        P4(w)
    }
    fn load(s: &[i32]) -> Self {
        P4([s[0], s[1], s[2], s[3]])
    }
    fn store(self, d: &mut [i32]) {
        d[..4].copy_from_slice(&self.0);
    }
    fn add(self, o: Self) -> Self {
        self.map2(o, i32::wrapping_add)
    }
    fn sub(self, o: Self) -> Self {
        self.map2(o, i32::wrapping_sub)
    }
    fn shl(self, n: Self) -> Self {
        self.map2(n, |a, n| a.wrapping_shl(n as u32))
    }
    fn sra1(self) -> Self {
        self.map2(self, |a, _| a >> 1)
    }
    fn or(self, o: Self) -> Self {
        self.map2(o, |a, b| a | b)
    }
    fn and(self, o: Self) -> Self {
        self.map2(o, |a, b| a & b)
    }
    fn evens(lo: Self, hi: Self) -> Self {
        P4([lo.0[0], lo.0[2], hi.0[0], hi.0[2]])
    }
    fn odds(lo: Self, hi: Self) -> Self {
        P4([lo.0[1], lo.0[3], hi.0[1], hi.0[3]])
    }
    fn low_halves(a: Self, b: Self) -> Self {
        P4([a.0[0], a.0[1], b.0[0], b.0[1]])
    }
    fn high_halves(a: Self, b: Self) -> Self {
        P4([a.0[2], a.0[3], b.0[2], b.0[3]])
    }
    fn dot(a: Self, b: Self) -> Self {
        P4([0; 4]).dot_add(a, b)
    }
    fn dot_add(self, a: Self, b: Self) -> Self {
        let p = |i: usize| (a.0[i] as i64).wrapping_mul(b.0[i] as i64);
        let l = self.lanes();
        P4::from_lanes([
            l[0].wrapping_add(p(0)).wrapping_add(p(1)),
            l[1].wrapping_add(p(2)).wrapping_add(p(3)),
        ])
    }
    fn sra64(self, n: Self) -> Self {
        let (l, n) = (self.lanes(), n.lanes());
        P4::from_lanes([l[0].wrapping_shr(n[0] as u32), l[1].wrapping_shr(n[1] as u32)])
    }
}

#[cfg(target_arch = "mips")]
pub(crate) use mips::M4;

#[cfg(target_arch = "mips")]
#[allow(unsafe_code)]
mod mips {
    //! Every method here runs MSA instructions. `M4` values exist only
    //! inside the `#[target_feature(enable = "msa")]` entry points, which
    //! are called only after `msa()` returned true; that is the safety
    //! argument of every `unsafe` block below. The loads and stores take a
    //! bounds-checked four-element slice; MSA's `ld.w`/`st.w` have no
    //! alignment requirement.
    //!
    //! `core::arch::mips` intrinsics, as in the MP3 decoder's patch
    //! (crates/vendor/symphonia-bundle-mp3/README.q2.md).
    use super::V4;
    use core::arch::mips::*;
    use std::mem::transmute;

    #[repr(C, align(16))]
    struct Words([i32; 4]);

    /// One MSA register.
    #[derive(Clone, Copy)]
    pub(crate) struct M4(v4i32);

    #[inline(always)]
    fn d(v: v4i32) -> v2i64 {
        unsafe { transmute::<v4i32, v2i64>(v) }
    }
    #[inline(always)]
    fn w(v: v2i64) -> v4i32 {
        unsafe { transmute::<v2i64, v4i32>(v) }
    }
    #[inline(always)]
    fn b(v: v4i32) -> v16u8 {
        unsafe { transmute::<v4i32, v16u8>(v) }
    }
    #[inline(always)]
    fn wb(v: v16u8) -> v4i32 {
        unsafe { transmute::<v16u8, v4i32>(v) }
    }

    impl V4 for M4 {
        #[inline(always)]
        fn new(words: [i32; 4]) -> Self {
            // One aligned load; never assembled lane by lane (insert.w is
            // 2 cycles per lane on top of 3.5 from the GPR, x2000-msa.md).
            let words = Words(words);
            unsafe { M4(__msa_ld_w::<0>(words.0.as_ptr() as *mut u8)) }
        }
        #[inline(always)]
        fn load(s: &[i32]) -> Self {
            let s = &s[..4];
            unsafe { M4(__msa_ld_w::<0>(s.as_ptr() as *mut u8)) }
        }
        #[inline(always)]
        fn store(self, dst: &mut [i32]) {
            let dst = &mut dst[..4];
            unsafe { __msa_st_w::<0>(self.0, dst.as_mut_ptr() as *mut u8) }
        }
        #[inline(always)]
        fn add(self, o: Self) -> Self {
            unsafe { M4(__msa_addv_w(self.0, o.0)) }
        }
        #[inline(always)]
        fn sub(self, o: Self) -> Self {
            unsafe { M4(__msa_subv_w(self.0, o.0)) }
        }
        #[inline(always)]
        fn shl(self, n: Self) -> Self {
            unsafe { M4(__msa_sll_w(self.0, n.0)) }
        }
        #[inline(always)]
        fn sra1(self) -> Self {
            unsafe { M4(__msa_srai_w::<1>(self.0)) }
        }
        #[inline(always)]
        fn or(self, o: Self) -> Self {
            unsafe { M4(wb(__msa_or_v(b(self.0), b(o.0)))) }
        }
        #[inline(always)]
        fn and(self, o: Self) -> Self {
            unsafe { M4(wb(__msa_and_v(b(self.0), b(o.0)))) }
        }
        #[inline(always)]
        fn evens(lo: Self, hi: Self) -> Self {
            // pckev.w(a, b) is b0 b2 a0 a2.
            unsafe { M4(__msa_pckev_w(hi.0, lo.0)) }
        }
        #[inline(always)]
        fn odds(lo: Self, hi: Self) -> Self {
            // pckod.w(a, b) is b1 b3 a1 a3.
            unsafe { M4(__msa_pckod_w(hi.0, lo.0)) }
        }
        #[inline(always)]
        fn low_halves(a: Self, o: Self) -> Self {
            // ilvr.d(a, b) is b's low half, then a's.
            unsafe { M4(w(__msa_ilvr_d(d(o.0), d(a.0)))) }
        }
        #[inline(always)]
        fn high_halves(a: Self, o: Self) -> Self {
            unsafe { M4(w(__msa_ilvl_d(d(o.0), d(a.0)))) }
        }
        #[inline(always)]
        fn dot(a: Self, o: Self) -> Self {
            unsafe { M4(w(__msa_dotp_s_d(a.0, o.0))) }
        }
        #[inline(always)]
        fn dot_add(self, a: Self, o: Self) -> Self {
            unsafe { M4(w(__msa_dpadd_s_d(d(self.0), a.0, o.0))) }
        }
        #[inline(always)]
        fn sra64(self, n: Self) -> Self {
            unsafe { M4(w(__msa_sra_d(d(self.0), d(n.0)))) }
        }
    }
}

// 0: not looked up yet, 1: scalar, 2: MSA. The capability never changes
// within a process; concurrent first callers repeat the cheap lookup.
static MSA: AtomicU8 = AtomicU8::new(0);

/// Whether the kernels use MSA: on MIPS when the kernel publishes
/// `HWCAP_MIPS_MSA` (and `set_msa` has not turned it off), never elsewhere.
#[inline]
pub(crate) fn msa() -> bool {
    match MSA.load(Ordering::Relaxed) {
        0 => {
            let present = detect();
            MSA.store(if present { 2 } else { 1 }, Ordering::Relaxed);
            present
        }
        state => state == 2,
    }
}

/// `Some(false)` forces the scalar kernels, `Some(true)` asks for MSA (still
/// only where the kernel reports it), `None` returns to the default.
pub(crate) fn set_msa(want: Option<bool>) {
    let state = match want {
        None => 0,
        Some(want) => {
            if want && detect() {
                2
            }
            else {
                1
            }
        }
    };
    MSA.store(state, Ordering::Relaxed);
}

#[cfg(all(target_arch = "mips", target_os = "linux", not(q2_flac_no_msa)))]
#[allow(unsafe_code)]
fn detect() -> bool {
    extern "C" {
        fn getauxval(kind: std::os::raw::c_ulong) -> std::os::raw::c_ulong;
    }
    const AT_HWCAP: std::os::raw::c_ulong = 16;
    // Linux MIPS UAPI: HWCAP_MIPS_MSA = 1 << 1, published when the kernel
    // saves MSA context.
    const HWCAP_MIPS_MSA: std::os::raw::c_ulong = 1 << 1;
    // SAFETY: getauxval takes no pointers and only reads the process's
    // auxiliary vector; an unknown key returns 0.
    unsafe { getauxval(AT_HWCAP) & HWCAP_MIPS_MSA != 0 }
}

#[cfg(not(all(target_arch = "mips", target_os = "linux", not(q2_flac_no_msa))))]
fn detect() -> bool {
    false
}

/// xorshift32 for the generated inputs of the unit tests and `q2::selftest`.
pub(crate) struct Rng(pub u32);

impl Rng {
    pub(crate) fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
    /// Uniform in `0..n`.
    pub(crate) fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
    /// A signed value of `bits` bits (1..=32).
    pub(crate) fn signed(&mut self, bits: u32) -> i32 {
        (self.next() as i32) >> (32 - bits)
    }
}
