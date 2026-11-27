# symphonia-metadata 0.5.5, patched: long tag values and attached pictures are skipped unread

The player uses artwork and metadata from the library. The patched readers bound allocations for embedded tag values and pictures.

| Item | Value |
| --- | --- |
| Base | symphonia-metadata 0.5.5 from crates.io: `.cargo_vcs_info.json` (upstream commit `6d533f26150953a882a6a111ebd13f0abf7129d5`, path `symphonia-metadata`), crate SHA256 `36306ff42b9ffe6e5afc99d49e121e0bd62fe79b9db7b9681d48e29fa19e6b16` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-robust.patch` (the whole difference, `diff -ruN` against the base) |
| Licence | MPL-2.0 (`Cargo.toml`). The changed files stay under MPL-2.0 |
| Wired by | `[patch.crates-io] symphonia-metadata = { path = "crates/vendor/symphonia-metadata" }` in the workspace `Cargo.toml` |

Changed lines are marked `[q2 patch]`. The limit is
`symphonia_core::io::Q2_MAX_TAG_BYTES` (64 KiB).

## What changed

| Place | Upstream | Here |
| --- | --- | --- |
| `id3v2/frames.rs`, the ID3v2.2, 2.3 and 2.4 frame readers | every frame body read into memory | a frame longer than the limit is skipped with `ignore_bytes` and reported as an unsupported frame, which the tag reader already tolerates |
| `flac.rs` `read_picture_block` | `vec![0; len]` for the media type and the description; the picture read and stored | media type or description longer than the limit: decode error. A picture longer than the limit is skipped unread and not reported |
| `vorbis.rs` `read_comment_no_framing` | `vec![0; comment_length]` | a comment longer than the limit (a base64 picture, a damaged length) is skipped unread; the others are allocated as their bytes arrive |

A tag value or picture of at most 64 KiB is read and reported as before.

## Consequence for other users of this crate

`Metadata` no longer contains pictures or text values longer than 64 KiB.
Nothing in this repository reads them. A feature that wants embedded
artwork needs this limit raised or a streaming reader.
