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
commit. Keep both executables, their `.THIRD-PARTY-NOTICES` files,
`source-commit`, `result.json`, and the `symbols/` directory with the output
when transferring the pair to the image builder. The image builder takes
`--app build/target-player/q2nd`.
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

## Third-party notices

Before compiling, the build writes `q2nd.THIRD-PARTY-NOTICES` and
`q2-portal.THIRD-PARTY-NOTICES` with `tools/third_party_notices.py`, which
needs the cargo-about release pinned in `tools/notices/notices.toml`:

```sh
cargo install --locked cargo-about --version 0.9.3 --features cli
```

Each file lists the Cargo packages of that executable for the Q2 target, the
standard library crates the X2000 compiler builds and the bundled C and
Rust code that `notices.toml` names (sonic, the DDSketch mapping, vo-aacenc,
libopenaptx, libldac), then each distinct license text once.
`result.json` records their SHA-256 under `third_party_notices`. A
dependency whose license is not in the `accepted` list of
`tools/notices/about.toml`, or which has no license text, stops the build;
`python3 tools/third_party_notices.py check` runs the same test in CI. The
image builder installs the files as
`/usr/share/licenses/q2nd/THIRD-PARTY-NOTICES` and
`/usr/share/licenses/q2-portal/THIRD-PARTY-NOTICES`.

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

## Code size

On the Q2 code size is resident memory: after the first frame
[`platform/resident.rs`](../crates/q2nd/src/platform/resident.rs) locks the
executable's code and its libraries, because the root is xz
squashfs in 128 KiB blocks and an evicted code page costs a block's
decompression when it next runs. The release profile in
[`Cargo.toml`](../Cargo.toml) trades size for speed per crate:

- Fat LTO, one codegen unit, `panic = "abort"`.
- `opt-level = 3` for q2nd, the symphonia decoders and formats, the image
  decoders, the font crates and ring. Decoders need it: their inner loops
  depend on inlining and unrolling that `s` gives up. q2nd stays at 3
  because generic code is compiled at the opt-level of the crate that
  instantiates it, and q2nd instantiates tokio's tasks, serde's
  deserializers and the image pipeline.
- `opt-level = "s"` for every other dependency and for subsonic.
- SQLite is built without FTS3, FTS5, R\*Tree, DBSTAT, STAT4, soundex and
  extension loading
  ([`q2/toolchain/libsqlite3-flags`](../q2/toolchain/libsqlite3-flags)), at
  `-Os` because libsqlite3-sys follows the package's opt-level. A host test
  checks `PRAGMA compile_options` against the flags file.
- The same type deserialized two ways is compiled twice. `Config` is read
  only through `Config::from_toml`; follow that pattern for large
  `#[derive(Deserialize)]` types.

Not all of the executable's code is locked. GNU ld collects the code that
only `--bench` (`q2nd::perf`) and `--script` run into a `.q2tools` section
([`link/q2tools.ld`](../crates/q2nd/link/q2tools.ld), given by `build.rs`
for Q2 builds), which `resident.rs` leaves unlocked. The standard library is
built with `backtrace-trace-only`, so std carries no symbolizer and a panic
backtrace (printed only with `RUST_BACKTRACE`) lists raw addresses; resolve
them with the unstripped executable in `symbols/`. The unwind tables stay
locked unless the build sets `--cfg q2_unlock_unwind_tables`.

`tools/locked_size.py` computes the locked bytes of a build from its ELF
sections, as `resident.rs` would lock them. `tools/elf_size.py` writes the
section size report of each executable.

## MSA code

The X2000 compiler builds q2nd for the `x2000` CPU with MSA, the X2000's
128-bit SIMD extension. q2-toolchain describes its
[code generation](https://github.com/q2-nova/q2-toolchain/blob/main/docs/x2000-toolchain.md)
and the [X2000 instruction costs](https://github.com/q2-nova/q2-toolchain/blob/main/docs/x2000-msa.md).
Hand-written kernels take one of two forms:

- a C leaf compiled by the SDK's GCC with `-mmsa` behind a bounded FFI
  boundary: [`vendor/eq/stereo.c`](../crates/q2nd/vendor/eq/stereo.c) for
  the equalizer and [`vendor/pcm/convert.c`](../crates/q2nd/vendor/pcm/convert.c)
  for PCM conversion, built by `build.rs`, which sets `q2_eq_msa`;
- Rust `core::arch::mips` intrinsics inside
  `#[target_feature(enable = "msa")]` functions, as in
  [`gfx_msa.rs`](../crates/q2nd/src/gfx_msa.rs) and the vendored decoders.
  The `stdarch_mips` and `mips_target_feature` features are enabled only for
  `target_arch = "mips"` (`main.rs`); host builds use the stable toolchain
  of `rust-toolchain.toml`.

Rules for an MSA routine:

- Keep the portable routine. Host builds and tests run it, and the tests
  compare the two.
- Check the kernel's capability bit (`HWCAP_MIPS_MSA`) once and cache it,
  as `audio/eq_msa.rs` `available()` does.
- Make the boundary one call per block, row or packet, never per sample or
  pixel.
- Keep output bit-identical to the portable routine: the same arithmetic
  order, no fused multiply-add unless the portable path uses it, the same
  rounding and saturation. Test both on generated input, including lengths
  that are not a multiple of the vector width, extremes and NaN where they
  can occur. Where correctness beyond identical outputs matters, such as
  index arithmetic or bounds, prove it with Kani as `crates/machines` does.

## Benchmarks

`q2nd --bench SPEC.json` runs one isolated, deterministic workload
([`perf.rs`](../crates/q2nd/src/perf.rs)) and prints one JSON line with its
CPU time and an output hash (`sample_md5`, `pixel_md5` or UI checksums). It
never reads user configuration or opens audio hardware.
[`tests/perf/fixtures.py`](../tests/perf/fixtures.py) generates the audio
and artwork inputs with FFmpeg. A new kind must contain only the work it
measures: the comparison tools read the process's whole CPU time, so finer
timings go in the result (`decode_us`, `conversion_us`). `tools/pgo.py
compare` runs the codec, image, EQ, UI and chart cases on two builds under
`qemu-mipsel` and requires equal hashes.

Stage timers in the vendored MP3, FLAC and ALAC decoders are compiled only
with `--cfg q2_mp3_profile`, `q2_flac_profile` or `q2_alac_profile`; the
`decode` case then reports them as `mp3_profile`, `flac_profile` or
`alac_profile`. `q2_pcm_scalar` and `q2_gfx_scalar` build the portable
paths in place of the MSA kernels. `build-target.py` sets the compiler flags
itself and passes no extra `--cfg`, so it builds none of these variants.

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
`stall-sampler`:

```sh
python3 tools/build-target.py \
  --sdk RELOCATED_SDK \
  --toolchain-repo Q2_TOOLCHAIN_CHECKOUT \
  --compiler-work X2000_COMPILER_WORK \
  --os-repo Q2_OS_CHECKOUT \
  --emulator-diagnostics \
  --out build/target-player-diagnostic
```

Its stall reports carry the runtime, thread, task, poll and elapsed time,
with `stack_status` `unavailable` and no frames. Only this build accepts
`--unwinder PREFIX`, which adds the stack frames. PREFIX is a static
libunwind built for the target with the SDK's GCC, with
`include/libunwind.h` and `lib/libunwind.a`; this repository does not build
one.

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
