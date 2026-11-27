# symphonia-format-riff 0.5.5, patched: length limits for WAV and AIFF

This demuxer validates lengths in WAV and AIFF. The damaged-file suite includes inputs whose inner and outer lengths agree but would otherwise cause excessive allocations.

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

The player regression `riff_lengths_that_the_file_agrees_on_do_not_size_an_allocation` exercises both excessive lengths and a valid oversized metadata value.

## Not changed

The AIFF text chunks are not parsed by this version. The ADPCM block
arithmetic in `wave/mod.rs` and `aiff/mod.rs` was read, not changed: its
products feed `PacketInfo::with_blocks`, whose packets the limit above
bounds.
