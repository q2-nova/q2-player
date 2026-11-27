# Tests

The [Tests workflow](../.github/workflows/tests.yml) runs on every pull
request and on `main`. It has three jobs:

| Job | Command | What it covers |
| --- | --- | --- |
| `rust` | `cargo test --workspace --locked --offline` | Every Rust unit and integration test on the host. |
| `python` | `python3 tools/run-tests.py` | Every `test_*.py` under `tests/` and `tools/`. |
| `target` | `tools/target-tests.py build`, then `run` | The Rust tests built for the Q2 and run under `qemu-mipsel`. |

The [lint workflow](../.github/workflows/lint.yml) runs the clippy and
64-bit atomic checks of the [runtime rules](architecture.md#runtime-rules)
and the clippy negative control.

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

## Target tests under QEMU

The Q2 has 32-bit atomics, MSA kernels behind a runtime check and the X2000
compiler; host tests cover none of these. `tools/target-tests.py` builds the
workspace's tests for the Q2 with the inputs and release profile of
`tools/build-target.py` and runs each test under `qemu-mipsel -cpu P5600`
with the SDK sysroot. With the inputs of `tools/fetch-build-tools.py`:

```sh
python3 tools/fetch-build-tools.py --out build/rc-inputs
I=build/rc-inputs SDK=build/rc-inputs/sdk/mipsel-buildroot-linux-gnu_sdk-buildroot
python3 tools/target-tests.py build --sdk $SDK --toolchain-repo $I/q2-toolchain \
  --compiler-work $I/compiler --os-repo $I/q2-os --out build/target-tests
python3 tools/target-tests.py run --sdk $SDK --qemu-user $I/qemu/bin/qemu-mipsel \
  --out build/target-tests-run q2=build/target-tests
```

`build` fetches the locked Cargo sources of the workspace and of the
compiler's standard library, then builds offline with
`-Zpanic-abort-tests`. It writes the test executables to `bin/` and
`tests.json`, which records each executable's SHA-256, the source commit and
the compiler. `--package NAME` builds one package.

`run` lists the tests of each executable and runs each in its own emulator,
as libtest's child process (`__RUST_TEST_INVOKE`; exit status 50 is a pass),
from the package directory with a fresh `TMPDIR` and `Q2ND_RESOURCE_ROOT`
laid out as the image's `/usr/share/q2nd` (fonts and AutoEQ data).
`--filter TEXT` selects tests whose `EXECUTABLE::TEST` name contains `TEXT`.
`result.json` holds the counts, the skipped tests with their reasons, the
failed tests and the last output of each failure. With one build, `run`
exits 1 when a test fails.

`run` does not run ignored tests or the tests listed in
[`tests/target-skip.toml`](../tests/target-skip.toml), each with the reason
it cannot pass under qemu-user: it runs the test executable as a child
process, configures a pseudo-terminal, checks a child's `RLIMIT_DATA`, or
expects `posix_spawn` of a missing program to fail. A listed name that
matches no runnable test fails the run; [issue 19](https://github.com/q2-nova/q2-player/issues/19)
records how to run them on the target. Tests that recover from a panic are
marked `#[cfg_attr(panic = "abort", ignore = ...)]`: the target build
aborts on a panic, as the installed player does.

To compare two compilers, build the same source once with each compiler
(its `--toolchain-repo` and `--compiler-work`) and pass both builds:

```sh
python3 tools/target-tests.py run --sdk $SDK --qemu-user $I/qemu/bin/qemu-mipsel \
  --out build/target-ab base=build/target-tests-a candidate=build/target-tests-b
```

The first build is the reference. `run` prints the tests whose result
differs and exits 1 when a test passes in the reference and fails in
another build. A test that fails in every build is listed in `result.json`
and is not a compiler finding.

## Clippy negative control

[`tests/clippy-bad`](../tests/clippy-bad/src/lib.rs) is a crate outside the
workspace with one mistake per lint the lint workflow enforces:
`disallowed_methods`, `disallowed_types`, `await_holding_lock` and
`await_holding_refcell_ref`. The lint workflow runs the same clippy command
on it and fails unless clippy reports all four. Without the repository's
`clippy.toml`, clippy reports only the two await lints and the step fails.
