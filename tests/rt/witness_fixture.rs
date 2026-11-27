// Compiled by tests/rt/test_blocking_witness.py with the product's
// crates/q2nd/src/rt/blocking.rs in place of BLOCKING_RS and one case in
// place of the CASE line. Async code that makes its own witness to call a
// blocking function must not compile.
#![allow(dead_code)]
mod rt {
    #[path = "BLOCKING_RS"]
    pub mod blocking;

    pub fn pool_output<R: Send>(work: impl FnOnce(&blocking::Blocking) -> R) -> R {
        work(&blocking::Blocking::new())
    }

    /// As rt::spawn_blocking does it.
    pub fn on_the_pool(work: impl FnOnce(&blocking::Blocking)) {
        work(&blocking::Blocking::new());
    }
}
use rt::blocking::Blocking;

fn write_to_the_card(_: &Blocking) {}

pub fn task() {
    // CASE
}
