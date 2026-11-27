# symphonia-codec-alac 0.5.5, patched: packet decoding restructured, MSA output kernel

Requirement: Product feedback, 2026-11-18, `projects/cpu-optimization/README.md`:
restructure the ALAC decoder for SIMD together with hand-written MSA,
measured on the Q2. Results and the stage breakdown:
`docs/app-size-and-speed.md`, "ALAC decoding".

| Item | Value |
| --- | --- |
| Base | symphonia-codec-alac 0.5.5 from crates.io: `.cargo_vcs_info.json` (upstream commit `6d533f26150953a882a6a111ebd13f0abf7129d5`, path `symphonia-codec-alac`), crate SHA256 `8413fa754942ac16a73634c9dfd1500ed5c61430956b33728567f667fdd393ab` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-simd.patch` (the whole difference, `diff -ruN` against the base), `src/q2fast.rs`, `src/q2bits.rs`, `src/q2simd.rs`, `src/q2prof.rs`, `src/q2tests.rs` |
| Licence | MPL-2.0 (`Cargo.toml`). The changed and added files stay under MPL-2.0 |
| Wired by | `[patch.crates-io] symphonia-codec-alac = { path = "crates/vendor/symphonia-codec-alac" }` in the workspace `Cargo.toml`; `q2nd` also names it directly for `symphonia_codec_alac::q2` |

Changed lines and files are marked `[q2 patch]`.

## What changed

`lib.rs` keeps all of upstream's decoding code. `decode_inner` first offers
the packet to `q2fast::decode_packet`. That function declines (`None`)
anything it does not handle: every error, elements other than SCE, LFE, CPE
and END, a Rice parameter above 16, an element shorter than its predictor,
a mixing shift above 31. Upstream's code then decodes the packet from the
start, so errors and odd packets behave as before by construction.

| Stage | Upstream | Here |
| --- | --- | --- |
| Bit reading (`q2bits.rs`) | symphonia-core's `BitReaderLtr`: 64-bit bit cache, `io::Result` per read | a bit position and a 32-bit window built from four bytes |
| Adaptive Rice codes | up to three reads per code (prefix, suffix, last suffix bit) | one window per code: the prefix (at most 9 bits) and the suffix (at most 16) are in its 25 stream bits; count-leading-zeros of the complement gives the prefix; the suffix's two lengths are selected without a branch; the bit position stays in a register. Scalar: each code's parameter depends on the previous value |
| Adaptive prediction | iterator chains over a 32-entry coefficient slice; one branch on the residual's sign per sample | one loop per order (4, 5, 6, 8 unrolled; others generic), the coefficients and the differences from the oldest sample in registers; one coefficient-update loop for both residual signs (the sign is a mask, not a branch). Scalar; see below |
| Mixing, tail bits, output shift | three passes over the planes | one pass per element; four frames per vector with MSA (`mulv.w`, `sra.w`, `sll.w`, `or.v`; 0.5-1.0 cycle each, [X2000 instruction costs](https://github.com/q2-nova/q2-toolchain/blob/main/docs/x2000-msa.md)); the tail bits are kept as two planes so they load as vectors |

### Why the predictor is scalar

Each sample depends on the previous one twice: through the history and
through the coefficients, which move by one step per tap after every
non-zero residual, oldest tap first, until the residual's remainder changes
sign (a data-dependent early exit). In vector form the path from one sample
to the next is `subv.w`, `mulv.w`, a horizontal sum (two shuffles and two
adds), the rounding shift, the add and the sign-extension clip: about 30
cycles of MSA latency (3-4 per instruction, [X2000 instruction costs](https://github.com/q2-nova/q2-toolchain/blob/main/docs/x2000-msa.md): "a
loop-carried dependency through one MSA instruction per element: 3 cycles
per step against 1 for scalar integer code"). The coefficient update adds a
prefix sum over the taps on the same path. The scalar loop's path is about
13 cycles. Running the two channels as two lanes does not help either: the
early exit differs per channel and the per-tap chain is then 6 cycles per
tap in MSA. Abandoned for MSA; the scalar restructuring is kept.

## Exactness

The restructured code performs upstream's operations on the same 32-bit
values in the same order, with wrapping arithmetic where upstream's wraps
when compiled without overflow checks (the release profile). Two
rewritings:
- upstream's `val = past0 - sample`, `sign = val.signum()`: with
  `d = sample - past0`, `sign * val` is `|d|` and the coefficient moves by
  `signum(d)` for a positive residual and `-signum(d)` for a negative one.
  With `s = residual >> 31` (0 or -1), `(x ^ s) - s` is `x` or `-x`, which
  selects both the coefficient step and `|d|` or `-|d|` before the shift;
- a code whose suffix is 0 or 1 is one bit shorter and adds nothing:
  `value + ((suffix - 1) & -(suffix > 1))` and a length of
  `prefix + k + (suffix > 1)` replace upstream's second read;
- the output shift is applied in the element's output pass instead of over
  the whole buffer afterwards. Frames no element writes are zero and stay
  zero under the shift.

## Damaged files

The generated damaged-file suite in `q2nd` covers oversized frame
allocations and invalid partial-frame lengths.

| Place | Upstream | Here |
| --- | --- | --- |
| `MagicCookie::try_read` | any `frame_length`; the decoder's buffers are sized by it (a damaged cookie asked for 4 GiB) | decode error for 0 or more than 65536 frames (`Q2_MAX_FRAME_LENGTH`; encoders write 4096) |
| `decode_sce_or_cpe` | a partial frame's 32-bit sample count sliced the output unchecked: `range end index 268438289 out of range for slice of length 4096` | decode error when the count exceeds the frame length. `q2fast` already declined such a packet, which sent it to this code |

## Switches

- `symphonia_codec_alac::q2::msa()`, `set_msa(Some(false) | Some(true) | None)`:
  which output kernel runs; for tests and benchmarks.
- `q2::selftest()`: the scalar output pass against the current kernel on
  generated input.
- `--cfg q2_alac_no_msa`: the capability check always says no. The
  structure-only arm of an A/B.
- `--cfg q2_alac_profile`: stage timers (`src/q2prof.rs`), reported by the
  `decode` benchmark as `alac_profile`.

## Checks

- Unit tests (`src/q2tests.rs`), against upstream's code in the same crate:
  12,000 generated packets and 600 sequences of 5 (1-8 channels, bit depths
  1-32, every predictor order 0-31, modes 0 and 15, partial and uncompressed
  elements, tail bits of 8 and 16, mixing, random element payloads of five
  bit densities, truncated payloads): the same samples or the same error;
  the vector output kernel on a portable vector type against the scalar
  code. Run `cargo test --offline -p symphonia-codec-alac` in the workspace,
  also with `--release` (wrapping arithmetic, so the packets that make
  upstream panic under overflow checks are compared too).
- The player’s `--bench` mode `lossless-check` compares scalar and MSA output on the target architecture. Run it against a generated fixture with the target build and a matching QEMU user emulator or device.

To update: copy the new crates.io release here, remove `Cargo.lock` and
`.cargo-ok`, apply `q2-simd.patch`, update the table above, run
`cargo tree --offline -i symphonia-codec-alac` to relock, then the checks.
