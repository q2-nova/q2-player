//! The witness that a function is allowed to block.
//!
//! A function that sleeps in the kernel for card, process or device I/O
//! takes `&Blocking`. The only ways to get one are [`super::spawn_blocking`]
//! (a pool thread) and [`Blocking::dedicated_thread`] (the entry of a thread
//! that exists to block). Construction rejects marked executors and the
//! token cannot cross threads. It does not prevent retaining a token while
//! entering a local executor: reviewed owner/caller boundaries and runtime
//! guards supply that separate protection. `tools/test_async_gates.py`
//! checks construction and thread-transfer failures.
//!
//! This file depends on `std` only.
use std::cell::Cell;

thread_local! {
    /// Set on a thread while it polls a runtime.
    static EXECUTOR: Cell<bool> = const { Cell::new(false) };
    /// Set on a blocking-pool thread while it runs a job.
    static POOL: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as running a pool job until the guard drops.
pub(super) struct PoolGuard(bool);

pub(super) fn enter_pool() -> PoolGuard {
    PoolGuard(POOL.with(|p| p.replace(true)))
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        POOL.with(|p| p.set(self.0));
    }
}

/// Whether the calling thread is running a blocking-pool job now. A job
/// must not wait for async work: that work may need a pool thread itself
/// (DNS does), and the pool has three.
pub fn on_pool() -> bool {
    POOL.with(Cell::get)
}

/// Whether the calling thread may wait for a task of the I/O runtime: not on
/// a runtime thread and not in a pool job.
pub fn may_wait_for_tasks() -> bool {
    !on_executor() && !on_pool()
}

/// Marks the calling thread as polling a runtime until the guard drops.
pub(super) struct ExecutorGuard(bool);

pub(super) fn enter_executor() -> ExecutorGuard {
    ExecutorGuard(EXECUTOR.with(|e| e.replace(true)))
}

impl Drop for ExecutorGuard {
    fn drop(&mut self) {
        EXECUTOR.with(|e| e.set(self.0));
    }
}

/// Whether the calling thread is polling a runtime now.
pub fn on_executor() -> bool {
    EXECUTOR.with(Cell::get)
}

/// A thread-bound witness minted at an admitted synchronous owner boundary.
pub struct Blocking {
    // A witness belongs to the thread executing this synchronous section.
    // A pool result must be Send; refusing both Send and Sync prevents moving
    // the token out or sharing a borrowed token with another thread.
    _private: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Blocking {
    /// For [`super::spawn_blocking`] only.
    pub(super) fn new() -> Blocking {
        // The deployed build must reject a mistaken mint too. !Send/!Sync
        // protects thread transfer, not entry on a current-thread executor.
        assert!(!on_executor(), "a blocking section was entered on a runtime thread");
        Blocking { _private: std::marker::PhantomData }
    }

    /// For the first lines of a dedicated blocking thread (the player, the
    /// database owner). `tools/async_inventory.py --coverage` fails for a
    /// call outside the functions listed in `docs/async/blocking-allow.toml`.
    #[allow(dead_code)] // Also compiled alone by the witness negative controls.
    pub(crate) fn dedicated_thread() -> Blocking {
        Blocking::new()
    }
}
