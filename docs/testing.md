# Tests

The [Tests workflow](../.github/workflows/tests.yml) runs on every pull
request and on `main`. It has two jobs:

| Job | Command | What it covers |
| --- | --- | --- |
| `rust` | `cargo test --workspace --locked --offline` | Every Rust unit and integration test on the host. |
| `python` | `python3 tools/run-tests.py` | Every `test_*.py` under `tests/` and `tools/`. |

The [lint workflow](../.github/workflows/lint.yml) runs the clippy and
64-bit atomic checks of the [runtime rules](architecture.md#runtime-rules).

## Host tests

Run the Rust tests with the repository toolchain:

```sh
cargo fetch --locked
cargo test --workspace --locked --offline
```

A test that needs an input the host does not have is marked
`#[ignore = "REASON"]`; the reason names the input or says the test is a
measurement run by hand. `cargo test -- --ignored NAME` runs one.

Run the Python tests from the repository root:

```sh
python3 tools/run-tests.py            # every test_*.py under tests/ and tools/
python3 tools/run-tests.py tests/ui/test_ux_faults.py
```

Each file runs as `python3 -m unittest -v` from its own directory, in its
own process and in parallel. The runner prints one line per file and every
skipped test with its reason, prints the full output of a failing file and
exits 1 when a file fails. A test that needs an optional input skips with a
reason that names the environment variable to set: `tests/perf/test_pgo.py`
compares two target builds when `Q2_PGO_PLAIN_BUILD`, `Q2_PGO_BUILD`,
`Q2_SDK` and `Q2_QEMU_USER` are set, and `tests/perf/test_startup_profile.py`
checks the profile format against q2-os when `Q2_OS_REPO` is set (the
[PGO workflow](../.github/workflows/pgo.yml) sets it).

`tests/audio/test-player-position.py` and `tests/display/night-activation.py`
are comparison scripts, not unit tests: the first needs a baseline copy of
`player.rs` and `alsa.rs`, the second an installed root and a target build.
The runner does not run them.
