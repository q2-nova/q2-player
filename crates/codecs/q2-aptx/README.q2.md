# q2-aptx

aptX and aptX-HD encoder for the A2DP source. `src/lib.rs` is a safe wrapper
(`Encoder::new(hd)`, `Encoder::encode(&[i16], &mut Vec<u8>)`) over the vendored
C library libopenaptx 0.2.0. `build.rs` compiles the C with `cc` at `-O2`.

## What is vendored

Source: https://github.com/pali/libopenaptx, tag `0.2.0`, commit
`2459ed4686eaef0a19dfa3f330a960813c5f60de` (2020-04-11), fetched 2026-11-21 as
`https://github.com/pali/libopenaptx/archive/refs/tags/0.2.0.tar.gz`
(sha256 `cca8b6d82d2256369b314855b3496ac2f0029adbc0adb66c770c0bf38c2c2cfc`).

| File in `vendor/openaptx/` | Upstream file | Change |
|---|---|---|
| `openaptx.c` | `openaptx.c` | none |
| `openaptx.h` | `openaptx.h` | none |
| `LICENSE` | `COPYING` | renamed |
| `UPSTREAM.json` | - | written here: url, tag, commit, hashes |

`openaptx.c` holds encoder and decoder in one file, so the decoder is compiled
too; the Rust wrapper declares only `aptx_init`, `aptx_encode` and
`aptx_finish`. Not vendored: `Makefile`, `README`, `openaptxenc.c`,
`openaptxdec.c` (command-line tools).

The library exports the unprefixed C symbols `aptx_*`. Linking another copy of
libopenaptx into the same binary would collide.

## Licence

libopenaptx 0.2.0 is LGPL-2.1-or-later. Checked in the fetched archive:

- `COPYING` is the GNU Lesser General Public License, version 2.1 text.
- The headers of `openaptx.c` and `openaptx.h` say "version 2.1 of the
  License, or (at your option) any later version".
- The upstream `README` says "derived from ffmpeg 4.0 project and licensed
  under LGPLv2.1+".

Version 0.2.1 (commit `811bc18586d634042618d633727ac0281d4170b8`, 2021-04-25) is
GPL-3.0-or-later: its `COPYING` is the GPL version 3 text, its source headers
say GPL version 3 or later, and its commit message says "License was upgraded
to GPLv3+". 0.2.1 was fetched and read for this check only (archive sha256
`a19b3b642def6f86441b73dd4efda6e6f13d49e7e2d6e5e91cc7eb464ebb505a`); nothing
from it is in this crate. Do not update past 0.2.0.

The C is linked statically into the player. Binary releases accompany the
library’s source and LGPL license with the exact player source and a
[documented relink procedure](../../../docs/relink.md).

## Patents

Checked on 2026-11-21. This is a record of what was looked up, not legal
advice.

Verified from the cited source:

- EP0398973B1, "Method and apparatus for electrical signal coding", applicant
  Audio Processing Technology Ltd, inventor S.M.F. Smyth: priority 1988-02-13,
  filed 1989-02-10, granted 1994-10-12. Google Patents lists its status as
  "Revoked" and its anticipated expiration as 2009-02-10
  (https://patents.google.com/patent/EP0398973B1/en). The listed family
  members are EP, JP, AT, DE, GB and WO; no US member is listed.
- Wikipedia's aptX article names EP0398973B1 as the patent that describes the
  aptX algorithm, and lists the related patents for aptX HD and later variants
  as "?" (https://en.wikipedia.org/wiki/AptX, read 2026-11-21).
- The Android Open Source Project carries aptX and aptX HD encoder sources
  under Apache-2.0 in `packages/modules/Bluetooth/system/embdrv/encoder_for_aptx`
  and `encoder_for_aptxhd` (file headers "Copyright (C) 2022 The Android Open
  Source Project", Apache License 2.0). Apache-2.0 section 3 grants a patent
  licence from contributors for their contributions to that code. It does not
  cover libopenaptx.
- FFmpeg has shipped an aptX and aptX HD encoder and decoder under LGPL-2.1+
  since 4.0 (2018); libopenaptx is derived from it (upstream README).

Not verified:

- That EP0398973B1 is the only patent on the aptX algorithm. The claims were
  not read against the code, and no search for other aptX patents (US filings
  in particular) was made.
- Any patent that covers aptX HD. In this code
  it differs from aptX in its quantiser tables and codeword width. No patent
  number for it was found, and none was ruled out.
- That Qualcomm is the contributor of the AOSP sources. The file headers name
  only the Android Open Source Project; the AOSP history page refused an
  anonymous request (HTTP 403).
- Any Qualcomm statement that aptX or aptX HD is free of patent claims. None
  was found.
- Trademark and certification terms. "aptX" is a Qualcomm trademark; the terms
  for using the name or logo on a product were not looked up.

## Behaviour

- Input: interleaved stereo `i16`, a multiple of 8 samples (4 stereo frames)
  per call; other lengths panic. Each sample is passed to the codec as
  `sample << 8` (24-bit).
- Output: 4 bytes per 4 stereo frames for aptX (352.8 kbit/s at 44.1 kHz),
  6 bytes for aptX HD (529.2 kbit/s). Each codeword is left then right,
  big-endian, as libopenaptx and FFmpeg write it.
- The codec is sample-rate agnostic; the rate is signalled in the A2DP
  configuration.
- The wrapper never calls `aptx_encode_finish`, so the last 90 frames of a
  stream stay in the codec delay line. A continuous A2DP stream does not need
  them.
- `encode` converts through a 768-byte stack buffer and allocates only when
  `out` grows.

## Tests

`cargo test --offline --manifest-path crates/codecs/q2-aptx/Cargo.toml`

- `byte_rate`: N stereo frames give N bytes (aptX) and 1.5 N bytes (HD).
- `encoder_vectors`: one second of an integer-generated signal (two tones,
  about 878 Hz and 4.8 kHz, plus LCG noise, peak about 11900); FNV-1a 64 of
  the input and of both encoded streams are constants in `src/lib.rs`.
- `call_boundaries_do_not_change_the_stream`, `appends_and_returns_appended_length`,
  `rejects_partial_codeword`, `encoder_is_send`.
- `dump_for_external_decoder`: with `Q2_APTX_DUMP=<dir>` writes `signal.s16`,
  `signal.aptx`, `signal.aptxhd` (4 s); otherwise does nothing.

## Benchmark

```sh
cargo run --release --offline --manifest-path crates/codecs/q2-aptx/Cargo.toml --example bench
```

## Re-fetch and verify

```sh
mkdir -p build/codec-aptx/dl build/codec-aptx/x020
curl -sSL -o build/codec-aptx/dl/libopenaptx-0.2.0.tar.gz \
  https://github.com/pali/libopenaptx/archive/refs/tags/0.2.0.tar.gz
sha256sum build/codec-aptx/dl/libopenaptx-0.2.0.tar.gz   # cca8b6d8...c2cfc
tar -xzf build/codec-aptx/dl/libopenaptx-0.2.0.tar.gz -C build/codec-aptx/x020
for f in openaptx.c openaptx.h; do
  cmp build/codec-aptx/x020/libopenaptx-0.2.0/$f crates/codecs/q2-aptx/vendor/openaptx/$f
done
cmp build/codec-aptx/x020/libopenaptx-0.2.0/COPYING crates/codecs/q2-aptx/vendor/openaptx/LICENSE
sha256sum crates/codecs/q2-aptx/vendor/openaptx/*        # compare with UPSTREAM.json
```

## Integration notes

- A member of the repository workspace (root `Cargo.toml`); it has no
  `[workspace]` table or `.gitignore` of its own.
- The build dependency is `cc = "=1.10.0"`, which pulls `shlex` and
  `find-msvc-tools`.
