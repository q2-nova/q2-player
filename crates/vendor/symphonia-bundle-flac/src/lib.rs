// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
// [q2 patch] The MSA vectors use core::arch::mips intrinsics inside
// #[target_feature] functions; both are nightly features, and the Q2 build
// uses the pinned nightly. Host builds (stable) compile none of it.
#![cfg_attr(target_arch = "mips", feature(stdarch_mips, mips_target_feature))]
// [q2 patch] `deny`, not `forbid`: the MSA kernels and the capability check
// (q2simd.rs, q2frame.rs) allow it locally.
#![deny(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

// [q2 patch] Stage timers, only with --cfg q2_flac_profile.
#[cfg(q2_flac_profile)]
mod q2prof;
/// [q2 patch] Times the rest of the enclosing block as one profile stage.
#[allow(unused_macros)]
macro_rules! q2_span {
    ($stage:ident) => {
        #[cfg(q2_flac_profile)]
        let _q2_span = crate::q2prof::span(crate::q2prof::$stage);
    };
}

/// [q2 patch] Switches of the Q2 copy (README.q2.md).
pub mod q2 {
    /// Stage nanoseconds, stage calls and the cost of one empty span;
    /// clears the counters. Stage numbers: `q2prof.rs`.
    #[cfg(q2_flac_profile)]
    pub fn profile_take() -> ([u32; 8], [u32; 8], u32) {
        crate::q2prof::take()
    }

    /// Whether the kernels use MSA now.
    pub fn msa() -> bool {
        crate::q2simd::msa()
    }

    /// `Some(false)` forces the scalar kernels, `Some(true)` asks for MSA
    /// (used only where the kernel reports it), `None` restores the default.
    /// Process-wide; for tests and benchmarks.
    pub fn set_msa(want: Option<bool>) {
        crate::q2simd::set_msa(want)
    }

    /// Runs the prediction and output kernels on generated input with the
    /// scalar code and with the current kernels (`msa()`), and compares every
    /// sample. `Ok` is the number of comparisons.
    pub fn selftest() -> Result<u32, String> {
        crate::q2frame::selftest()
    }
}

// [q2 patch] The restructured subframe decoder.
mod q2bits;
mod q2frame;
mod q2simd;

mod decoder;
mod demuxer;
mod frame;
mod parser;
mod validate;

pub use decoder::FlacDecoder;
pub use demuxer::FlacReader;
