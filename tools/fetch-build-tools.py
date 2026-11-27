#!/usr/bin/env python3
"""Resolve tagged Q2 prereleases and admit their exact build assets."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile

ROOT = Path(__file__).resolve().parents[1]
SDK_BASE = 'mipsel-buildroot-linux-gnu_sdk-buildroot'
TAG = r'v[0-9]+\.[0-9]+\.[0-9]+-rc\.[1-9][0-9]*'
ASSETS = {
    'q2-os': (SDK_BASE + '.tar.gz', 'sdk-manifest.json', 'sdk-test.json', 'buildroot.config',
              'SHA256SUMS', 'target.tar.gz', 'platform.json', 'buildroot-legal-info.tar.gz',
              'xImage', 'rescue.cpio', 'shanling-q2.dtb', 'kernel-modules.tar.gz', 'linux.config',
              'kernel-base.json', 'RELEASE-SHA256SUMS'),
    'q2-toolchain': ('compiler.tar.gz', 'compiler.tar.gz.sha256'),
    'tsrs': ('tsrsd-q2.tar.gz', 'tsrsd-q2.tar.gz.sha256'),
    'qemu': ('qemu-linux-x86_64.tar.gz', 'qemu-linux-x86_64.tar.gz.sha256'),
}


def run(*args):
    return subprocess.check_output(args, text=True).strip()


def sha(path):
    with path.open('rb') as data:
        return hashlib.file_digest(data, 'sha256').hexdigest()


def read(path):
    return json.loads(path.read_text())


def validate_versions(versions):
    if set(versions) != set(ASSETS):
        raise ValueError('release lock must name q2-os, q2-toolchain, tsrs and qemu')
    for name, tag in versions.items():
        prefix = 'qemu-' if name == 'qemu' else ''
        if not isinstance(tag, str) or not re.fullmatch(prefix + TAG, tag):
            raise ValueError(f'{name}: expected a release candidate tag')


def regular(directory, name):
    item = directory / name
    if item.resolve() != item or not item.is_file():
        raise ValueError(f'missing regular asset {name}')
    return item


def verify_sums(directory, payload, filename, *, exact=True):
    entries = {}
    for line in regular(directory, filename).read_text().splitlines():
        match = re.fullmatch(r'([a-f0-9]{64}) [ *]([A-Za-z0-9_.-]+)', line)
        if not match or match[2] in entries:
            raise ValueError(f'{filename}: invalid or duplicate SHA-256 entry')
        entries[match[2]] = match[1]
    if set(entries) != set(payload) and (exact or not set(payload) <= set(entries)):
        raise ValueError(f'{filename}: missing or unexpected checksum entries')
    for name in payload:
        if sha(regular(directory, name)) != entries[name]:
            raise ValueError(f'{filename}: checksum mismatch for {name}')
    return {name: entries[name] for name in payload}


def unpack(archive, dest, allowed):
    dest.mkdir(exist_ok=False)
    with tarfile.open(archive, 'r:gz') as data:
        members = data.getmembers()
        names = set()
        for member in members:
            path = Path(member.name)
            if not path.parts and member.isdir():
                continue
            if (path.is_absolute() or '..' in path.parts or not path.parts
                    or path.parts[0] not in allowed or path in names):
                raise ValueError(f'unexpected archive member: {member.name}')
            names.add(path)
        data.extractall(dest, filter='data')


def prerelease(owner, name, tag, out):
    repository = f'{owner}/' + ('q2-toolchain' if name == 'qemu' else name)
    data = json.loads(run('gh', 'release', 'view', tag, '--repo', 'github.com/' + repository,
                          '--json', 'tagName,isPrerelease,isDraft,assets'))
    if data['tagName'] != tag or data['isPrerelease'] is not True or data['isDraft'] is not False:
        raise ValueError(f'{repository}: expected published prerelease {tag}')
    names = {item['name'] for item in data['assets']}
    expected = ASSETS[name]
    if not set(expected) <= names:
        raise ValueError(f'{repository}: release is missing required assets')
    output = out / (name + '-assets')
    output.mkdir()
    for file in expected:
        subprocess.run(['gh', 'release', 'download', tag, '--repo', 'github.com/' + repository,
                        '--pattern', file, '--output', output / file], check=True)
    commit = json.loads(run('gh', 'api', '--hostname', 'github.com', f'repos/{repository}/commits/{tag}'))
    if commit['commit']['verification']['verified'] is not True or not re.fullmatch(r'[a-f0-9]{40}', commit['sha']):
        raise ValueError(f'{repository}: unverified release commit')
    actual = {file: sha(regular(output, file)) for file in expected}
    for asset in data['assets']:
        if asset['name'] in actual and asset.get('digest') and asset['digest'] != 'sha256:' + actual[asset['name']]:
            raise ValueError(f'{repository}: GitHub release digest mismatch')
    return output, {'repository': repository, 'tag': tag, 'commit': commit['sha'], 'assets_sha256': actual}


def checkout(record, dest):
    subprocess.run(['git', '-c', 'credential.helper=', '-c', 'credential.helper=!gh auth git-credential',
                    'clone', '--depth=1', '--branch', record['tag'],
                    f"https://github.com/{record['repository']}.git", dest], check=True)
    if run('git', '-C', dest, 'rev-parse', 'HEAD') != record['commit']:
        raise ValueError('tag moved since the release was read')


def prepare(owner, out):
    if not re.fullmatch(r'[A-Za-z0-9-]+', owner):
        raise ValueError('expected a GitHub owner')
    versions = read(ROOT / 'release-lock.json')
    validate_versions(versions)
    pins = read(ROOT / 'component-lock.json')
    out.mkdir(parents=True, exist_ok=False)
    downloads = {name: prerelease(owner, name, tag, out) for name, tag in versions.items()}
    for name in ('q2-os', 'q2-toolchain', 'tsrs'):
        checkout(downloads[name][1], out / ('tsrs-source' if name == 'tsrs' else name))
    os.environ.pop('GH_TOKEN', None)
    os.environ.pop('GITHUB_TOKEN', None)
    for name in ('q2-os', 'q2-toolchain', 'tsrs'):
        pins[name] = downloads[name][1]['commit']

    sdk_assets = downloads['q2-os'][0]
    # Verify the release inventory and independently run the OS qualification verifier.
    verify_sums(sdk_assets, tuple(n for n in ASSETS['q2-os'] if n != 'RELEASE-SHA256SUMS'),
                'RELEASE-SHA256SUMS')
    sdk_record = json.loads(run(sys.executable, out / 'q2-os/os/sdk_release.py', 'verify',
                                '--bundle', sdk_assets, '--os-commit', pins['q2-os']))
    if sdk_record['buildroot_commit'] != pins['buildroot']:
        raise ValueError('SDK Buildroot source differs from the pinned source')
    unpack(sdk_assets / (SDK_BASE + '.tar.gz'), out / 'sdk', {SDK_BASE})
    sdk = out / 'sdk' / SDK_BASE
    subprocess.run([sdk / 'relocate-sdk.sh'], cwd=sdk, check=True)
    gcc = sdk / 'bin/mipsel-buildroot-linux-gnu-gcc'
    if (run(gcc, '-dumpmachine') != 'mipsel-buildroot-linux-gnu'
            or not Path(run(gcc, '-print-sysroot')).resolve().is_relative_to(sdk.resolve())):
        raise ValueError('SDK relocation or target mismatch')

    compiler, tsrs, qemu = (downloads[name][0] for name in ('q2-toolchain', 'tsrs', 'qemu'))
    for directory, archive in ((compiler, 'compiler.tar.gz'), (tsrs, 'tsrsd-q2.tar.gz'),
                               (qemu, 'qemu-linux-x86_64.tar.gz')):
        verify_sums(directory, (archive,), archive + '.sha256')
    unpack(compiler / 'compiler.tar.gz', out / 'compiler', {'compiler-bundle.json', 'install', 'licenses'})
    compiler_manifest = read(out / 'compiler/compiler-bundle.json')
    compiler_pin = read(out / 'q2-toolchain/q2/toolchain/llvm/pin.json')
    if (compiler_manifest.get('producer_commit') != pins['q2-toolchain']
            or compiler_manifest.get('pin') != compiler_pin
            or compiler_manifest.get('host') != 'x86_64-unknown-linux-gnu'
            or compiler_pin['rust_commit'] != pins['rust'] or compiler_pin['llvm_commit'] != pins['llvm-project']
            or compiler_manifest.get('install_receipt_sha256') != sha(out / 'compiler/install/x2000-toolchain.json')):
        raise ValueError('compiler bundle does not match the tagged toolchain')
    unpack(tsrs / 'tsrsd-q2.tar.gz', out / 'tsrs',
           {'tsrsd', 'result.json', 'size.json', 'symbols', 'licenses'})
    if read(out / 'tsrs/result.json').get('source_commit') != pins['tsrs']:
        raise ValueError('TSRS artifact does not match its tag')
    unpack(qemu / 'qemu-linux-x86_64.tar.gz', out / 'qemu',
           {'bin', 'share', 'qemu-artifact.json', 'COPYING', 'LICENSE'})
    qemu_root = out / 'qemu'
    qemu_manifest = read(regular(qemu_root, 'qemu-artifact.json'))
    if (qemu_manifest.get('source_commit') != pins['qemu']
            or qemu_manifest.get('recipe_commit') != downloads['qemu'][1]['commit']):
        raise ValueError('QEMU bundle does not match its source and producer pins')
    for name in ('bin/qemu-mipsel', 'bin/qemu-system-mipsel'):
        if sha(regular(qemu_root, name)) != qemu_manifest['files'][name]['sha256']:
            raise ValueError('QEMU binary differs from its receipt')
    (out / 'component-lock.json').write_text(json.dumps(pins, indent=2) + '\n')
    (out / 'producer-inputs.json').write_text(
        json.dumps({name: info for name, (_, info) in downloads.items()}, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--owner', default='q2-nova', help='component release owner (default: q2-nova)')
    mode.add_argument('--stock-assets', type=lambda p: Path(p).resolve())
    parser.add_argument('--out', type=lambda p: Path(p).resolve(), required=True)
    args = parser.parse_args()
    try:
        if args.stock_assets is None:
            prepare(args.owner, args.out)
        else:
            archive = 'q2-stock-qualified-inputs.tar.gz'
            verify_sums(args.stock_assets, (archive,), archive + '.sha256')
            unpack(args.stock_assets / archive, args.out,
                   {'image', 'hardware-qualification.json', 'update.tar', 'release-inputs.json'})
    except (OSError, KeyError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'{error}\n')


if __name__ == '__main__':
    main()
