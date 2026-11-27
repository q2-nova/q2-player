// [q2 patch] Four-lane values for the restructured kernels, and the MSA
// capability check (README.q2.md).
//
// Each kernel is written once over `Lanes`: four independent values (four
// time slots, four sub-bands or four adjacent samples) that go through the
// same additions and multiplications in the same order as upstream's scalar
// code. No lane ever reads another lane's arithmetic result, so every lane's
// result has the bits upstream computes.
//
// `S4` is the portable implementation (an array, scalar code). `M4` exists on
// MIPS only: one MSA register, used inside `#[target_feature(enable = "msa")]`
// entry points that are reached only when the kernel reports MSA (`msa()`).

use std::ops::{Add, Mul, Neg, Sub};
use std::sync::atomic::{AtomicU8, Ordering};

/// 16-byte alignment for tables and state the kernels load as vectors: an
/// aligned `ld.w` costs 1.0 cycle on the X2000, an unaligned one 1.5 and one
/// across a 32-byte line 2.0 (q2-toolchain docs/x2000-msa.md).
#[repr(C, align(16))]
pub(crate) struct Aligned16<T>(pub T);

pub(crate) trait Lanes:
    Copy + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> + Neg<Output = Self>
{
    fn splat(x: f32) -> Self;
    /// `s[0..4]`.
    fn load(s: &[f32]) -> Self;
    /// To `d[0..4]`.
    fn store(self, d: &mut [f32]);
    /// Lanes 3, 2, 1, 0.
    fn reverse(self) -> Self;
    /// `out[i][j] = rows[j][i]`.
    fn transpose(rows: [Self; 4]) -> [Self; 4];
    /// `f32::clamp(-1.0, 1.0)` per lane (NaN stays NaN).
    fn clamp1(self) -> Self;
    /// Lanes 1 and 3 with the sign bit flipped.
    fn neg_odd(self) -> Self;
    /// Lanes below `n` from `self`, the others from `other`.
    fn keep_first(self, n: usize, other: Self) -> Self;
}

/// Portable lanes: scalar arithmetic on an array.
#[derive(Clone, Copy)]
pub(crate) struct S4(pub [f32; 4]);

macro_rules! s4_binary {
    ($trait:ident, $method:ident, $op:tt) => {
        impl $trait for S4 {
            type Output = S4;
            #[inline(always)]
            fn $method(self, o: S4) -> S4 {
                S4([self.0[0] $op o.0[0], self.0[1] $op o.0[1], self.0[2] $op o.0[2], self.0[3] $op o.0[3]])
            }
        }
    };
}
s4_binary!(Add, add, +);
s4_binary!(Sub, sub, -);
s4_binary!(Mul, mul, *);

impl Neg for S4 {
    type Output = S4;
    #[inline(always)]
    fn neg(self) -> S4 {
        S4([-self.0[0], -self.0[1], -self.0[2], -self.0[3]])
    }
}

impl Lanes for S4 {
    #[inline(always)]
    fn splat(x: f32) -> Self {
        S4([x; 4])
    }
    #[inline(always)]
    fn load(s: &[f32]) -> Self {
        S4([s[0], s[1], s[2], s[3]])
    }
    #[inline(always)]
    fn store(self, d: &mut [f32]) {
        d[..4].copy_from_slice(&self.0);
    }
    #[inline(always)]
    fn reverse(self) -> Self {
        S4([self.0[3], self.0[2], self.0[1], self.0[0]])
    }
    #[inline(always)]
    fn transpose(r: [Self; 4]) -> [Self; 4] {
        [
            S4([r[0].0[0], r[1].0[0], r[2].0[0], r[3].0[0]]),
            S4([r[0].0[1], r[1].0[1], r[2].0[1], r[3].0[1]]),
            S4([r[0].0[2], r[1].0[2], r[2].0[2], r[3].0[2]]),
            S4([r[0].0[3], r[1].0[3], r[2].0[3], r[3].0[3]]),
        ]
    }
    #[inline(always)]
    fn clamp1(self) -> Self {
        S4([
            self.0[0].clamp(-1.0, 1.0),
            self.0[1].clamp(-1.0, 1.0),
            self.0[2].clamp(-1.0, 1.0),
            self.0[3].clamp(-1.0, 1.0),
        ])
    }
    #[inline(always)]
    fn neg_odd(self) -> Self {
        S4([self.0[0], -self.0[1], self.0[2], -self.0[3]])
    }
    #[inline(always)]
    fn keep_first(self, n: usize, other: Self) -> Self {
        let mut out = other;
        for i in 0..n.min(4) {
            out.0[i] = self.0[i];
        }
        out
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
    //! `core::arch::mips` intrinsics, not `std::simd`: LLVM turns a
    //! `std::simd` load or store of an unaligned `[f32; 4]` into four scalar
    //! word copies through an aligned stack slot, and a two-input shuffle
    //! into `splati.w`/`insve.w` chains (measured in the first variant of
    //! this patch). The intrinsics give `ld.w`, `st.w`, `ilvr`/`ilvl` and
    //! `shf.w` directly. No `fmadd.w`: it is fused and would change rounding.
    use super::Lanes;
    use core::arch::mips::*;
    use std::mem::transmute;
    use std::ops::{Add, Mul, Neg, Sub};

    /// MSA lanes: one 128-bit register.
    #[derive(Clone, Copy)]
    pub(crate) struct M4(v4f32);

    impl Add for M4 {
        type Output = M4;
        #[inline(always)]
        fn add(self, o: M4) -> M4 {
            unsafe { M4(__msa_fadd_w(self.0, o.0)) }
        }
    }
    impl Sub for M4 {
        type Output = M4;
        #[inline(always)]
        fn sub(self, o: M4) -> M4 {
            unsafe { M4(__msa_fsub_w(self.0, o.0)) }
        }
    }
    impl Mul for M4 {
        type Output = M4;
        #[inline(always)]
        fn mul(self, o: M4) -> M4 {
            unsafe { M4(__msa_fmul_w(self.0, o.0)) }
        }
    }
    impl Neg for M4 {
        type Output = M4;
        #[inline(always)]
        fn neg(self) -> M4 {
            // bnegi.w flips bit 31, the sign, as scalar negation does.
            unsafe { M4(transmute::<v4u32, v4f32>(__msa_bnegi_w::<31>(transmute::<v4f32, v4u32>(self.0)))) }
        }
    }

    #[repr(C, align(16))]
    struct Words([u32; 4]);
    static ODD_SIGNS: Words = Words([0, 0x8000_0000, 0, 0x8000_0000]);
    /// `FIRST[n]`: all ones in the lanes below n.
    static FIRST: [Words; 5] = [
        Words([0, 0, 0, 0]),
        Words([!0, 0, 0, 0]),
        Words([!0, !0, 0, 0]),
        Words([!0, !0, !0, 0]),
        Words([!0, !0, !0, !0]),
    ];

    impl Lanes for M4 {
        #[inline(always)]
        fn splat(x: f32) -> Self {
            unsafe { M4(transmute::<v4i32, v4f32>(__msa_fill_w(x.to_bits() as i32))) }
        }
        #[inline(always)]
        fn load(s: &[f32]) -> Self {
            let s = &s[..4];
            unsafe { M4(transmute::<v4i32, v4f32>(__msa_ld_w::<0>(s.as_ptr() as *mut u8))) }
        }
        #[inline(always)]
        fn store(self, d: &mut [f32]) {
            let d = &mut d[..4];
            unsafe { __msa_st_w::<0>(transmute::<v4f32, v4i32>(self.0), d.as_mut_ptr() as *mut u8) }
        }
        #[inline(always)]
        fn reverse(self) -> Self {
            // shf.w: result lane i is source lane (imm >> 2i) & 3; 0x1b is 3, 2, 1, 0.
            unsafe { M4(transmute::<v4i32, v4f32>(__msa_shf_w::<0x1b>(transmute::<v4f32, v4i32>(self.0)))) }
        }
        #[inline(always)]
        fn transpose(r: [Self; 4]) -> [Self; 4] {
            unsafe {
                let r0 = transmute::<v4f32, v4i32>(r[0].0);
                let r1 = transmute::<v4f32, v4i32>(r[1].0);
                let r2 = transmute::<v4f32, v4i32>(r[2].0);
                let r3 = transmute::<v4f32, v4i32>(r[3].0);
                // ilvr.w(a, b) is b0 a0 b1 a1; ilvl.w(a, b) is b2 a2 b3 a3.
                let t0 = transmute::<v4i32, v2i64>(__msa_ilvr_w(r1, r0));
                let t1 = transmute::<v4i32, v2i64>(__msa_ilvl_w(r1, r0));
                let t2 = transmute::<v4i32, v2i64>(__msa_ilvr_w(r3, r2));
                let t3 = transmute::<v4i32, v2i64>(__msa_ilvl_w(r3, r2));
                // ilvr.d(a, b) is b's low half then a's; ilvl.d the high halves.
                [
                    M4(transmute::<v2i64, v4f32>(__msa_ilvr_d(t2, t0))),
                    M4(transmute::<v2i64, v4f32>(__msa_ilvl_d(t2, t0))),
                    M4(transmute::<v2i64, v4f32>(__msa_ilvr_d(t3, t1))),
                    M4(transmute::<v2i64, v4f32>(__msa_ilvl_d(t3, t1))),
                ]
            }
        }
        #[inline(always)]
        fn clamp1(self) -> Self {
            // Two compares and two selects, as `f32::clamp`: NaN stays NaN.
            // bsel.v(mask, a, b) is b where the mask is set, a elsewhere.
            unsafe {
                let lo = transmute::<v4i32, v4f32>(__msa_fill_w((-1.0f32).to_bits() as i32));
                let hi = transmute::<v4i32, v4f32>(__msa_fill_w(1.0f32.to_bits() as i32));
                let below = transmute::<v4i32, v16u8>(__msa_fclt_w(self.0, lo));
                let x = __msa_bsel_v(below, transmute::<v4f32, v16u8>(self.0), transmute::<v4f32, v16u8>(lo));
                let above = transmute::<v4i32, v16u8>(__msa_fclt_w(hi, transmute::<v16u8, v4f32>(x)));
                M4(transmute::<v16u8, v4f32>(__msa_bsel_v(above, x, transmute::<v4f32, v16u8>(hi))))
            }
        }
        #[inline(always)]
        fn neg_odd(self) -> Self {
            unsafe {
                let signs = transmute::<v4i32, v16u8>(__msa_ld_w::<0>(ODD_SIGNS.0.as_ptr() as *mut u8));
                M4(transmute::<v16u8, v4f32>(__msa_xor_v(transmute::<v4f32, v16u8>(self.0), signs)))
            }
        }
        #[inline(always)]
        fn keep_first(self, n: usize, other: Self) -> Self {
            unsafe {
                let mask = transmute::<v4i32, v16u8>(__msa_ld_w::<0>(FIRST[n.min(4)].0.as_ptr() as *mut u8));
                M4(transmute::<v16u8, v4f32>(__msa_bsel_v(
                    mask,
                    transmute::<v4f32, v16u8>(other.0),
                    transmute::<v4f32, v16u8>(self.0),
                )))
            }
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

#[cfg(all(target_arch = "mips", target_os = "linux", not(q2_mp3_no_msa)))]
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

#[cfg(not(all(target_arch = "mips", target_os = "linux", not(q2_mp3_no_msa))))]
fn detect() -> bool {
    false
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Lanes, S4};

    /// xorshift32 for the bit-exactness tests.
    pub(crate) struct Rng(pub u32);
    impl Rng {
        pub(crate) fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0
        }
        /// Uniform in [-1, 1).
        pub(crate) fn float(&mut self) -> f32 {
            (self.next() >> 8) as f32 / 8_388_608.0 - 1.0
        }
    }

    #[test]
    fn s4_lane_helpers() {
        let a = S4([1.0, 2.0, 3.0, 4.0]);
        assert_eq!(a.reverse().0, [4.0, 3.0, 2.0, 1.0]);
        assert_eq!(a.neg_odd().0, [1.0, -2.0, 3.0, -4.0]);
        assert_eq!(a.keep_first(2, S4::splat(9.0)).0, [1.0, 2.0, 9.0, 9.0]);
        let t = S4::transpose([a, S4([5.0, 6.0, 7.0, 8.0]), S4::splat(0.0), S4::splat(-1.0)]);
        assert_eq!(t[0].0, [1.0, 5.0, 0.0, -1.0]);
        assert_eq!(t[3].0, [4.0, 8.0, 0.0, -1.0]);
        assert_eq!(S4([-2.0, -0.0, 0.5, 7.0]).clamp1().0.map(f32::to_bits), [-1.0f32, -0.0, 0.5, 1.0].map(f32::to_bits));
        assert!(S4::splat(f32::NAN).clamp1().0[0].is_nan());
    }
}
