# q2-aac: AAC-LC encoder for the A2DP source

Requirement: Product feedback, 2026-11-21: an AAC-LC encoder for the Bluetooth A2DP source;
no non-commercial licence terms; each licence recorded.

`q2-aac` wraps the VisualOn AAC encoder (vo-aacenc, Apache-2.0, fixed-point C) and
packs access units as RFC 3016 LATM. Everything below dated 2026-11-21 was fetched
or measured that day.

## Licence decision

Chosen: **vo-aacenc**, Apache-2.0.

| Candidate | Licence | Non-commercial clause | Arithmetic | Verdict |
| --- | --- | --- | --- | --- |
| vo-aacenc (VisualOn, from Android Stagefright) | Apache-2.0 | no | fixed point, integer only | chosen |
| fdk-aac (Fraunhofer) | "Software License for The Fraunhofer FDK AAC Codec Library for Android" (SPDX `FDK-AAC`, not OSI-approved) | no | fixed point | permitted by the owner's rule, with the conditions below; not chosen |
| FFmpeg native `aac` encoder | LGPL-2.1-or-later | no | floating point | permitted; not separable without a libavcodec/libavutil subset; not chosen |
| FAAC | LGPL-2.1-or-later | no | floating point | permitted; not evaluated further |

Evidence, from the licence texts fetched:

- **vo-aacenc.** `vendor/vo-aacenc/LICENSE` is upstream's `COPYING`. Compared word by
  word with https://www.apache.org/licenses/LICENSE-2.0.txt it is the Apache License
  2.0 up to the end of the terms, with the list markers of section 4 written
  `1.`-`4.` instead of `(a)`-`(d)`. Every vendored C file carries the Apache-2.0
  header with "Copyright 2003-2010, VisualOn, Inc.". `NOTICE` says "These files are
  Copyright 2003-2010 VisualOn, but released under the Apache2 License." No field-of-use
  or non-commercial term. Obligations when distributing: include the licence, keep the
  notices, include the NOTICE text, mark modified files.
- **fdk-aac.** Text read from `NOTICE` in https://github.com/mstorsjo/fdk-aac
  (sha256 `95ec80da40b4af12ad4c4f3158c9cfb80f2479f3246e4260cb600827cc8c7836`).
  It has no non-commercial restriction: section 2 permits "redistribution and use in
  source and binary forms, with or without modification ... without payment of
  copyright license fees". Its conditions: keep the licence text in source and in the
  documentation of binaries; give recipients of binaries the complete source free of
  charge; do not charge copyright licence fees for the library; modified versions carry
  dated change notices and are renamed "Third-Party Modified Version of the Fraunhofer
  FDK AAC Codec Library for Android"; no use of Fraunhofer's name. Section 3: "NO
  EXPRESS OR IMPLIED LICENSES TO ANY PATENT CLAIMS ... You may use this FDK AAC Codec
  software or modifications thereto only for purposes that are authorized by
  appropriate patent licenses." That sentence makes the copyright permission depend on
  patent status, which this project cannot establish (see Patents). Other readings of
  the same text: the FSF lists it as a free licence, GPL-incompatible, with a warning
  about the patent term (https://www.gnu.org/licenses/license-list.html#fdk); the
  Fedora wiki says it was to be reclassified not-allowed in August 2022 "because of its
  'no patent licenses' feature" while `fdk-aac-free` stays packaged
  (https://fedoraproject.org/wiki/Licensing/FDK-AAC); SPDX marks `FDK-AAC` as not
  OSI-approved. Under "no non-commercial terms, licence recorded" it is acceptable if
  the owner accepts section 3 and the source-offer and renaming conditions.
- **FFmpeg.** `LICENSE.md`: "Most files in FFmpeg are under the GNU Lesser General
  Public License version 2.1 or later". `libavcodec/aacenc.c` (2090 lines) has the
  LGPL-2.1+ header, includes 20 FFmpeg headers (`avcodec.h`, `codec_internal.h`,
  `encode.h`, `put_bits.h`, `psymodel.h`, `libavutil/float_dsp.h`, ...), works on
  `float` spectra and is written against `AVCodecContext`/`AVFrame`/`AVPacket`.
  Extracting it means carrying the psychoacoustic model, the coefficient coders, TNS,
  LPC, the MDCT (`libavutil/tx`) and parts of the codec framework, then maintaining
  that subset. Statically linked LGPL code also obliges the distributor to let
  recipients relink.
- **FAAC.** https://github.com/knik0/faac `COPYING` is LGPL-2.1; `libfaac/tns.c` has an
  LGPL-2.1+ header and includes `math.h`. Not examined beyond that.
- No other permissively licensed AAC-LC encoder was found.

Why vo-aacenc: the only candidate under a standard permissive licence with no
condition tied to patents, integer-only (the committed output hashes are identical on
x86_64 and mipsel), about 100 kB of code and a five-function C API. Its weakness is coding
efficiency (see Measurements): it does not use all of a high bitrate. If listening on
the Q2 shows that matters, fdk-aac is the alternative, subject to the owner accepting
its section 3.

## Patents

Not legal advice. What was read, and what could not be established:

Verified from fetched documents:

- Via Licensing Alliance still runs the AAC patent pool, and it covers AAC-LC.
  https://www.via-la.com/licensing-2/aac/ : "The license encompasses AAC Low Complexity
  (AAC-LC), High Efficiency AAC ...". "An AAC patent license is needed by manufacturers
  or developers of end-user encoder and/or decoder products." "There are no patent
  license fees due for the distribution of bit-streams encoded in AAC". Fees on that
  page: US $0.98 per unit for the first 500,000 units; initial fee US $15,000, or
  US $1,000 for entities with fifteen or fewer employees and under US $1 million
  revenue.
- The pool's patent list of Q3 2026
  (https://www.via-la.com/wp-content/uploads/2026/09/26Q3-AAC-Patent-List-website.pdf,
  98 pages, sha256 `22daba3c12ff47e28f153638cce6636de8848c8bc66067ab4ed64ea29e8e641a`)
  gives licensor, country and patent number only: no expiry dates and no mapping from
  patent to AAC profile. It has 645 US entries. The lowest-numbered US utility patent
  in it is 7,391,870; no US patent numbered below 7,000,000 is listed, so the US
  patents granted in the 1990s and early 2000s have left the list. 65 entries are
  reissues (`RE...`), whose original dates the list does not give.
- Neither licence that was considered grants pool patent rights. Apache-2.0 section 3
  grants only the contributors' own claims. vo-aacenc ships
  `aacenc/patent_disclaimer.txt`: "THIS IS NOT A GRANT OF PATENT RIGHTS. Google makes
  no representation or warranty that the codecs ... are unencumbered by third-party
  patents." The FDK licence excludes patent licences outright.

Not verified:

- Whether any patent still in force, in any country, is necessarily infringed by an
  AAC-LC encoder. The list does not say which patents read on AAC-LC rather than
  HE-AAC, xHE-AAC, AAC-ELD or MPEG-D DRC, and no patent was read.
- Expiry dates. Secondary sources disagree: English Wikipedia's "Fraunhofer FDK AAC"
  article says "The US patents on LC-AAC have expired", citing the Fedora wiki page,
  whose current text does not contain that statement; its "Advanced Audio Coding"
  article says "the last baseline AAC patent expires in 2028", citing a Hydrogenaudio
  forum post that could not be fetched. Neither is a primary source.
- Whether building and using this encoder on the owner's own player, or distributing
  the player's source or binaries, needs a pool licence.

## What is vendored

`vendor/vo-aacenc/` is a subset of https://github.com/mstorsjo/vo-aacenc at commit
`a277487e051e92e99a532294eed3c673f4d879f2` (2017-06-28, master, 11 commits after tag
v0.1.3). Among the later commits: a fix for the average band energy falling below
the minimum energy, and compiler-warning fixes. `UPSTREAM.json` has the archive URL and
SHA-256 and the SHA-256 of each file.

- Kept: `aacenc/src/*.c` (26 files), `aacenc/inc/*.h`, `aacenc/basic_op/*`,
  `common/cmnMemory.c`, six headers of `common/include/`, `COPYING` as `LICENSE`,
  `NOTICE`, `aacenc/NOTICE`, `aacenc/patent_disclaimer.txt`.
- Left out: the ARM assembly (`aacenc/src/asm`), the Android OMX wrappers (`*.cpp`),
  sample code, the PDF manual, the autotools files, the command-line encoder and
  `voAMRWB.h`.
- Local changes to vendored files: none. Each file equals its archive member.

`build.rs` compiles the C with `-O2 -fwrapv -DUSE_DEAULT_MEM` (upstream's spelling;
it selects the library's malloc/free operators). `-fwrapv` is added because UBSan
reports wrapping signed additions in `tns.c` and `basic_op.h`; the flag defines them.
`tns.c` alone is compiled with `-D__unused=`, which upstream's `Makefile.am` passes to
every file; it is limited to the one file that needs it because libc headers may use
`__unused` as an identifier. The compiler and target flags are what `cc` reads from the
environment (`CC_<target>`, `CFLAGS_<target>`, ...).

To re-fetch and verify (scripts are in the git-ignored `build/codec-aac/`):

```sh
curl -sL -o build/codec-aac/dl/vo-aacenc-a277487e.tar.gz \
  https://github.com/mstorsjo/vo-aacenc/archive/a277487e051e92e99a532294eed3c673f4d879f2.tar.gz
sha256sum build/codec-aac/dl/vo-aacenc-a277487e.tar.gz   # e67b2461...6a3b96, as in UPSTREAM.json
python3 -I build/codec-aac/make_upstream.py verify \
  crates/codecs/q2-aac/vendor/vo-aacenc build/codec-aac/dl/vo-aacenc-a277487e.tar.gz
```

Without the script: unpack the archive in an empty directory and compare each path in
`UPSTREAM.json` `files` with the same path in the archive (`LICENSE` with `COPYING`).

## API notes

- `Encoder::new(rate, bitrate)`: rate 44100 or 48000; bitrate 8000 to 320000. The C
  library replaces a stereo bitrate outside that range with its default without
  reporting it, so the wrapper refuses it.
- One access unit per 1024 stereo frames, at most 1536 bytes. The stream is constant
  bitrate with a bit reservoir; unused bits are written as fill elements.
- Encoder delay: 1600 samples (measured below). A sink hears the first input sample
  1600 samples into the decoded stream.
- `Encoder` is `Send`: the C library has no writable globals (its objects have no
  `.data`/`.bss` symbols).
- `latm` writes `audioMuxVersion` 0, so the header is 45 bits and an element is 7 bytes
  longer than its access unit plus one byte per full 255 bytes. For 44.1 kHz the
  element starts `20 00 12 10 1F E0`.
- The encoder's allocation failure path is not hardened: `cmnMemAlloc` returns success
  when `malloc` returns NULL, and the library then writes through the null pointer.
  The encoder allocates only in `Encoder::new`.

## Measurements (2026-11-21)

Host: AMD Ryzen 9 5900XT, GCC, FFmpeg 8.1.3. Nothing here was measured on the Q2.

### Tests

`cargo test --offline --manifest-path crates/codecs/q2-aac/Cargo.toml`: 10 tests pass
(7 encoder, 3 LATM). The encoder-vector hashes (FNV-1a over 100 access units each) were
reproduced by the same C sources built with the rig SDK's
`mipsel-buildroot-linux-gnu` GCC 15.3.0 and run under qemu-user
(`build/codec-aac/xcheck.sh`): all four equal the committed x86_64 values. The crate
itself was not built through cargo for the Q2 target.

`CC=clang build/codec-aac/asan.sh` (ASan and UBSan) over the same runs (four
configurations of 100 frames, then 30 s at 256 and 320 kbit/s): no address errors;
UBSan reports left shifts of negative values (several files) and the signed-addition
wraps named above.

### Bench

`cargo run --release --offline --manifest-path crates/codecs/q2-aac/Cargo.toml --example bench`

| Bitrate | Host, ms per audio second | Measured bitrate |
| --- | --- | --- |
| 256000 | 8.4 | 256000 |
| 320000 | 8.2 | 319725 |

The mipsel build under qemu-user on the same host takes 48 to 49 ms per audio second;
that is an emulator figure, not a Q2 figure. Code size for mipsel at `-O2`: 99.9 kB
text and read-only tables, 68 bytes data (`build/codec-aac/mips_size.sh`).

### FFmpeg decode check

```sh
Q2_AAC_DUMP=$PWD/build/codec-aac/dump cargo test --offline \
  --manifest-path crates/codecs/q2-aac/Cargo.toml --test encoder dump_for_ffmpeg
python3 -I build/codec-aac/ffmpeg_check.py build/codec-aac/dump   # needs numpy: the main checkout's build/venv
```

Input: the tests' generated signal, 10 s at 44.1 kHz (441000 frames, 430 access units):
440 Hz and 3 kHz tones on the left, 660 Hz and 5 kHz on the right, white noise at
+-512 on both. FFmpeg decodes the ADTS file and the LOAS file (each `latm()` element
behind a LOAS sync header) to the same PCM at every bitrate, without errors.

- Decoded length: 440320 frames = 430 x 1024 (9.985 s) for 430 access units.
- Delay, found by least-squares search over 0 to 4096 samples: 1600 samples on both
  channels at 128, 256 and 320 kbit/s.

Waveform SNR over the aligned overlap after the first 4 frames, left / right, in dB.
"Below 16 kHz" counts only spectral content under 16 kHz (the encoder low-passes at
20 kHz, which caps the full-band figure on a signal with white noise). The other two
encoders are FFmpeg's own, on the same input, measured the same way.

| kbit/s | q2-aac | q2-aac below 16 kHz | FFmpeg `aac` below 16 kHz | `libfdk_aac` below 16 kHz |
| --- | --- | --- | --- | --- |
| 128 | 31.4 / 22.7 | 33.1 / 23.4 | 29.0 / 27.4 | 35.2 / 32.9 |
| 256 | 32.4 / 25.4 | 34.4 / 26.1 | 38.4 / 36.4 | 44.6 / 40.9 |
| 320 | 34.3 / 30.7 | 37.8 / 33.8 | 44.3 / 42.2 | 47.6 / 44.0 |

Second input, `Q2_AAC_INPUT=build/codec-aac/pink.s16`: 10 s of independent pink noise
per channel from FFmpeg's `anoisesrc` (amplitude 0.4). Delay 1600, length 430 x 1024.

| kbit/s | q2-aac | q2-aac below 16 kHz | FFmpeg `aac` below 16 kHz | `libfdk_aac` below 16 kHz |
| --- | --- | --- | --- | --- |
| 128 | 9.1 / 9.1 | 9.9 / 10.1 | 10.2 / 10.2 | 13.1 / 13.1 |
| 256 | 14.4 / 14.4 | 16.6 / 16.7 | 17.8 / 17.9 | 23.5 / 23.4 |
| 320 | 15.7 / 15.7 | 18.6 / 18.7 | 22.0 / 21.9 | 27.5 / 27.5 |

vo-aacenc does not spend all of a high bitrate. Its access units hit the requested
size, but part of each is fill elements (runs of zero bytes): zero bytes are 0.9 %,
14.5 % and 21.9 % of the stream at 128, 256 and 320 kbit/s on the generated signal,
and 9.8 %, 13.1 % and 20.5 % on pink noise, against at most 1.2 % for the other two
encoders. From 256 to 320 kbit/s its SNR rises 1.3 to 7.7 dB depending on signal and
channel. Waveform SNR is not a listening test; no listening test and no real music
were used.
