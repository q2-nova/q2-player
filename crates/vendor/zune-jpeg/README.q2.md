# zune-jpeg 0.5.15, patched: MSA colour conversion and chroma upsampling

Requirement: Product feedback, 2026-11-18, `projects/cpu-optimization/README.md`:
hand-written MSA for measured hot paths, verified bit-identical in the
emulator. Measured with hardware counters on the Q2 (q2-os `docs/hardware.md`),
decoding `image/cover.jpg`: `ycbcr_to_rgb_inner_16_scalar::<false>` 23% of
cycles, `upsample_horizontal` 6.6%, `upsample_vertical` 4.5%. Upstream has
AVX2 and NEON versions of these and nothing for MIPS.

| Item | Value |
| --- | --- |
| Base | zune-jpeg 0.5.15 from crates.io: `.cargo_vcs_info.json` (upstream commit `31d81fed7551c8ccea456d9d8e2b1fd8bebb6995`, path `crates/zune-jpeg`), crate SHA256 `27bc9d5b815bc103f142aa054f561d9187d191692ec7c2d1e2b4737f8dbd7296` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-msa.patch` (the whole difference, `diff -ruN` against the base), `src/q2msa.rs` |
| Licence | MIT OR Apache-2.0 OR Zlib (`Cargo.toml`, `LICENSE-*`). The changed files stay under the same terms |
| Wired by | `[patch.crates-io] zune-jpeg = { path = "crates/vendor/zune-jpeg" }` in the workspace `Cargo.toml`; `q2nd` also names it directly for `zune_jpeg::q2` |

Changed lines are marked `[q2 patch]`.

## What changed

Output is the scalar code's, byte for byte. On every target except MIPS
with MSA the scalar functions run as before.

| Stage | Upstream on MIPS | Here, with MSA |
| --- | --- | --- |
| YCbCr to RGB (`worker::color_convert_ycbcr`, RGB output, width 16 or more) | `ycbcr_to_rgb_inner_16_scalar::<false>` through a function pointer per 16 pixels; the row's last 16 pixels through a temporary | `q2msa::rgb_row`, one call per row: 16 pixels per iteration, the last 16 pixels of the row converted in place |
| Horizontal upsampling (`choose_horizontal_samp_function`) | `scalar::upsample_horizontal` | `q2msa::upsample_horizontal`: 8 inputs to 16 outputs per iteration, scalar ends |
| Vertical upsampling (`choose_v_samp_function`) | `scalar::upsample_vertical` | `q2msa::upsample_vertical`: 8 inputs to 8 outputs in each of two rows per iteration |
| Both (`choose_hv_samp_function`, 4:2:0) | `scalar::upsample_hv` | `q2msa::upsample_hv`: the same vertical pass into the scratch row, then two horizontal passes |

Other output colour spaces (RGBA, BGR, BGRA), rows narrower than 16 pixels,
grayscale, CMYK/YCCK post-processing, the IDCT and Huffman decoding are
upstream's code.

The three `choose_*_samp_function` return the `q2msa` entries where they
returned the scalar functions (after the unchanged AVX2 and NEON branches).
Each entry checks `q2msa::msa()` per call (one relaxed atomic load) and
calls either the kernel or the scalar function. `color_convert_ycbcr`
checks once per call and then calls `q2msa::rgb_row` per row.
`color_convert::scalar` and `upsampler::scalar` became `pub(crate)`.
`lib.rs` enables `stdarch_mips` and `mips_target_feature` on MIPS only and
gains the public module `q2`. `Cargo.toml` declares `cfg(q2_jpeg_no_msa)`.

### Arithmetic

Colour, per pixel, in the scalar code: `cb - 128` and `cr - 128` in `i16`,
then in `i32`

    y0 = y * 16384 + 8191
    r = (y0 + cr * 22970) >> 14
    g = (y0 + cr * -11700 + cb * -5638) >> 14
    b = (y0 + cb * 29032) >> 14

each clamped to 0..=255. The kernel subtracts 128 with `addv.h` (wraps as
the release build's `i16` does), interleaves y with cr and y with cb
(`ilvr.h`/`ilvl.h`) and takes `dpadd_s.w` of those pairs against
`[16384, coefficient]` pairs, starting from 8191 in each word: one
instruction gives a pixel's whole `i32` sum for red or blue, two for green
(the second with `[0, -5638]` against the (y, cb) pairs). The sums are the
scalar code's `i32` values; none can overflow (|y * 16384| <= 2^29, largest
product 29032 * 32768 < 2^30). Then `srai.w` 14, `sat_s.w` to 9 bits
(-256..=255, which the `pckev.h` to halfwords keeps), `maxi_s.h` 0 and
`pckev.b` to bytes: the clamp to 0..=255. R, G and B bytes interleave with
`ilvr.b`/`ilvl.b` and four `vshf.b` into three 16-byte stores of exactly the
group's 48 bytes.

Upsampling in the scalar code is `(3 * a + 2 + b) >> 2` in `i16`
(arithmetic shift). The kernels use `mulv.h` by 3, `addvi.h` 2, `addv.h`,
`srai.h` 2 in halfword lanes, which wrap as the release build's `i16`
arithmetic does; the horizontal kernel interleaves the even and odd outputs
with `ilvr.h`/`ilvl.h`. A row that does not divide into vectors ends with
one vector over its last 8 elements, which recomputes the overlap from the
same inputs (input and output are different slices). The horizontal
filter's two ends, and rows shorter than a vector (8 elements; 10 for the
horizontal filter), use the same expression with wrapping scalar operations.

A debug build of the scalar code panics on `i16` overflow where the release
build wraps. The kernels exist only on MIPS; the Q2 build is a release build.

### Instruction choices

- No general-register to vector moves in a loop: coefficients are splat
  once per row (`fill.w`), the shuffle controls loaded once per row from a
  16-byte aligned table.
- `.h` and `.w` lanes only.
- `dpadd_s.w` on interleaved `i16` pairs instead of widening y, cb and cr
  to words (four `ilv` per plane and three `mulv.w` per four pixels more).
- `core::arch::mips` intrinsics in `#[target_feature(enable = "msa")]`
  functions; helpers are `#[inline]` functions with the same feature, no
  closures.

## Switches

- `zune_jpeg::q2::msa()`, `set_msa(Some(false) | Some(true) | None)`: which
  path runs; for tests and benchmarks.
- `zune_jpeg::q2::check(seed, rounds)`: compares the dispatching entries
  with the scalar functions on generated input;
  `zune_jpeg::q2::check_decoder_range()` on every decoder-range input.
- `--cfg q2_jpeg_no_msa`: the capability check always says
  no.

## Checks

- The crate's unit tests: upstream's, plus `q2msa::tests` (the dispatching
  entries against the scalar functions on generated input and on every
  decoder-range input; the row helper
  that `check` uses as its reference against `worker::color_convert`) and
  `upsampler::tests::q2_impl`. On a host these cover the scalar path and the
  dispatch. The crate is not a workspace member; run them with
  `cargo test --offline --locked -p zune-jpeg` from the workspace root.
- The player’s `--bench` mode `jpeg-check` compares scalar and MSA output on the target architecture. Run it against a generated fixture with the target build and a matching QEMU user emulator or device.

To update: copy the new crates.io release here, remove `Cargo.lock` and
`.cargo-ok`, apply `q2-msa.patch`, update the table above, run
`cargo tree --offline -i zune-jpeg` to relock, then the checks.
