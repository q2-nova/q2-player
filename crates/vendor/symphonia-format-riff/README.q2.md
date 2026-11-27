# symphonia-format-riff 0.5.5, patched: length limits for WAV and AIFF

Requirement, review and evidence: as `../symphonia-core/README.q2.md`
(Product feedback, 2026-11-21, goal G7; `docs/validation/code-review-2026-11-21.md`,
round 2, item 3).

WAV and AIFF files go through this demuxer. Round 1's damaged-file test
changed length fields at random; a field that disagrees with its enclosing
chunk is refused before it is used, so 40,000 cases found nothing. A file
whose lengths agree with each other reaches two places that had no limit:

| Item | Value |
| --- | --- |
| Base | symphonia-format-riff 0.5.5 from crates.io: `.cargo_vcs_info.json` (upstream commit `6d533f26150953a882a6a111ebd13f0abf7129d5`, path `symphonia-format-riff`), crate SHA256 `c2d7c3df0e7d94efb68401d81906eae73c02b40d5ec1a141962c592d0f11a96f` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-robust.patch` (the whole difference, `diff -ruN` against the base) |
| Licence | MPL-2.0 (`Cargo.toml`). The changed files stay under MPL-2.0 |
| Wired by | `[patch.crates-io] symphonia-format-riff = { path = "crates/vendor/symphonia-format-riff" }` in the workspace `Cargo.toml` |

Changed lines are marked `[q2 patch]`.

## What changed

| Place | Upstream | Here |
| --- | --- | --- |
| `wave/chunks.rs` `InfoChunk::parse` | `vec![0u8; len]` for a LIST/INFO value of any declared length ("TODO: Apply limit"): up to 4 GiB zeroed before a byte is read. The Q2 refuses that allocation, which aborts the player | a value above `symphonia_core::io::Q2_MAX_TAG_BYTES` (64 KiB) is skipped unread and reported empty, as the other metadata readers do. The player reads no embedded tags |
| `common.rs` `next_packet` (WAV and AIFF) | a packet is up to 1152 blocks of the `fmt ` chunk's block size, a 16-bit field: 72 MiB read into memory when the file has it | decode error above `Q2_MAX_PACKET_BYTES` (1 MiB; 1152 frames of eight 32-bit channels are 36 KiB). The player fails the track (`Engine::step`) |

Test: `riff_lengths_that_the_file_agrees_on_do_not_size_an_allocation`
(`crates/q2nd/src/audio/decoder/corrupt_tests.rs`). Before: one allocation of
1,073,741,824 bytes for a 60-byte file. After: nothing above 4 MiB, and a
real 2 MiB INFO value is skipped while the audio plays.

## Not changed

The AIFF text chunks are not parsed by this version. The ADPCM block
arithmetic in `wave/mod.rs` and `aiff/mod.rs` was read, not changed: its
products feed `PacketInfo::with_blocks`, whose packets the limit above
bounds.
