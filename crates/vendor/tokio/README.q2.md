# tokio 1.54.1, patched: one thread wakeup per timer expiry

Requirement: Product feedback, 2026-11-12, `docs/design-intent.md`, "Patch tokio's timer
to one wakeup per expiry".

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

A 250 ms sleep started at elapsed time 21 ms has deadline 271: level 1, slot
256–319. Measured with `tools/tokio-timer/epoll_trace.c` on the host:

```
upstream  epoll_wait(timeout=235) = 0 after 235.2 ms   <- wakes at 256, the slot start
upstream  epoll_wait(timeout=16)  = 0 after 16.1 ms    <- wakes at 272, the deadline
patched   epoll_wait(timeout=251) = 0 after 251.3 ms
```

A 1 s sleep usually crosses one level (two wakeups) and sometimes two
(three); a 50 ms sleep crosses a 64 ms boundary about three times in four.
That is the 1.75 / 2.0 / 2.2 wakeups per expiry measured in stage 1.

Not the cause: millisecond rounding (the deadline is rounded up once, and
mio rounds the `epoll_wait` timeout up), and the `unpark` a timer
registration sends when it is earlier than the promised wake. That `unpark`
makes the next `epoll_wait` return at once without sleeping (visible in the
trace as `= 1 after 0.0 ms`): two extra system calls per registration, no
context switch. It is left as upstream has it.

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

- tokio's own unit tests of the time driver and wheel, plus two added for
  `next_wake_time` (`src/runtime/time/wheel/mod.rs`): 62 library tests pass
  in a scratch copy built with `rt,rt-multi-thread,time,test-util,macros`.
  The registry cache lacks `tokio-test`, `mockall`, `futures-test`,
  `async-stream`, `futures-concurrency` and `nix`'s `memoffset`, so the
  scratch copy drops those dev-dependencies and the three `sync` test modules
  and the `task::coop` test module that need `tokio-test`; tokio's
  integration tests (`tests/`) were not run.
- `cargo test`, `rt::tests::timers_wake_the_thread_once_and_async_work_never_stalls`:
  wakeups against expiries for 250 ms and 1 s sleeps, an interval, eight
  concurrent timers and cancelled timers, measured in a child process (in
  the shared test process kernel lock contention adds voluntary switches).
  It fails when `park_internal` uses `next_expiration_time` again: 12
  wakeups for 6 expiries.
- `tools/tokio-timer/measure.py --gate`: the same program linked against
  upstream and against this copy, on the host and under `qemu-mipsel`;
  upstream must show the second wakeup.
