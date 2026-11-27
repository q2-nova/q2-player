# Decoder fixtures

Input of `audio/decoder/corrupt_tests.rs`. Stereo at 44.1 kHz with the
right channel at minus half the left, no tags. Generated on 2026-11-21
with ffmpeg 8.1.3 (`-map_metadata -1 -fflags +bitexact -flags:a +bitexact`).
The lossy files hold a 440 Hz sine mixed with noise at 0.05; the lossless
files hold 440 Hz and 3111 Hz sines, which keeps them small.

| File | Encoder | Length | Frames |
| --- | --- | --- | --- |
| `tone.mp3` | `libmp3lame -b:a 64k` | 0.35 s | 15435 |
| `tone-aac.m4a` | `aac -b:a 64k` | 0.35 s | 15435 |
| `tone.ogg` | `libvorbis -b:a 64k` | 0.35 s | 15435 |
| `tone.flac` | `flac -sample_fmt s16` | 0.25 s | 11025 |
| `tone24.flac` | `flac -sample_fmt s32 -bits_per_raw_sample 24` | 0.25 s | 11025 |
| `tone-alac.m4a` | `alac -sample_fmt s16p` | 0.25 s | 11025 |

The tests build the tagged variants (ID3v2 with a picture, a FLAC PICTURE
block, an MP4 `covr` atom) and the WAV file in code.
