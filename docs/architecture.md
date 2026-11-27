# Player architecture

[`q2nd`](../crates/q2nd/) is the native player. It owns navigation, playback,
the local library and the client connections to Navidrome and Audiobookshelf.
The [OS](https://github.com/q2-nova/q2-os) supplies the device platform,
supervision, networking helpers and installation. The browser demo shares the
renderer, settings and playback decisions with the native player.

## Execution

| Thread or pool | Work |
| --- | --- |
| Main thread | Runs the UI's current-thread executor, handles input messages, updates the screen stack and draws. |
| `player` | Owns the playback cursor, decoding, PCM output and interruption of audio reads. Spoken packet read-ahead can use a separate `spoken-demux` thread while its source is open. |
| `io` | Runs a current-thread asynchronous executor for sockets, timers and readiness. Wi-Fi monitoring, evdev reads, network streams, artwork, page requests and the Tailscale supervisor run here as tasks. |
| I/O blocking pool | Up to three on-demand workers for blocking work such as transfer writes and DNS. |
| Finite jobs | A separate on-demand pool of up to three workers for finite local work. Isolated single-worker lanes cover operations such as evdev discovery and close. |
| `db` | Owns SQLite and serializes submitted library operations. |
| `bluetooth` | Starts when Bluetooth is enabled. Owns the controller transport and sends encoded audio. |
| `ts-spawn` | Starts when Tailscale supervision needs to spawn its child process. The OS supplies the daemon executable, which runs as a separate process. |

The [runtime](../crates/q2nd/src/rt/mod.rs) separates UI, asynchronous I/O and
blocking work. Stream and page tasks share the I/O executor. Input dispatch
continues on the UI thread after evdev readiness delivers events. The
[state machines](../crates/machines/) hold transitions for playback, output,
storage, radio doze, sleep and USB. The platform and service owners apply
their effects.

### Runtime rules

- Spawn tasks with `rt::Runtime::spawn`. It times every poll, so the stall
  detector ([`rt/stall.rs`](../crates/q2nd/src/rt/stall.rs)) logs a task
  that holds its executor for 50 ms or more.
- Use `tokio::time` for timers. The workspace patches tokio to branch
  `q2/1.54.1` of [q2-nova/tokio](https://github.com/q2-nova/tokio), which
  wakes the runtime thread once per timer expiry.
- Do blocking work (card, process and device I/O, sleeps, waits) only in
  functions that take `&rt::Blocking`. Only `rt::spawn_blocking` (a pool
  thread) and `Blocking::dedicated_thread` (the start of a thread that
  exists to block) hand out a witness.
- Do not hold a `std` lock or a `RefCell` borrow across an `.await`.
- Do not use `AtomicU64` or `AtomicI64`. The Q2 target has 32-bit atomics
  only.

The [lint workflow](../.github/workflows/lint.yml) checks the rules it can.
Clippy runs with every lint group allowed, so only the lints the source
denies fail it: `await_holding_lock` and `await_holding_refcell_ref` in the
whole of `q2nd`, and the `clippy.toml` lists of blocking methods and socket
types in `rt` and `net`. A grep rejects 64-bit atomics outside
`crates/vendor`.

The [witness tests](../tests/rt/test_blocking_witness.py) compile
`rt/blocking.rs` alone. They guarantee that a `Blocking` cannot be built
outside `rt`, forged from its fields, returned from a pool job or shared
with another thread, and that construction panics on a thread marked as an
executor, in release builds too. They do not show that a function that
blocks takes `&Blocking`: a blocking call compiles without one, and clippy
catches only the listed methods and types in `rt` and `net`. They also do
not stop code from keeping a witness while it enters a local executor. The
stall detector reports blocking on an executor at run time.

## Library and offline listening

The [library](../crates/q2nd/src/library/) owns the card database, server
response cache, artwork and media files. Cached pages can appear immediately
while a server refresh runs. Navidrome album catalog sync reuses the catalog
until the server's scan marker changes. Audiobookshelf sync checks library
markers and page ETags, with a full resync for changes a marker misses.

The [audio source](../crates/q2nd/src/audio/source.rs) tries a download, then
a suitable cached copy, then a stream. A streamed file can play while it
fills and can be reused from the cache once complete. Fetch-ahead is bounded
by media size, the cache cap and available card space. Explicit downloads
have their own queue. Shuffle Songs can start from a playable download or
cached track while a larger queue is prepared.

Authentication, connectivity and Offline Mode inform whether requests can
reach a server. Offline play reports and other supported server mutations
are queued for retry. The player restores its saved queue paused at start.
Books and podcasts have persistent positions and separate display and
playback controls.

## Playback and platform

The [player engine](../crates/q2nd/src/audio/player.rs) handles commands,
opens a source and decodes packets into PCM. A command can interrupt an audio
read. Compatible tracks can transition without reopening the PCM, and paused
output closes after its idle window. The [mixer](../crates/q2nd/src/audio/mixer.rs)
configures the headphone output. The PCM path applies software volume and
ReplayGain and can include headphone correction and graphic EQ.

On the installed Q2, `--mainline` uses [DRM/KMS and the Linux device
interfaces](../crates/q2nd/src/platform/drm_test.rs), with ALSA output at
`hw:Q2,0`. For development, `--sim` opens a desktop window and
`--headless` can drive an in-memory display from a script. The QEMU
fixture can use `--drm-test` for KMS exercises. The player and connection
portal are built together for the installed system. [Target builds](target-build.md)

The player uses hardware-capable MIPS SIMD paths where supported, and
the vendored decoders include bounded reads and damaged-file controls.
[Decoder fixtures](../crates/q2nd/src/audio/decoder/fixtures/README.md)
· [SQLite build options](../q2/toolchain/libsqlite3-flags)

## Interface and diagnostics

The [UI](../crates/q2nd/src/ui/) owns a screen stack, status bar, menus and
settings. The renderer updates the visible rows and renders to a 375 × 320
canvas. Long alphabetical lists support alphabet navigation and artwork
draws from cached sizes when offline. Page loads and user-started actions
have their own scheduling lanes so background refreshes can yield.

The maintained [UI hierarchy](../tests/ui/ui-hierarchy.md) records the menus
and cross-screen actions covered by the generator. The
[first-listen fixture](../tests/e2e/README.md) exercises local playback,
pause and a second run in Offline Mode.

The [UX recorder](../crates/q2nd/src/ux.rs) records bounded numeric events
for failures and listening journeys. The player presents concise recovery
messages and keeps detailed failure codes in diagnostics. **Settings →
System → About → Report a Problem** records a diagnostic marker when that
feature is enabled. Playback timing instrumentation is opt-in through
`[diagnostics] playtrace = true`.
