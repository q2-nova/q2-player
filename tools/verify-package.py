#!/usr/bin/env python3
"""Pack and boot an unpublished Q2 emulator candidate using the pinned OS package format."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
PINS = ROOT / 'component-lock.json'
KEY_PATHS = {'etc/q2-bootstrap/hostkey', 'root/.ssh/authorized_keys'}
PAGE = 2048
STRIDE = 2112
ERASE = 131072


def sha(data):
    return hashlib.sha256(data).hexdigest()


def git(repo, *args):
    return subprocess.check_output(['git', '-C', repo, *args], text=True).strip()


def os_package(repo, pins):
    if git(repo, 'rev-parse', 'HEAD') != pins['q2-os'] or git(repo, 'status', '--porcelain'):
        raise ValueError('expected a clean q2-os checkout at the locked commit')
    path = repo / 'os/hardware/package.py'
    spec = importlib.util.spec_from_file_location('q2_os_package', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def filesystem_tool(tool, cwd, *args):
    if shutil.which(tool):
        cmd = [tool, *args]
    else:
        cmd = ['podman', 'run', '--rm', '--network=none', '--security-opt', 'label=disable',
               '-v', f'{cwd}:/files:ro', '-w', '/files', 'q2-os-build', tool, *args]
    return subprocess.check_output(cmd, cwd=cwd)


def check_packed_keys(names):
    if KEY_PATHS.intersection(names) or any(
        n.startswith('etc/dropbear/') and n.endswith('_host_key') for n in names
    ):
        raise ValueError('candidate includes an SSH authorization or host key')


def pack(args, pins, fmt):
    image = args.image.resolve()
    m = json.loads((image / 'artifact.json').read_text())
    if (m.get('diagnostic_ssh') != {'profile': 'disabled'} or m.get('qemu_fixture') is not True
            or m.get('hardware_qualified') is not False or m.get('emulator_shell') is not False):
        raise ValueError('expected a keyless emulator candidate with the serial shell disabled')
    if m['kernel_source_commit'] != pins['linux'] or m['tsrsd']['source_commit'] != pins['tsrs']:
        raise ValueError('kernel or daemon differs from the player lock')
    source = m['app_source_commit']
    if git(ROOT, 'rev-parse', 'HEAD') != source or git(ROOT, 'status', '--porcelain'):
        raise ValueError('use a clean player checkout at the paired application build commit')
    for name in ('xImage', 'rootfs.squashfs', 'rescue.cpio', 'shanling-q2.dtb'):
        path = image / name
        if path.is_symlink() or sha(path.read_bytes()) != m['artifacts'][name]['sha256']:
            raise ValueError(f'candidate artifact differs from the build manifest: {name}')
    names = filesystem_tool('unsquashfs', image, '-ls', 'rootfs.squashfs').decode().splitlines()
    check_packed_keys({n.removeprefix('squashfs-root/') for n in names})
    with (image / 'rescue.cpio').open('rb') as archive:
        listing = subprocess.check_output(['cpio', '-it', '--quiet'], stdin=archive).decode().splitlines()
    check_packed_keys({n.removeprefix('./') for n in listing})
    release = json.loads(filesystem_tool('unsquashfs', image, '-cat', 'rootfs.squashfs',
                                         'etc/q2-os-release.json'))
    if (release.get('release_tag') != args.version or release.get('app_source_commit') != source
            or release.get('kernel_source_commit') != pins['linux'] or not release.get('qemu_fixture')):
        raise ValueError('installed release metadata differs from the selected image or package version')
    for name, digest in [('usr/bin/q2nd', m['app_sha256']),
                         ('usr/libexec/q2-network/q2-portal', m['portal_helper']['sha256']),
                         ('usr/libexec/q2-network/tsrsd', m['tsrsd']['sha256'])]:
        if sha(filesystem_tool('unsquashfs', image, '-cat', 'rootfs.squashfs', name)) != digest:
            raise ValueError(f'packaged executable differs from paired build: {name}')
    # Read the package back before publication. A failed check leaves no final archive.
    args.out.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(dir=args.out) as tmp:
        staged = Path(tmp) / 'update.tar'
        report = fmt.build_package(image / 'xImage', image / 'rootfs.squashfs', staged,
                                   args.version, args.installed_version)
        blobs, _ = fmt.read_package(staged)
        for member, name in [(fmt.KERNEL, 'xImage'), (fmt.ROOTFS, 'rootfs.squashfs')]:
            if sha(blobs[member]) != m['artifacts'][name]['sha256']:
                raise ValueError(f'package readback differs: {name}')
        staged.rename(args.out / 'update.tar')
    print(json.dumps(report, indent=2))


def write_pages(nand, offset, capacity, data):
    if len(data) > capacity:
        raise ValueError('emulator partition too small')
    nand.seek(offset // PAGE * STRIDE)
    for pos in range(0, len(data), PAGE):
        nand.write(data[pos:pos + PAGE].ljust(STRIDE, b'\xff'))


def boot(args, fmt):
    # Always use bytes just read from the exact update archive.
    blobs, report = fmt.read_package(args.package)
    args.out.mkdir(parents=True, exist_ok=False)
    work = args.out.resolve()
    (work / 'xImage').write_bytes(blobs[fmt.KERNEL])
    seed = work / 'userdata'
    seed.mkdir()
    (work / 'volume.cfg').write_text('[user_data]\nmode=ubi\nimage=userdata.ubifs\nvol_id=0\n'
                                     'vol_type=dynamic\nvol_name=user_data\nvol_size=16MiB\n'
                                     'vol_flags=autoresize\n')
    subprocess.run(['podman','build','-q','--iidfile',work / 'mtd-image',
                    '-f',args.os_repo / 'os/install/TelemetryFixture.Containerfile',
                    args.os_repo / 'os/install'],check=True)
    subprocess.run(['podman','run','--rm','--network=none','--security-opt','label=disable',
                    '-v',f'{work}:/fixture','-w','/fixture',(work/'mtd-image').read_text().strip(),
                    'sh','-ec','mkfs.ubifs -U -r userdata -m 2048 -e 126976 -c 198 -o userdata.ubifs && '
                    'ubinize -o userdata.ubi -m 2048 -p 131072 -s 2048 volume.cfg'],check=True)
    nand_path = work / 'nand.img'
    with nand_path.open('xb') as nand:
        block = bytes([255]) * (1024 * STRIDE)
        for _ in range(128):
            nand.write(block)
        write_pages(nand, 0x100000, 6*1024**2, blobs[fmt.KERNEL])
        write_pages(nand, 0x700000, 75*1024**2, blobs[fmt.ROOTFS])
        write_pages(nand, 0x6440000, 0x18c0000, (work/'userdata.ubi').read_bytes())
    console = work / 'serial.log'
    binary = args.qemu.resolve()
    if args.native_qemu:
        command = [binary]
    else:
        command = ['podman', 'run', '--rm', '--cidfile', str(work/'qemu.cid'), '--network=none',
                   '--security-opt', 'label=disable', '-v', f'{binary}:/q2/system:ro',
                   '-v', f'{work}:/fixture', '-w', '/fixture', 'q2-qemu-build', '/q2/system']
    data = getattr(args, 'qemu_data', None)
    if data is not None:
        if not args.native_qemu or not data.is_dir():
            raise ValueError('native QEMU data must name an existing directory')
        command += ['-L', data.resolve()]
    cmd = [*command, '-M','q2', '-m','128M','-smp','2','-display','none','-monitor','none',
           '-nic','none','-serial','stdio','-kernel','xImage','-drive','if=mtd,format=raw,file=nand.img']
    with console.open('xb') as output:
        proc = subprocess.Popen(cmd,cwd=work,stdin=subprocess.DEVNULL,stdout=output,stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + args.timeout
            while True:
                log = console.read_bytes().splitlines()
                if b'Q2_INSTALL_ROOT_FAILED' in log or any(b'Kernel panic' in l for l in log):
                    raise ValueError(f'kernel or installed root failed; see {console}')
                if b'Q2_INSTALLED_ROOT_READY' in log and b'Q2_SIM_PLAYER_READY' in log:
                    break
                if proc.poll() is not None or time.monotonic() >= deadline:
                    raise ValueError(f'emulator stopped or timed out before player readiness; see {console}')
                time.sleep(1)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            cidfile = work / 'qemu.cid'
            if not args.native_qemu and cidfile.is_file():
                subprocess.run(['podman', 'rm', '--force', '--ignore', cidfile.read_text().strip()],
                               check=True, stdout=subprocess.DEVNULL, timeout=25)
    result = {'update_tar_sha256': report['update_tar_sha256'], 'qemu_sha256': sha(binary.read_bytes()),
              'installed_root_ready': True, 'player_ready': True, 'domain': 'direct kernel boot in QEMU'}
    (work/'result.json').write_text(json.dumps(result, indent=2)+'\n')
    print(json.dumps(result, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='operation', required=True)
    package = sub.add_parser('pack')
    package.add_argument('--image', type=Path, required=True)
    package.add_argument('--version', required=True)
    package.add_argument('--installed-version', required=True)
    emulator = sub.add_parser('boot')
    emulator.add_argument('--package', type=Path, required=True)
    emulator.add_argument('--qemu', type=Path, required=True)
    emulator.add_argument('--native-qemu', action='store_true',
                          help='use a QEMU binary built for this host instead of its build container')
    emulator.add_argument('--qemu-data', type=Path, help='optional native QEMU firmware/data directory')
    emulator.add_argument('--timeout', type=int, default=420)
    for command in (package, emulator):
        command.add_argument('--out', type=Path, required=True, help='fresh directory')
        command.add_argument('--component-lock', type=Path, default=PINS)
        command.add_argument('--os-repo', type=lambda x: Path(x).resolve(), required=True)
    args = parser.parse_args()
    pins = json.loads(args.component_lock.read_text())
    fmt = os_package(args.os_repo, pins)
    if args.operation == 'pack':
        pack(args, pins, fmt)
    else:
        boot(args, fmt)


if __name__ == '__main__':
    main()
