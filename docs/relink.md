# Modify and relink aptX in the Q2 player

The Q2 player statically links libopenaptx 0.2.0 for aptX and aptX HD. The
library is licensed under the GNU Lesser General Public License, version 2.1
or, at your option, any later version. The player’s original code is
GPL-3.0-or-later. See the libopenaptx license and source attribution in
`crates/codecs/q2-aptx/vendor/openaptx/`. Binary releases include the notice,
complete license text and the corresponding player source in
`player-source.tar.gz`.

## Get matching sources and build tools

Verify the firmware archive and source archive with the release’s
`SHA256SUMS`. The source archive’s `source-manifest.json` binds the two by
firmware hash and records the player commit and component revisions. Where the
build used tagged producer releases, it records their tags and asset hashes.

Check out that player commit in Git. You can also use the source archive:
extract it, enter the `q2-player` directory and initialize and commit a
local Git repository before building. The commit ID in a fresh local Git
repository will differ from the original release.

On an x86-64 Linux host with Python 3.12+, GitHub CLI, Rustup and the host
runtime libraries for your selected toolchain, run these commands from the
player checkout. The tagged inputs are those selected by the release source’s
`release-lock.json`.

```sh
python3 tools/fetch-build-tools.py --owner q2-nova --out build/relink-inputs
nightly=$(jq -er .rust_toolchain build/relink-inputs/q2-os/os/sources.json)
rustup toolchain install "$nightly" --profile minimal --component rust-src
cargo "+$nightly" fetch --locked
cargo "+$nightly" fetch --locked --manifest-path \
  build/relink-inputs/compiler/install/lib/rustlib/src/rust/library/Cargo.toml
```

Compare the fetched `component-lock.json` and `producer-inputs.json` with
the source manifest. If a stock build used independently prepared
components, obtain the recorded source revisions and build or obtain the
matching SDK and compiler from those revisions, following their repository
guides. Supply their actual paths to the same target builder below.

## Rebuild with your library changes

Edit the vendored `openaptx.c` or `openaptx.h` and commit the changes in
your local checkout. Keep the API compatible with the Rust wrapper in
`crates/codecs/q2-aptx/src/lib.rs`. Cargo compiles your edited C files and
links them statically into the rebuilt `q2nd`. The build also creates the
matching portal.

```sh
python3 tools/build-target.py --component-lock build/relink-inputs/component-lock.json \
  --sdk build/relink-inputs/sdk/mipsel-buildroot-linux-gnu_sdk-buildroot \
  --toolchain-repo build/relink-inputs/q2-toolchain \
  --compiler-work build/relink-inputs/compiler \
  --os-repo build/relink-inputs/q2-os --out build/relinked-player
```

The output includes both target executables and their build records. Use
the OS builder and the [release guide](release-candidate.md) to assemble
a package for the intended environment. Test a rebuilt image in QEMU
before considering hardware. The stock qualification applies to the
exact package tested on a device.
