#!/usr/bin/env python3
"""Verify locked release inputs and compose a Q2 stock candidate or QEMU rehearsal."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
IMAGE_FILES = ('artifact.json', 'xImage', 'rootfs.squashfs', 'rescue.cpio', 'shanling-q2.dtb')
VERIFIER = ROOT / 'tools/verify-package.py'


def sha(path):
    with path.open('rb') as data:
        return hashlib.file_digest(data, 'sha256').hexdigest()


def regular(root, name):
    path = root / name
    if path.resolve() != path or path.is_symlink() or not path.is_file():
        raise ValueError(f'regular in-bundle file required: {name}')
    return path


def load(path):
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise ValueError(f'expected JSON object: {path.name}')
    return value


def admit(root, expected_sha, mode, source, pins):
    root = root.resolve()
    manifest = regular(root, 'release-inputs.json')
    if not re.fullmatch(r'[0-9a-f]{64}', expected_sha) or sha(manifest) != expected_sha:
        raise ValueError('release input manifest SHA-256 mismatch')
    record = load(manifest)
    if (record.get('schema') != 1 or record.get('mode') != mode
            or record.get('player_commit') != source or record.get('components') != pins):
        raise ValueError('release input mode or locked source revisions differ')
    if not all(re.fullmatch(r'V[A-Za-z0-9.]{4}', record.get(key, ''))
               for key in ('version', 'installed_version')) or record['version'] == record['installed_version']:
        raise ValueError('invalid or identical stock firmware versions')
    files = record.get('files', {})
    needed = {'image/' + name for name in IMAGE_FILES}
    needed |= {'hardware-qualification.json'} if mode == 'stock-draft' else {'qemu-system-mipsel'}
    if set(files) != needed:
        raise ValueError('release input manifest has missing or unexpected files')
    for name in needed:
        if files[name] != sha(regular(root, name)):
            raise ValueError(f'release input SHA-256 mismatch: {name}')
    return record


def stock_qualification(path, source, pins, record, package_sha):
    evidence = load(path)
    if (evidence.get('schema') != 1 or evidence.get('domain') != 'physical-stock-q2'
            or evidence.get('player_commit') != source or evidence.get('components') != pins
            or evidence.get('update_tar_sha256') != package_sha
            or evidence.get('version') != record['version']
            or evidence.get('starting_version') != record['installed_version']
            or evidence.get('device_model') != 'Shanling Q2'
            or any(evidence.get(test) is not True for test in (
                'stock_install_passed', 'generated_local_playback_passed', 'recovery_passed'))):
        raise ValueError('manual stock hardware qualification does not cover the exact package and sources')
    for key in ('conditions', 'procedure'):
        if not isinstance(evidence.get(key), str) or not evidence[key].strip():
            raise ValueError(f'manual stock qualification needs {key}')
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode', choices=('stock-draft', 'qemu-rehearsal'), required=True)
    for arg in ('bundle', 'os-repo', 'out'):
        parser.add_argument('--' + arg, type=Path, required=True)
    parser.add_argument('--manifest-sha256', required=True)
    args = parser.parse_args()

    # Only import pinned OS helpers after verify-package validates its checkout.
    import runpy
    verifier = runpy.run_path(str(VERIFIER))
    pins = load(ROOT / 'component-lock.json')
    source = verifier['git'](ROOT, 'rev-parse', 'HEAD')
    if verifier['git'](ROOT, 'status', '--porcelain'):
        raise ValueError('release needs a clean player checkout')
    bundle = args.bundle.resolve()
    record = admit(bundle, args.manifest_sha256, args.mode, source, pins)
    fmt = verifier['os_package'](args.os_repo.resolve(), pins)
    image = bundle / 'image'
    manifest = load(image / 'artifact.json')
    if manifest.get('os_source_commit') != pins['q2-os']:
        raise ValueError('image OS differs from the player lock')
    for name in IMAGE_FILES[1:]:
        path = image / name
        ident = manifest['artifacts'][name]
        if ident['bytes'] != path.stat().st_size or ident['sha256'] != record['files']['image/' + name]:
            raise ValueError(f'installed OS manifest mismatch: {name}')

    args.out.mkdir(parents=True, exist_ok=False)
    common = ['--os-repo', args.os_repo.resolve()]
    if args.mode == 'qemu-rehearsal':
        qemu = bundle / 'qemu-system-mipsel'
        qemu.chmod(0o755)
        package = args.out / 'emulator-package' / 'update.tar'
        subprocess.run([sys.executable, VERIFIER, 'pack', *common, '--image', image,
                        '--version', record['version'], '--installed-version', record['installed_version'],
                        '--out', package.parent], check=True)
        boot = args.out / 'emulator-boot'
        subprocess.run([sys.executable, VERIFIER, 'boot', *common, '--package', package,
                        '--qemu', qemu, '--native-qemu', '--out', boot], check=True)
        result = load(boot / 'result.json')
        if (result['update_tar_sha256'] != sha(package)
                or result['qemu_sha256'] != record['files']['qemu-system-mipsel']
                or result['installed_root_ready'] is not True or result['player_ready'] is not True):
            raise ValueError('QEMU result does not cover the exact rehearsal package')
    else:
        stock = args.out / 'verified'
        stock.mkdir()
        package = stock / 'update.tar'
        check_stock(verifier, fmt, image, manifest, record, pins, source, package)
        package_sha = sha(package)
        evidence = stock_qualification(bundle / 'hardware-qualification.json', source, pins, record, package_sha)
        result = dict(update_tar_sha256=package_sha, version=record['version'],
                      starting_version=evidence['starting_version'],
                      hardware_qualification_sha256=record['files']['hardware-qualification.json'])
    result['mode'] = args.mode
    result['player_commit'] = source
    result['components'] = pins
    result['release_inputs_sha256'] = args.manifest_sha256
    with (args.out / 'result.json').open('x') as output:
        json.dump(result, output, indent=2)
        output.write('\n')


def check_stock(verifier, fmt, image, m, record, pins, source, package):
    if (m.get('qemu_fixture') is not False or m.get('emulator_shell') is not False
            or m.get('diagnostic_ssh') != {'profile': 'disabled'}
            or m.get('integration_hook_present') is not True
            or m.get('app_source_commit') != source or m.get('kernel_source_commit') != pins['linux']
            or m.get('tailscale_daemon') != 'rs' or m.get('tsrsd', {}).get('source_commit') != pins['tsrs']):
        raise ValueError('stock draft needs the locked keyless production image with real services')
    tool = verifier['filesystem_tool']
    names = tool('unsquashfs', image, '-ls', 'rootfs.squashfs').decode().splitlines()
    verifier['check_packed_keys']({name.removeprefix('squashfs-root/') for name in names})
    with (image / 'rescue.cpio').open('rb') as archive:
        listing = subprocess.check_output(['cpio', '-it', '--quiet'], stdin=archive).decode().splitlines()
    verifier['check_packed_keys']({name.removeprefix('./') for name in listing})
    release = json.loads(tool('unsquashfs', image, '-cat', 'rootfs.squashfs', 'etc/q2-os-release.json'))
    if (release.get('qemu_fixture') is not False or release.get('private_configuration_imported') is not False
            or release.get('release_tag') != record['version'] or release.get('app_source_commit') != source
            or release.get('kernel_source_commit') != pins['linux']):
        raise ValueError('stock filesystem release metadata differs from the selected sources')
    for name, digest in [('usr/bin/q2nd', m['app_sha256']),
                         ('usr/libexec/q2-network/q2-portal', m['portal_helper']['sha256']),
                         ('usr/libexec/q2-network/tsrsd', m['tsrsd']['sha256'])]:
        if verifier['sha'](tool('unsquashfs', image, '-cat', 'rootfs.squashfs', name)) != digest:
            raise ValueError(f'stock executable differs from its installed OS manifest: {name}')
    fmt.build_package(image / 'xImage', image / 'rootfs.squashfs', package, record['version'],
                      record['installed_version'])
    blobs, _ = fmt.read_package(package, sha(package))
    for member, name in ((fmt.KERNEL, 'xImage'), (fmt.ROOTFS, 'rootfs.squashfs')):
        if verifier['sha'](blobs[member]) != m['artifacts'][name]['sha256']:
            raise ValueError(f'stock package readback differs: {name}')


if __name__ == '__main__':
    main()
