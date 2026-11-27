# Navidrome server status

The player marks Navidrome unavailable only on evidence: a failed health
probe after a failure, three foreground failures in a row, or a transport
that is down. One slow or failed request leaves it online, so a request that
times out behind a loaded Tailscale proxy does not stop downloads or turn
artwork off. Requirements SERVER-001 and SERVER-002 are in the
[specification](specification.md#network).

The machine is `q2_machines::server`
([`crates/machines/src/server.rs`](../crates/machines/src/server.rs)). Its
shell is [`library/health.rs`](../crates/q2nd/src/library/health.rs), and
the UI loop reports the transport (`observe_transport` in
[`ui/mod.rs`](../crates/q2nd/src/ui/mod.rs)).

## Signals

| Signal | Source | Meaning |
| --- | --- | --- |
| Request outcome | every Navidrome request (`Library::observe`) | Success, or a failure class (`subsonic::Failure`): timeout, refused or DNS, proxy, server error (5xx), other network failure. Auth and Subsonic errors are answers and say nothing about reachability. A request the player cancels reports nothing. |
| Priority | `library::with_priority` scope around the request; foreground outside a scope | Foreground: the listener waits (a page with nothing cached, a song starting, an action). Background: downloads, fetch-ahead, queued plays, queue saves, sync rounds. Low: cover prefetch, refreshes behind cached rows (Cover Flow's album list, any list page with a cache), nearby-album warming. |
| Transport | UI loop, every tick | Down when Wi-Fi is off, when Wi-Fi has had no IPv4 address for 10 s outside a doze and its 60 s reconnect allowance, or when Tailscale (if enabled) is known stopped. Up with an address and Tailscale running. A Tailscale status read that fails gives no answer, and the transport keeps its last value. Simulated Wi-Fi is always up. |
| Probe | `Library::probe` | Subsonic `ping`, at most two attempts of 8 s each, 1 s apart, one probe at a time. A ping takes the idle pooled connection or a new one, so it never waits behind another response. It waits out Offline Mode and a radio doze. |

## States

The machine holds the status, the foreground failures since the last
success (at most 3), whether a probe runs and whether the transport is down.
The state and a reset generation share one atomic word, so each event is one
compare-and-swap and no lock is taken. A probe's result counts only for the
generation it started in.

| Status | Requests | Shown |
| --- | --- | --- |
| Online | go to the server | Settings → Connections → Navidrome Server: Connected |
| Checking | go to the server; downloads, artwork and fetch-ahead continue | Settings: "Checking…"; the status bar is unchanged |
| Offline | pages fall back to the card; downloads, network artwork, fetch-ahead and album warming stop | status bar: "Navidrome unavailable", or the Wi-Fi or Tailscale badge when that link failed; Settings: Unavailable; a page with nothing cached names the cause ("Can't reach Navidrome", "The server took too long", "The server had a problem") |

Offline Mode and a radio doze show "Offline" in Settings, not the server's
status. The player starts Offline (not yet reached), and the first answer
makes it Online.

| Event | From | To |
| --- | --- | --- |
| Any answer, or a probe that got one | any | Online, failures 0 |
| Foreground or background failure | Online | Checking; the probe starts |
| Third foreground failure in a row | Online, Checking | Offline |
| Probe failed | Checking | Offline |
| Probe failed after an answer in between | Online | Checking; another probe |
| Low-priority failure | any | unchanged |
| Transport down | any | Offline; no probe until it is back |
| Transport up | Offline | the probe starts |
| Offline cadence (sync worker, every 20 s) | Offline with a transport | the probe starts |
| Settings → Connections opened, not in Offline Mode or a doze | Online | Checking; the probe starts |
| Settings → Connections opened, not in Offline Mode or a doze | Offline with a transport, no probe running | the probe starts |
| Server or account changed | any | Offline (not yet reached) |

Opening Connections probes, so it never shows Connected for a server that
stopped answering after its last request. With no requests and Settings
closed nothing is measured: the next request or a transport change decides.

The machine's tests check every state and event, and a model check explores
every reachable state. Five Kani proofs show that no single failure takes an
online server offline, a low-priority failure changes nothing, any answer is
Online, Offline is entered only on the evidence above, and, over every
sequence of 8 events, at most one probe runs and Checking always has one:

```sh
cargo kani -p q2-machines --harness server::
```

## Thresholds and bounds

- One failure is never enough: a single request can time out under load.
- The probe is a second, independent piece of evidence. Two attempts, so one
  lost ping does not decide. 8 s per attempt allows a loaded proxy and stays
  under the 20 s read timeout of other requests.
- Three foreground failures in a row, with no answer between them, go
  offline without waiting for the probe.
- One answer brings the server back.

A server that stops answering goes offline at the latest:

- at once when the transport goes down;
- about 1 s after a refused connection or a name that does not resolve
  (both pings fail at once, 1 s apart);
- about 41 s after a foreground page request is sent to a host that never
  answers: 4 s on the kept connection, 20 s on a new one, then two 8 s pings
  and the 1 s pause.

## Low-priority traffic

- Artwork loads on two tasks of the I/O runtime: one for requested covers,
  one that warms Cover Flow's window.
- A cover request is limited to 10 s in all, and its failure never counts
  against the server.
- Prefetched covers (Cover Flow's warming, upcoming songs) wait while a
  foreground request is in flight, the probe runs or a player stream is
  downloading. Covers being drawn do not wait.
- Downloads yield between chunks to playback, foreground requests and the
  probe. Fetch-ahead yields to foreground requests and the probe, but not
  while a player stream downloads, because the player may have joined that
  transfer.

## Memory

The Q2 has 118 MiB of RAM, and the player locks its own code in memory
([`platform/resident.rs`](../crates/q2nd/src/platform/resident.rs)). The
decoded-art cache competes with the file pages that the Tailscale daemon
runs from, and every proxied request slows when those pages are evicted.
The cache limits keep it small:

| Limit | Size | Holds |
| --- | --- | --- |
| Decoded art in memory | 3 MiB | Cover Flow's drawn and warmed window (about 25 covers of 150 px, 88 KiB each) and Home's 320 px slides (400 KiB each) |
| After Cover Flow closes | 1 MiB | Now Playing's cover and a list's thumbnails |
| While the screen is off | 128 KiB | the most recent Now Playing cover and its thumbnail |

Anything evicted reloads from the card's PNG cache. When Cover Flow closes,
the player shrinks the cache and runs `malloc_trim(0)` on a blocking worker.
The player also trims after the screen goes off (at most every 10 minutes,
reported to telemetry) and after a server document of 256 KiB or more. The
heap uses one malloc arena and a fixed 128 KiB mmap threshold, so large
transient buffers such as cover decodes return to the kernel when freed.

### Measuring memory on the host

Two headless runs measure the player's memory over generated libraries
served by `tests/ui/ux_fault_fixture.py`:

```sh
cc -O2 -shared -fPIC -o build/heapcount.so tests/perf/heapcount.c -ldl
python3 tests/perf/document_memory.py --binary build/native-test-target/debug/q2nd --label NAME
python3 tests/perf/coverflow_memory.py --binary build/native-test-target/debug/q2nd [--albums 3000]
```

- `document_memory.py` browses Artists, a large playlist, Favorite Songs
  and Albums (2,000 artists, 5,000 playlist songs, 2,000 starred songs and
  3,000 albums by default) twice, with `heapcount.so` preloaded. Per script
  step it reports the live heap, the heap peak since the previous step
  (every allocation counted), the glibc arena, decoded art and RSS.
  `--play` adds playing the playlist; `--screen-off S` adds the screen-off
  trim.
- `coverflow_memory.py` scrolls Cover Flow past `--covers` albums and
  reports what stays resident after it closes and what the trim releases.

Each run is a measurement, not a comparison. Across runs of the same
binary and library the live heap at a step repeats within about 40 KiB and
the peak within about 200 KiB (it depends on when concurrent work
allocates), so one run describes a binary to that resolution; timings vary
more. The numbers are the host's (x86-64 pointers
and allocator); a record's pointers and lengths are half as large on the
Q2, and decoded-art bytes are the same. Results are in `result.json` under
`build/perf/`.

## Sync requests and bytes

`tests/perf/sync_host_measure.py` runs the ignored Rust test
`measure_sync_through_proxy` through a counting proxy: the album catalog
built twice (full, then unchanged) and, with `--abs`, every Audiobookshelf
library synced twice and its stored pages compared with the server's. Per
phase it reports requests, status codes and body bytes by endpoint, the time
spent upstream, and the player's database rows and WAL bytes written.

```sh
python3 tests/perf/sync_host_measure.py --generated 300 --abs --out build/sync/NAME.json
python3 tests/perf/sync_host_measure.py --url http://127.0.0.1:4533 --user q2 --password q2test
```

`--generated N` starts the official Navidrome image, pinned by digest, with
N generated albums of three tracks; `--abs` adds the generated
Audiobookshelf of the [spoken journey](../tests/e2e/README.md). Both run on
internal Podman networks and are removed at the end. `--url` names a
Navidrome that already holds a generated library. Request and byte
counts are exact for one server version and library and stand in for the
radio time a sync costs on the Q2; times are descriptive. `--tree` runs
another checkout's test against the same servers.

## Diagnostics

- UX recorder: `server-failure` (priority and class of each counted
  failure), `server-probe` (attempt and result) and the `navidrome` state
  (1 online, 2 offline, 3 checking). `tools/ux_triage.py` decodes them.
- Player log: `server transport up` or `down`, `server checking`, `server
  reachable`, `server unreachable`, `heap trim after browsing`.

## Tests

- `q2-machines` `server`: the state and event rules, the reachable-state
  model check and the Kani proofs.
- `library::health_tests`, against a generated server: a server that
  answers late and drops requests stays online; one that never answers goes
  offline within the probe bound; a refused server goes offline at once;
  three foreground failures go offline before the probe ends; a recheck
  finds a server that failed while nothing asked; transport down and back;
  a cancelled request is not evidence; background transfers yield to a
  waiting request and the probe.
