# Contributing to the Q2 Nova player

Bug reports, fixes and features are welcome as GitHub issues and pull
requests. For a vulnerability, follow [SECURITY.md](SECURITY.md) instead of
opening an issue.

## Build and test

The desktop simulator and the host tests need no Q2. With the repository's
Rust toolchain (`rust-toolchain.toml`), Python 3 and FFmpeg:

```sh
cargo fetch --locked
cargo test --workspace --locked --offline
python3 tools/run-tests.py
python3 tools/check_docs.py
python3 tests/e2e/first-listen.py --out build/my-first-listen
```

[Tests](docs/testing.md) explains each suite and how to build the Rust tests
for the Q2 and run them under QEMU. The [target build
guide](docs/target-build.md) builds the player for the device.

## What a pull request must pass

These workflows run on every pull request:

| Workflow | Checks |
| --- | --- |
| Tests | Rust and Python host tests, and the Rust tests built for the Q2 and run under `qemu-mipsel` |
| Documents | Relative links and anchors in Markdown, and requirement IDs in the specification |
| Source privacy | File names in the history carry no local account or hosting state |

These run when a pull request changes the files they cover:

| Workflow | Checks |
| --- | --- |
| Player lint | Clippy with the runtime rules, 64-bit atomic use, and the clippy negative control |
| Simulator first listen | Generated card playback, Offline Mode and an Audiobookshelf journey in the native simulator |
| UX journeys | Fault, sleep-timer and offline browsing journeys in the simulator |
| Portal sign-in journeys | Wi-Fi sign-in flows against generated portals |
| PGO and startup profile controls | The profile-guided build and startup read-ahead tooling |

Commits on `main` must be signed.

## Changing behavior

- A change to what the player does updates the matching requirement in
  [docs/specification.md](docs/specification.md). `tools/check_docs.py`
  checks that each cited ID is defined.
- A change to menus or navigation regenerates the UI map with
  `python3 tests/ui/map.py --write` and states the reason for the change in
  the pull request (UX-002).
- Keep the portable routine beside any MSA kernel, with tests comparing the
  two ([MSA code](docs/target-build.md#msa-code)).

## How releases are qualified

A player tag builds the paired player and portal, composes the OS image from
the components in `release-lock.json`, and boots that image under QEMU
before attaching it to a prerelease. A release for stock Shanling Q2
hardware adds a physical test of the exact `update.tar`: installation from
stock firmware, playback of generated media and recovery to stock.
[Release candidates](docs/release-candidate.md)

## License

Contributions are accepted under the repository's license,
`GPL-3.0-or-later` ([COPYING](COPYING)). Third-party code keeps its own
license; record its origin and license beside it.
