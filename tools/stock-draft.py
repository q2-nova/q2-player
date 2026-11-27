#!/usr/bin/env python3
"""Admit the exact stock rehearsal payload and stage it in a new draft."""

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path


TAG = r'stock-v[0-9]+\.[0-9]+\.[0-9]+-rc\.[1-9][0-9]*'
PAYLOADS = ('stock-qualification-update.tar', 'result.json', 'player-source.tar.gz')


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def admit(directory, tag, commit):
    if not re.fullmatch(TAG, tag) or not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('invalid stock tag or player commit')
    sums = directory / 'SHA256SUMS'
    expected = ''.join(f'{digest(directory / name)}  {name}\n' for name in PAYLOADS)
    if sums.read_text() != expected:
        raise ValueError('stock assets differ from SHA256SUMS')
    report = json.loads((directory / 'result.json').read_text())
    if (report['player_tag'] != tag or report['player_commit'] != commit
            or report['profile'] != 'stock-qualification-candidate'
            or report['version'] != 'V0.02'
            or report['update_tar_sha256'] != expected.split()[0]
            or report['installed_root_ready'] is not True
            or report['player_ready'] is not None
            or report['hardware_qualified'] is not False):
        raise ValueError('stock draft requires the matching booted, unqualified archive')


def api(endpoint, *args):
    return json.loads(subprocess.check_output(['gh', 'api', endpoint, *args]))


def stage(directory, repository, tag, commit):
    admit(directory, tag, commit)
    endpoint = f'repos/{repository}/releases'
    existing = api(endpoint, '--paginate', '--slurp')
    if any(row['tag_name'] == tag for page in existing for row in page):
        raise ValueError('a release or draft already uses this stock tag')
    draft = api(endpoint, '-X', 'POST', '-f', f'tag_name={tag}',
                '-f', f'target_commitish={commit}', '-F', 'draft=true',
                '-F', 'prerelease=true', '-f', 'make_latest=false',
                '-f', f'name=Q2 physical qualification {tag}',
                '-f', 'body=Maintainer-only physical qualification. Keep this draft private. '
                      'The package has passed the QEMU installed-root check only.')
    release_id = int(draft['id'])
    if draft['draft'] is not True or draft['prerelease'] is not True or draft['tag_name'] != tag:
        raise ValueError('GitHub did not create the requested stock draft')
    for name in (*PAYLOADS, 'SHA256SUMS'):
        asset = api(f'https://uploads.github.com/{endpoint}/{release_id}/assets?name={name}',
                    '-X', 'POST', '-H', 'Content-Type: application/octet-stream',
                    '--input', str(directory / name))
        if asset['name'] != name or asset['digest'] != f'sha256:{digest(directory / name)}':
            raise ValueError(f'GitHub returned a different asset digest for {name}')
    current = api(f'{endpoint}/{release_id}')
    if current['draft'] is not True:
        raise ValueError('stock qualification must remain draft-only')
    print(current['html_url'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--directory', required=True, type=Path)
    parser.add_argument('--repository', required=True)
    parser.add_argument('--tag', required=True)
    parser.add_argument('--commit', required=True)
    args = parser.parse_args()
    stage(args.directory, args.repository, args.tag, args.commit)


if __name__ == '__main__':
    main()
