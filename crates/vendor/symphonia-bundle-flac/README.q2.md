# symphonia-bundle-flac 0.5.5, patched: subframes restructured for SIMD, with MSA kernels

The FLAC decoder is restructured for SIMD together with hand-written MSA,
measured on the Q2.

| Item | Value |
| --- | --- |
| Base | symphonia-bundle-flac 0.5.5 from crates.io: `.cargo_vcs_info.json` (upstream commit `6d533f26150953a882a6a111ebd13f0abf7129d5`, path `symphonia-bundle-flac`), crate SHA256 `c91565e180aea25d9b80a910c546802526ffd0072d0b8974e3ebe59b686c9976` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-simd.patch` (the whole difference, `diff -ruN` against the base), `src/q2frame.rs`, `src/q2bits.rs`, `src/q2simd.rs`, `src/q2prof.rs`, `src/q2tests.rs` |
| Licence | MPL-2.0 (`Cargo.toml`). The changed and added files stay under MPL-2.0 |
| Wired by | `[patch.crates-io] symphonia-bundle-flac = { path = "crates/vendor/symphonia-bundle-flac" }` in the workspace `Cargo.toml`; `q2nd` also names it directly for `symphonia_bundle_flac::q2` |

Changed lines and files are marked `[q2 patch]`. The demuxer (`demuxer.rs`,
`parser.rs`) and the frame header code (`frame.rs`) are unchanged.

## What changed

`decoder.rs` keeps all of upstream's subframe code. `decode_inner` first
offers the frame to `q2frame::decode_frame`. That function declines
(`None`) anything it does not handle: every error, reserved values, a block
of 0 samples, a frame whose channel count is not the stream's, more than 32
bits in a subframe, and decoding with verification. Upstream's code then
decodes the frame from the start, so errors and odd frames behave as before
by construction.

| Stage | Upstream | Here |
| --- | --- | --- |
| Layout | one channel at a time: residuals, prediction, then passes over whole planes | two channels at a time in their output planes: both subframes' residuals, then prediction and the output pass alternating over runs of 480 frames (3.75 KiB, inside the 32 KiB L1) |
| Bit reading (`q2bits.rs`) | symphonia-core's `BitReaderLtr`: 64-bit bit cache, `io::Result` per read | a bit position and a 32-bit window built from four bytes; reads past the end return zeros and are detected once per partition |
| Rice residuals | `read_unary_zeros` + `read_bits_leq32` per residual | the stream as big-endian words, two of them in registers: a code's window is two shifts and an OR, count-leading-zeros gives the quotient, three shifts the remainder; the next word moves in by conditional move. One branch per code (quotient and remainder inside the 32-bit window), no load on the code-to-code path. Parameters above 20 and long quotients take a two-read path. Scalar: the codes are serial in the bit stream |
| Linear prediction | per channel, direct form, 64-bit `mult`/`madd` through HI/LO | both channels at once in transposed form: a vector's two 64-bit lanes are the two channels; per sample one `sra.d` and N `dpadd_s.d` (N = 4, 8, 12 or 32 by order) |
| Fixed prediction | 64-bit sums | 32-bit wrapping sums, history in registers |
| Decorrelation, wasted-bits shift, output shift | up to three passes over the planes | one pass, four frames per vector with MSA, 32 frames behind the predictor |

### The vector predictor

Direct form computes each sample from the N previous ones, so a new sample
enters N products of later samples one at a time. The transposed form keeps
N partial sums per channel: `s[j]` is the part of the prediction of frame
`i + j` that is known before frame `i`. A step is

    y        = s[0] >> shift
    s[j]     = s[j + 1] + tap[j] * y          (j < N - 1)
    s[N - 1] = (residual[i + N] << shift) + tap[N - 1] * y

N independent multiply-accumulates of the one new vector `y`; no multiply
waits for another. The loop is unrolled N steps, which keeps every partial
sum in a register (the accumulator of one `dpadd_s.d` is the source of none).

MSA instruction choices follow [X2000 instruction costs](https://github.com/q2-nova/q2-toolchain/blob/main/docs/x2000-msa.md):
- `dpadd_s.d`/`dotp_s.d` (32x32 to 64-bit products, 64-bit accumulate): 1.0
  cycle per instruction, latency 5. `mulv.d` is 4.0 cycles (i64 lanes are
  lane-serial) and is not used. The 64-bit lanes hold exact sums, so there is
  no 32-bit/64-bit split by sample width: 16-bit and 24-bit streams run the
  same kernel.
- The loop-carried path is `dpadd_s.d` then `sra.d`, once per sample pair.
- The residual is folded into the sum N frames ahead (`dotp_s.d` with
  `1 << shift`), off the loop-carried path.
- Lane = channel: a recurrence through MSA instructions is slower per step
  than scalar code (3-5 cycles against 1), so the vector runs two
  independent streams, as the rules there say.
- Whole-vector `ld.w`/`st.w` only: four frames of each plane per load and
    store, arranged with `ilvr.d`/`ilvl.d` and `pckev.w`/`pckod.w` (1.0 cycle
    each); no `insert.w`/`copy_s.w`. The planes are symphonia's buffer and
    not 16-byte aligned: 2 loads per 4 frames at 1.5 cycles instead of 1.0.
- One call per run of at most 480 frames into a
  `#[target_feature(enable = "msa")]` function; the vector operations are
  inlined into it.

A first version decoded into one interleaved work buffer (8 bytes per
frame, 37 KiB for a 4,608-frame block). On the Q2 its output pass ran at L2
copy speed (5 cycles per sample) and prediction at 12; the planar layout
with 480-frame runs replaced it (`docs/app-size-and-speed.md`).

## Exactness

- Prediction: per lane the kernel computes upstream's sum in 64 bits. With
  |tap| <= 2^14 (15-bit precision) and 32-bit samples, 32 products sum to
  less than 2^51, so nothing wraps. Adding `residual << shift` before the
  arithmetic shift equals adding the residual after it, and the low 32 bits
  of the result are the wrapping 32-bit sum upstream stores.
- Fixed prediction: integer coefficients and no shift, so the low 32 bits of
  upstream's 64-bit sum are the wrapping 32-bit sum.
- Sample arithmetic wraps where upstream's wraps when compiled without
  overflow checks (the release profile). With overflow checks upstream
  panics on such streams; valid streams do not overflow.

## Switches

- `symphonia_bundle_flac::q2::msa()`, `set_msa(Some(false) | Some(true) | None)`:
  which kernels run; for tests and benchmarks.
- `q2::selftest()`: the scalar prediction and output code against the current
  kernels on generated input (every kernel length, shifts 0-15, samples of
  1-32 bits, every decorrelation).
- `--cfg q2_flac_no_msa`: the capability check always says
  no. The structure-only arm of an A/B.
- `--cfg q2_flac_profile`: stage timers (`src/q2prof.rs`), reported by the
  `decode` benchmark as `flac_profile`.

## Checks

- Unit tests (`src/q2tests.rs`, `src/q2bits.rs`), against upstream's code in
  the same crate:
  - 1,600 generated valid frames (1-8 channels, 4-32 bits, every subframe
    type, orders 1-32, precisions 1-15, shifts 0-15, Rice parameters to 30,
    escaped partitions, wasted bits, every channel assignment): accepted by
    the restructured decoder, samples equal;
  - 6,000 truncated, bit-flipped and random frames: the same samples or the
    same error;
  - the vector kernels on a portable vector type against the scalar code;
  - the bit reader against `BitReaderLtr`.
  Run `cargo test --offline -p symphonia-bundle-flac` in the workspace, also
  with `--release` (wrapping arithmetic, so the damaged frames that make
  upstream panic under overflow checks are compared too).
- The player’s `--bench` mode `lossless-check` compares scalar and MSA output on the target architecture. Run it against a generated fixture with the target build and a matching QEMU user emulator or device.

To update: copy the new crates.io release here, remove `Cargo.lock` and
`.cargo-ok`, apply `q2-simd.patch`, update the table above, run
`cargo tree --offline -i symphonia-bundle-flac` to relock, then the checks.
