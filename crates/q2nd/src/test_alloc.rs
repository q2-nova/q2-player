//! Heap accounting for tests: the test binary's global allocator counts the
//! calling thread's allocations inside `measure`. Tests use it for allocation
//! budgets (a steady-state path that must not allocate, a parser that must
//! not ask for more memory than its input justifies). Not part of the player.
//!
//! Only the measuring thread is counted, so tests running in parallel do not
//! disturb each other. Memory freed by another thread is not subtracted.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `alloc`, `alloc_zeroed` and growing or moving `realloc` calls.
    pub allocations: u64,
    /// Bytes those calls asked for.
    pub bytes: u64,
    /// The largest single request.
    pub largest: usize,
    /// The most bytes held at once, counted from the start of `measure`.
    pub peak: usize,
    live: isize,
}

thread_local! {
    // Const-initialized and without destructors: reading them from inside
    // the allocator neither allocates nor registers a thread-exit hook.
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static STATS: Cell<Stats> = const { Cell::new(Stats { allocations: 0, bytes: 0, largest: 0, peak: 0, live: 0 }) };
}

pub struct Counting;

fn note(size: usize) {
    // `try_with`: a thread that is being torn down has no counters left.
    let _ = ACTIVE.try_with(|active| {
        if !active.get() { return; }
        let _ = STATS.try_with(|stats| {
            let mut s = stats.get();
            s.allocations += 1;
            s.bytes += size as u64;
            s.largest = s.largest.max(size);
            s.live += size as isize;
            s.peak = s.peak.max(s.live.max(0) as usize);
            stats.set(s);
        });
    });
}

fn release(size: usize) {
    let _ = ACTIVE.try_with(|active| {
        if !active.get() { return; }
        let _ = STATS.try_with(|stats| {
            let mut s = stats.get();
            s.live -= size as isize;
            stats.set(s);
        });
    });
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the `GlobalAlloc` contract. The accounting before and after touches
// only const-initialized thread-local `Cell`s of `Copy` data: it does not
// allocate, lock, unwind or re-enter the allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: the caller's layout, passed through.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: the caller's layout, passed through.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        release(layout.size());
        // SAFETY: the caller's pointer and the layout it was allocated with.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        release(layout.size());
        note(size);
        // SAFETY: the caller's pointer, its layout and the new size.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

struct Stop;
impl Drop for Stop {
    fn drop(&mut self) { ACTIVE.with(|active| active.set(false)); }
}

/// Runs `work` and returns what this thread allocated meanwhile. Not
/// reentrant. Counting stops when `work` unwinds.
pub fn measure<R>(work: impl FnOnce() -> R) -> (R, Stats) {
    assert!(!ACTIVE.with(Cell::get), "nested allocation measurement");
    STATS.with(|stats| stats.set(Stats::default()));
    ACTIVE.with(|active| active.set(true));
    let stop = Stop;
    let result = work();
    drop(stop);
    (result, STATS.with(Cell::get))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_this_threads_requests_peak_and_largest_only_inside_measure() {
        let outside = vec![0u8; 1 << 20];
        let ((), stats) = measure(|| {
            let first = vec![1u8; 4096];
            drop(first);
            let second = vec![2u8; 1024];
            let mut grown: Vec<u8> = Vec::with_capacity(16);
            grown.reserve_exact(8192);
            std::hint::black_box((&second, &grown));
        });
        drop(outside);
        assert_eq!(stats.allocations, 4, "{stats:?}");
        assert_eq!(stats.bytes, 4096 + 1024 + 16 + 8192);
        assert_eq!(stats.largest, 8192);
        // 4096 was freed before the others; the growth replaced its 16 bytes.
        assert_eq!(stats.peak, 1024 + 8192);
        let ((), idle) = measure(|| { std::hint::black_box(1 + 1); });
        assert_eq!(idle, Stats::default());
    }

    #[test]
    fn another_threads_allocations_are_not_counted() {
        let ((), stats) = measure(|| {
            // The spawn itself allocates here; the worker's megabyte does not count.
            std::thread::scope(|scope| { scope.spawn(|| drop(std::hint::black_box(vec![0u8; 1 << 20]))); });
        });
        assert!(stats.largest < 1 << 20, "{stats:?}");
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn an_unwinding_measurement_stops_counting() {
        assert!(std::panic::catch_unwind(|| measure(|| panic!("generated"))).is_err());
        let ((), stats) = measure(|| drop(std::hint::black_box(vec![0u8; 64])));
        assert_eq!((stats.allocations, stats.largest), (1, 64));
    }
}
