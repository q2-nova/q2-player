#!/usr/bin/env python3
"""Build paired Q2 player and portal artifacts with explicit cross-build inputs."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TARGET = 'mipsel-q2-os-linux-gnu'
SDK_PREFIX = 'mipsel-buildroot-linux-gnu-'
PACKAGES = ('q2nd', 'q2-portal')


def git(path, *args):
    return subprocess.check_output(['git', '-C', path, *args], text=True).strip()


def clean_revision(path, expected=None):
    commit = git(path, 'rev-parse', '--verify', 'HEAD^{commit}')
    if expected is not None and commit != expected:
        raise ValueError(f'{path}: expected locked commit {expected}, found {commit}')
    if git(path, 'status', '--porcelain', '--untracked-files=normal'):
        raise ValueError(f'{path}: commit source changes before building paired artifacts')
    return commit


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def run(*args, env=None):
    subprocess.run(args, cwd=ROOT, env=env, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ('sdk', 'toolchain-repo', 'compiler-work', 'os-repo', 'out'):
        parser.add_argument('--' + flag, type=Path, required=True)
    parser.add_argument('--component-lock', type=Path, default=ROOT / 'component-lock.json')
    parser.add_argument('--producer-inputs', type=Path, help='admitted CI producer identities')
    parser.add_argument('--unwinder', type=Path, help='optional target libunwind prefix')
    parser.add_argument('--profile-use', type=Path, help='optional q2nd PGO profile for these inputs')
    args = parser.parse_args()
    sdk = args.sdk.resolve()
    sdk_bin = sdk / 'bin'
    os_repo = args.os_repo.resolve()
    toolchain = args.toolchain_repo.resolve()
    compiler_work = args.compiler_work.resolve()
    out = args.out.absolute()
    compiler_helper = toolchain / 'tools/x2000_toolchain.py'
    target_base = toolchain / 'q2/toolchain/mipsel-q2-linux-gnu.json'
    linker = os_repo / 'os/rust-link.py'
    settings = os_repo / 'os/sources.json'
    gcc = sdk_bin / (SDK_PREFIX + 'gcc')
    ar = sdk_bin / (SDK_PREFIX + 'ar')
    readelf = sdk_bin / (SDK_PREFIX + 'readelf')
    for executable in (linker, gcc, ar, readelf, sdk_bin / (SDK_PREFIX + 'objcopy'),
                       sdk_bin / (SDK_PREFIX + 'strip'), sdk_bin / (SDK_PREFIX + 'nm'),
                       sdk_bin / (SDK_PREFIX + 'addr2line')):
        if not executable.is_file() or not os.access(executable, os.X_OK):
            parser.error(f'executable required: {executable}')
    for path in (compiler_helper, target_base, settings, args.profile_use):
        if path is not None and not path.is_file():
            parser.error(f'file required: {path}')
    if args.unwinder and not all((args.unwinder / file).is_file()
                                 for file in ('include/libunwind.h', 'lib/libunwind.a')):
        parser.error('--unwinder must provide include/libunwind.h and lib/libunwind.a')
    components = json.loads(args.component_lock.read_text())
    component_commits = {}
    try:
        source_commit = clean_revision(ROOT)
        for name, path in (('q2-os', os_repo), ('q2-toolchain', toolchain)):
            expected = components.get(name)
            if not isinstance(expected, str) or not re.fullmatch(r'[0-9a-f]{40}', expected):
                raise ValueError(f'component-lock.json requires a full {name} commit')
            component_commits[name] = clean_revision(path, expected)
    except (subprocess.CalledProcessError, ValueError) as error:
        parser.error(str(error))

    lock = json.loads(settings.read_text())
    rust_toolchain = lock['rust_toolchain']
    os.environ['Q2_X2000_WORK'] = str(compiler_work)
    module = importlib.util.spec_from_file_location('x2000_toolchain', compiler_helper)
    x2000 = importlib.util.module_from_spec(module)
    module.loader.exec_module(x2000)
    compiler = x2000.installed()
    compiler_receipt = compiler / 'x2000-toolchain.json'
    if json.loads(compiler_receipt.read_text())['pin']['rust_toolchain'] != rust_toolchain:
        parser.error('OS Rust version and X2000 compiler disagree')
    target_spec = x2000.spec(json.loads(target_base.read_text()))
    identities = {
        'compiler_receipt': sha(compiler_receipt), 'rustc': sha(compiler / 'bin/rustc'),
        'sdk_gcc': sha(gcc), 'sdk_ar': sha(ar), 'os_linker': sha(linker),
        'os_settings': sha(settings), 'compiler_helper': sha(compiler_helper),
    }
    if args.unwinder:
        identities['unwinder'] = sha(args.unwinder / 'lib/libunwind.a')
    if args.profile_use:
        identities['profile_use'] = sha(args.profile_use)
    compiler_env = x2000.cargo_env(sdk=sdk)
    cache_data = (identities, target_spec, {key: str(value) for key, value in compiler_env.items()},
                  str(sdk), str(linker), str(args.unwinder), str(args.profile_use))
    cache_key = hashlib.sha256(json.dumps(cache_data, sort_keys=True).encode()).hexdigest()[:20]
    target_dir = ROOT / 'build' / ('player-target-' + cache_key)
    target_dir.mkdir(parents=True, exist_ok=True)

    out.mkdir(parents=True, exist_ok=False)
    target = out / (TARGET + '.json')
    target.write_text(json.dumps(target_spec, indent=2) + '\n')
    env = dict(os.environ, Q2_OS_CC=str(gcc), CC_mipsel_q2_os_linux_gnu=str(gcc),
               AR_mipsel_q2_os_linux_gnu=str(ar), CARGO_TARGET_MIPSEL_Q2_OS_LINUX_GNU_LINKER=str(linker),
               CARGO_BUILD_JOBS='2', CARGO_TARGET_DIR=str(target_dir),
               CARGO_PROFILE_RELEASE_DEBUG='line-tables-only', CARGO_PROFILE_RELEASE_STRIP='none',
               SOURCE_DATE_EPOCH=str(lock['source_date_epoch']))
    env.update(compiler_env)
    env.pop('Q2_UNWIND_PREFIX', None)
    env.pop('RUSTFLAGS', None)
    env.pop('CARGO_ENCODED_RUSTFLAGS', None)
    rustflags = [f'--remap-path-prefix={ROOT}=/q2-player',
                 f'--remap-path-prefix={Path.home() / ".cargo"}=/cargo']
    if args.unwinder:
        env['Q2_UNWIND_PREFIX'] = str(args.unwinder.resolve())
    base = ['cargo', '+' + rust_toolchain, 'build', '--release', '--offline', '--locked',
            '-Zbuild-std=std,panic_abort',
            '-Zbuild-std-features=panic-unwind,backtrace,backtrace-trace-only',
            '-Zjson-target-spec', '--target', target]
    for name in PACKAGES:
        flags = rustflags.copy()
        options = []
        if name == 'q2nd':
            options.append('--no-default-features')
            if args.profile_use:
                flags.append('-Cprofile-use=' + str(args.profile_use.resolve()))
        run(*base, '-p', name, *options, env=dict(env, CARGO_ENCODED_RUSTFLAGS='\x1f'.join(flags)))

    artifacts = {}
    if args.producer_inputs is not None:
        producer_record = args.producer_inputs.read_bytes()
        json.loads(producer_record)
        (out / 'producer-inputs.json').write_bytes(producer_record)
        identities['producer_inputs'] = sha(out / 'producer-inputs.json')
    for name in PACKAGES:
        image = out / name
        symbols = out / 'symbols'
        if name != 'q2nd':
            symbols /= name
        run(sys.executable, ROOT / 'tools/elf_symbols.py', '--source', target_dir / TARGET / 'release' / name,
            '--deployed', image, '--symbols', symbols, '--prefix', sdk_bin / SDK_PREFIX,
            '--toolchain', rust_toolchain, '--source-commit', source_commit,
            '--rustc', compiler / 'bin/rustc', env=env)
        raw = image.read_bytes()[:20]
        if raw[:6] != b'\x7fELF\x01\x01' or raw[18:20] != b'\x08\x00':
            raise ValueError(f'{name} must be a little-endian MIPS ELF')
        run(sys.executable, ROOT / 'tools/elf_size.py', '--binary', image, '--readelf', readelf,
            '--out', out / (name + '.size.json'))
        artifacts[name] = {'sha256': sha(image), 'symbols_receipt_sha256': sha(symbols / 'receipt.json')}
    (out / 'source-commit').write_text(source_commit + '\n')
    with (out / 'result.json').open('x') as receipt:
        json.dump({'passed': True, 'source_commit': source_commit, 'target': TARGET,
                   'components': component_commits, 'inputs_sha256': identities, 'artifacts': artifacts}, receipt, indent=2)
        receipt.write('\n')


if __name__ == '__main__':
    main()
