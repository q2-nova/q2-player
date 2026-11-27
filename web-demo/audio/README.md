# Browser demo audio

The MP3s contain original four-bar arrangements and original fictional excerpts written for the browser demo. They use no recordings or live broadcasts. The music tracks share one preview within each fictional album, and the radio stations share one instrumental preview. Each spoken item has its own recording. The browser repeats these short previews while the mock player displays the fixture's full timeline.

| File key | Fixture | Arrangement or voice |
| --- | --- | --- |
| `tidal` | Tidal Geometry | Slow glass tones over sustained ambient chords |
| `northline` | Northline | Bright guitar-like plucks with an indie backbeat |
| `afterhours` | After Hours | Minor-key synth hook with a four-on-the-floor kick |
| `sundial` | Sundial | Soft piano and sustained C-major chords |
| `softsignal` | Soft Signal | Syncopated breakbeat and bell lead |
| `fieldnotes` | Field Notes | Folk fingerpicking and brushed percussion |
| `copper` | Copper & Clay | Swung reed melody, piano chords and walking bass |
| `pinksky` | Under a Pink Sky | Bright synth-pop melody and dance percussion |
| `radio` | All radio stations | Warm electric piano over a laid-back beat |
| `spoken-0` | The Lantern Atlas | Flite `slt` |
| `spoken-1` | A Field Guide to the Night | Flite `rms` |
| `spoken-2` | A City Beneath the City | Flite `awb` |
| `spoken-3` | The Last Night Train | Flite `awb` |
| `spoken-4` | What Rivers Remember | Flite `slt` |
| `spoken-5` | The Orchard Almanac | Flite `rms` |

To regenerate from the repository root, run `python3 web-demo/audio/generate.py` with NumPy, Carnegie Mellon Flite and FFmpeg with `libmp3lame` installed. The generator contains the composition data, instruments and complete spoken scripts. It uses seeded percussion, synthesizes speech offline and writes 44.1 kHz MP3s. The checked-in set was produced with NumPy 2.4.6, Flite 2.2 and FFmpeg 8.1.3. The demo ships only the rendered audio. The synthesis tools and voice models are build-time dependencies.
