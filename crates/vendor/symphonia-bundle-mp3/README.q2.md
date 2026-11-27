# symphonia-bundle-mp3 0.5.5, patched: layer 3 restructured for SIMD, with MSA kernels

Requirement: Product feedback, 2026-11-18, `projects/cpu-optimization/README.md`:
restructure the MP3 decoder for SIMD together with hand-written MSA, measured
on the Q2. Results and the stage breakdown:
`docs/app-size-and-speed.md`, "MP3 decoding".

| Item | Value |
| --- | --- |
| Base | symphonia-bundle-mp3 0.5.5 from crates.io: `.cargo_vcs_info.json` (upstream commit `6d533f26150953a882a6a111ebd13f0abf7129d5`, path `symphonia-bundle-mp3`), crate SHA256 `4872dd6bb56bf5eac799e3e957aa1981086c3e613b27e0ac23b176054f7c57ed` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-simd.patch` (the whole difference, `diff -ruN` against the base), `src/q2simd.rs`, `src/q2prof.rs`, `src/layer3/q2huffman.rs`, `src/q2ref/` (upstream's files, compiled only for tests) |
| Licence | MPL-2.0 (`Cargo.toml`; each source file carries the notice). The changed files stay under MPL-2.0 |
| Wired by | `[patch.crates-io] symphonia-bundle-mp3 = { path = "crates/vendor/symphonia-bundle-mp3" }` in the workspace `Cargo.toml`; `q2nd` also names it directly for `symphonia_bundle_mp3::q2` |

Changed lines and files are marked `[q2 patch]`.

## What changed

Output is bit-identical to upstream for every stage and every input: each
f32 the decoder returns has upstream's bits. No stage reorders a
floating-point sum.

| Stage | Upstream | Here |
| --- | --- | --- |
| Huffman decoding (`layer3/q2huffman.rs`) | symphonia-core's `BitReaderLtr`: 64-bit bit cache, `io::Result` per read | 32-bit window computed from the bit position; tables built from the same code lists; the sign set by OR instead of a multiply by ±1.0. Scalar |
| Requantize (`layer3/requantize.rs`) | `powf` per scale-factor band; scalar multiply | 2^(e/4) from a table of the 567 possible exponents (same expression); four samples per multiply |
| Mid-side stereo (`layer3/stereo.rs`) | scalar | four samples per step |
| Antialias (`layer3/hybrid_synthesis.rs`) | 8 scalar butterflies per boundary | two vectors of four butterflies, lower samples reversed |
| IMDCT, window, overlap (`layer3/hybrid_synthesis.rs`) | one scalar 36-point IMDCT per sub-band; overlap `[sb][i]` | four sub-bands per IMDCT (lane = sub-band); overlap `[i][sb]` so the lanes are adjacent floats; empty sub-bands copied four at a time |
| Frequency inversion | a pass over the granule | sign flips as the IMDCT output is stored; the pass is gone |
| Polyphase synthesis (`synthesis.rs`) | per time slot: strided gather, scalar DCT-32, scalar V build, scalar 16-tap window over 32 outputs | DCT-32 on four time slots at once (lane = time slot; the input is 32 plain vector loads); V vectors built with forward, negated and reversed vector stores; window with four adjacent outputs per vector, eight accumulators, each output's 16 products added in upstream's order; 32 V vectors of state instead of 16, 16-byte aligned |

The kernels are written once over `q2simd::Lanes` (four independent lanes).
`S4` is the portable implementation; `M4` is one MSA register and exists
only on MIPS, inside `#[target_feature(enable = "msa")]` entry points
reached only when `getauxval(AT_HWCAP) & 2` (`HWCAP_MIPS_MSA`) is set. The
check is cached in one `AtomicU8`. `lib.rs` goes from `forbid(unsafe_code)` to
`deny`, with `allow` on the MSA lane type, the capability check and the
entry points.

MSA instruction choices follow [X2000 instruction costs](https://github.com/q2-nova/q2-toolchain/blob/main/docs/x2000-msa.md):
- `fmul.w` + `fadd.w`, never `fmadd.w` (fused: different rounding; and 7
  cycles of accumulator latency);
- eight independent accumulators in the window;
- whole-vector `ld.w`/`st.w` only, no lane-by-lane assembly (`fill.w`,
  `insert.w`, `copy_s.w` cost 7 cycles per round trip); constants are splat
  once per call;
- transposes with `ilvr.w`/`ilvl.w`/`ilvr.d`/`ilvl.d`, reversal with `shf.w`
  (1 cycle each);
- state and tables 16-byte aligned (aligned `ld.w` 1.0 cycle, unaligned 1.5);
- the V vectors of a batch are stored before the first window starts, so a
  window does not load a vector stored a few instructions earlier (12 cycles);
- `core::arch::mips` intrinsics, not `std::simd`: LLVM lowered `std::simd`
  unaligned loads and stores to four scalar word copies through the stack
  and the transposes to `splati.w`/`insve.w` chains.

The clamp keeps `f32::clamp`'s result for NaN (`fclt.w` + `bsel.v` twice).
`fmax.w`/`fmin.w` would be two instructions shorter per vector and return
±1.0 for NaN.

## Conventions of `q2simd.rs` (for the other decoders)

- **Choose lanes that are independent outputs.** Four time slots, four
  sub-bands, four adjacent samples: each lane runs the scalar algorithm
  unchanged, so the result is bit-identical without an error bound. Do not
  split one sum across lanes.
- **Choose lanes that are adjacent in memory**, and change a state layout
  when that makes them adjacent (`overlap[i][sb]`). Where the natural layout
  is the other axis, transpose four vectors (8 instructions, 8 cycles);
  never gather lane by lane.
- **One generic kernel, two lane types.** Write the kernel as
  `#[inline(always)] fn kernel<V: Lanes>(..)`. The public function checks
  `q2simd::msa()` and calls either `kernel::<S4>` or a
  `#[cfg(target_arch = "mips")] #[target_feature(enable = "msa")] unsafe fn
  kernel_msa` that only calls `kernel::<M4>`. `M4`'s methods are
  `#[inline(always)]` wrappers of `core::arch::mips` intrinsics; they inline
  into the `target_feature` function (check with `nm`: no `__msa_` symbol
  may remain, and `objdump` should show no `lw`/`sw` in the loops).
- **Host tests run `S4`** and compare with upstream's code kept under
  `src/q2ref/` (`#[cfg(test)]`, `#[path]`). The MSA path is compared with
  `S4` in the emulator and on the Q2 by a benchmark kind that decodes with
  `set_msa(Some(false))` and then with the default.
- **Loads and stores take slices** (`V::load(&x[i..i + 4])`), so they stay
  bounds-checked; fixed-size arrays let LLVM remove the checks.
- **Align state and tables to 16 bytes** (`Aligned16`, `#[repr(align(16))]`).
- **Keep a store and the load of the same data apart** (12 cycles on the
  X2000): order the work so other work sits between them.
- Nightly features are enabled only for MIPS:
  `#![cfg_attr(target_arch = "mips", feature(stdarch_mips, mips_target_feature))]`.

## Switches

- `symphonia_bundle_mp3::q2::msa()`, `set_msa(Some(false) | Some(true) | None)`:
  which path runs; for tests and benchmarks.
- `--cfg q2_mp3_no_msa`: the capability check always says
  no. The structure-only arm of an A/B.
- `--cfg q2_mp3_profile`: stage timers (`src/q2prof.rs`), reported by the
  `decode` benchmark as `mp3_profile`. `--cfg q2_mp3_profile_fine` adds the
  DCT-32/window split inside synthesis (two more timer pairs per batch and
  slot; about 1.5 µs each on the Q2).

## Checks

- The crate's unit tests compare every rewritten stage with upstream's code
  (`src/q2ref/`) bit for bit on generated input: synthesis (every batch
  shape, state across calls), antialias + hybrid synthesis + frequency
  inversion (every block type and rzero, 400 granules of carried overlap),
  requantize, mid-side, and the Huffman reader (20,000 random bitstreams:
  same samples, same rzero, errors in the same cases). The crate is not a
  workspace member; run them with
  `cargo test --offline --manifest-path crates/vendor/symphonia-bundle-mp3/Cargo.toml`.
  They cover the scalar lanes.
- The player’s `--bench` mode `mp3-check` compares scalar and MSA output on the target architecture. Run it against a generated fixture with the target build and a matching QEMU user emulator or device.

To update: copy the new crates.io release here, remove `Cargo.lock` and
`.cargo-ok`, apply `q2-simd.patch`, refresh `src/q2ref/` from the new
release's `synthesis.rs`, `layer3/hybrid_synthesis.rs`,
`layer3/requantize.rs` and `layer3/stereo.rs`, update the table above, run
`cargo tree --offline -i symphonia-bundle-mp3` to relock, then the checks.
