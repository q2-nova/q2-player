#!/usr/bin/env python3
"""Admit the exact stock rehearsal payload and stage it in a new draft."""

import argparse
import hashlib
import json
import re
import subprocess
from pathlib import Path


TAG = r'stock-v[0-9]+\.[0-9]+\.[0-9]+-rc\.[1-9][0-9]*'
PAYLOADS = ('update.tar', 'result.json', 'player-source.tar.gz', 'q2nd-symbols.tar.gz')


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def metadata(path):
    record = json.loads(path.read_text())
    version, starting = (record[key] for key in ('version', 'starting_version'))
    if (not all(isinstance(v, str) and re.fullmatch(r'V[A-Za-z0-9.]{4}', v)
                for v in (version, starting)) or version == starting):
        raise ValueError('stock trial needs different five-character candidate and starting versions')
    return version, starting


def read_archive(os_repo, package, sha):
    return json.loads(subprocess.check_output([
        'python3', os_repo / 'os/hardware/package.py', 'check', package, '--sha256', sha,
    ]))


def admit(directory, tag, commit, trial, os_repo):
    if not re.fullmatch(TAG, tag) or not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('invalid stock tag or player commit')
    sums = directory / 'SHA256SUMS'
    expected = ''.join(f'{digest(directory / name)}  {name}\n' for name in PAYLOADS)
    if sums.read_text() != expected:
        raise ValueError('stock assets differ from SHA256SUMS')
    report = json.loads((directory / 'result.json').read_text())
    version, starting = trial
    package_sha = expected.split()[0]
    packed = report['stock_pack']
    if (report['player_tag'] != tag or report['player_commit'] != commit
            or report['profile'] != 'stock-qualification-candidate'
            or report['version'] != version or report['starting_version'] != starting
            or packed['version'] != version or packed['starting_version'] != starting
            or packed['update_tar_sha256'] != package_sha
            or report['update_tar_sha256'] != package_sha
            or report['installed_root_ready'] is not True
            or report['player_ready'] is not None
            or report['hardware_qualified'] is not False):
        raise ValueError('stock draft requires the matching booted, unqualified archive')
    if read_archive(os_repo, directory / 'update.tar', package_sha)['version'] != version:
        raise ValueError('stock trial version differs from the archived updater manifest')
    symbols(directory / 'q2nd-symbols.tar.gz')


def symbols(archive):
    """The release build's symbol companions; refuses an emulator diagnostic build."""
    import runpy
    runpy.run_path(str(Path(__file__).with_name('release-symbols.py')))['check'](archive, None)


def api(endpoint, *args):
    return json.loads(subprocess.check_output(['gh', 'api', endpoint, *args]))


def stage(directory, repository, tag, commit, metadata_path, os_repo):
    trial = metadata(metadata_path)
    admit(directory, tag, commit, trial, os_repo)
    version, starting = trial
    endpoint = f'repos/{repository}/releases'
    existing = api(endpoint, '--paginate', '--slurp')
    if any(row['tag_name'] == tag for page in existing for row in page):
        raise ValueError('a release or draft already uses this stock tag')
    # The workflow has already verified the existing signed tag. Do not
    # request a target or latest-release state change when creating its draft.
    draft = api(endpoint, '-X', 'POST', '-f', f'tag_name={tag}',
                '-F', 'draft=true', '-F', 'prerelease=true',
                '-f', f'name=Q2 physical qualification {tag}',
                '-f', 'body=Maintainer-only physical qualification. Keep this draft private. '
                      f'Candidate firmware: {version}. Provisional starting firmware: {starting}. '
                      'Physical trial pending. Passed the QEMU installed-root check only.')
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
    parser.add_argument('--metadata', required=True, type=Path)
    parser.add_argument('--os-repo', required=True, type=Path)
    args = parser.parse_args()
    stage(args.directory, args.repository, args.tag, args.commit, args.metadata, args.os_repo)


if __name__ == '__main__':
    main()
