# Bluetooth headphones

The player streams to Bluetooth headphones with its own host stack, inside
`q2nd`, over the RTL8733BS controller's UART. There is no BlueZ, D-Bus or
kernel Bluetooth subsystem. The wiring, the kernel support (UART0 pins, the
`bt-power` hold, the 3 Mbaud UART) and the firmware files are in q2-os
[docs/bluetooth.md](https://github.com/q2-nova/q2-os/blob/main/docs/bluetooth.md).
Requirements BLUETOOTH-001 to BLUETOOTH-003 and EQ-OUTPUT-001 are in the
[specification](specification.md#bluetooth).

## Stack

The host is `crates/bt` (no `unsafe`, no I/O) with the thread, PCM and
settings in `crates/q2nd/src/bluetooth`.

| | BlueZ + bluez-alsa | Kernel HCI/L2CAP + own profiles | Rust host in `q2nd` (used) |
| --- | --- | --- | --- |
| Processes | `dbus-daemon`, `bluetoothd`, `bluealsa`, `hciattach` | none extra | none extra |
| Memory | binaries about 1.9 MB plus glib and dbus, three heaps | kernel text always resident when built in | code in `q2nd`'s locked image; one thread; buffers under 100 kB (PCM queue 70 kB at 44.1 kHz) |
| Kernel | `CONFIG_BT` and friends, and a `btrtl` entry for the 8723F, which Linux 7.3 lacks | same as BlueZ | none |
| Host tests | need a kernel with `vhci` | need a kernel | complete, against a fake controller and headphones in-process |

The Rust host costs no kernel memory, daemon or D-Bus, ships as an app
update, and every layer runs in host tests. Its risk is interoperability: it
has met few real devices. BTstack is excluded by its non-commercial licence.
If a class of headphones cannot be served and the fault is in the lower
layers, the fallback is the kernel's HCI and L2CAP under the same profiles,
PCM boundary (`bluetooth::pcm`) and settings.

- **H5** (`h5.rs`): SLIP framing, link establishment, sequence numbers,
  acknowledgement, window 4, no CRC. An unacknowledged packet is resent
  after 250 ms during bring-up and 100 ms once the controller runs at the
  fast rate.
- **Bring-up** (`host.rs`, `rtl.rs`): H5 sync at 115200; read the local
  version and ROM version; vendor rate change (0xfc17) to the UART setting
  in use; download the patch for that ROM version and the configuration with
  its UART entry rewritten to that rate and flow setting; then reset, buffer
  sizes, address, event mask, Secure Simple Pairing on, class of device
  `0x28041c`, name `Q2`, page scan on, inquiry scan off. A controller that
  kept its patch (the player restarted) answers only at the fast rate; the
  sync tries 115200, the fast rate and a failed attempt's setting.
- **Pairing**: Secure Simple Pairing, IO capability NoInputNoOutput, no
  out-of-band data, general bonding without MITM (Just Works, numeric
  comparison accepted). Passkey and out-of-band requests are refused at
  once. Legacy PIN `0000` for headphones without SSP. Link keys go to
  `/state/bluetooth/devices` (mode 0600); a refused key is dropped and the
  screen asks to pair again.
- **Devices of another class** (a computer, a speaker not of class
  audio/video): `Command::ConnectSink` opens the ACL link unauthenticated and
  sends one SDP ServiceSearchRequest for the Audio Sink UUID 0x110b. A record
  found: pairing goes on as for headphones. None: `Event::NotSink`, the link
  is closed and nothing is paired. No answer in 10 s, an error or a refused
  channel: closed unpaired. A device paired this way reconnects without the
  check.
- **Inquiry results** carry class, RSSI, name, and whether the extended
  inquiry response's UUID lists name the Audio Sink (`sink` in
  `Event::Found`).
- **One ACL link**, authenticated and encrypted before any audio channel
  (the sink check's SDP channel is the exception). Incoming connections are
  accepted only from paired headphones.
- **L2CAP** basic mode, 672-byte receive MTU, the peer's MTU for sending.
- **SDP server**: A2DP source 1.3, AVRCP target 1.4 (category 1, player) and
  AVRCP controller 1.4 (category 2, amplifier), over AVCTP 1.3. **SDP
  client**: the sink search above, and once per connection, 3 s after the
  stream opens, the headphones' Device ID record (vendor and product, for
  the [EQ match](#headphone-eq-per-output)).
- **AVDTP**: one source endpoint per codec. Initiator when the Q2 connects;
  acceptor when the headphones reconnect and configure the stream, with a
  fallback to initiating after 1.5 s.
- **AVRCP** 1.4. Target: play, pause, stop, next, previous, volume up and
  down (pass-through), playback status. Controller: the headphones' absolute
  volume ([Volume](#volume)).
- **Scan**: a general inquiry of 10.24 s with results as they arrive, a
  second one when the first found nothing, then name requests for devices
  whose response had no name. An inquiry not reported complete by 14 s is
  cancelled; with devices found the scan goes on to their names. The
  controller sometimes runs an inquiry past its length right after a fresh
  patch download; the cause is not known.

## Codecs

| Codec | Encoder | Licence | Arithmetic | Bit rate at 44.1 kHz | Notes |
| --- | --- | --- | --- | --- | --- |
| SBC | `crates/bt/src/sbc.rs` | this repository's | f64 | 328 kbit/s (bitpool 53) | mandatory; always the fallback |
| aptX | libopenaptx 0.2.0, `crates/codecs/q2-aptx` | LGPL-2.1-or-later | integer | 352.8 kbit/s | 0.2.1 and later are GPL-3.0-or-later and not used |
| aptX HD | same | LGPL-2.1-or-later | integer | 529.2 kbit/s | 16-bit input in the top of 24 bits |
| LDAC | AOSP libldac 2.0.2 (`eeee1a3f`), `crates/codecs/q2-ldac` | Apache-2.0 | float (integer build behind the `fixed-point` feature) | 909 / 606 / 303 kbit/s (990 / 660 / 330 at 48 kHz) | encoder only; needs a 679-byte media MTU |
| AAC-LC | vo-aacenc (`a277487e`), `crates/codecs/q2-aac` | Apache-2.0 | integer | 256 kbit/s constant, up to 320 | LATM, one access unit per element |

SBC uses 8 subbands, joint stereo or stereo, loudness or SNR allocation, and
bitpool up to 53. Each codec crate's `README.q2.md` has the fetched version,
hashes, licence evidence and how to verify it. The crates' FFI is the only
`unsafe` on the codec path: 20 sites (3 extern blocks, 3 `unsafe impl Send`,
14 blocks), all in each crate's `src/lib.rs`, each with a SAFETY comment;
`grep -nE 'unsafe (extern|impl|\{)' crates/codecs/*/src/lib.rs` lists them.
The three encoders add 197 kB to the release binary, which is locked in
memory.

Licence and patent notes:

- **aptX, aptX HD (LGPL)**: the encoder is linked statically. LGPL-2.1
  section 6 requires that a recipient of the binary can relink it with a
  changed library; this repository's source makes that possible. The aptX
  algorithm patent EP0398973B1 is listed as expired or revoked. Not checked:
  other aptX or aptX HD patents, and Qualcomm's trademark and certification
  terms.
- **LDAC**: Apache-2.0 (Sony copyright) with a patent grant from its
  contributors. Its `NOTICE` says certification is required to use LDAC in
  products; under Apache-2.0 a NOTICE is informational. Sony's programme
  terms for a non-Android device were not read. LDAC is a Sony trademark.
- **AAC**: vo-aacenc states it grants no patent rights. The Via Licensing
  pool still lists AAC-LC. Whether a listed patent is in force for AAC-LC
  was not established. fdk-aac (patent-licence condition), FFmpeg's `aac`
  (LGPL, float, not separable from libavcodec) and FAAC are not used.
- **AAC quality**: vo-aacenc fills 13 to 22 % of a 256 to 320 kbit/s stream
  with padding and measures below FFmpeg's and FDK's encoders at those rates.

### Negotiation

`crates/bt/src/codec.rs` holds each codec's capability bytes, the choice from
a sink's offer and the configuration sent. Stream endpoints: SEID 1 SBC,
2 AAC, 3 aptX, 4 aptX HD, 5 LDAC.

- **The Q2 initiates** (first pairing, or Connect in Settings): discover the
  sink's endpoints, read each one's capabilities and configure the first
  codec of the preference that both sides have.
- **The headphones initiate** (their reconnect): any codec this build has is
  accepted when the configuration is within the offer. Before the stream is
  reported to the player, the host reads the sink's endpoints once per link.
  If they offer a codec ranked above theirs, it closes their stream (AVDTP
  Close, then the media channel) and configures the better one; if they
  refuse, it configures theirs again. If they refuse the Close, do not answer
  within 4 s, or start the stream first, their stream stays.
- **Preference** (`[bluetooth] codec`, Settings › Bluetooth › Preferred
  Codec): `auto` is LDAC, aptX HD, aptX, AAC, then SBC; a named codec is that
  codec, then SBC. A change applies at the next connection.
- **Reason shown** on the device page and in Bluetooth Details: the best both
  sides have, chosen in Settings, the headphones lack the chosen one, chosen
  by the headphones, or changed from the one the headphones chose.
- **Rates**: 44.1 kHz when the sink offers it, else 48 kHz, for every codec.
  LDAC at 88.2 and 96 kHz is not offered.
- **Framing**: SBC and LDAC put a frame-count byte after the RTP header; AAC
  is a LATM element fragmented to the MTU with the RTP marker on the last
  fragment; aptX HD is bare codewords after the RTP header; aptX has no RTP
  header.
- A sink whose media MTU is under 679 bytes cannot take LDAC; the encoder
  refuses, the link is closed with a message, and the listener picks another
  codec.

### Encoder cost

`q2nd --bt-probe bench [--seconds N]` encodes generated 44.1 kHz stereo with
each codec as a stream would, with no controller; `cpu_percent` is the share
of one core while that codec plays.

| Codec | Host x86-64, ms per audio second | qemu-user `-cpu P5600`, ms per audio second |
| --- | --- | --- |
| SBC | 1.4 | 79.7 |
| aptX | 9.2 | 40.0 |
| aptX HD | 9.7 | 42.0 |
| AAC 256 kbit/s | 8.7 | 53.2 |
| LDAC 303 / 606 / 909 kbit/s | 2.1 / 2.2 / 2.4 | 59.5 / 61.0 / 61.7 |

qemu-user computes floating point in software, which inflates SBC and LDAC
against the integer codecs, and runs at the host's speed. The figures show
only that no encoder is far from real time; the Q2 figure is the same command
on the device. The encoders are C built by `cc` at `-O2`; an X2000 build
needs its MSA flags in `CFLAGS_mipsel_q2_os_linux_gnu` for them to be
vectorized.

## Audio route

```text
Decode → EQ + ReplayGain → software volume → sink ─ ALSA hw:Q2,0 (wired)
                                                  ├ ALSA plughw (USB DAC)
                                                  └ q2bt → resample → encoder → AVDTP
```

- `audio::output::Route::Bluetooth` is selected while the headphones' stream
  is open and takes precedence over USB and the jack. The player opens the
  PCM `q2bt` (`audio/pcm_backend.rs`), implemented in process by
  `bluetooth::pcm`. It follows ALSA's contract as the sink uses it: a 400 ms
  queue, start at half full, blocking writes, `-EPIPE` after an underrun,
  delay in source frames, no hardware pause. EQ, ReplayGain and the retained
  PCM rules apply unchanged.
- **Format**: the stream is 44.1 kHz when offered, else 48 kHz. Other source
  rates pass through a polyphase Kaiser-windowed sinc resampler
  (`resample.rs`); mono is doubled. The stream is not reconfigured per
  track.
- **Latency**: up to 400 ms in the PCM queue, one media packet (20 ms at
  7 frames) and the headphones' own buffer, typically 100 to 250 ms. The
  delay the player sees covers frames not yet sent, so the position runs
  ahead by the headphones' buffer.
- **Connect** while playing: playback moves to the headphones and continues.
- **Disconnect** (Settings, headphones switched off or out of range): the
  route returns to the wired choice and playback pauses with no error and no
  Last Problem; unsent audio plays when Play is pressed. A write that the
  disconnect fails is taken as the pause. A write that stalls first
  ("Bluetooth headphones stopped taking audio") is shown until the link
  drops, which clears it. A stream the headphones never opened leaves
  "These headphones did not accept audio."
- **Pause**: the stream is suspended after 2 s without audio and started
  again on the next write, so seeks and track changes do not toggle it.
- **Switching off** in Settings does not wait: the thread disconnects and
  closes the port, and a new start waits for it.

## Streaming over the UART

### UART settings

`[bluetooth] uart_mode = "auto"` (the default) tries, in order (`AUTO_UART`
in `bluetooth/mod.rs`):

1. 3 Mbaud with RTS/CTS (rate code `0x04928001`, the flow bit set in the
   configuration's UART entry), the stock firmware's setting;
2. 1.5 Mbaud with RTS/CTS (`0x04928002`);
3. 1.5 Mbaud without flow control.

A kernel that does not take 3 Mbaud (termios reads another rate back,
`Uart::accepts`) starts at step 2, with one log line and no fallback
counted. `uart_mode = "manual"` tries `baud` and `flow_control`, then
1.5 Mbaud without flow control.

A bring-up that fails before Ready (no H5 sync answer, a start-up command or
firmware fragment unanswered or refused, a refused rate or port setting) is
tried at the next setting. The thread logs `Bluetooth: bring-up at B baud,
flow control F failed (…); trying B2 baud, flow control F2`, keeps the
message as the session's last error and counts it (`uart_fallbacks`). The
page shows `Starting…` throughout. The setting that reaches Ready is used by
later starts of the same process; if all fail, Try Again starts from the
first. `--bt-probe` with `--baud` or `--flow` does not fall back.

Between attempts the thread drops the power hold for 100 ms. That resets the
controller to its ROM at 115200 only when the Wi-Fi slot is powered down; with
Wi-Fi on, BT_DIS_N stays high. An HCI Reset keeps the patch and rate. So the
next attempt's sync tries 115200, its own rate and the failed setting
(`Settings::previous` in `crates/bt/src/host.rs`); a controller found at the
failed setting is taken as patched, moved to the new rate with 0xfc17, and
the start-up commands follow without a download. That command keeps the
controller's own flow-control setting. Not covered with Wi-Fi on: a
controller left at a rate the UART cannot produce (Wi-Fi Off, then Try Again,
resets it), and a controller left in ROM at the failed setting after a
partial download (it comes up without its patch).

### Line budget

Bytes per media packet: the codec payload, RTP 12 (not aptX), the LDAC/SBC
count byte, ACL 4, L2CAP 4, H5 4, two delimiters, and 2/256 for SLIP escapes;
11 bits a byte (8E1). 1.5 Mbaud carries 136.4 kB/s, 3 Mbaud 272.7 kB/s.

| Codec, rate | Packets/s | Bytes/packet | kB/s | 1.5 Mbaud | 3 Mbaud |
| --- | --- | --- | --- | --- | --- |
| SBC bitpool 53, MTU 895 | 49 | 867 | 42.7 | 31 % | 16 % |
| AAC 256 kbit/s, MTU 672 (2 fragments per access unit) | 86 | 402 | 34.6 | 25 % | 13 % |
| aptX 352.8 kbit/s | 100 | 458 | 45.8 | 34 % | 17 % |
| aptX HD 529.2 kbit/s | 100 | 691 | 69.3 | 51 % | 25 % |
| LDAC 303 (MQ) | 57 | 692 | 39.8 | 29 % | 15 % |
| LDAC 363 (Q1) | 69 | 692 | 47.7 | 35 % | 17 % |
| LDAC 452 (Q0) | 86 | 692 | 59.6 | 44 % | 22 % |
| LDAC 606 (SQ) | 115 | 692 | 79.5 | 58 % | 29 % |
| LDAC 909 (HQ) | 172 | 692 | 119.3 | 87 % | 44 % |
| LDAC 660 at 48 kHz | 125 | 692 | 86.5 | 63 % | 32 % |
| LDAC 990 at 48 kHz | 188 | 692 | 129.8 | 95 % | 48 % |

At the measured 3 Mbaud line rate (2.70 Mbaud) LDAC 990 uses about 53 %. The
headroom sets how fast a stall is caught up: at LDAC 606 on 1.5 Mbaud a
100 ms gap takes about 140 ms of full-rate sending; at 909, about 670 ms.

### Pacing and bit rate

- **Pacing** (`bluetooth/pacing.rs` `Pacer`, `bluetooth/media.rs`): by the
  clock with a 60 ms lead, built at 5 % of stream time; a stall is caught up
  as fast as the controller takes it, up to 300 ms behind; beyond that the
  schedule restarts (counted). Packets more than 40 ms late are counted, and
  stalls of 100 ms or more are marked with their length. Late packets are
  dropped oldest first when the controller stops taking them.
- **LDAC adaptive bit rate** (`pacing.rs` `Abr`, `q2-ldac` `set_level`):
  libldac's steps HQ 909, SQ 606, Q0 452, Q1 363, MQ 303 kbit/s, applied
  from the next frame. The input is the queue between encoder and air in
  packets (host queues, H5, controller buffers, plus lateness): 8 or more and
  growing for 200 ms is one step down; 14 or more for 200 ms is MQ at once;
  3 or fewer for 5 s is one step up, never above `ldac_quality`. A step up
  undone within 30 s doubles the wait (up to 60 s); two quiet minutes reset
  it. `ldac_adaptive` (default true) turns it on; `ldac_quality` (default
  `standard`, 606/660 kbit/s) is the ceiling.
- **Coexistence report** (`host.rs`): Realtek's profile report (vendor
  command 0xfc19, as the stock `rtk_coex.c` sends) goes to the controller when
  the link comes up, when streaming starts and stops, and at disconnect. The
  Wi-Fi firmware's coexistence tables differ with A2DP known (TDMA 113, 8 ms
  Wi-Fi slots) from without (TDMA 103/104, 32 to 48 ms). Answered by the fake
  controller; its effect on the Q2's controller is not verified.
- **Link quality**: while streaming, Read RSSI and Get Link Quality every 2 s,
  Read Transmit Power Level and Read AFH Channel Map every 10 s, kept out of
  the event timeline.

### Thread priority

The `bluetooth` thread sets its nice value to −10 at start
(`bluetooth::raise_priority`, one `setpriority(PRIO_PROCESS, 0, -10)` on the
calling thread) and logs `Bluetooth: thread at nice -10` or a warning. The
installed services run `q2nd` as root, so it has CAP_SYS_NICE. The thread
encodes as well as sends, and the decode thread feeds it; a realtime policy
would let a long encode hold off decode, UI and network on the two logical
CPUs. At nice −10 it has about nine times the weight of a nice-0 thread under
EEVDF. `wake_late` and `wake_late_max_ms` in telemetry measure it.

## Volume

For absolute volume the Q2 is the AVRCP controller and the headphones are a
category 2 target:

1. When the control channel opens, GetCapabilities(events).
2. If the volume event (0x0d) is listed, RegisterNotification for it. The
   interim response marks the headphones as taking absolute volume and
   reports their level.
3. At that first report a volume saved for the device is sent with
   SetAbsoluteVolume; without one the player takes the headphones' level.
4. Player changes (wheel, headset keys) go out as SetAbsoluteVolume, one at
   a time; changes made while one waits are merged and sent when the
   response arrives or after 2 s. A level the headphones hold is not sent
   again.
5. A CHANGED notification (their buttons) is reported and the registration
   renewed. A CHANGED that echoes or rounds a level the player is setting is
   not reported back.

Headphones that register for volume at the Q2's target, or send
SetAbsoluteVolume to it, are answered too. Levels map player 0..=100 to
AVRCP 0..=127 (`to_level`, `from_level` in `bluetooth/mod.rs`); every player
volume survives the round trip.

- **One volume.** With absolute volume active, the player's samples stay at
  unity (volume 0 still mutes; EQ and ReplayGain headroom still apply). The
  wheel and the headphones change the same value, and the on-screen volume
  follows the headphones' buttons.
- **Volume Limit** applies on pairing, reconnect, the wheel and the
  headphones' own controls. Lowering it turns down an output above it;
  raising it keeps the listening level. A saved level is clamped before
  reconnecting; a headphone report above the limit gets a corrective set,
  and software attenuation stays until the headphones report a level within
  it. Headphones that round a requested level up past the limit keep
  software attenuation.
- **Without absolute volume**: software volume (0.5 dB a step). Pass-through
  VOLUME UP/DOWN steps the player's volume by 4 (2 dB). Headphones that
  change only their own amplifier without telling the source keep that level
  separate.
- **Per device**: the jack keeps `[playback] volume`; each paired device
  keeps `volume <address> <0-100>` in `/state/bluetooth/devices`, written 2 s
  after the last change and when the link drops. A new device starts at the
  jack's volume capped at 60, or at the headphones' level when they take
  absolute volume. Addresses are not logged.
- **Keys before the stream opens.** A pass-through volume key carries the
  headphones' address to the player and steps only that device's volume. A
  key pressed while their stream is not yet open (headphones that reconnect
  themselves can press before the player has checked their codecs) is held
  and applied when the stream opens, and dropped if the link or the stream
  setup ends first. A headphone key never changes the jack's volume. A level
  that headphones with absolute volume report before the stream opens is
  saved for that device.

## Headphone EQ per output

- The graphic EQ (bands, presets) is the same on every output.
- **Jack**: the profile chosen in Settings › Sound › Equalizer while no
  Bluetooth output is in use, kept in `[playback.eq]` ("Pinned").
- **Each Bluetooth device**: its own profile in the device store
  (`eq <address> auto|chosen on|off <catalog path> <name>`); it never uses
  the jack's.
- **Automatic match** at the first connection against the packaged AutoEQ
  catalog (`assets/autoeq/index.tsv`) by `audio/eq_match.rs`: the device name
  without a possessive ("Alex's AirPods Pro") or an `LE_` prefix; the brand
  from the Device ID vendor (Bluetooth SIG or USB IDs of Apple, Sony,
  Samsung, Google, Bose) or the name; Apple's AirPods product IDs for renamed
  pairs (from public reports, unverified); and the class of device (speakers,
  cars and computers get none). The model must agree once spaces,
  punctuation and filler words are gone, or every word of the model must be
  in the name with no model number missing. Among equal models the plain
  measurement wins over variants, then oratory1990, crinacle, Rtings,
  Innerfidelity. No match: no profile, and a later Device ID read tries
  again.
- The matched profile comes from the card's AutoEQ cache or is downloaded
  once; offline and not cached, the page says "Not downloaded" and it is
  tried when Wi-Fi returns. A toast names a profile when a match is applied.
- **Switching**: the EQ changes to the device's profile when its stream
  opens and back to the jack's when it closes.
- **UI** (Settings › Sound › Equalizer, also from Now Playing): an Output row
  ("Headphone Jack" or the device's name); Headphone EQ On/Off for that
  output; Choose Headphones; the profile row with its reason ("Pinned",
  "Matched automatically", "Chosen", "Finding a match…", "No automatic
  match", "Not downloaded"); and Match Automatically.

## Settings

Settings › Bluetooth is a top-level item after Connections
(`ui/screens/bluetooth.rs`):

- **Bluetooth**: the switch with the controller's state (`Starting…`,
  `Ready`, or why it is unavailable, with Try Again); Last Problem and Try
  Again after a failed attempt; the paired devices with their state (Playing
  or Connected with codec and bit rate, the step of an attempt, or when last
  connected); Pair New Device; Preferred Codec. Paired devices and Pair New
  Device are offered while the controller starts; their requests wait.
- **A paired device**: Connect, Disconnect or Stop Connecting; Status; Codec
  (bit rate, LDAC quality, sampling rate) while connected, else Last
  Connected; Type; Forget This Device… with a confirmation. Connecting one
  device disconnects another first.
- **Pair New Device**: a search in rounds while the page is open (one
  inquiry after another with a 1 s pause, for up to two minutes, not while a
  device is connected), listing devices with signal (Strong, Good, Weak) and
  type (Headphones, Speaker, Audio device, Takes Bluetooth audio, Other
  device), headphones and speakers first. Choosing one shows "Pairing with
  …" and its step; success closes the page with "Connected to …"; failure
  says why with Try Again. Back stops the search. An audio-class device pairs
  at once; any other only after the SDP check, and one without an A2DP sink
  is refused with "This device does not take Bluetooth audio (no A2DP sink).
  It was not paired."
- **Bluetooth Details** (long Center on the Bluetooth page): controller
  state, firmware (patch loaded or kept, ROM, revision, subversion), link and
  pairing step with RSSI and link quality while streaming, codec, the last
  error, H5 and command counters, scan counts, dropped packets, underruns,
  credit stalls, the checks after Wi-Fi power-ups and waiting requests. No
  addresses or names.

`seen <address> <seconds since 1970>` records in `/state/bluetooth/devices`
give Last Connected.

### Requests, timeouts and retries

- A scan or connection the host cannot start yet waits in order, up to 45 s,
  then shows "Bluetooth was busy with another connection. Try again." A new
  connection replaces one still waiting; Disconnect or Forget ends a waiting
  one. The automatic reconnect at Ready gives way (HCI Create Connection
  Cancel), is not resumed, and is not started when a scan, connection or
  search is already asked for.
- Each step is bounded: 12 s from Create Connection to Connection Complete
  (the page is cancelled), 10 s for the SDP check, 30 s from the link to
  encryption (counted again after a passed SDP check). Each ends with a
  message naming what did not happen. A page that completes after its
  cancel is closed.
- Entering Pair New Device ends any connection attempt in progress (a page
  is cancelled, an unencrypted link closed) and drops waiting connections,
  with no message; a connected device stays connected. Nothing pages the old
  device again. The next Bluetooth start reconnects the last-used device,
  which is the newly paired one if pairing succeeded.
- A failed start shows Try Again, which starts the controller again without
  switching off.

The 45 s, 12 s and 30 s limits are choices, not measurements.

### After the Wi-Fi chip powers up again

Radio doze, Wi-Fi Off/On and Offline Mode power the combo chip down and up
through the network helper. Each helper `up` after a `down` reaches the
Bluetooth thread, which asks the controller for its version (3 s):

- no answer: the controller lost power with the Wi-Fi side; the thread
  brings it up again as a start does;
- an answer with no headphones connected, connecting or scanning: it runs
  the start-up commands again (HCI Reset and the configuration; no download,
  no page), so the controller starts over beside the new Wi-Fi driver
  instance; `resync_after_wifi = false` turns this off;
- an answer with headphones connected: nothing.

`/run/q2/bluetooth-status` holds one line for diagnosis: phase, link, codec,
the check counts and H5 counters, no addresses or names.

### Configuration

`[bluetooth]` in `config.toml`: `enabled` (false), `device` (`/dev/ttyS0`,
or `fake`), `uart_mode` (`auto`), `baud` (1500000) and `flow_control`
(false) for `manual`, `firmware_dir`, `codec` (`auto`), `ldac_quality`
(`standard`), `ldac_adaptive` (true), `resync_after_wifi` (true). Firmware
is looked for in `firmware_dir`, `/lib/firmware/rtlbt` and
`/state/bluetooth/firmware`. The simulator and headless runs use the fake
controller with two fixture headphones and a fixture computer (class
0x7c0104, an A2DP sink with SBC and AAC in its SDP records, no UUIDs in its
inquiry response).

## Telemetry

Every UX report while Bluetooth is on carries a `bluetooth` object, and the
snapshot carries the output route: state, codec, rate, bit rate, LDAC level,
MTU, SBC bitpool, UART rate and flow control, UART fallbacks, the
headphones' class, name and Device ID, RSSI, link quality, transmit power,
AFH channels, controller buffers in use, H5 and UART receive error counts,
credit stalls, late packets, schedule restarts, queue depth, LDAC changes,
underruns, late thread wakes (20 ms or more past the timeout), the longest
UART write and the slowest encode. Flight-recorder marks: `bt-stall`,
`bt-transport`, `bt-rate`, `bt-state`, `wifi-event` and `wifi-scan`.
`python3 tools/ux_triage.py --boot latest` prints them.

## Probe commands

`q2nd --bt-probe` drives the controller without the player. Output is counts,
classes and states, never addresses or names; each command stops the
controller when it ends. `scan`, `pair` and `play` change controller state,
and `pair` writes `/state/bluetooth/devices`. Pause the installed player
first.

| Command | Does |
| --- | --- |
| `version [--firmware DIR] [--baud N] [--flow]` | bring-up; prints `at_start` and `after_patch` versions, ROM version, `uart baud= flow=`, H5 and UART error counters |
| `diag [--inquiry-length N]` | `version`, then the Bluetooth clock read twice and one timed inquiry with its event timeline |
| `scan` | the same search as Settings; `found`, `named`, and the inquiry timeline when it timed out |
| `pair [--nearest] [--any]` | scan and pair one device. `--nearest` needs it at −60 dBm or stronger and 10 dB above the next. `--any` considers every class; a device not of the audio class pairs only after the SDP check |
| `play SECONDS [--codec C] [--ldac-quality Q] [--ldac-fixed]` | a 440 Hz tone at −20 dBFS to the paired headphones; pass is the tone heard, the `stream codec=` asked for, `dropped_packets=0`, `underruns=0` |
| `wait SECONDS` | wait for the headphones to reconnect |
| `lines` | CTS with the controller off and on, to show the RTS/CTS wiring |
| `forget-all` | remove every paired device from the store |
| `bench [--seconds N]` | the encoder cost table above |

With Wi-Fi off, BT_DIS_N follows only the probe's hold, so each probe starts
from a controller reset to its ROM.

## Tests

- `crates/bt`: SBC frame lengths, a decoder written from the synthesis side
  of the specification (30 dB or better at bitpool 53) and stream hashes;
  H5 through loss and corruption; the Realtek files (the stock files when
  `Q2_BT_FIRMWARE_DIR` points at them); resampler pass-band and alias
  rejection for 13 rate pairs. The host against the fake controller:
  bring-up with download and rate change, a kept patch, a controller at a
  failed attempt's setting moved to the new rate, a silent UART, scan, Just
  Works and PIN pairing, the stream, reconnect from either side, refused
  keys, restricted sinks, a hung sink, buttons, SDP, dropped late audio and
  random input. A computer of class 0x7c0104 through the SDP check; one
  without a sink refused unpaired; one whose SDP never answers closed after
  10 s. An inquiry never reported complete ends at 14 s with what it found.
  A stalled pairing times out at 30 s and an immediate retry pairs; a page
  with no answer is cancelled at 12 s. AVRCP PDUs (SetAbsoluteVolume,
  RegisterNotification, GetCapabilities, pass-through volume, refusals,
  random input) and absolute volume against fake headphones in both
  directions. `stack_tests.rs` checks the sync order and the rate move at
  the byte level.
- Codec crates: stream hashes for generated input, rate and framing checks,
  and an environment-gated dump for an external decoder. Decoded once by
  external decoders: SBC by FFmpeg at a 73-sample delay (39.2, 32.3 and
  31.1 dB for joint bitpool 53, stereo 48 kHz bitpool 51, SNR bitpool 35);
  aptX and aptX HD by FFmpeg (42.3 / 39.5 and 52.7 / 50.0 dB left / right,
  delay 90); AAC by FFmpeg (delay 1,600, 32 to 34 dB left at 256 to
  320 kbit/s); LDAC by libldacdec (67 / 46 / 42 dB at High / Standard /
  Mobile, delay 128). The test signal carries broadband noise, so these
  figures describe that signal.
- `crates/q2nd`: the PCM endpoint; tty settings on a pseudo-terminal; the
  thread against the fake (pairing, key file mode, 48 kHz mono through the
  resampler, pacing, suspend, a restart that waits for its predecessor, the
  fixture computer to an AAC stream, headphones switched off mid-stream);
  the route policy; player tests (connect moves playback, disconnect pauses,
  Play resumes with no audio lost); the request handling (requests during
  start-up and the reconnect, Pair New Device ending other attempts); the
  UART fallback order; the level mapping, device store, per-device volume
  and EQ through a reconnect; the EQ matcher on the packaged catalog; and
  the Settings pages in every controller, link and pairing state.
- `bluetooth/sim_tests.rs` steps host, pacer, encoder and fake controller by
  1 ms: the UART carries 136 bytes a millisecond, the radio sends one ACL
  packet per 3.75 ms and can stall or slow, writes can be lost and the thread
  held off the CPU; the headphones are a buffer that starts at 100 ms.

| Scenario | Gaps | Lowest buffer |
| --- | --- | --- |
| LDAC 606, clean, 6 s | 0 | 96 ms |
| LDAC 606, a lost media write every 1.5 s | 0 | 33 ms (the bit rate ends at MQ) |
| LDAC 606, 30 ms of no air every 100 ms for 5 s | 0 | 96 ms |
| LDAC, air 3× slower for 8 s, fixed 606 (control) | > 0 | — |
| LDAC, the same, adaptive, 48 s | 0 | 47 ms (back at 606 at the end) |
| AAC 256: 4 lost writes, 20 air stalls of 30 ms, 4 × 80 ms with the thread off the CPU | 0 | 47 ms |

- Headless: `--script` through Settings › Bluetooth, Pair New Device, a
  device page, Bluetooth Details and Preferred Codec. The simulator's fake
  runs at about a real controller's pace (start about 1 s, page and pairing
  steps 0.7 s); tests run it at full speed.

## What has run on a Q2

| Fact | Level |
| --- | --- |
| Bring-up at 115200 8E1, ROM version 2, patch download, rate change and the `bt-power` hold | device-verified |
| `version` at 3 Mbaud with RTS/CTS: Ready, 0 command timeouts, 0 H5 resends or bad frames | device-verified |
| Scan finds a discoverable device and its name with Wi-Fi on; the baseband clock runs and inquiries complete | device-verified |
| Pairing with headphones and with a Linux PC (through the SDP sink check); SBC to the PC with 0 dropped packets and 0 underruns | device-verified |
| SBC and AAC-LC tones to headphones with 0 dropped packets and 0 underruns, heard clean; music in the player; playback pauses when the headphones switch off | device-verified |
| LDAC and AAC music streams to headphones, with short dropouts before the H5 resend, pacing and adaptive bit-rate changes | device-verified |
| The H5, pacing, adaptive bit rate, coexistence report, thread priority and UART fallback changes | host |
| aptX and aptX HD; absolute volume, headphone volume keys, Device ID query and EQ match; a 3 Mbaud stream; Volume Limit on headphones | host |

## Limits

- One connected device; no multipoint and no track titles on the
  headphones' display.
- System sleep with Bluetooth on is not handled: suspend stalls the thread
  and the link is expected to drop.
- Stopping Bluetooth leaves the controller patched and at the fast rate
  unless the kernel offers the power hold; the next start finds it there
  after 1.5 s.
- Wi-Fi and Bluetooth share the 2.4 GHz radio. Streaming over Wi-Fi while
  sending audio is not measured.
