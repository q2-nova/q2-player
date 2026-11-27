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

The command above builds development artifacts. A release planner can supply
`--profile-use` with a profile collected from the matching player workload.
The result records the SHA-256 of each supplied optional input.

`result.json` also records the build's `profile`, `release_eligible`, the
Cargo `features` of each package and `feature_set_sha256`, their digest. A
release build has `profile` `release` and enables no optional q2nd feature.
A `--profile-use` profile must come from an instrumented build with the same
feature set as the release build that uses it.

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
