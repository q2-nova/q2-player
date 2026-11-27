# Playback trace

The playback trace records, in RAM, what the player, the output and the cache
transfers did around a stall or an output recovery, as numbered events and
one-second activity windows on CLOCK_BOOTTIME. It is off by default because
its cost on the Q2 is not measured. The code is
[`crates/q2nd/src/playtrace.rs`](../crates/q2nd/src/playtrace.rs).

```toml
[diagnostics]
playtrace = true
```

The key is read once at start. `Q2ND_PLAYTRACE=1` or `=0` in the player's
environment overrides it; the installed launcher does not set it.

The trace records numbers and fixed names only. Serials are counters the
trace assigns; no song ID, title, path, address, server name or log text
enters it.

## Records

Two fixed rings of 512 records each, one of events and one of windows, at
most 96 KiB together. An overwritten record is counted in the ring's `lost`
total.

### Events

Each event has a sequence number, a CLOCK_BOOTTIME time in microseconds and
three numbers `a`, `b`, `c`.

| Event | Meaning | a | b | c |
| --- | --- | --- | --- | --- |
| `request` | `PlayerHandle::send` queued a play-control command | command | request serial | |
| `handled` | the player thread began applying it | command | request serial | |
| `open` | a track's source opened and its format was probed | track serial | origin | source ms << 16 \| probe ms, each at most 65535 |
| `first-pcm` | the decoder returned the first frames after a request or open | track serial | request serial | |
| `first-submit` | the output accepted the first frames after it | track serial | request serial | audio already queued, ms |
| `paused` | the output's pause call returned | serial of the pause or play-pause request; 0 for another cause (sleep, output fault, failed save) | 1 if queued audio was kept | frames held |
| `resumed` | the output's resume returned | serial of the play, play-pause or resume request; 0 after an output route change | | |
| `status` | a status with a changed play state was stored and the UI notified | request serial | state | track serial |
| `track-end` | the track ended | track serial | 1 end of file, 2 closed, 3 output released | |
| `recovery` | ALSA write recovery, also logged as `ALSA_WRITE_RECOVERY` | errno; 0 for a recovery the output counted but did not keep | audio queued before the write, ms | track serial |
| `slow-read` | the file part of one source read reached 20 ms, or its wait for download or network bytes reached 100 ms | read µs | of which waiting for bytes | track serial |
| `slow-decode` | one packet's decode took 20 ms or more | µs | | track serial |
| `slow-write` | one output write blocked 300 ms or more | µs | audio queued before it, ms | track serial |
| `low-buffer` | queued audio fell under 100 ms, once per crossing | ms | | track serial |
| `write-gap` | 300 ms or more from the end of one output write to the start of the next, with no playback request, pause or resume between | µs | audio queued before the later write, ms | track serial |
| `sink-open`, `sink-close` | the PCM opened or was released | rate | channels | |
| `xfer-start` | a cache transfer started | transfer serial | 1 player stream, 0 fetch-ahead | |
| `xfer-join` | a reader joined it | transfer serial | readers | 1 if it was orphaned |
| `xfer-orphan` | its last reader left | transfer serial | percent (255 unknown) | KiB written |
| `xfer-cancel` | cancel requested | transfer serial | percent | KiB written |
| `xfer-end` | its transport ended | transfer serial | 1 complete, 2 stopped by cancel, 3 failed | KiB written |
| `cache-publish` | the completed file was renamed into the cache and indexed | transfer serial | 1 published, 2 rename failed, 3 index failed | KiB written |
| `fetch-ahead`, `fetch-ahead-end` | a fetch-ahead burst began or ended | songs; or 1 complete, 2 newer request, 3 offline | | |
| `slow-card-write` | one transfer write to the card blocked 50 ms or more | µs | KiB written so far | |
| `doze` | the radio went down for cached playback | audio queued, ms | | |
| `wake` | the radio was asked up | reason | audio queued, ms | |

Commands: 1 play, 2 play-pause, 3 resume, 4 pause, 5 next, 6 previous,
7 jump, 8 seek-by, 9 scrub, 10 volume. Origins: 1 offline, 2 cached,
3 streaming, 4 radio. States: 0 stopped, 1 playing, 2 paused, 3 buffering.
Wake reasons follow `q2_machines::doze::WakeReason` from 1: 1 doze disabled,
2 Wi-Fi off or Offline Mode, 3 screen on, 4 AirPlay, 5 queue stopped, 6 low
audio ahead. `xfer-end` outcome 1 does not mean the file entered the cache;
`cache-publish` records that.

`slow-read`, `slow-decode`, `slow-write`, `low-buffer` and `write-gap` are
each limited to one event a second, so a radio or a starving stream cannot
fill the ring. The windows keep the maxima, and the track summary's
`ev_limited` counts what was withheld. Radio reads and audiobook range reads
count wholly as waiting for network bytes. Requests, completions,
recoveries and transfer events are never limited.

### Volume events

A volume change adds numbered records under the serial of its volume
command (0 for an automatic gain change). They bound the player's own work,
not when the listener hears the change.

| Event | a | b | c |
| --- | --- | --- | --- |
| `volume-begin` | serial | slider 0–100 | delay of the previous write, frames; 4294967295 unknown |
| `volume-plan` | serial | planned attenuation, half-dB steps | flags; bits 8–15 the planned codec attenuation |
| `volume-gain` | serial | old software gain, f32 bits | planned software gain, f32 bits |
| `volume-phase` | serial | phase; bit 31 set on a successful return | retained frames at return, or 4294967295 |
| `volume-end` | serial | 0 on success, else the phase that failed | 0 success, 1 early return |

Flags: 1 headphones, 2 retarget of running retained PCM, 4 already paused,
8 hardware volume path, 16 software gain unchanged, 32 running PCM ramp,
64 mixed codec and PCM ramp. Phases: 1 plan, 2 pause, 3 codec, 4 retarget,
5 resume, 6 parked or pending output rebase. A volume command does not count
as a playback start: it records no `first-pcm` or `first-submit` and does not
end a write gap.

### Windows

A window summarises one second in which the player decoded or a transfer
wrote. Columns, as in the dump's `# W` line:

`seq t_us dur_us packets frames rate read_sum_us read_max_us fill_sum_us fill_max_us decode_sum_us decode_max_us render_max_us write_sum_us write_max_us buf_min_ms buf_last_ms ahead_min_kib xfer_bytes xfer_active card_write_max_us dirty_kb writeback_kb gap_max_us`

`fill` is the part of source reads spent waiting for download bytes,
`render` the EQ, gain and tempo work before a write, `buf` the audio queued
before a write, `ahead_min_kib` the least downloaded-but-unread data ahead
of the playing stream, and `dirty_kb` and `writeback_kb` the newest
`/proc/meminfo` sample a transfer thread took in the window. `gap_max_us`
covers everything the player thread did between two writes, including work
that has no timer of its own, so a stall with no slow read or decode still
shows. A measurement belongs to the window in which it completed, so a stall
longer than a second appears in the window that starts when it ends.

## Endpoints

Play, pause and skip have separate request and completion endpoints; report
them separately, not as one latency.

- `request` → `handled`: the command waited for the player thread, which may
  have been in a read, a decode or an output write.
- `handled` → `first-pcm`: source open, probe and first decode.
- `first-pcm` → `first-submit`: DSP and the output accepting frames.
  `first-submit` is submission to ALSA; sound follows no earlier than its
  `c` plus the device's own latency.
- `paused`, `resumed`: the ALSA pause or resume call returned.
- `status`: the status the UI reads changed and its notify callback ran. The
  drawn frame comes later and is not recorded.

None of these observes the analog output.

## Clocks

Every timestamp is CLOCK_BOOTTIME, the base of `metrics::uptime`, the `t=`
of the player log and `/proc/uptime`, so a rig aligns the trace with its own
samples. Decode, render, write and gap durations are differences of those
timestamps. Source-read durations use `Instant` (CLOCK_MONOTONIC), which
stops in suspend: a suspend inside a read is missing from the read and
present in the write gap that spans it. The `ALSA_WRITE_RECOVERY` log line
and the `recovery` event share one clock reading.

## Cost

Off, the player's added work per packet is a few atomic loads: no lock, no
source wrapper and no clock read (a test asserts the last). On, the trace
owns no timer and no polling thread; a paused or idle player records
nothing, and windows advance only on a decode or a transfer write. One mutex
guards the rings, and only fixed-size memory updates happen under it: no file
I/O, allocation or formatting. Transfer threads read `/proc/meminfo` outside
it, at most once a second. The host test
`overhead_per_packet_is_bounded_on_the_host` fails above 50 µs a packet
enabled or 5 µs disabled.

## Getting it out

**Dump on request.** With the trace on and `Q2ND_RUNTIME` set (`/run/q2nd`
on the Q2), a write to the FIFO `$Q2ND_RUNTIME/playtrace.ctl` makes the
player write `$Q2ND_RUNTIME/playtrace.dump`, mode 0600, replaced atomically.
The dump never goes to the log. At start the player removes any
`playtrace.ctl`, `playtrace.dump` and `playtrace.dump.tmp` left in the
runtime directory, then makes the FIFO when the trace or the numeric metrics
recorder is on. With the trace off, a write to the FIFO produces no dump. A
writer must guard itself:

- write only if the path is a FIFO (`[ -p ]`); otherwise `echo >` creates a
  regular file;
- write under a timeout, since opening a FIFO blocks while no reader has it
  open, as when the player has exited;
- the dump appears after the write returns: read `now_us` from the old
  header first, poll until the file's `now_us` is larger, and check the
  `end` line.

```sh
ctl=/run/q2nd/playtrace.ctl; dump=/run/q2nd/playtrace.dump
old=$(sed -n '1s/.*now_us=\([0-9]*\).*/\1/p' "$dump" 2>/dev/null); old=${old:-0}
[ -p "$ctl" ] && timeout 2 sh -c "echo dump > $ctl" || echo "playtrace unavailable"
n=0
while [ "$n" -lt 20 ]; do
    new=$(sed -n '1s/.*now_us=\([0-9]*\).*/\1/p' "$dump" 2>/dev/null)
    [ "${new:-0}" -gt "$old" ] && break
    n=$((n + 1)); sleep 0.1
done
```

Dump format, one record per line:

```
playtrace 1 pid=PID now_us=T enabled=0|1 window_us=1000000
events capacity=N total=N lost=N
E seq t_us kind a b c
windows capacity=N total=N lost=N
# W seq t_us dur_us ...
W seq t_us ...
P seq t_us ...
end EVENT_LINES WINDOW_LINES
```

`P` is the window still open; `-1` is an absent value. The `end` line counts
the event and window lines, so a truncated file is detectable. The window
ring holds 512 seconds of activity: dump at least every eight minutes of
playback to lose no window.

**One line per track** in the player log when a track ends or the next one
opens:

```
PLAYTRACE_TRACK v=1 t=SECONDS.MS track= origin= reason= dur_ms= packets= frames= rate= read_max_us= read_sum_ms= fill_max_us= decode_max_us= decode_sum_ms= write_max_us= gap_max_us= buf_min_ms= recoveries= slow_reads= ev_limited= ev_lost= win_lost=
```

`t` is boottime. `buf_min_ms` is the least audio queued when a write began.
`ev_lost` and `win_lost` are the rings' overwrite counts so far.

## Tests

- `playtrace::tests`: ring overflow and loss counts, windows, the disabled
  path, the configuration key and environment override, the summary line,
  endpoint serials, event rate limits, download wait against file read, the
  FIFO dump and removal of stale files, the overhead bound.
- `audio::player::playtrace_tests`: through the real player thread, decoder
  and a fake PCM, an injected read stall empties the queue and the trace
  places the recovery after it, inside the write gap; pause, resume and skip
  reach their endpoints in order.
- `library` tests: a skip traced as start, orphan, cancel and end of one
  transfer.
