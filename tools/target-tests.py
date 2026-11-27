#!/usr/bin/env python3
"""Build the player's unit tests for the Q2 target and run them under qemu-mipsel.

  tools/target-tests.py build --sdk SDK --toolchain-repo T --compiler-work C --os-repo O --out DIR
  tools/target-tests.py run --sdk SDK --qemu-user QEMU --out DIR [--filter S] NAME=DIR [NAME=DIR ...]

`build` compiles the test harnesses of the workspace (every member runs on the Q2) with
the X2000 compiler and release profile of tools/build-target.py and writes
the executables and `tests.json` to DIR. It fetches the locked Cargo sources
(player and standard library) first and then builds offline.

The harnesses are built with -Zpanic-abort-tests, so libtest would run each
test in a child process by executing itself again, which qemu-user cannot do
without a binfmt_misc handler. `run` does what libtest's child does instead:
one emulator per test (`-cpu P5600`, the SDK sysroot, the qemu-user prefix of
tools/pgo.py) with `__RUST_TEST_INVOKE=NAME`; exit status 50 is a pass,
anything else a failure. The emulator runs in the package's directory, as
`cargo test` does, with a fresh short TMPDIR and Q2ND_RESOURCE_ROOT laid out
as the image's /usr/share/q2nd. Ignored tests are not run, nor the tests
tests/target-skip.toml lists with the reason they cannot pass here.

With one build, `run` exits 1 when any test fails. With two or more (a
compiler A/B), it reports the tests whose result differs between builds and
exits 1 when a test passes in the first and fails in another; a test that
fails in every build is listed in result.json and is not a compiler finding.
"""
import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGET = 'mipsel-q2-os-linux-gnu'
SDK_PREFIX = 'mipsel-buildroot-linux-gnu-'
PASS = 50  # libtest's TR_OK
SKIP = ROOT / 'tests/target-skip.toml'


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


pgo = load(ROOT / 'tools/pgo.py', 'pgo')


def sha(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def build(args):
    sdk, toolchain, os_repo = args.sdk.resolve(), args.toolchain_repo.resolve(), args.os_repo.resolve()
    out = args.out.resolve()
    if out.exists():
        raise SystemExit(f'{out} exists; use a fresh output directory')
    gcc, ar = sdk / 'bin' / (SDK_PREFIX + 'gcc'), sdk / 'bin' / (SDK_PREFIX + 'ar')
    linker, settings = os_repo / 'os/rust-link.py', os_repo / 'os/sources.json'
    for path in (gcc, ar, linker, settings, toolchain / 'tools/x2000_toolchain.py'):
        if not path.is_file():
            raise SystemExit(f'file required: {path}')
    lock = json.loads(settings.read_text())
    rust_toolchain = lock['rust_toolchain']
    os.environ['Q2_X2000_WORK'] = str(args.compiler_work.resolve())
    x2000 = load(toolchain / 'tools/x2000_toolchain.py', 'x2000_toolchain')
    compiler = x2000.installed()
    if json.loads((compiler / 'x2000-toolchain.json').read_text())['pin']['rust_toolchain'] != rust_toolchain:
        raise SystemExit('OS Rust version and X2000 compiler disagree')
    target_spec = x2000.spec(json.loads((toolchain / 'q2/toolchain/mipsel-q2-linux-gnu.json').read_text()))
    target_dir = ROOT / 'build/target-tests-target'
    env = dict(os.environ, Q2_OS_CC=str(gcc), CC_mipsel_q2_os_linux_gnu=str(gcc),
               AR_mipsel_q2_os_linux_gnu=str(ar), CARGO_TARGET_MIPSEL_Q2_OS_LINUX_GNU_LINKER=str(linker),
               CARGO_BUILD_JOBS=str(args.jobs), CARGO_TARGET_DIR=str(target_dir),
               CARGO_PROFILE_RELEASE_DEBUG='line-tables-only', CARGO_PROFILE_RELEASE_STRIP='none',
               SOURCE_DATE_EPOCH=str(lock['source_date_epoch']))
    env.update(x2000.cargo_env(sdk=sdk))
    for name in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'Q2_UNWIND_PREFIX'):
        env.pop(name, None)
    cargo = ['cargo', '+' + rust_toolchain]
    std_manifest = compiler / 'lib/rustlib/src/rust/library/Cargo.toml'
    for manifest in (ROOT / 'Cargo.toml', std_manifest):
        subprocess.run([*cargo, 'fetch', '--locked', '--manifest-path', str(manifest)], cwd=ROOT, env=env, check=True)

    out.mkdir(parents=True)
    target = out / (TARGET + '.json')
    target.write_text(json.dumps(target_spec, indent=2) + '\n')
    packages = [arg for name in args.package for arg in ('-p', name)] if args.package else ['--workspace']
    metadata = json.loads(subprocess.check_output([*cargo, 'metadata', '--offline', '--locked', '--no-deps',
                                                   '--format-version', '1'], cwd=ROOT, env=env))
    package_names = {p['manifest_path']: p['name'] for p in metadata['packages']}
    command = [*cargo, 'build', '--release', '--tests', '--offline', '--locked', *packages, '--no-default-features',
               '-Zbuild-std=std,panic_abort,test', '-Zjson-target-spec', '-Zpanic-abort-tests',
               '--target', str(target), '--message-format=json-render-diagnostics']
    messages = subprocess.run(command, cwd=ROOT, env=env, check=True, stdout=subprocess.PIPE, text=True).stdout
    binaries = []
    (out / 'bin').mkdir()
    for line in messages.splitlines():
        message = json.loads(line)
        if message.get('reason') != 'compiler-artifact' or not message.get('executable') or not message['profile']['test']:
            continue
        package_dir = Path(message['manifest_path']).parent
        kind = message['target']['kind'][0]
        name = f"{package_names[message['manifest_path']]}-{kind}-{message['target']['name']}"
        copy = out / 'bin' / name
        shutil.copy2(message['executable'], copy)
        binaries.append({'name': name, 'cwd': str(package_dir.relative_to(ROOT)), 'sha256': sha(copy)})
    if not binaries:
        raise SystemExit('cargo built no test executables')
    record = {'source_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
              'source_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain', '--', 'crates', 'Cargo.toml',
                                                            'Cargo.lock'], cwd=ROOT, text=True).strip()),
              'compiler': subprocess.check_output([env['RUSTC'], '-vV'], text=True),
              'compiler_receipt_sha256': sha(compiler / 'x2000-toolchain.json'),
              'binaries': sorted(binaries, key=lambda b: b['name'])}
    (out / 'tests.json').write_text(json.dumps(record, indent=1) + '\n')
    print(f'{len(binaries)} test executables in {out / "bin"}')


def no_core():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))  # a failing test aborts


def list_tests(prefix, binary, cwd, ignored):
    command = [*prefix, str(binary), '--list', '--format', 'terse'] + (['--ignored'] if ignored else [])
    listed = subprocess.run(command, capture_output=True, text=True, cwd=cwd, preexec_fn=no_core)
    if listed.returncode != 0:
        raise SystemExit(f'{binary} --list failed: {listed.stderr[-400:]}')
    return [line[:-len(': test')] for line in listed.stdout.splitlines() if line.endswith(': test')]


def run(args):
    arms = []
    for arm in args.builds:
        name, _, directory = arm.partition('=')
        if not name or not directory:
            raise SystemExit(f'{arm}: expected NAME=BUILD_DIRECTORY')
        record = json.loads((Path(directory) / 'tests.json').read_text())
        arms.append((name, Path(directory).resolve(), {b['name']: b for b in record['binaries']}))
    reference_name, reference_dir, reference = arms[0]
    for name, _, binaries in arms[1:]:
        if set(binaries) != set(reference):
            raise SystemExit(f'{name} and {reference_name} hold different test executables')
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    prefix = pgo.user_prefix(args.qemu_user, args.sdk)
    # The image's /usr/share/q2nd, as resources::directory finds it on the host.
    resources = out / 'resources'
    resources.mkdir()
    (resources / 'fonts').symlink_to(ROOT / 'assets/fonts')
    for kind in (ROOT / 'crates/q2nd/assets').iterdir():
        (resources / kind.name).symlink_to(kind)
    skip = {test: group['reason'] for group in tomllib.loads(args.skip.read_text())['skip'] for test in group['tests']}
    cases, listed = [], set()
    for binary, entry in sorted(reference.items()):
        cwd = ROOT / entry['cwd']
        path = reference_dir / 'bin' / binary
        ignored = set(list_tests(prefix, path, cwd, True))
        tests = [f'{binary}::{test}' for test in list_tests(prefix, path, cwd, False) if test not in ignored]
        listed.update(tests)
        cases += [(binary, key.split('::', 1)[1]) for key in tests if args.filter in key and key not in skip]
    unknown = sorted(set(skip) - listed)
    if unknown:
        raise SystemExit(f'{args.skip} lists tests that do not exist or are ignored: {unknown}')
    if not cases:
        raise SystemExit('no tests selected')

    def one(job):
        arm, directory, (binary, test) = job
        # A short path: some tests bind Unix sockets under TMPDIR (108-byte limit).
        with tempfile.TemporaryDirectory(prefix='q2t-') as scratch:
            command = [*prefix, '-E', f'__RUST_TEST_INVOKE={test}', '-E', f'TMPDIR={scratch}',
                       '-E', f'Q2ND_RESOURCE_ROOT={resources}',
                       '-E', f'CARGO_MANIFEST_DIR={ROOT / reference[binary]["cwd"]}', str(directory / 'bin' / binary)]
            try:
                p = subprocess.run(command, capture_output=True, timeout=args.timeout,
                                   cwd=ROOT / reference[binary]['cwd'], preexec_fn=no_core)
                return arm, binary, test, p.returncode, (p.stdout + p.stderr)[-600:].decode(errors='replace')
            except subprocess.TimeoutExpired:
                return arm, binary, test, None, f'timeout after {args.timeout} s'

    results = {name: {} for name, _, _ in arms}
    notes = {}
    jobs = [(name, directory, case) for name, directory, _ in arms for case in cases]
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        for arm, binary, test, code, output in pool.map(one, jobs):
            key = f'{binary}::{test}'
            results[arm][key] = code == PASS
            if code != PASS:
                notes[f'{arm} {key}'] = {'exit': code, 'output': output}
    keys = [f'{binary}::{test}' for binary, test in cases]
    failed = {arm: sorted(k for k in keys if not r[k]) for arm, r in results.items()}
    differing = sorted(k for k in keys if len({results[arm][k] for arm in results}) > 1)
    regressed = [k for k in differing if results[reference_name][k]]
    (out / 'result.json').write_text(json.dumps({
        'tests': len(keys), 'filter': args.filter, 'qemu_user_sha256': sha(args.qemu_user),
        'skipped': {key: reason for key, reason in sorted(skip.items()) if args.filter in key},
        'builds': {name: str(directory) for name, directory, _ in arms},
        'passed': {arm: len(keys) - len(f) for arm, f in failed.items()}, 'failed': failed,
        'differing': differing, 'notes': notes}, indent=1) + '\n')
    print(f'{len(keys)} tests, {sum(args.filter in key for key in skip)} skipped ({args.skip.name});',
          '; '.join(f'{arm} passed {len(keys) - len(f)}' for arm, f in failed.items()))
    if len(arms) == 1:
        for key in failed[reference_name]:
            print(f'failed: {key}\n{notes[f"{reference_name} {key}"]["output"]}')
        return 1 if failed[reference_name] else 0
    for key in differing:
        print('differs:', key, {arm: results[arm][key] for arm in results})
    return 1 if regressed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='command', required=True)
    b = sub.add_parser('build', help='build the test executables for the Q2 target')
    for name in ('sdk', 'toolchain-repo', 'compiler-work', 'os-repo', 'out'):
        b.add_argument('--' + name, type=Path, required=True)
    b.add_argument('--package', action='append', help='package to build (repeatable; default: the workspace)')
    b.add_argument('--jobs', type=int, default=2, help='Cargo build jobs (default 2)')
    r = sub.add_parser('run', help='run built tests under qemu-mipsel')
    r.add_argument('--sdk', type=Path, required=True)
    r.add_argument('--qemu-user', type=Path, required=True)
    r.add_argument('--out', type=Path, required=True)
    r.add_argument('--filter', default='', help='run only tests whose EXECUTABLE::TEST name contains this text')
    r.add_argument('--timeout', type=int, default=900, help='seconds per test (default 900)')
    r.add_argument('--skip', type=Path, default=SKIP, help='tests not to run, with reasons (default: tests/target-skip.toml)')
    r.add_argument('--jobs', type=int, default=os.cpu_count() or 2)
    r.add_argument('builds', nargs='+', metavar='NAME=DIR', help='build output directories; the first is the reference')
    args = parser.parse_args()
    return build(args) if args.command == 'build' else run(args)


if __name__ == '__main__':
    sys.exit(main())
