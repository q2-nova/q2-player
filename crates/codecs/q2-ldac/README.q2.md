# q2-ldac

LDAC encoder for the Q2's Bluetooth A2DP source: a safe Rust wrapper
(`src/lib.rs`) over the `ldacBT` API of AOSP libldac, vendored as C under
`vendor/libldac/` and compiled by `build.rs` with the `cc` crate.

There is no decoder here. libldac is encoder-only.

## What is vendored

| | |
|---|---|
| Source | https://android.googlesource.com/platform/external/libldac |
| Commit | `eeee1a3f5f8df1282e3a6d297085885fd886737b` (head of `refs/heads/main` on 2026-11-21; committed 2025-01-10), tree `d5e246f173f24c2e7743af9091a27d1329fcf2ce` |
| Library version | 2.0.2 (`LDACBT_LIB_VER_*` in `src/ldacBT_api.c`) |
| Files | `src/*.c`, `src/*.h` (29 files), `inc/ldacBT.h`, `LICENSE`, `NOTICE` |
| Left out | `abr/` (adaptive bit rate library), `fuzzer/`, `Android.bp`, `METADATA`, `MODULE_LICENSE_APACHE2`, `OWNERS` |
| Local changes | None. Every vendored file is byte-identical to upstream. |

`vendor/libldac/UPSTREAM.json` has the sha256 of every vendored file.

`build.rs` compiles `src/ldaclib.c` and `src/ldacBT.c`, which `#include` all
the other `.c` files; upstream's `Android.bp` lists the same two sources and
the same `-O2`. The fixed-point `*_fixp_*` files are compiled only with the
`fixed-point` feature.

## Licence

Apache License 2.0. Evidence, all read from the commit above:

- `LICENSE` is the Apache-2.0 text.
- All 30 `.c`/`.h` files carry the Apache-2.0 header with
  `Copyright (C) 2003 - 2016/2017` or `2013 - 2016/2017 Sony Corporation`.
- `Android.bp` declares `SPDX-license-identifier-Apache-2.0` with `LICENSE`
  and `NOTICE` as licence text; `MODULE_LICENSE_APACHE2` exists.
- No file contains non-commercial terms.

`NOTICE` is not a copyright notice. Its whole text is:

```
---------------
 Certification
---------------
   Taking the certification process is required to use LDAC in your products.
   For the detail of certification process, see the following URL:
      https://www.sony.net/Products/LDAC/aosp/
```

Apache-2.0 section 4(d) requires a distribution of this code to carry the
`NOTICE` text, and says the contents of a NOTICE file "are for informational
purposes only and do not modify the License".

## LDAC status

Verified on 2026-11-21:

- The encoder source is published by AOSP under Apache-2.0 (above).
  Section 3 of the licence is a patent grant from each contributor for its
  contribution; section 6 grants no trademark rights.
- Sony's `NOTICE` in the same repository says certification "is required to
  use LDAC in your products" and points to
  https://www.sony.net/Products/LDAC/aosp/.
- That page is addressed "To Android Smartphone and Tablet Developers". It
  offers a login and an inquiry form "for Corporate customers (In case you
  wish to license agreement)". The certification terms are behind the login.
- Sony's LDAC developer page (https://www.sony.net/Products/LDAC/) says:
  "LDAC source code was contributed to Android Open Source Project, allowing
  SRC (Smartphone) manufacturers to install LDAC software for FREE", with the
  footnote "A license from Sony is required to implement the LDAC software
  (SNK)". SRC is the A2DP source (this encoder), SNK the sink (a decoder).
- The same pages write the name as "LDAC™".
- AOSP libldac contains no decoder.

Not verified:

- The content, cost or conditions of Sony's certification programme, and
  whether it has any commercial/non-commercial distinction. The terms are not
  public.
- Whether Sony's certification statement applies to a product that is not an
  Android phone or tablet, such as this player. Sony's "for FREE" sentence
  names smartphone manufacturers only.
- Whether the certification statement binds a user of the Apache-2.0 code
  beyond the licence text. The licence says a NOTICE does not modify it; this
  is not legal advice.
- Trademark registrations for "LDAC" (no registry was checked), and so what
  showing the LDAC name or logo in the UI or documentation requires.
- Patents. Nothing beyond the Apache-2.0 section 3 grant was checked.

## API

```rust
pub enum Quality { High, Standard, Mobile }
pub struct Packet { pub data: Vec<u8>, pub frames: u8 }
impl Encoder {
    pub fn new(rate: u32, quality: Quality, mtu: usize) -> Result<Encoder, String>;
    pub fn frame_samples(&self) -> usize;
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Option<Packet>, String>;
    pub fn bitrate(&self) -> u32;   // kbit/s, as libldac reports it
}
pub mod a2dp { /* capability constants from ldacBT.h */ }
```

Behaviour that comes from libldac:

- Input is 16-bit stereo. Rates 44100, 48000, 88200, 96000. An LDAC frame is
  128 samples per channel at 44.1/48 kHz and 256 at 88.2/96 kHz.
- libldac sizes packets for a 2-DH5 baseband packet whatever MTU it is given.
  It rejects an MTU below 679, reserves 18 bytes for headers and never puts
  more than 661 bytes of frames in a packet. An MTU of 895 gives the same
  packets as 679.

  | Quality | Frame bytes | Frames per packet | Payload bytes | kbit/s at 44.1/88.2 | kbit/s at 48/96 |
  |---|---|---|---|---|---|
  | High | 330 | 2 | 660 | 909 | 990 |
  | Standard | 220 | 3 | 660 | 606 | 660 |
  | Mobile | 110 | 6 | 660 | 303 | 330 |

- `Packet::data` is whole LDAC frames, each starting with the sync byte 0xAA.
  The caller adds the RTP header and the one-byte LDAC media payload header
  (frame count in the low four bits).
- `encode` returns the packet on the call that completes it: every 2nd, 3rd
  or 6th call. No frame is held back at the start of a stream.
- The quality is fixed for the life of an `Encoder`. Adaptive bit rate
  (`ldacBT_alter_eqmid_priority`, `abr/`) is not wrapped.
- There is no flush. Frames of an incomplete packet are dropped with the
  `Encoder`.
- `ldaclib_init_encode` writes four global table pointers
  (`set_mdct_table_ldac`). `lib.rs` takes a process-wide `RwLock` for writing
  around initialisation and for reading around encoding, so encoders on
  different threads do not race on them.

A2DP codec capability (vendor-specific codec, 8 bytes of codec information),
from `inc/ldacBT.h`:

| Octet | Content |
|---|---|
| 0-3 | Vendor id 0x0000012D (Sony), little-endian: `2D 01 00 00` |
| 4-5 | Codec id 0x00AA, little-endian: `AA 00` |
| 6 | Sampling frequency mask: 0x20 44100, 0x10 48000, 0x08 88200, 0x04 96000, 0x02 176400, 0x01 192000; bits 7-6 zero |
| 7 | Channel mode mask: 0x04 mono, 0x02 dual channel, 0x01 stereo; bits 7-3 zero |

This encoder accepts the first four rates and uses stereo only.

## Float and fixed point

libldac has two signal paths chosen at compile time. The default is
single-precision float (`SCALAR` is `float`; `sqrt` and `fabs` from libm).
`-D_32BIT_FIXED_POINT` selects a 32-bit integer path; upstream's `Android.bp`
recommends it "for devices without a FPU". The Q2 has an FPU, so the default
build is float. The cargo feature `fixed-point` builds the integer path. The
two produce different bits.

Nothing in libldac uses SIMD intrinsics. MSA is used only if the C compiler
vectorises with `-mmsa`.

`build.rs` passes `-ffp-contract=off` where the compiler accepts it, so that
no target fuses multiply-adds and the float output is the same everywhere.
With the rig SDK's GCC 15.3 for mips32r2 it changes nothing: the float build
with and without the flag is the same size and has the same `madd.s`/`msub.s`
count (15; those instructions round twice on MIPS32r2 and are not a
contraction).

## Tests

`cargo test --offline --manifest-path crates/codecs/q2-ldac/Cargo.toml`

- `frame_samples_and_bitrates`: the table above for all four rates.
- `packets_respect_mtu_and_frame_counts`: MTU 679, 895 and 1005, all rates
  and qualities; every packet is 660 bytes with 2, 3 or 6 frames and fits the
  MTU with 13 bytes of RTP and payload header.
- `encoder_vectors`: FNV-1a (64-bit) of the payloads of 600 calls on a
  generated signal (1 kHz and 7 kHz tones plus noise, integer arithmetic
  only, `tests/common/signal.rs`) for eight rate/quality pairs. Float build
  only.
- `threads_do_not_interfere`, `bad_arguments_are_errors`, `encoder_is_send`.
- `dump_for_external_decoder`: does nothing unless `Q2_LDAC_DUMP=<dir>` is
  set; then writes 5 s per rate and quality as `ldac_<quality>_<rate>.ldac`
  (raw LDAC frames) and `.pcm` (the input, interleaved s16le).

## Benchmark

```sh
cargo run --release --offline --manifest-path crates/codecs/q2-ldac/Cargo.toml --example bench
```

## Re-fetch and verify

```sh
C=eeee1a3f5f8df1282e3a6d297085885fd886737b
mkdir -p build/codec-ldac/dl build/codec-ldac/src/libldac
curl -sSL -o build/codec-ldac/dl/libldac-$C.tar.gz \
  https://android.googlesource.com/platform/external/libldac/+archive/$C.tar.gz
tar -xzf build/codec-ldac/dl/libldac-$C.tar.gz -C build/codec-ldac/src/libldac
diff -r build/codec-ldac/src/libldac/src crates/codecs/q2-ldac/vendor/libldac/src
diff build/codec-ldac/src/libldac/inc/ldacBT.h crates/codecs/q2-ldac/vendor/libldac/inc/ldacBT.h
diff build/codec-ldac/src/libldac/LICENSE crates/codecs/q2-ldac/vendor/libldac/LICENSE
diff build/codec-ldac/src/libldac/NOTICE crates/codecs/q2-ldac/vendor/libldac/NOTICE
```

gitiles builds the tarball on request and its bytes change between downloads
of the same commit (three fetches gave three sha256 values), so the archive
hash in `UPSTREAM.json` identifies only the copy fetched on 2026-11-21. The
stable checks are the per-file sha256 values in `UPSTREAM.json` and the git
blob ids of the commit's tree
(`.../libldac/+/<commit>/src/?format=JSON`); the vendored files were checked
against both.

## Integration notes

- A member of the repository workspace (root `Cargo.toml`); it has no
  `[workspace]` table, `Cargo.lock` or `.gitignore` of its own.
- The only dependency is the build-dependency `cc` (pinned `=1.10.0`).
- `build.rs` emits `cargo:rustc-link-lib=m` on Unix targets for `sqrt`.
- A distribution that includes this code must include `vendor/libldac/LICENSE`
  and the `NOTICE` text.
