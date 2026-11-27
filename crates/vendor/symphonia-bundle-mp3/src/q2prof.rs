// [q2 patch] Stage timers for `q2nd --bench` (docs/target-build.md#benchmarks).
// Compiled only with `--cfg q2_mp3_profile`; a normal build contains
// none of this.
//
// Nanoseconds per stage accumulate in 32-bit counters (the Q2 has no 64-bit
// atomics), enough for 4.2 s per stage. `q2::profile_take` returns and clears
// them.
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

pub const STAGES: usize = 16;
pub const SIDE_INFO: usize = 0;
pub const MAIN_DATA: usize = 1;
pub const REQUANTIZE: usize = 2;
pub const STEREO: usize = 3;
pub const REORDER_ANTIALIAS: usize = 4;
pub const HYBRID: usize = 5;
pub const FREQ_INVERSION: usize = 6;
pub const SYNTHESIS: usize = 7;
pub const LAYER3: usize = 8;
pub const PACKET: usize = 9;
pub const DCT32: usize = 10;
pub const WINDOW: usize = 11;
pub const RENDER: usize = 12;

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
