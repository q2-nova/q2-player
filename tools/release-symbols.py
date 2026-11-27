#!/usr/bin/env python3
"""Pack or check the q2nd and q2-portal symbol companions of a release build.

The archive holds the build-target.py `symbols/` directory (unstripped and
debug files with their receipts) and that build's `result.json` as
`build-result.json`. It is a release asset next to `update.tar`, never part
of it. Only a release build (no stall sampler) is accepted.
"""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import tarfile

NAME = 'q2nd-symbols.tar.gz'
RESULT = 'build-result.json'
COMPANIONS = {'q2nd': 'symbols', 'q2-portal': 'symbols/q2-portal'}
LIMIT = 1024 ** 3


def sha(data):
    return hashlib.sha256(data).hexdigest()


def verify(files, app_sha256=None):
    """Check a {posix name: bytes} bundle; return its build result."""
    if RESULT not in files:
        raise ValueError(f'symbols bundle needs {RESULT}')
    result = json.loads(files[RESULT])
    if (result.get('passed') is not True or result.get('profile') != 'release'
            or result.get('release_eligible') is not True):
        raise ValueError('symbols must come from a passed release build, not an emulator diagnostic build')
    artifacts = result.get('artifacts', {})
    if app_sha256 is not None and artifacts.get('q2nd', {}).get('sha256') != app_sha256:
        raise ValueError('symbols belong to a different q2nd')
    expected = {RESULT}
    for name, directory in COMPANIONS.items():
        receipt_name = f'{directory}/receipt.json'
        receipt_bytes = files.get(receipt_name)
        if receipt_bytes is None or sha(receipt_bytes) != artifacts.get(name, {}).get('symbols_receipt_sha256'):
            raise ValueError(f'{name} symbols receipt differs from the build result')
        receipt = json.loads(receipt_bytes)
        companions = receipt.get('artifacts', {})
        if receipt.get('passed') is not True or companions.get('deployed', {}).get('sha256') != artifacts[name].get('sha256'):
            raise ValueError(f'{name} symbols describe a different executable')
        expected.add(receipt_name)
        for kind in ('unstripped', 'debug'):
            member = f'{directory}/{name}.{kind}'
            if member not in files or sha(files[member]) != companions.get(kind, {}).get('sha256'):
                raise ValueError(f'{name} {kind} companion differs from its receipt')
            expected.add(member)
    if set(files) != expected:
        raise ValueError('symbols bundle has missing or unexpected files')
    return result


def collect(build):
    build = Path(build)
    files = {RESULT: (build / 'result.json').read_bytes()}
    for path in sorted((build / 'symbols').rglob('*')):
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ValueError(f'regular symbol file required: {path}')
        if path.is_file():
            files[path.relative_to(build).as_posix()] = path.read_bytes()
    executable = (build / 'q2nd').read_bytes()
    return files, sha(executable)


def pack(build, out):
    files, app_sha256 = collect(build)
    verify(files, app_sha256)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode='w', format=tarfile.PAX_FORMAT) as archive:
        for name in sorted(files):
            info = tarfile.TarInfo(name)
            info.size, info.mode, info.mtime = len(files[name]), 0o644, 0
            archive.addfile(info, io.BytesIO(files[name]))
    with Path(out).open('xb') as output:
        output.write(gzip.compress(raw.getvalue(), mtime=0))
    return sha(Path(out).read_bytes())


def read(archive):
    files, total = {}, 0
    with tarfile.open(archive, mode='r:gz') as bundle:
        for member in bundle:
            name = PurePosixPath(member.name)
            if member.isdir():
                continue
            if (not member.isfile() or name.is_absolute() or '..' in name.parts
                    or member.name != name.as_posix() or member.name in files):
                raise ValueError(f'unexpected symbols bundle member: {member.name}')
            total += member.size
            if total > LIMIT:
                raise ValueError('symbols bundle too large')
            files[member.name] = bundle.extractfile(member).read()
    return files


def check(archive, app_sha256):
    return verify(read(archive), app_sha256)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='operation', required=True)
    packing = sub.add_parser('pack')
    packing.add_argument('--build', type=Path, required=True, help='build-target.py output directory')
    packing.add_argument('--out', type=Path, required=True)
    checking = sub.add_parser('check')
    checking.add_argument('--archive', type=Path, required=True)
    checking.add_argument('--app-sha256', required=True)
    args = parser.parse_args()
    if args.operation == 'pack':
        print(pack(args.build, args.out))
    else:
        check(args.archive, args.app_sha256)


if __name__ == '__main__':
    main()
