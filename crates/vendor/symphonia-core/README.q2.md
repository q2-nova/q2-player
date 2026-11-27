# symphonia-core 0.5.5, patched: allocations follow the bytes read; no panic on a zero time base

Requirement: Product feedback, 2026-11-21, goal G7 (`PROJECTS.md`): review the code,
add the missing correctness and performance tests, and use them to fix what
they find. Review, evidence and what remains:
`docs/validation/code-review-2026-11-21.md`.

The installed player is built with `panic = "abort"`, and a refused
allocation aborts as well. A length or count field of a damaged file reached
`vec![0; len]` and `Vec::with_capacity(count)` in the demuxers and metadata
readers, and a zero timescale reached a `panic!`. The test
`audio::decoder::corrupt_tests` (crate `q2nd`) damages small files of every
container and fails on a panic or on one allocation above 4 MiB.

| Item | Value |
| --- | --- |
| Base | symphonia-core 0.5.5 from crates.io: `.cargo_vcs_info.json` (upstream commit `6d533f26150953a882a6a111ebd13f0abf7129d5`, path `symphonia-core`), crate SHA256 `ea00cc4f79b7f6bb7ff87eddc065a1066f3a43fe1875979056672c9ef948c2af` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-robust.patch` (the whole difference, `diff -ruN` against the base) |
| Licence | MPL-2.0 (`Cargo.toml`). The changed files stay under MPL-2.0 |
| Wired by | `[patch.crates-io] symphonia-core = { path = "crates/vendor/symphonia-core" }` in the workspace `Cargo.toml` |

Changed lines are marked `[q2 patch]`.

## What changed

| Place | Upstream | Here |
| --- | --- | --- |
| `io::ReadBytes::read_boxed_slice_exact`, `read_boxed_slice` | `vec![0u8; len]`, then the read | capacity `min(len, Q2_READ_STEP)` (64 KiB), then growth in 64 KiB steps as the bytes arrive. An audio packet is one allocation of its length, as before; a length the stream cannot supply ends in the read error without the allocation |
| `io::Q2_READ_STEP`, `io::Q2_MAX_TAG_BYTES` | none | the step above, and the 64 KiB limit past which the patched symphonia-metadata and symphonia-format-isomp4 skip a tag value or attached picture unread |
| `units::TimeBase::new` | panics on a zero numerator or denominator | a zero becomes 1. The MP4 demuxer refuses a zero timescale before it gets here; the other callers pass sample rates, and the player refuses a stream whose decoded rate is 0 |

Every demuxer and metadata reader calls `read_boxed_slice_exact`, the
unvendored ones (Ogg, RIFF, `symphonia-utils-xiph`) included, so the first
row bounds all of them by the bytes the file holds.

## Not changed

A length field that the file can satisfy still allocates its bytes: a
damaged field in a 500 MB audiobook can read up to the rest of the file. The
readers that take lengths from tags, tables and samples apply their own
limits (`symphonia-metadata/README.q2.md`,
`symphonia-format-isomp4/README.q2.md`). The Ogg and RIFF readers have no
such limits here; `docs/validation/code-review-2026-11-21.md` lists them
under what remains.
