# libsqlite3-sys 0.39.1, patched: SQLite's compile options from the repository

Requirement: Product feedback, 2026-11-17: host tests use the same SQLite as the Q2, so a
bug in the device's SQLite build reproduces on the host.

| Item | Value |
| --- | --- |
| Base | libsqlite3-sys 0.39.1 from crates.io: `.cargo_vcs_info.json` (upstream commit `fb7c711f4e988db149f43cac68f7865b25d0b10e`, path `libsqlite3-sys`), crate SHA256 `14a5f01b87b92705d07b433c06f96cbc764637d81a0bffb5734ce17cf0f9a2b3` |
| Removed from the crate's files | `Cargo.lock`, `.cargo-ok` |
| Added | this file, `q2-flags-from-file.patch` (the whole difference, in `build.rs`) |
| Licence | MIT, `LICENSE` (unchanged); the bundled SQLite is public domain |
| Wired by | `[patch.crates-io] libsqlite3-sys = { path = "crates/vendor/libsqlite3-sys" }` in the workspace `Cargo.toml` |

The bundled build reads its extra `-D`/`-U` options from
`q2/toolchain/libsqlite3-flags` (relative to this directory:
`../../../q2/toolchain/libsqlite3-flags`) for every build: host tests, the
simulator and the Q2. Upstream reads them from the `LIBSQLITE3_FLAGS`
environment variable; that variable now stops the build, so an environment
cannot produce a second SQLite. Changed lines are marked `[q2 patch]`.

`library::db::sqlite_build_tests` checks `PRAGMA compile_options` against the
flags file, so a dependency update that drops this patch (Cargo then warns
"patch was not used" and builds the registry copy) fails the host tests.

To update: copy the new crates.io release here, remove `Cargo.lock` and
`.cargo-ok`, apply `q2-flags-from-file.patch`, update the table above, run
`cargo tree --offline -i libsqlite3-sys` to relock and run the host tests.
