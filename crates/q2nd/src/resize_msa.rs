//! MSA row accumulation for the exact Triangle resize (`gfx/resize.rs`, Q2 only).
//!
//! `acc[k] += f32(row[k]) * w` for 16 bytes per iteration: the bytes widen to
//! words in vector registers (`ilvr`/`ilvl` against zero), `ffint_u.w`
//! converts them exactly, then `fmul.w` and `fadd.w`, which round like the
//! scalar `mul.s` and `add.s` the portable loop compiles to. Lanes are
//! independent elements, so each element's sum keeps its tap order. The
//! weight moves to a vector register once per row.
//!
//! `--cfg q2_gfx_scalar` builds a Q2 binary that never enters this.

#[cfg(all(q2_eq_msa, not(q2_gfx_scalar)))]
mod kernel {
    use core::arch::mips::*;
    use core::mem::transmute as cast;

    /// Accumulates the leading multiple of 16 elements; returns that count.
    #[target_feature(enable = "msa")]
    pub(super) fn accumulate(acc: &mut [f32], row: &[u8], w: f32) -> usize {
        let n = acc.len().min(row.len()) & !15;
        let (a, r) = (acc.as_mut_ptr(), row.as_ptr());
        // SAFETY: `n` is no larger than either slice, which are borrowed for
        // the call (`acc` exclusively), so every 16-byte load from `r + i`
        // and 16-byte load/store at `a + i .. a + i + 16` floats with
        // `i + 16 <= n` stays inside them. MSA `ld`/`st` accept any byte
        // alignment. The intrinsics need MSA, which this function's target
        // feature and its caller's capability check provide.
        unsafe {
            let wv: v4f32 = cast(__msa_fill_w(w.to_bits() as i32));
            let zero = __msa_ldi_b::<0>();
            let mut i = 0;
            while i < n {
                let bytes = __msa_ld_b::<0>(r.add(i) as *mut u8);
                let lo: v8i16 = cast(__msa_ilvr_b(zero, bytes));
                let hi: v8i16 = cast(__msa_ilvl_b(zero, bytes));
                let z: v8i16 = cast(zero);
                let words: [v4u32; 4] = [cast(__msa_ilvr_h(z, lo)), cast(__msa_ilvl_h(z, lo)),
                                         cast(__msa_ilvr_h(z, hi)), cast(__msa_ilvl_h(z, hi))];
                let p = a.add(i) as *mut u8;
                let s0: v4f32 = cast(__msa_ld_w::<0>(p));
                let s1: v4f32 = cast(__msa_ld_w::<16>(p));
                let s2: v4f32 = cast(__msa_ld_w::<32>(p));
                let s3: v4f32 = cast(__msa_ld_w::<48>(p));
                __msa_st_w::<0>(cast(__msa_fadd_w(s0, __msa_fmul_w(__msa_ffint_u_w(words[0]), wv))), p);
                __msa_st_w::<16>(cast(__msa_fadd_w(s1, __msa_fmul_w(__msa_ffint_u_w(words[1]), wv))), p);
                __msa_st_w::<32>(cast(__msa_fadd_w(s2, __msa_fmul_w(__msa_ffint_u_w(words[2]), wv))), p);
                __msa_st_w::<48>(cast(__msa_fadd_w(s3, __msa_fmul_w(__msa_ffint_u_w(words[3]), wv))), p);
                i += 16;
            }
        }
        n
    }
}

/// Leading RGB elements accumulated with MSA: a multiple of 16, or zero
/// without MSA (the caller finishes with the scalar loop).
#[cfg(all(q2_eq_msa, not(q2_gfx_scalar)))]
pub(crate) fn accumulate_rgb(acc: &mut [f32], row: &[u8], w: f32) -> usize {
    if !crate::gfx_msa::available() { return 0; }
    // SAFETY: the kernel published MSA for this process, which is all
    // `kernel::accumulate` requires of its caller.
    unsafe { kernel::accumulate(acc, row, w) }
}

#[cfg(not(all(q2_eq_msa, not(q2_gfx_scalar))))]
pub(crate) fn accumulate_rgb(_: &mut [f32], _: &[u8], _: f32) -> usize { 0 }
