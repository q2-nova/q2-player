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

## Downloading build tools

For a Linux x86-64 device build, use Python 3.11+ and the GitHub CLI. When the
first SDK and compiler releases pass qualification, the player will select them
in `build-tools.lock.json`. Until then use the source build above.

```sh
python3 tools/fetch-build-tools.py --out build/downloads
```

The command reads the release tags, downloads the named files into `sdk/` and
`compiler/`, and checks every SHA-256. Repeating it with those files present
verifies them offline. A failed download leaves the requested directory
available for a retry. Use the producer instructions to unpack and relocate
each bundle. Private component releases require GitHub CLI read access.

The lock has `format` set to `1` and a `tools` object with `sdk` and `compiler`
entries. Each specifies a GitHub `repository`, exact `tag`, `files` mapping
asset names to SHA-256 values, and `sources` mapping component names to commits
from `component-lock.json`. The SDK identifies `q2-os` and `buildroot`. The
compiler identifies `q2-toolchain`, `llvm-project` and `rust`. The release tag
must resolve to the producer commit. Include every archive or archive part
required for that tool. A player fork can keep using these same releases.
