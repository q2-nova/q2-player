# Equalizer

Settings → Sound → Equalizer, also opened from the Now Playing menu, has two
layers with separate switches: a ten-band graphic EQ and a headphone
correction from the AutoEQ catalog. Both are off by default and saved in
`[playback.eq]`. The graphic EQ is the same on every output; the headphone
correction belongs to the output in use: the jack's pinned profile or each
Bluetooth device's own ([Headphone EQ per output](bluetooth.md#headphone-eq-per-output),
EQ-OUTPUT-001 in the [specification](specification.md#audio-and-sound)).
EQ and ReplayGain run in software on every output; volume and the codec are
in [Headphone output](audio.md).

## Controls

- **Graphic EQ** switches the graphic layer.
- **Presets**: Flat, Bass Boost, Bass Cut, Treble Boost, Treble Cut, Rock,
  Pop, Jazz, Classical and Vocal (`audio/eq.rs` `PRESETS`). Choosing one
  replaces the ten bands and turns Graphic EQ on; the headphone layer is
  unchanged. Bands that match no preset show as Custom.
- **Adjust Bands**: ten bands at 31 Hz to 16 kHz in octaves, ±12 dB in
  1 dB steps. The wheel chooses a band; Center starts adjusting it and the
  wheel sets its level; Center or Back ends the adjustment. Changing a band
  turns Graphic EQ on.
- **Reset Bands…** asks for confirmation (Cancel selected) and sets the
  bands to 0 dB. The headphone layer is kept.
- **Headphone EQ** switches the headphone layer for the output shown on the
  Output row. With no profile chosen it opens the chooser.
- **Choose Headphones** lists the catalog alphabetically, with letter mode
  ([Wheel and jogging](interaction.md#lists)) and a search that matches
  every word typed. A check marks the profile in use. Choosing one loads it,
  turns the headphone layer on and shows "Headphone EQ applied".
- **The profile row** shows the profile's name and why it is in use
  (Pinned, Matched automatically, Chosen). Center opens EQ Curves.

The page footer reads "Automatic headroom is always on".

## Headphone catalog

The catalog is AutoEQ's recommended list at a pinned revision, 6,033
entries, plus four Beyerdynamic Custom Studio switch positions from an older
pinned revision (EQ-CUSTOM-STUDIO-001). Revision, provenance and license are
in the [AutoEQ catalog snapshot](../crates/q2nd/assets/autoeq/README.md) and
the [Custom Studio profiles](../crates/q2nd/assets/autoeq/custom-studio/README.md).
On the Q2 the catalog is read from `/usr/share/q2nd/autoeq` and each file is
checked against its MD5 (RESOURCES-001); a missing or changed file shows
"Headphone resources are unavailable; restore the packaged files".

The four Custom Studio profiles are packaged and load offline. Any other
profile is downloaded once from `raw.githubusercontent.com` at the pinned
revision ([`subsonic/src/autoeq.rs`](../crates/subsonic/src/autoeq.rs)):
TLS, a 5 s connect and 15 s total deadline, at most 16 KiB, and no
Navidrome credentials. The download connects directly, not through
Tailscale. The profile is saved under `autoeq/` in the player's data
directory and the active profile is also stored in the settings, so a
restart needs no network. In Offline Mode an unsaved profile is not
downloaded ("Profile not saved; connect to download"). A profile that fails
to download or parse leaves the current correction in place.

The player applies AutoEQ's precomputed corrections. It does not optimize a
correction or measure headphones.

## Processing

[`audio/eq.rs`](../crates/q2nd/src/audio/eq.rs):

- Filters are [RBJ biquads](https://www.w3.org/TR/audio-eq-cookbook/): peak,
  low shelf and high shelf. Profiles are Equalizer APO `ParametricEQ.txt`
  exports with `Preamp`, and `PK`, `LSC` or `HSC` filters with explicit Q.
  A profile with any other filter, a non-finite number, a value out of range
  (10 Hz to 24 kHz, ±30 dB, Q 0.1 to 20, preamp −60 to 0 dB) or more than
  20 filters is rejected whole. Saved settings are checked again before use.
- Headphone filters run before graphic filters. Coefficients are computed at
  the decoded sample rate, and filters at or above 0.49 × the rate are left
  out, so the top graphic bands have no effect on low-rate radio streams.
  Coefficients and filter state are f64, with state per channel.
- **Headroom.** The gain is lowered by the combined response's peak plus
  1 dB, or by the profile's preamp if that is lower, so a steady tone at
  any frequency from 0 Hz to half the sample rate stays at least 1 dB below
  full scale. The peak is an upper bound at most 0.01 dB above the true
  maximum: the search divides the band at each filter's turning points,
  between which every filter is monotonic, and halves intervals until each
  interval's bound is within 0.01 dB of the largest gain found. A peak
  below 0 dB counts as 0 dB. This is not a limiter: a transient can still
  exceed full scale and is clamped at the final conversion.
- Decoded samples stay floating point through the EQ. ReplayGain and volume
  are applied at the final saturating conversion ([ReplayGain](replaygain.md)).
- A settings change crossfades from the old filter chain to the new one over
  10 ms without interrupting the stream. Seeks, track changes and format
  changes clear filter state; a gapless transition at the same format keeps
  it. With both layers off, or flat with no preamp, the EQ is bypassed.
- Steady stereo shares coefficient loads between the channels and keeps
  each channel's operation order. On MIPS, a C leaf
  ([`audio/eq_msa.rs`](../crates/q2nd/src/audio/eq_msa.rs)) runs up to 30
  filters with MSA when the kernel sets `HWCAP_MIPS_MSA`; otherwise, and for
  mono, multichannel and crossfades, the scalar path runs.

EQ covers all audio `q2nd` decodes: Navidrome streams, downloads and cached
songs, Audiobookshelf media and web radio.

## EQ Curves

The profile row opens EQ Curves
([`ui/screens/eq_curves.rs`](../crates/q2nd/src/ui/screens/eq_curves.rs)): a
logarithmic graph from 20 Hz to the lesser of 20 kHz and 0.49 × the sample
rate, with the headphone correction (blue), the graphic EQ (orange) and
their total (green). A layer that is off is drawn and labelled "off" and
does not count in the total. The wheel moves a frequency marker and the
readouts show each curve's gain there. Center adds the automatic headroom
to the total. The curves use the playback filters at the decoded rate, or a
48 kHz preview with nothing playing. They show the digital EQ only, not
ReplayGain, volume or the headphones' acoustics. Viewing them changes no
setting.

With Clicker on, each marker step sounds a click whose pitch follows the
frequency: the 20 Hz to 20 kHz axis maps onto 500 to 4,000 Hz (SPEAKER-001).

## Benchmarks

`q2nd --bench` has two EQ cases that read no settings and open no audio
device: `{"kind":"eq","iterations":400}` runs generated stereo PCM through
the conversion and EQ path, and `{"kind":"autoeq","scratch":"DIR"}`
downloads a public profile into DIR and reloads it offline.
