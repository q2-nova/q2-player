# Build the Q2 player and portal

Use a committed player checkout and the component revisions selected by this
repository. Supply a relocated Buildroot SDK, the matching X2000 compiler
checkout and built compiler work directory, and the OS checkout with its
target linker shim. The paths can live anywhere on the host. The builder
does not obtain or search for these inputs.

```sh
cargo fetch --locked
python3 tools/build-target.py \
  --sdk RELOCATED_SDK \
  --toolchain-repo Q2_TOOLCHAIN_CHECKOUT \
  --compiler-work X2000_COMPILER_WORK \
  --os-repo Q2_OS_CHECKOUT \
  --out build/target-player
```

The build runs Cargo offline and produces `q2nd` and `q2-portal` from the same
commit. Keep both executables, `source-commit`, `result.json`, and the
`symbols/` directory with the output when transferring the pair to the
image builder. The image builder takes `--app build/target-player/q2nd`.
The symbols directory holds the unstripped and debug files separately from
the executables installed on the device. `tools/release-symbols.py pack`
archives it with `result.json` as `q2nd-symbols.tar.gz`, the release asset
described in [release candidates](release-candidate.md). Each executable also has a size
report. Use a fresh output directory for each build.

The command above builds development artifacts. The result records the
SHA-256 of each supplied optional input.

`result.json` also records the build's `profile`, `release_eligible`, the
Cargo `features` of each package and `feature_set_sha256`, their digest. A
release build has `profile` `release` and enables no optional q2nd feature.

## Profile-guided release build

Every release build of q2nd is profile-guided. `tools/pgo.py profile` takes
the same inputs and the QEMU MIPS user-mode emulator. It builds an
instrumented q2nd with `build-target.py --profile-generate` and the release
feature set, trains it under `qemu-mipsel` and writes `q2nd.profdata`,
`receipt.json` and `coverage.json`. The release build then uses the profile:

```sh
python3 tools/pgo.py profile --sdk RELOCATED_SDK \
  --toolchain-repo Q2_TOOLCHAIN_CHECKOUT --compiler-work X2000_COMPILER_WORK \
  --os-repo Q2_OS_CHECKOUT --qemu-user QEMU_MIPSEL --out build/pgo
python3 tools/build-target.py --sdk RELOCATED_SDK \
  --toolchain-repo Q2_TOOLCHAIN_CHECKOUT --compiler-work X2000_COMPILER_WORK \
  --os-repo Q2_OS_CHECKOUT --profile-use build/pgo/q2nd.profdata \
  --out build/target-player
```

The compiler supplies the profile runtime, `llvm-profdata` and the shared
driver `tools/q2_pgo.py` of q2-toolchain. Training version 2 uses the
generated fixtures of `tests/perf/fixtures.py` (FFmpeg): `q2nd --bench`
decodes of each codec, JPEG, PNG and WebP covers at 50, 150 and 320 px, EQ,
240 UI frames, storage charts, config start-up, and three headless tours on
a seeded card. There is no network training yet: the player has no
generated Navidrome, Audiobookshelf, radio, sync, scrobble or portal
fixtures for `qemu-mipsel`, so the receipt lists the network read path as a
known gap.

The receipt records the profile's SHA-256, the training version, the
compiler identity, the feature set, the coverage summary (functions and
regions with counts per crate and named hot paths) and the training
minutes. `build-target.py --profile-use` refuses a profile whose receipt
names another feature set or compiler, and records the receipt under `pgo`
in `result.json`. `tools/release-candidate.py check-profile` also requires
the receipt's source commit to be the release source.

`tools/pgo.py compare --plain BUILD --pgo BUILD` runs the codec, image, EQ,
UI and chart cases on a build without a profile and a profile-guided build
of the same source under `qemu-mipsel`, and requires equal decoded samples,
pixels and UI checksums. `tests/perf/test_pgo.py` runs it when
`Q2_PGO_PLAIN_BUILD`, `Q2_PGO_BUILD`, `Q2_SDK` and `Q2_QEMU_USER` are set.

## Startup read-ahead

The installed image reads ahead the parts of q2nd that Home and first Play
need, as BOOT-002 of the q2-os specification requires.
`tools/startup-profile.py` records those parts for one build and writes
them beside it, where the image builder reads them:

```sh
python3 tools/startup-profile.py --player build/target-player/q2nd \
  --sdk RELOCATED_SDK --qemu-user QEMU_MIPSEL --os-repo Q2_OS_CHECKOUT
```

It runs the deployed q2nd under `qemu-mipsel` with the same runner as
`tools/pgo.py`, in `--drm-test` mode with the OS checkout's
`os/platform/display.c` drawing into host memory. For three generated songs
it seeds a card, waits for Home with the song paused, plays it, and records
which 128 KiB blocks of the executable are resident at each point. The
result is `build/target-player/startup-read-ahead/profile.json`: the union
of the blocks, the executable's SHA-256 and size, and the digests of the
producer, emulator, bridge and consumer. Before writing it, the tool admits
it with the OS checkout's `os/install/startup_read_ahead.py`. The image
builder, given `--app build/target-player/q2nd`, checks it again against the
installed executable, records the ranges in `artifact.json` under
`startup_read_ahead`, and adds them to root init. Without the file, root
init reads the whole executable. Both release workflows run the tool after
the release build and check that the composed image records it. A host that
cannot read `/proc/PID/pagemap` gets a profile with `blocks` `null`, and the
image reads the whole executable.

## Emulator diagnostic build

The stall sampler is an emulator diagnostic only. It records the stack of an
executor that stays blocked, and the wall time of slow UI and PCM operations.
Release builds do not compile it: q2nd then starts no observer thread,
references no `q2_stall_*` symbol and links no unwinder. Crash, panic,
process-exit, health, memory and UX reports are the same in both builds.
Their stall fields (`thread`, `task`, `poll`, `frames`, `stack_status`)
keep their empty values, so the report schema is unchanged.

Build it with `--emulator-diagnostics`, which enables the q2nd Cargo feature
`stall-sampler`. Only this build accepts `--unwinder` with a target
libunwind install prefix, which adds stacks to the sampler's reports:

```sh
python3 tools/build-target.py \
  --sdk RELOCATED_SDK \
  --toolchain-repo Q2_TOOLCHAIN_CHECKOUT \
  --compiler-work X2000_COMPILER_WORK \
  --os-repo Q2_OS_CHECKOUT \
  --emulator-diagnostics --unwinder LIBUNWIND_PREFIX \
  --out build/target-player-diagnostic
```

Its `result.json` has `profile` `emulator-diagnostic` and `release_eligible`
`false`. The q2nd executable contains the text
`Q2ND-EMULATOR-DIAGNOSTIC-BUILD` and logs it at startup. The release tools
refuse such a build: `tools/verify-package.py pack-stock` and the
`--paired-build-receipt` check always, and `tools/verify-package.py pack`
unless `--allow-diagnostic-build` is given for local emulator use.
`tools/release-candidate.py` packs without that option, and
`tools/release-symbols.py` accepts only release builds.

On the host, `cargo build -p q2nd --features stall-sampler` builds the
simulator with the sampler. Without the feature, the build fails if
`Q2_UNWIND_PREFIX` is set.
