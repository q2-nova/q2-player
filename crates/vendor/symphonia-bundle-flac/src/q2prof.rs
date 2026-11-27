// [q2 patch] Stage timers for `q2nd --bench` (docs/target-build.md#benchmarks).
// Compiled only with `--cfg q2_flac_profile`; a normal build contains
// none of this.
//
// Nanoseconds per stage accumulate in 32-bit counters (the Q2 has no 64-bit
// atomics), enough for 4.2 s per stage. `q2::profile_take` returns and clears
// them.
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

pub const STAGES: usize = 8;
/// The whole of `decode_inner`.
pub const PACKET: usize = 0;
/// Frame sync and header.
pub const HEADER: usize = 1;
/// Residuals: partition headers and Rice codes.
pub const RESIDUAL: usize = 2;
/// Fixed and LPC prediction.
pub const PREDICT: usize = 3;
/// Inter-channel decorrelation, wasted-bits and output shifts.
pub const OUTPUT: usize = 4;
/// Subframe headers, warm-up samples, coefficients, constant and verbatim subframes.
pub const SUBFRAME_OTHER: usize = 5;

static NS: [AtomicU32; STAGES] = [const { AtomicU32::new(0) }; STAGES];
static CALLS: [AtomicU32; STAGES] = [const { AtomicU32::new(0) }; STAGES];

pub struct Span(usize, Instant);

#[inline(always)]
pub fn span(stage: usize) -> Span {
    Span(stage, Instant::now())
}

impl Drop for Span {
    #[inline(always)]
    fn drop(&mut self) {
        let ns = self.1.elapsed().as_nanos() as u32;
        NS[self.0].fetch_add(ns, Ordering::Relaxed);
        CALLS[self.0].fetch_add(1, Ordering::Relaxed);
    }
}

/// Stage nanoseconds, stage calls, and the mean cost of one empty span in
/// nanoseconds (4096 spans); clears the counters.
pub fn take() -> ([u32; STAGES], [u32; STAGES], u32) {
    let mut ns = [0; STAGES];
    let mut calls = [0; STAGES];
    for i in 0..STAGES {
        ns[i] = NS[i].swap(0, Ordering::Relaxed);
        calls[i] = CALLS[i].swap(0, Ordering::Relaxed);
    }
    let t = Instant::now();
    for _ in 0..4096 {
        drop(span(STAGES - 1));
    }
    let empty = (t.elapsed().as_nanos() / 4096) as u32;
    NS[STAGES - 1].store(0, Ordering::Relaxed);
    CALLS[STAGES - 1].store(0, Ordering::Relaxed);
    (ns, calls, empty)
}
