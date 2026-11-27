# q2-aac: AAC-LC encoder for the A2DP source

`q2-aac` wraps the VisualOn AAC encoder (vo-aacenc, Apache-2.0, fixed-point C) and
packs access units as RFC 3016 LATM.

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

To verify the vendored subset, download the archive named in
[`UPSTREAM.json`](vendor/vo-aacenc/UPSTREAM.json) and check its SHA-256.
Unpack into an empty directory, then compare each listed path and hash.
The vendored `LICENSE` corresponds to `COPYING` in the archive.

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

## Test and benchmark

```sh
cargo test --offline --manifest-path crates/codecs/q2-aac/Cargo.toml
cargo run --release --offline --manifest-path crates/codecs/q2-aac/Cargo.toml --example bench
```

Set `Q2_AAC_DUMP` to a generated output directory and run the
`dump_for_ffmpeg` encoder test to produce ADTS and LOAS files for
independent decoding.
