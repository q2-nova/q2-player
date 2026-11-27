# Telemetry

Telemetry is off unless `config.toml` turns it on. When on, the player writes
numeric reports to a bounded outbox on the device and sends them over HTTP to
a receiver while Wi-Fi is already connected. No public receiver exists: the
receiver is [`tools/diagnostic_receiver.py`](../tools/diagnostic_receiver.py),
run by whoever wants the reports. Requirements TELEMETRY-001 to
TELEMETRY-006 and WAKE-TELEMETRY-001 are in the
[specification](specification.md#errors-telemetry-and-budgets). The code is
`crates/q2nd/src/diagnostics.rs` and its modules, `ux.rs`, `ux/bluetooth.rs`
and `platform/memory.rs`.

## Turning it on

```toml
[telemetry]
enabled = true
```

The player reads the section once at start; there is no Settings row.
`endpoint` and `token` name the receiver: `endpoint` is a base `http://` or
`https://` URL with no path, credentials, query or fragment, and `token` is
64 hexadecimal characters. Without them the player reads
`/state/diagnostics/transport.json`, `{"endpoint": "...", "token": "..."}`.
With neither, it logs `Numeric diagnostics unavailable` and records nothing.
An invalid section disables telemetry.

`stream` is `production` (the default) or `automated-test`. Test runs set it
through an overlay, `/run/q2/telemetry-test.toml` on the device or the file
named by `Q2ND_TELEMETRY_OVERLAY`, whose `[telemetry]` keys replace the saved
ones for that run and never reach a saved `config.toml`. A missing overlay is
ignored; an invalid one disables telemetry. On the device,
`q2nd --telemetry-context-reload NONCE` (32 lowercase hexadecimal
characters) asks the running player, over `/run/q2/telemetry-test.sock`, to
re-read the overlay. It prints a receipt with the process identity and the
applied stream once the change is in effect. Reports keep the stream they
were captured under, across retries and restarts.

Turning telemetry off discards the queued reports and crash journals at the
next start, and removes the capture-context marker. Kernel crash archives
under `/state/crash` are separate and stay.

## Reports

Every report carries a random UUID, the SHA-256 of the running executable,
the boot ID, the process ID and start time, and the stream. Reports contain
numbers and fixed codes only: no credentials, addresses, SSIDs, titles, media
IDs, audio, screenshots, ordinary log lines or free text. The one exception
is the advertised name of connected Bluetooth headphones in UX reports.

| `kind` | Recorded when | Contents |
| --- | --- | --- |
| `crash` | A panic or fatal signal (`SIGABRT`, `SIGILL`, `SIGFPE`, `SIGSEGV`, `SIGBUS`) | A fixed marker, written by the handler with one 16-byte `pwrite` to a journal opened before the crash. The next start imports it. Panic text is not recorded. |
| `process-exit` | The OS launcher sees the player exit (`q2nd --telemetry-process-exit`) | The shell's wait status and a reason: `unexpected-exit`, `commanded-stop` or `launcher-cleanup`. A status of 139 is not decoded as `SIGSEGV`. |
| `kernel-archive-observed` | Start finds a kernel crash archive in `/state/crash` | Record and byte counts and the collecting boot. The source boot is null: the archive does not prove which boot crashed. |
| `health` | Start, first frame, screen wake, Wi-Fi association, Tailscale ready, the connectivity badge, audio errors and recoveries, and each screen-off heap trim | One metric and value. |
| `health` with a distribution | Wake and journey latencies | A DDSketch with 1 % relative accuracy in microseconds, at most 64 observations. Adjacent queued reports of the same metric and capture context merge. |
| `memory` | Each screen-off heap trim, at most every 10 minutes, with no wakeup of its own | `/proc/meminfo` gauges, `/proc/vmstat` reclaim and refault counters with their change since the last sample, and for the player and its Tailscale daemon resident and peak memory, major faults and `smaps_rollup`; the player's heap and decoded-art bytes; how long the reads took. A field the kernel lacks is null. |
| `ux` | A shown error or automatic recovery, a slow, failed or timed-out journey, a state stuck for 3 s, a stop the listener did not ask for, or Report a Problem | A fixed code, the journey and its outcome and latency, a numeric state snapshot (Wi-Fi, Navidrome and Audiobookshelf reachability, doze, Offline Mode, screen, play state, page loads, idle time, output route) and the last 64 flight-recorder events. Text is reduced to a 32-bit hash of its non-digit bytes. |
| `stall`, `waiting`, `completed` | An executor stays blocked, or a slow UI or PCM operation (emulator diagnostic build only) | See below. |

Journeys with distributions are `shuffle-to-audio`, `shuffle-albums-to-audio`,
`music-play-to-audio`, `spoken-play-to-audio`, `resume-to-audio`,
`skip-to-audio`, `page-open` and `info-open`; wake times are
`input-to-present` and `resume-return-to-present`. These are software
boundaries: a frame handed to the display, or PCM accepted by the output.
Output underruns are reported as `recovered` with code `output`, at most one
every 10 s.

While Bluetooth is on, each UX report carries a `bluetooth` object: link and
codec state, controller and UART counters, and the headphones' class of
device, Device ID record and advertised name (printable characters, at most
64 bytes). Never a Bluetooth address. The fields are listed in
[Bluetooth telemetry](bluetooth.md#telemetry).

### Stall reports

The stall sampler exists only in the emulator diagnostic build
([build it](target-build.md#emulator-diagnostic-build)). It records the
runtime, thread, task, poll and elapsed time of an executor that stays
blocked. Stack frames, as module-relative offsets, are added only when that
build links an unwinder; otherwise `stack_status` is `unavailable`. Release
builds compile no sampler and link no unwinder; their reports keep the stall
fields (`thread`, `task`, `poll`, `frames`, `stack_status`) at their empty
values. In every build, the runtime's poll timer
([`rt/stall.rs`](../crates/q2nd/src/rt/stall.rs)) logs a task that holds its
executor for 50 ms or more; that is a log line, not a report.

## Storage and limits

The outbox is `/state/diagnostics/outbox-v2/` on the device (`diagnostics/`
under the data root elsewhere): at most 256 reports of at most 12 KiB, with
16 places kept for crash and process-exit reports. Crash journals are at most
256 files, and a process publishes at most 64 capture slots. Health, memory
and UX reports pass a queue of 64 entries and at most 64 admissions a minute.
A caller never waits on the filesystem: an admission that finds the capture
lock held by a delivery write moves to a blocking worker (at most 16 at once),
and anything refused for contention, rate, capacity or storage adds to the
`dropped_reports` count carried by later reports.

## Delivery

While telemetry is on and the network is up, delivery runs on the I/O
runtime. It never wakes Wi-Fi. Each attempt posts up to 16 reports and 60 KiB
of one stream to `ENDPOINT/v2/production/reports` or
`ENDPOINT/v2/automated-test/reports` with `Authorization: Bearer TOKEN`, a
five-second timeout and no redirects. A report is deleted only after the
receiver's acknowledgement lists exactly the IDs sent. An attempt follows
the network coming up or a new report, and otherwise comes every 60 seconds.

## Receiver and tools

```sh
python3 tools/diagnostic_receiver.py --out RECEIVER_DIR [--listen ADDRESS] [--port PORT]
```

starts a receiver in a new private directory, on `127.0.0.1:9842` unless
told otherwise. It generates the token in `RECEIVER_DIR/transport-token`;
`--resume` reopens an existing directory with the same address and token. It
keeps production and automated-test reports in separate SQLite stores of at
most 20,000 reports each, refuses new reports when full, stores a repeated
report once and refuses a report whose captured stream does not match its
path. `--summarize production` (or `automated-test`) prints the merged wake
distributions without starting the server.

- `python3 tools/ux_triage.py --store RECEIVER_DIR/production/reports.sqlite [--boot latest]`
  prints UX incidents with decoded state and flight-recorder history, and
  maps text hashes back to string literals in the source. `--memory` prints
  the memory samples.
- `python3 tools/ux_budgets.py --store ...` compares each journey's measured
  quantile with [`ux-budgets.json`](ux-budgets.json) (UX-008).

The counters describe what the player saw. They do not establish a reset
cause, a battery saving or audible latency.
