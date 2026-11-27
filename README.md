# Q2 Nova player

A wheel-driven player for the Shanling Q2 with local playback and server-backed music and audiobooks. This repository owns the Rust workspace, browser player and contributor first-listen check. Device releases will be published here after integration and stock-device qualification.

## First listen

With the repository’s Rust toolchain, the simulator’s native development libraries, Python and FFmpeg installed, run:

```sh
python3 tests/e2e/first-listen.py --out build/my-first-listen
```

The check builds the native simulator, generates a short audiobook, plays it, pauses and repeats in Offline Mode. Use a fresh output name for each run. The SQLite compile options used by this workspace are tracked in `q2/toolchain/libsqlite3-flags`.

See [simulator prerequisites and checks](tests/e2e/README.md) and the [browser demo](web-demo/README.md). The [component lock](component-lock.json) selects exact revisions in the q2-nova repositories. Physical installation and download availability will be documented when a stock-device package is qualified.

## Device build and emulator rehearsal

Use the revisions in `component-lock.json` for the OS, toolchain, Rust, LLVM, Linux, Buildroot, QEMU and tsrs. The source repositories can live anywhere. The private Q2 repositories require GitHub read access. Build a relocatable SDK and target root through q2-os and the compiler through q2-toolchain. Build tsrsd using that SDK and compiler. A native first-listen run can be completed independently.

From a clean player checkout, set `OS` and `TOOLCHAIN` to the pinned checkouts, `SDK` to the relocated SDK, `COMPILER_WORK` to the completed compiler build, and `TSRS_ARTIFACT` to the target daemon output. Set `QEMU_SYSTEM` to the system emulator produced by the locked public QEMU fork. These commands use fresh output directories.

```sh
python3 tools/build-target.py --os-repo "$OS" --toolchain-repo "$TOOLCHAIN" \
  --sdk "$SDK" --compiler-work "$COMPILER_WORK" --out "$PWD/build/device"

python3 "$OS/os/install/prepare.py" --dest "$OS/build/install-linux"
python3 "$OS/os/install/build.py" --qemu-fixture --release-tag V0.01 \
  --kernel-source "$OS/build/install-linux" --buildroot-target "$OS/build/os/out/target" \
  --player-source "$PWD" --app "$PWD/build/device/q2nd" --tsrsd "$TSRS_ARTIFACT" \
  --output "$OS/build/os/emulator-candidate"

python3 tools/verify-package.py pack --os-repo "$OS" --image "$OS/build/os/emulator-candidate" \
  --version V0.01 --installed-version V0.00 --out build/emulator-package
python3 tools/verify-package.py boot --os-repo "$OS" --package build/emulator-package/update.tar \
  --qemu "$QEMU_SYSTEM" --out build/emulator-boot
```

The pack step checks both filesystems for SSH keys and verifies the installed player, portal and daemon against their build records. The boot step reads the kernel and root filesystem from that exact archive, creates disposable NAND and userdata, and checks the serial console for a running player. This package is for QEMU only. Physical first install and recovery need separate qualification.

## Building a release candidate

The [release lock](release-lock.json) selects the OS (SDK and platform), compiler,
QEMU and network daemon by their release tags. The **Player release candidate**
workflow builds the paired player and portal when a player RC tag is pushed.
It composes, packs and boots the exact QEMU image before attaching the emulator
rehearsal to a prerelease. See the [release workflow](docs/release-candidate.md)
for the separate physical stock qualification path.

On an x86-64 Linux host with Python 3.12+ and GitHub CLI, the same lock can
fetch and verify the component assets for a local build:

```sh
python3 tools/fetch-build-tools.py --out build/rc-inputs
```

The command reads checksums from the selected releases, relocates the SDK, and
records the resolved source commits and asset hashes under its output directory.
Use a fresh output directory. App forks use the q2-nova component releases by
default. Set the Actions repository variable `Q2_COMPONENTS_OWNER` or pass
`--owner` locally to choose another component owner. Private component
repositories require read access.

## License

Original Q2 player code is available under the GNU General Public License,
version 3 or (at your option) any later version (`GPL-3.0-or-later`). See
[COPYING](COPYING). The codec packages, vendored libraries, fonts, datasets
and other third-party material carry their own licenses and notices.
