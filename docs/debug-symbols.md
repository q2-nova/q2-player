# Debug symbols

The `q2nd` and `q2-portal` executables on the device are stripped. Each
release keeps their unstripped and debug files in a separate asset,
`q2nd-symbols.tar.gz`, so a numeric address from a crash or a panic
backtrace resolves to a function and source line (FOOTPRINT-001 in the
[specification](specification.md#engineering)).

## How a build produces them

[`tools/build-target.py`](../tools/build-target.py) builds with
`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` and
`CARGO_PROFILE_RELEASE_STRIP=none`, and remaps source paths to
`/q2-player` and the Cargo home to `/cargo`. For each executable
[`tools/elf_symbols.py`](../tools/elf_symbols.py) then writes, under the
build's `symbols/` directory (`symbols/q2-portal/` for the portal):

- `NAME.unstripped`: the compiler's output, copied before anything changes it;
- `NAME.debug`: the debug companion (`objcopy --only-keep-debug`);
- `receipt.json`: the SHA-256 and size of the deployed, unstripped and debug
  files, the build ID, the hashes of the binutils used and a source-line
  witness.

The deployed executable is `strip` output with a `.gnu_debuglink` to the
companion. Before writing the receipt the tool checks that every allocated
section is unchanged by stripping, that the debug link's name and CRC match
the companion, that no symbol table or line table is left in the deployed
file, and that the companion resolves a defined function to a source line.
Rust's allocated `.debug_gdb_scripts` section is kept when it holds only
script entries; any other debug section fails the build. The tool refuses an
existing output rather than overwrite it.

The line tables give function, file and line. They do not give complete
local-variable or type information, and optimization inlines or removes
frames.

## Release asset

[`tools/release-symbols.py`](../tools/release-symbols.py) `pack` archives
`symbols/` with the build's `result.json` as `build-result.json`. It accepts
only a passed release build (`profile` `release`, no stall sampler) and
checks each companion against its receipt and the receipt against the
executable's hash. The release workflows attach the archive next to
`update.tar` and list it in `SHA256SUMS`
([release candidates](release-candidate.md)).

## Resolving an address

Match the executable first: the SHA-256 of the installed `q2nd` must equal
`artifacts.q2nd.sha256` in `build-result.json`. Telemetry reports carry the
running executable's SHA-256 for this ([telemetry](telemetry.md)). Then:

```sh
SDK/bin/mipsel-buildroot-linux-gnu-addr2line -a -f -C -e symbols/q2nd.debug 0xADDRESS
```

`SDK` is the relocated Buildroot SDK. Map `/q2-player` to a checkout of
the recorded `source_commit`. `readelf -h` on the executable shows its type:
for `EXEC` the runtime address is the link address; for `DYN` subtract the
load bias first.

Target builds compile std with `backtrace-trace-only`, so a panic backtrace
(`RUST_BACKTRACE=1`) prints raw addresses, one per frame, and the binary
carries no symbolizer. Resolve them with the command above.
