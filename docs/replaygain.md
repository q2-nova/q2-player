# ReplayGain

Settings → Sound → Volume Normalization (`[playback] replay_gain`) is Off
(default), Track or Album. The gains and peaks come from the `replayGain`
field Navidrome returns for each song; the player reads no tags from the
file. Track uses the track gain and peak and falls back to the album's;
Album does the reverse. A song without a gain plays unchanged.

## The gain applied

`audio::player::replay_gain_factor` sets one factor per song:

1. The gain is clamped to −48 to +15 dB (`REPLAY_GAIN_MIN_DB`,
   `REPLAY_GAIN_MAX_DB`). Quiet classical recordings need about +7 to
   +12 dB against the −18 LUFS reference; +15 dB covers them and lies below
   Rockbox's +17 dB tag clamp and MPD's +23.5 dB scale cap. The −48 dB floor
   is Rockbox's, so a damaged tag cannot silence a song.
2. With a usable peak (finite and positive), the factor is at most
   `1 / peak`, so the song's measured peak does not pass full scale.
3. Without a usable peak, the factor is at most 1.0: the song is attenuated
   as tagged but never boosted. A boost needs the server to report the room
   for it.
4. A gain that is not a finite number leaves the song unchanged.

The factor multiplies the samples at the final saturating conversion,
together with the listener's volume and after the
[equalizer](equalizer.md), which keeps its own headroom. On the headphone
jack the codec attenuation keeps headroom for a positive gain so each
volume step still changes the level by one step (AUDIO-008 in the
[specification](specification.md#audio-and-sound); [Headphone output](audio.md#volume)).
Bluetooth uses the same chain (BLUETOOTH-001).

There is no limiter. With the rules above ReplayGain does not raise a sample
above the peak the tagger measured, so a limiter would act only on a wrong
peak or on the overshoot between sample peak and true peak. Past full scale
the conversion clamps to the sample range. A look-ahead limiter would have
to run in every render path (with and without EQ, the spoken path,
`audio/gain.rs` and retained audio) and keep its gain history across
retained audio.

Tests in [`player_machine_tests.rs`](../crates/q2nd/src/audio/player_machine_tests.rs):
`replay_gain_has_a_ceiling_and_boosts_only_under_a_known_peak` and
`replay_gain_from_the_server_never_yields_a_non_finite_or_clipping_factor`.

## Other players

The rules follow the common practice: `min(gain, 1 / peak)`, with a ceiling
on the gain as MPD and Rockbox have.

| Player | Gain ceiling | Peak protection (default) | Limiter | No peak |
| --- | --- | --- | --- | --- |
| MPD | scale 15 (+23.5 dB), after preamp | `replaygain_limit`, on | none | limit skipped, full gain |
| mpv | none | on (`--replaygain-clip=no`) | none | REPLAYGAIN tags: peak 1.0, gain capped at 0 dB |
| VLC | none | `audio-replay-gain-peak-protection`, on | none | peak 1.0, gain capped at 0 dB |
| Rockbox | tag gain clamped to −48 to +17 dB | "Prevent clipping", off | none | no protection |
| Navidrome web player | none | always on | none (the browser clips) | full gain |
| Feishin (web engine) | none | on (`replayGainClip`) | none | peak 1.0, gain capped at 0 dB |
| DSub | final multiplier at most 1.0 | peak not read | none | n/a |
| Plexamp | none documented | not documented | on: −1 dB threshold, 5 ms attack, 100 ms release | n/a |

The ReplayGain 1.0 player proposal applies gain, then preamp, then hard
limiting of peaks above full scale. EBU R128 sets a maximum true peak of
−1 dBTP.

Sources:
MPD [`src/tag/ReplayGainInfo.cxx`](https://github.com/MusicPlayerDaemon/MPD/blob/master/src/tag/ReplayGainInfo.cxx),
[`doc/user.rst`](https://github.com/MusicPlayerDaemon/MPD/blob/master/doc/user.rst);
mpv [`player/audio.c`](https://github.com/mpv-player/mpv/blob/master/player/audio.c),
[`demux/demux.c`](https://github.com/mpv-player/mpv/blob/master/demux/demux.c);
VLC [`src/input/replay_gain.c`](https://code.videolan.org/videolan/vlc/-/blob/master/src/input/replay_gain.c);
Rockbox [`lib/rbcodec/metadata/replaygain.c`](https://github.com/Rockbox/rockbox/blob/master/lib/rbcodec/metadata/replaygain.c),
[`lib/rbcodec/dsp/dsp_misc.c`](https://github.com/Rockbox/rockbox/blob/master/lib/rbcodec/dsp/dsp_misc.c);
Navidrome [`ui/src/utils/calculateReplayGain.js`](https://github.com/navidrome/navidrome/blob/master/ui/src/utils/calculateReplayGain.js);
Feishin [`web-player.tsx`](https://github.com/jeffvli/feishin/blob/development/src/renderer/features/player/audio-player/web-player.tsx);
DSub [`DownloadService.java`](https://github.com/daneren2005/Subsonic/blob/edge/app/src/main/java/github/daneren2005/dsub/service/DownloadService.java);
Plexamp forum [limiter](https://forums.plex.tv/t/plexamp-3-limiter-possibly-causing-distortion-on-android/585205);
[EBU R128](https://tech.ebu.ch/docs/r/r128-2014.pdf);
[loudgain](https://github.com/Moonbase59/loudgain) (−18 LUFS reference).
