# symphonia-format-isomp4 0.5.5, patched: damaged MP4 files end in errors

Requirement, review and evidence: as `../symphonia-core/README.q2.md`
(Product feedback, 2026-11-21, goal G7; `docs/validation/code-review-2026-11-21.md`).

M4A (AAC, ALAC) and M4B files go through this demuxer. With the base crate,
2000 damaged copies of each of the three MP4 fixtures gave 46 files that would have ended the
installed player: `unwrap` on a missing descriptor, a zero timescale in
`TimeBase::new`, and table counts that reserved up to 64 GiB.

| Item | Value |
| --- | --- |
| Base | symphonia-format-isomp4 0.5.5 from crates.io: `.cargo_vcs_info.json` (upstream commit `6d533f26150953a882a6a111ebd13f0abf7129d5`, path `symphonia-format-isomp4`), crate SHA256 `243739585d11f81daf8dac8d9f3d18cc7898f6c09a259675fc364b382c30e0a5` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-robust.patch` (the whole difference, `diff -ruN` against the base) |
| Licence | MPL-2.0 (`Cargo.toml`). The changed files stay under MPL-2.0 |
| Wired by | `[patch.crates-io] symphonia-format-isomp4 = { path = "crates/vendor/symphonia-format-isomp4" }` in the workspace `Cargo.toml` |

Changed lines are marked `[q2 patch]`.

## What changed

| Place | Upstream | Here |
| --- | --- | --- |
| `atoms/mod.rs` `AtomHeader::table_entries`, `field_len` | none | a table's entry count must fit the atom that holds it; a field length is the atom's length minus its fixed part, checked |
| `atoms/stco.rs`, `co64.rs`, `stsz.rs`, `stts.rs`, `stsc.rs` | `Vec::with_capacity(count)` with the count as read ("TODO: Apply a limit") | `table_entries`: decode error when `count * entry size` exceeds the atom. A table atom of unknown length (size field 0) is refused |
| `atoms/stsc.rs` | `first_chunk - 1` and `n * samples_per_chunk` unchecked | chunk number 0 and a sample number that does not fit `u32`: decode error |
| `atoms/esds.rs` | `descriptor.unwrap()`; `data_len - 4` and `len - 3` unchecked; decoder specific info of any length | decode errors for a missing ES descriptor, a short atom, a short descriptor and decoder specific info above 64 KiB |
| `atoms/hdlr.rs` | `data_len - 24` unchecked, name read whole | checked; a name above 64 KiB is skipped unread |
| `atoms/ilst.rs` | tag values and names read whole ("TODO: Apply a limit"), lengths unchecked | checked; a value or name above 64 KiB (cover art, a damaged length) is skipped unread, and a skipped picture is not reported |
| `demuxer.rs` `try_new` | `TimeBase::new(1, 0)` panicked for a zero `mdhd` timescale | decode error |
| `demuxer.rs` `next_packet` | a sample of any `stsz` size read into memory | decode error above `Q2_MAX_SAMPLE_BYTES` (8 MiB; the largest sample of a codec mapped here is about 2 MiB). The player skips the packet (`Engine::step`) |

## Not changed

The other `unwrap`s in this crate follow an `is_none()` check that returns
an error. `stream.rs` and the fragmented-MP4 atoms (`moof`, `traf`, `trun`,
`sidx`) were not reviewed line by line; the damaged-file test reaches them
only when a damaged byte turns the fixture into something that looks
fragmented.
