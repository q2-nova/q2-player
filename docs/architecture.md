# Architecture

This page describes the application's structure; where it names a platform, "mainline"
is the installed product and "stock platform" is the earlier card mode
(`--device`, superseded 2026-11-02, deleted 2026-11-12; the paragraphs that
name it are historical).

One process, `q2nd`, with a few threads:

| Thread       | Does                                                                      |
| ------------ | ------------------------------------------------------------------------- |
| main         | The UI loop: input, screens, drawing, settings                            |
| `player`     | The queue, decoding (Symphonia) and the PCM (ALSA)                        |
| `input`      | Reads the keys and the wheel (evdev) on the Q2                            |
| `job0`-`job2`| Load pages from the server or the cache                                   |
| `stream`     | One per song being fetched into the cache                                 |
| `prefetch`   | Fetches upcoming audio in back-to-back bursts (`storage.fetch_batch`, default 4 since V0.56; one song at a time before) and queues its covers/thumbnails |
| `downloads`  | Downloads for offline listening, one song at a time                       |
| `art`        | Cover art: card cache, else the server                                    |
| `metadata`   | Bounded nearby-album prefetch; pauses offscreen and during music fetches |
| `sync`       | Sends plays, loves and ratings made offline; checks for the server        |
| `io`         | The I/O runtime (tokio, current-thread). Since 2026-11-12 it runs the Wi-Fi monitor: wpa_supplicant's state, the network helper, the radio's intent. It replaces the `wifi` thread and the per-action Wi-Fi threads |
| `blocking`   | Up to three pool threads, started on demand: file writes of the I/O runtime's tasks |
| `tailscale`  | Supervises the optional card-local daemon and polls login/connection state |
| `bluetooth`  | Only with Bluetooth on: owns the controller's UART, runs the `q2-bt` host (H5, HCI, L2CAP, AVDTP, AVRCP), encodes SBC from the `q2bt` PCM. Runs when Bluetooth is enabled |
| `playtrace`  | Only with `[diagnostics] playtrace = true`: blocks on a FIFO; writes the playback trace dump when asked |

Everything reaches the UI thread as a `UiMsg` on one channel; the loop sleeps
on it (16 ms while something animates, 250 ms with the screen on, 1 s off).

## Library and cache (`library/`)

`subsonic::Call<T>` names a request and how to read its answer.
`Library::fetch(call, policy)` returns it from the network, from the card's
`responses` table, or the network falling back to the table. Every answer is
stored. A page shows the cached answer at once and the server's when it
arrives (`ui/screens/menu.rs`), so a page browsed once opens offline.
Album pages prefer cached metadata younger than one day. After the wheel rests,
up to eight nearby album IDs are warmed by one worker; requests for the same
album are coalesced. This is incremental prefetch, not a whole-library mirror.
Artwork is requested at its display size and normalized on the art worker.
Its disposable disk cache uses atomic rename without a per-image forced flush.

Library sync:
Alphabetical Albums reads `getScanStatus` (Navidrome's `lastScan`, the song
and folder counts) before and after building the album catalog. A catalog
built under the current marker is reused: an unchanged library costs one
small request and no rows. A marker that moved during a build means the
offsets shifted, and the catalog is built again. A changed marker, or a star
or rating the server acknowledged, makes stored responses from before it
stale for `PreferCache`. A stored response that arrives unchanged renews its
time only. The Audiobookshelf sync has two kinds of round. A marker round
(the six-hour freshness, Sync Now on the connection page) asks each library
for its newest item and item count (`sort=addedAt`), a podcast library also
for its newest episode, and reads the personalized shelves; when no marker
moved since the last complete sync it ends there. When one moved it reads
the Books lists and group lists again (each page with its ETag, so an
unchanged one is a 304), only the shows with new episodes, and the first
latest-episode pages. A full resync (Full Resync, or in the background
while charging once a day, `full_sync_hours`) reads everything, asks again
for covers the server lacked, and removes the pages it no longer reached.
Series, author, narrator and genre pages are built from the stored book
rows, which name their groups; the group lists give the ids. A group the
rows cannot settle (a name with ", ", a series with repeated or missing
sequences) is fetched. Edits that move no marker (a rename) wait for the
next full resync.

Songs open through `Tracks` (the player's `TrackSource`), in this order:

1. a download (`offline/`),
2. a cached copy at the streaming quality or better (`cache/songs`),
3. a new stream: the file is created on the card and played while it fills
   (`audio/source.rs`, `ProgressiveReader`); once complete it is the cached
   copy.

A copy's variant is `raw` (the original) or `mp3-<kbit/s>`. Formats Symphonia
doesn't decode (Opus, WMA, APE, DSD) are always asked for as MP3 320. The
cache keeps under its cap and above the card's free-space reserve, evicting
least recently played first. Downloads move a good-enough cached copy over
instead of fetching again.

Speculative audio prefetch admits only songs with a known duration of at most
10 minutes and a selected-variant size of at most 32 MiB. Originals require a
positive server-reported file size; MP3 transcodes use duration × target bitrate
plus 64 KiB of header/padding headroom. These are metadata-based admission
limits, not a hard transfer cap if the server reports incorrect metadata.
Oversized or unknown-length songs still prefetch their artwork and stream
normally when selected. Explicit offline downloads are unaffected. This policy
skips oversized audio rather than keeping a first-minute partial file; partial
resume would need separate handling for range requests and transcoded streams.

Fetch-ahead also keeps to what the card can hold (`space_admitted`, 2026-11-21):
in queue order, the songs whose expected sizes fit in the smaller of the cache
cap and free space plus the cache minus the reserve. A card at its reserve
with an empty cache fetches nothing ahead; a song still streams when it is
reached. Eviction removes one song at a time, least recently used first, and
stops at the first removal that restores the cap and the reserve. A
free-space query that fails applies the cap only.

Shuffle Songs (UX-004/009) starts from a random downloaded or cached song.
The source candidate reads up to 17 row IDs per local table. At 16 or fewer,
every candidate enters the shuffled file check. For a larger table, the
reader seeks the highest row ID and makes 16 random probes. The selected
local song must have a nonempty file. Online, the player can append the server's 200
random songs once they arrive. The extension belongs to the exact seed
queue so a later play selection takes precedence. Offline, the extension
uses downloaded and cached songs. With
fewer than 3 songs on the card it fetches 3 random songs into the cache, once
per boot and after a shuffle that found none.

Navidrome reachability is a machine (`q2_machines::server`, shell
`library/health.rs`): request outcomes with the waiting listener's priority,
the transport (Wi-Fi address, Tailscale running) and a serialized `ping`
probe decide Online, Checking or Offline. One failure only starts the probe;
cover prefetch and refreshes behind cached rows never count; cover prefetch,
downloads and fetch-ahead yield to waiting requests and the probe.

HTTP connections are pooled per network epoch (`subsonic::http`): a Wi-Fi link
change (SSID or address) or the end of radio doze calls `network_changed()`,
which retires every pooled connection. An idempotent request on a reused
connection that sends no headers within 4 s is retried on a new connection
(UXI-003). The status-bar Wi-Fi badge shows unavailable only after 10 s outside
wpa_supplicant's COMPLETED state while the address is held, so rescans do not
flash it (UXI-002); without an address it changes at once.

Plays (`scrobble`), loves and ratings go to the server at once, or to the
`pending` table while offline; the `sync` thread sends them when the server
answers again. The play queue is saved to the card every 10 s (and to the
server's play queue), and restored, paused, at start.

## Player (`audio/player.rs`)

A loop: take commands; open the current song if none is open; decode one
packet and write it to the PCM. Songs follow each other in the same PCM when
their rate and channels match (gapless; Symphonia trims MP3/AAC padding).
A command cuts a wait for a stream's bytes (`interrupt`), so skipping never
waits on the network. The PCM closes, and the DAC powers down, after 20 s
without playing.

When `[diagnostics] playtrace` is on (off by default), the player, the
output and the cache transfers record bounded numeric telemetry in RAM
(`playtrace.rs`): read, decode and output-write times,
queued audio, recoveries, transfer lifecycle and request/completion
endpoints, on the boottime clock.

Output is S32_LE interleaved through ALSA. On mainline the PCM is `hw:Q2,0`
(four CS43131 DACs) and the volume is software gain applied to the samples,
with ReplayGain; the app sets the DACs to 0 dB at start (`audio/mixer.rs`)
and Headphone Power selects the codecs' full scale and class-H mode, written only
with the headphone PCM closed (kernel patch 0187). Moving the volume into
the codecs' 0.5 dB steps and opening the PCM at the source's bit depth are
requested and not started.

Stock platform (historical, superseded 2026-11-02): output is S32 interleaved
through ALSA's `plughw:0,0` (the CS43131), loaded
with `dlopen`. On the Q2 the volume is the DAC's own (0-100, 0.5 dB steps);
in the simulator and on Bluetooth it is applied to the samples, with
ReplayGain. Optional headphone correction and ten-band graphic EQ run before
that gain/conversion step, with independent bypass, combined-response headroom
and crossfades for live changes.

## State machines (`crates/machines`)

The queue cursor and play state, the power/storage gate, the output sink
lifecycle, the cache transfer slot, the Wi-Fi radio intent, the doze policy
and its telemetry cycle, the sleep/power-off transaction and the USB drive
request are pure `transition(state, event) -> (state, effect)` functions in
the crate `q2-machines`, proved with Kani and checked by exhaustive model
tests. The threads above build the events and carry out the effects.

## UI (`ui/`)

A stack of `Screen`s under a 30-pixel status bar. Most pages are a `Menu` over
a `Source` (`ui/screens/sources.rs`), drawn by `ListView`
(`ui/list.rs`): 44-pixel text rows or 72-pixel rows with 52-pixel art,
velocity-based acceleration and alphabet navigation on long alphabetical album
lists.
Only visible rows are inspected during drawing. Home retains its displayed
cover and preloads upcoming slides, with cached-size fallback when offline.
Drawing is software
(`gfx/`), into a 375x320 canvas that the Q2 platform turns onto its portrait
framebuffer.

Page loads share three background slots (`Ctx::task`); user-started actions and
the page the listener is waiting on use the foreground lane
(`Ctx::foreground_task`, `Ctx::page_task`) and never queue behind them.

### UX telemetry (`ux.rs`)

Every user-visible failure goes through `ux::present`: the screen shows a short
plain message and never an error code or raw error text (UX-006). The fixed
`ux::Code`, journey, a state snapshot (Wi-Fi, Navidrome/ABS reachability, doze,
Offline Mode, screen, play state, busy page slots) and a lock-free ring of the
last 64 numeric events go to the `ux` telemetry report. Journeys (Shuffle, Play,
Resume, Skip, page open, info open) end at the first accepted PCM write or rows
shown; successful journeys feed latency distributions and slow or failed ones
send a full report. A journey still unfinished after 3 s is reported stuck.
Every non-user playback stop is reported with its cause. Settings → System → About →
Report a Problem uploads the recorder on demand.

## Platform (`platform/`)

`Platform` is the screen, backlight, battery, jacks, suspend and power-off,
plus an `AudioHw` for the player. `drm_test.rs` is the mainline device
(DRM/KMS, evdev through the platform bridge, `MainlineHw` in `audio/alsa.rs`),
`q2.rs` the stock platform (framebuffer and vendor DAC ioctls), `sim.rs` a
desktop window, `Headless` a canvas in memory.

On mainline the player pins its executable mappings with `mlock` once after
the first frame (`platform/resident.rs`, V0.109, about 18 MiB): the root is xz
squashfs, and refaulting evicted code pages stalled every thread for
260–370 ms under memory pressure. Code size is therefore resident memory: the
release profile (V0.110) builds the player crate, symphonia, the image
decoders, fonts and ring for speed and every other dependency for size, and
SQLite leaves out the extensions the player never uses
(`q2/toolchain/libsqlite3-flags`, read by the patched
`crates/vendor/libsqlite3-sys` for every build, host tests included).
The code that only `--bench` and `--script` run (`q2nd::perf`,
`q2nd::script`) is linked into its own `.q2tools` section
(`crates/q2nd/link/q2tools.ld`, Q2 builds only), which `resident.rs` leaves
unlocked.
MP3 is decoded by a patched symphonia-bundle-mp3
(`crates/vendor/symphonia-bundle-mp3`): the same output bits, with four-lane
kernels that use MSA when the kernel reports it.
Damaged files end in errors (2026-11-21): the release build aborts on a panic
and on a refused allocation, so symphonia-core, symphonia-metadata and
symphonia-format-isomp4 are vendored with allocations that follow the bytes
read, checked MP4 table counts and lengths, and decode errors where a damaged
file made them panic (`crates/vendor/symphonia-core/README.q2.md` and its
neighbours). Tag values and embedded pictures above 64 KiB are skipped unread:
the player shows the server's artwork, and a cover of several megabytes was
read at every track start and held while the track played.
`audio/decoder/corrupt_tests.rs` damages small files of every container and
fails on a panic or an allocation above 4 MiB.
JPEG is decoded by a patched zune-jpeg (`crates/vendor/zune-jpeg`): the same
output bytes, with MSA colour conversion and chroma upsampling when the
kernel reports it.
The [target builder](target-build.md) can consume an explicit player profile
collected on generated audio, artwork and UI tours.

The heap is tuned at the start of `main`: one malloc arena (multiple glibc arenas cost memory) and a fixed 128 KiB mmap threshold, so large transient buffers (cover
decodes, response bodies) go back to the kernel when freed. When the screen
goes off, at most every 10 minutes, a blocking worker samples memory pressure
(`platform/memory.rs`), calls `malloc_trim` and reports the player's RSS,
MemAvailable, malloc's free bytes, the RSS it released and the memory sample
([telemetry](telemetry.md)). Decoded artwork is held up to 3 MiB; closing
Cover Flow keeps 1 MiB of it and trims the heap.
