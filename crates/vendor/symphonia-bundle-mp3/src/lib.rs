// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
// [q2 patch] The MSA lanes use core::arch::mips intrinsics inside
// #[target_feature] functions; both are nightly features, and the Q2 build
// uses the pinned nightly. Host builds (stable) compile none of it.
#![cfg_attr(target_arch = "mips", feature(stdarch_mips, mips_target_feature))]
// [q2 patch] `deny`, not `forbid`: the MSA kernels and the capability check
// (q2simd.rs) allow it locally.
#![deny(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

// [q2 patch] Stage timers, only with --cfg q2_mp3_profile.
#[cfg(q2_mp3_profile)]
mod q2prof;
/// [q2 patch] Times the rest of the enclosing block as one profile stage.
macro_rules! q2_span {
    ($stage:ident) => {
        #[cfg(q2_mp3_profile)]
        let _q2_span = crate::q2prof::span(crate::q2prof::$stage);
    };
}

// [q2 patch] Four-lane kernels' value types and the MSA check.
mod q2simd;
// [q2 patch] Upstream's synthesis, the bit-exact reference of the tests.
#[cfg(test)]
#[allow(dead_code)]
#[path = "q2ref/synthesis.rs"]
mod q2ref_synthesis;

// Shared modules.
mod common;
mod header;

// Demuxer module.
mod demuxer;

// Decoder modules.
#[cfg(any(feature = "mp1", feature = "mp2", feature = "mp3"))]
mod decoder;
#[cfg(any(feature = "mp1", feature = "mp2", feature = "mp3"))]
mod synthesis;

// Shared layer 1 & 2 decoder support module.
#[cfg(any(feature = "mp1", feature = "mp2"))]
mod layer12;

// Layer-specific decoder support modules.
#[cfg(feature = "mp1")]
mod layer1;
#[cfg(feature = "mp2")]
mod layer2;
#[cfg(feature = "mp3")]
mod layer3;

#[cfg(any(feature = "mp1", feature = "mp2", feature = "mp3"))]
pub use decoder::MpaDecoder;
pub use demuxer::MpaReader;

/// [q2 patch] Switches of the Q2 copy (README.q2.md).
pub mod q2 {
    /// Whether the kernels use MSA now: on MIPS when the kernel reports
    /// `HWCAP_MIPS_MSA`, never elsewhere.
    pub fn msa() -> bool {
        crate::q2simd::msa()
    }

    /// `Some(false)` forces the scalar kernels, `Some(true)` asks for MSA
    /// (still only where the kernel reports it), `None` restores the default.
    /// Process-wide; for tests and benchmarks that compare the two paths.
    pub fn set_msa(want: Option<bool>) {
        crate::q2simd::set_msa(want)
    }

    /// Stage nanoseconds, stage calls and the cost of one empty timer span;
    /// clears the counters. Stage numbers: `q2prof.rs`.
    #[cfg(q2_mp3_profile)]
    pub fn profile_take() -> ([u32; 16], [u32; 16], u32) {
        crate::q2prof::take()
    }
}

// For SemVer compatibility in v0.5.x series.
#[deprecated = "use `symphonia_bundle_mp3::MpaDecoder` instead"]
#[cfg(any(feature = "mp1", feature = "mp2", feature = "mp3"))]
pub type Mp3Decoder = MpaDecoder;

#[deprecated = "use `symphonia_bundle_mp3::MpaReader` instead"]
pub type Mp3Reader = MpaReader;
