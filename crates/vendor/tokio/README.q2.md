# tokio 1.54.1, patched: one thread wakeup per timer expiry

The Q2 patch avoids a separate wakeup at an intermediate timer-wheel slot before the timer expires.

| Item | Value |
| --- | --- |
| Base | tokio 1.54.1 from crates.io: `.cargo_vcs_info.json` (upstream commit `b53b2001ed2ccfd9b87e32a0c6da1a1eebdc80b8`, path `tokio`), crate SHA256 `b52fd3255258d26fac7264ab5a76b53c13367136dc011d3d6ad4f8ed070396a3` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-timer-single-wake.patch` (the whole difference in `src/`, `diff -ru` against the base) |
| Licence | MIT, `LICENSE` (unchanged) |
| Wired by | `[patch.crates-io] tokio = { path = "crates/vendor/tokio" }` in the workspace `Cargo.toml`, so every dependent crate uses it |
| Location | under `crates/` because the image manifest, the app build and the release tooling identify the app source as `crates`, `Cargo.toml` and `Cargo.lock` |

Changed lines are marked `[q2 patch]` in the source.

## Cause of the second wakeup

The time driver is a six-level hashed wheel (1 ms, 64 ms, 4 s, 4 min, 4 h,
12 day slots). A timer is stored at the level of the highest bit in which its
deadline differs from the wheel's elapsed time. `Driver::park_internal` slept
until `Wheel::next_expiration_time()`, which is the start of the next
occupied slot. For a slot above level 0 that instant is not a deadline: it is
when the slot's entries are moved down one level. The thread woke, moved the
entries, found nothing due and slept again until the deadline.

## The patch

`Wheel::next_wake_time` returns the earliest deadline stored in the next
occupied slot (a walk of that one slot's list) instead of the slot's start,
for levels 1–4. `park_internal` sleeps until then, and `process_at_time`
stores the same value as the promised wake so a new earlier timer still
unparks the driver. `Wheel::poll` already performs every cascade that is due
when it is called late, so no other code changes. The argument that the
earliest entry of that slot is the earliest timer in the wheel, and why the
top level keeps the slot start, is in the doc comment of `next_wake_time`.

Cost: one pass over the entries of one slot per park. No timer fires later
than before; a timer reset to a later time may wake the thread at its old
deadline once, as upstream does.

A timerfd would not remove the wakeup: the wake at the slot start is asked
for by the wheel, whatever delivers it.

## Checks

The timer-wheel unit tests cover `next_wake_time`. The player test
`rt::tests::timers_wake_the_thread_once_and_async_work_never_stalls`
measures wakeups with one-shot timers, intervals, concurrent timers and
cancellation in an isolated child process.
