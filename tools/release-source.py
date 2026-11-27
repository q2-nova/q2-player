#!/usr/bin/env python3
"""Archive the exact player source and libopenaptx relink inputs for a package."""
import argparse
import hashlib
from io import BytesIO
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
OPENAPTX = 'crates/codecs/q2-aptx/vendor/openaptx'
NEEDED = {
    'COPYING', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'release-lock.json',
    'component-lock.json', 'tools/build-target.py', 'tools/fetch-build-tools.py',
    'crates/codecs/q2-aptx/build.rs', 'crates/codecs/q2-aptx/src/lib.rs',
    *(f'{OPENAPTX}/{name}' for name in ('LICENSE', 'UPSTREAM.json', 'openaptx.c', 'openaptx.h')),
}
NOTICE = """Q2 Player includes libopenaptx 0.2.0, statically linked.
Copyright (C) 2017 Aurelien Jacobs
Copyright (C) 2018–2020 Pali Rohár
The library is licensed under the GNU Lesser General Public License,
version 2.1 or, at your option, any later version. The complete LGPL text,
license notices, and source accompany this notice. You may modify the player
for your own use, including replacing or modifying libopenaptx, and reverse
engineer the player to debug those modifications. See the relink guide.
"""


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def read_json(path):
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise ValueError('expected a JSON object')
    return value


def check_commit(commit):
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('expected a full player source commit')
    subprocess.run(['git', 'merge-base', '--is-ancestor', commit, 'HEAD'], cwd=ROOT, check=True)


def entry(archive, path, payload):
    member = tarfile.TarInfo(path)
    member.size = len(payload)
    member.mode = 0o644
    archive.addfile(member, BytesIO(payload))


def add_sources(archive, git_tar):
    names = set()
    with tarfile.open(fileobj=git_tar, mode='r:') as source:
        for member in source:
            name = PurePosixPath(member.name)
            if (name.is_absolute() or '..' in name.parts or not name.parts or name.parts[0] != 'q2-player'
                    or member.name in names or not (member.isfile() or member.isdir())):
                raise ValueError('invalid player source archive member')
            names.add(member.name)
            if member.pax_headers:
                member.pax_headers = {'path': member.name} if len(member.name) > 100 else {}
            member.uid = member.gid = 0
            member.uname = member.gname = ''
            member.mtime = 0
            archive.addfile(member, source.extractfile(member) if member.isfile() else None)


def bundle(package, record_path, output, producers=None):
    record = read_json(record_path)
    commit = record['player_commit']
    check_commit(commit)
    package_sha = sha(package)
    if record['update_tar_sha256'] != package_sha:
        raise ValueError('source package is not bound to the verified firmware archive')
    components = record['components']
    if (set(components) != set(json.loads((ROOT / 'component-lock.json').read_text()))
            or any(not re.fullmatch(r'[0-9a-f]{40}', value) for value in components.values())):
        raise ValueError('invalid component source lock')
    git_prefix = ['git', '-C', ROOT]
    source = subprocess.check_output([*git_prefix, 'ls-tree', '-r', '--name-only', '-z', commit])
    paths = set(source.decode().split('\0')[:-1])
    if not NEEDED <= paths:
        raise ValueError('the verified player source lacks necessary relink inputs')
    license_bytes = subprocess.check_output([*git_prefix, 'show', f'{commit}:{OPENAPTX}/LICENSE'])
    manifest = {
        'schema': 1, 'player_commit': commit, 'update_tar_sha256': package_sha,
        'components': components, 'libopenaptx_license_sha256': hashlib.sha256(license_bytes).hexdigest(),
    }
    if producers:
        sources = read_json(producers)
        if set(sources) != {'q2-os', 'q2-toolchain', 'tsrs', 'qemu'}:
            raise ValueError('missing tagged component release provenance')
        for name, item in sources.items():
            if (set(item) != {'repository', 'tag', 'commit', 'assets_sha256'}
                    or (name != 'qemu' and item['commit'] != components[name])
                    or not re.fullmatch(r'[a-f0-9]{40}', item['commit'])
                    or not re.fullmatch(r'[A-Za-z0-9-]+/(?:q2-os|q2-toolchain|tsrs)', item['repository'])
                    or not re.fullmatch(r'(?:qemu-)?v[0-9]+(?:\.[0-9]+){2}-rc\.[1-9][0-9]*', item['tag'])
                    or any(not re.fullmatch(r'[a-zA-Z0-9._-]+', key) or not re.fullmatch(r'[a-f0-9]{64}', value)
                           for key, value in item['assets_sha256'].items())):
                raise ValueError(f'invalid tagged component provenance: {name}')
        manifest['tagged_inputs'] = sources
    # Verify/rewrite archive member metadata before publishing and keep build
    # products, checkout metadata, symlinks and owner-specific paths outside.
    with tempfile.TemporaryFile() as git_tar:
        subprocess.run([*git_prefix, 'archive', '--prefix=q2-player/', commit],
                       stdout=git_tar, check=True)
        git_tar.seek(0)
        with tarfile.open(output, 'x:gz') as archive:
            add_sources(archive, git_tar)
            entry(archive, 'source-manifest.json', (json.dumps(manifest, indent=2) + '\n').encode())
            entry(archive, 'LIBOPENAPTX-NOTICE.txt', NOTICE.encode())
            entry(archive, 'LIBOPENAPTX-LICENSE.txt', license_bytes)
            guide = (ROOT / 'docs/relink.md').read_bytes()
            entry(archive, 'RELINK.md', guide.replace(b'(release-candidate.md)', b'(q2-player/docs/release-candidate.md)'))
    print(sha(output))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ('package', 'record', 'out'):
        parser.add_argument('--' + flag, type=Path, required=True)
    parser.add_argument('--producer-inputs', type=Path)
    a = parser.parse_args()
    bundle(a.package, a.record, a.out, a.producer_inputs)


if __name__ == '__main__':
    main()
