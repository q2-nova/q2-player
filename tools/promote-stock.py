#!/usr/bin/env python3
"""Verify an exact stock RC and optionally stage its stable GitHub draft."""
import argparse
import json
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import tarfile
import tempfile

TOOLS = Path(__file__).resolve().parent
STOCK = runpy.run_path(str(TOOLS / 'stock-draft.py'))
QUALIFICATION = runpy.run_path(str(TOOLS / 'release-candidate.py'))
ASSETS = (*STOCK['PAYLOADS'], 'SHA256SUMS')

def gh(*args):
    return json.loads(subprocess.check_output(['gh', *args]))


def signed_commit(repo, tag):
    ref = gh('api', f'repos/{repo}/git/ref/tags/{tag}')['object']
    if ref['type'] != 'tag':
        raise ValueError(f'{tag} must be an annotated, signed tag')
    signed = gh('api', f'repos/{repo}/git/tags/{ref["sha"]}')
    target = signed['object']
    if (signed['tag'] != tag or signed['verification']['verified'] is not True
            or target['type'] != 'commit'):
        raise ValueError(f'{tag} does not have a verified tag signature')
    commit = gh('api', f'repos/{repo}/commits/{target["sha"]}')
    if commit['commit']['verification']['verified'] is not True:
        raise ValueError(f'{tag} must point to a verified commit')
    return target['sha']


def release(repo, tag):
    return gh('release', 'view', tag, '--repo', repo, '--json',
              'tagName,isDraft,isPrerelease,assets,url')


def digests(release_data):
    return {asset['name']: asset['digest'] for asset in release_data['assets']}


def check(draft, directory, tag, commit, os_repo, hardware, os_release):
    if (draft['tagName'] != tag or not draft['isDraft'] or not draft['isPrerelease']
            or set(digests(draft)) != set(ASSETS)):
        raise ValueError('expected the unmodified stock RC draft and complete assets')
    local = {name: 'sha256:' + STOCK['digest'](QUALIFICATION['regular'](directory, name))
             for name in ASSETS}
    if digests(draft) != local:
        raise ValueError('local files differ from the GitHub stock RC draft')
    report = QUALIFICATION['load'](directory / 'result.json')
    approved = local['update.tar'].removeprefix('sha256:')
    trial = report['version'], report['starting_version']
    STOCK['admit'](directory, tag, commit, trial, os_repo)
    QUALIFICATION['stock_qualification'](
        hardware, commit, report['components'],
        {'version': trial[0], 'installed_version': trial[1]}, approved)

    with tarfile.open(directory / 'player-source.tar.gz') as source:
        names = source.getnames()
        if any(names.count(name) != 1 for name in (
                'source-manifest.json', 'RELINK.md',
                'LIBOPENAPTX-LICENSE.txt', 'LIBOPENAPTX-NOTICE.txt')):
            raise ValueError('missing or duplicated corresponding source and relink material')
        manifest = json.load(source.extractfile('source-manifest.json'))
    if (manifest['player_commit'] != commit or manifest['update_tar_sha256'] != approved
            or manifest['components'] != report['components']
            or manifest['tagged_inputs'] != report['release_inputs']):
        raise ValueError('corresponding source does not match the stock archive')

    os_input = report['release_inputs']['q2-os']
    expected = {name: 'sha256:' + sha for name, sha in os_input['assets_sha256'].items()}
    if (os_release['tagName'] != os_input['tag'] or os_release['isDraft']
            or not expected.items() <= digests(os_release).items()
            or 'buildroot-legal-info.tar.gz' not in expected):
        raise ValueError('matching OS corresponding-source release is unavailable')
    return local


def check_updater_bluetooth(update):
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary, 'rootfs.squashfs')
        with tarfile.open(update) as bundle, root.open('xb') as output:
            shutil.copyfileobj(bundle.extractfile('recovery-update/rootfs.squashfs'), output)
        STOCK['check_bluetooth'](root)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default='q2-nova/q2-player')
    parser.add_argument('--candidate', required=True)
    parser.add_argument('--stable', required=True)
    parser.add_argument('--directory', type=lambda p: Path(p).resolve(), required=True)
    parser.add_argument('--os-repo', type=Path, required=True)
    parser.add_argument('--hardware', type=Path, required=True)
    parser.add_argument('--notes', type=Path, required=True, help='reviewed release notes')
    parser.add_argument('--stage', action='store_true', help='stage checked bytes in a new draft')
    args = parser.parse_args()
    match = re.fullmatch(r'stock-(v[0-9]+\.[0-9]+\.[0-9]+)-rc\.[1-9][0-9]*', args.candidate)
    if not match or args.stable != match[1]:
        parser.error('the stable tag must match the stock RC version')
    notes = args.notes.read_text()
    report = QUALIFICATION['load'](args.directory / 'result.json')
    os_input = report['release_inputs']['q2-os']
    os_sha = signed_commit(os_input['repository'], os_input['tag'])
    checkout = subprocess.check_output(['git', '-C', args.os_repo, 'rev-parse', 'HEAD'], text=True).strip()
    modified = subprocess.check_output([
        'git', '-C', args.os_repo, 'status', '--porcelain', '--untracked-files=no'])
    if checkout != os_sha or report['components']['q2-os'] != os_sha or modified:
        raise ValueError('the OS checkout must be clean at the exact signed OS source')
    os_release = release(os_input['repository'], os_input['tag'])
    if (os_release['url'] not in notes or report['starting_version'] not in notes
            or report['version'] not in notes or 'player-source.tar.gz' not in notes
            or 'buildroot-legal-info.tar.gz' not in notes or 'stock-radio-source.tar.gz' not in notes):
        raise ValueError('notes must name tested firmware versions and matching source')
    source = signed_commit(args.repo, args.candidate)
    local = check(release(args.repo, args.candidate), args.directory, args.candidate,
                  source, args.os_repo, args.hardware, os_release)
    check_updater_bluetooth(args.directory / 'update.tar')
    print(f"Verified {args.candidate} stock archive {local['update.tar']}")
    if args.stage:
        if signed_commit(args.repo, args.stable) != source:
            raise ValueError('stable tag does not identify the qualified source')
        endpoint = f'repos/{args.repo}/releases'
        existing = gh('api', endpoint, '--paginate', '--slurp')
        if any(row['tag_name'] == args.stable for page in existing for row in page):
            raise ValueError('a stable release or draft already uses this tag')
        subprocess.run([
            'gh', 'release', 'create', args.stable, *(args.directory / name for name in ASSETS),
            '--repo', args.repo, '--verify-tag', '--draft', '--title', f'Shanling Q2 {args.stable}',
            '--notes-file', args.notes,
        ], check=True)
        staged = release(args.repo, args.stable)
        if not staged['isDraft'] or staged['isPrerelease'] or local != digests(staged):
            raise ValueError('stable draft did not retain the exact checked assets')


if __name__ == '__main__':
    main()
