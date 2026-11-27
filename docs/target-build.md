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
the executables installed on the device. Each executable also has a size
report. Use a fresh output directory for each build.

The command above builds development artifacts. A release planner can supply
`--profile-use` with a profile collected from the matching player workload,
and `--unwinder` with a target libunwind install prefix to enable stack reports.
The result records the SHA-256 of each supplied optional input.
