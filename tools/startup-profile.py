#!/usr/bin/env python3
"""Record the q2nd file blocks that Home and first Play read, for the image's read-ahead.

  tools/startup-profile.py --player BUILD/q2nd --sdk SDK --qemu-user QEMU --os-repo Q2_OS

Runs the deployed q2nd of a paired build (tools/build-target.py) under
qemu-mipsel, with the runner of tools/pgo.py, in --drm-test mode on
anonymous display buffers. For each of three generated songs it starts the
player on a seeded card, waits for Home with the song paused, then plays
it, and samples which pages of the executable are resident at both points.
The union, in 128 KiB blocks, is written as BUILD/startup-read-ahead/profile.json,
where the q2-os image builder (os/install/startup_read_ahead.py) reads it
beside --app, checks it against the installed executable and reads those
blocks ahead at boot. This is a read-ahead list, not a timing measurement.

The q2-os checkout supplies the display bridge source (os/platform/display.c)
and the consumer, which admits the profile before it is written.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import pgo  # noqa: E402

PAGE = 4096
SIDECAR = 'startup-read-ahead'
CONSUMER = 'os/install/startup_read_ahead.py'
DISPLAY = 'os/platform/display.c'
PRODUCERS = ('tools/startup-profile.py', 'tools/pgo.py', 'tests/perf/fixtures.py')
MEDIA = ('mp3.mp3', 'flac.flac', 'faststart.m4a')
STAGES = (('home', 'Paused'), ('first_play', 'Playing'))
MARKER = b'Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n'
SCRIPT = """record-text
await 20000 screen:Q2
await-play 12000 paused
present 12000
text home
wait 1500
play
await-play 12000 playing 180
text first_play
wait 3000
"""
# display.c with anonymous host buffers in place of KMS dumb buffers. Scanout
# stays disabled, so q2_display_begin copies each frame and completes inline.
BRIDGE = r'''
#define q2_display_open q2_kms_display_open
#define q2_display_close q2_kms_display_close
#define q2_display_enable q2_kms_display_enable
#include "display.c"
#undef q2_display_open
#undef q2_display_close
#undef q2_display_enable

void q2_display_close(struct display *d)
{
    if (!d) return;
    free(d->buffers[0].map);
    free(d->buffers[1].map);
    free(d);
}

struct display *q2_display_open(void)
{
    struct display *d = calloc(1, sizeof(*d));
    if (!d) return NULL;
    d->fd = -1;
    for (int i = 0; i < 2; i++) {
        d->buffers[i].pitch = 320 * 4;
        d->buffers[i].size = 320 * 4 * 375;
        d->buffers[i].map = calloc(1, d->buffers[i].size);
        if (!d->buffers[i].map) { q2_display_close(d); return NULL; }
    }
    return d;
}

int q2_display_enable(struct display *d, int on) { (void)on; d->enabled = 0; return 0; }
'''


def sha(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def consumer(os_repo):
    path = Path(os_repo) / CONSUMER
    if not path.is_file():
        raise SystemExit(f'{path}: the q2-os checkout has no startup read-ahead consumer')
    spec = importlib.util.spec_from_file_location('startup_read_ahead', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def bridge(out, cc, display):
    """Build libq2platform.so from display.c and BRIDGE with the SDK compiler."""
    out.mkdir()
    shutil.copyfile(display, out / 'display.c')
    (out / 'bridge.c').write_text(BRIDGE)
    library = out / 'libq2platform.so'
    subprocess.run([str(cc), '-O2', '-std=c11', '-fPIC', '-shared', '-D_TIME_BITS=64', '-D_FILE_OFFSET_BITS=64',
                    '-I', str(out), str(out / 'bridge.c'), '-o', str(library)], check=True, timeout=60)
    return library


def resident_blocks(pid, player, block):
    """Blocks of `player` with a resident page in process `pid`'s mappings."""
    info = player.stat()
    present = set()
    with open(f'/proc/{pid}/pagemap', 'rb', buffering=0) as pages:
        for line in Path(f'/proc/{pid}/maps').read_text().splitlines():
            span, _, offset, _, inode, *path = line.split(maxsplit=5)
            if int(inode) != info.st_ino or path != [str(player)]:
                continue
            if not (Path(f'/proc/{pid}/root') / player.relative_to('/')).samefile(player):
                raise ValueError('player mapping does not belong to the profiled executable')
            begin, end = (int(value, 16) for value in span.split('-'))
            offset = int(offset, 16)
            count = max(0, (min(end - begin, info.st_size - offset) + PAGE - 1) // PAGE)
            data = os.pread(pages.fileno(), 8 * count, 8 * (begin // PAGE))
            if len(data) != 8 * count:
                raise ValueError('incomplete page presence sample')
            # Bit 63 of each 64-bit pagemap entry: the page is resident.
            present.update((offset + i * PAGE) // block for i, high in enumerate(data[7::8]) if high & 128)
    if 0 not in present:
        raise ValueError('player header was absent from the mapped-page sample')
    return sorted(present)


def combine(record, scenes):
    """Add the scenes and their union per stage to `record`.

    Without page presence (no access to pagemap), `blocks` is None and the
    image reads the whole executable ahead.
    """
    record['scenes'] = scenes
    groups = {stage: [scene['blocks'][stage] for scene in scenes.values()] for stage, _ in STAGES}
    if any(blocks is None for group in groups.values() for blocks in group):
        record.update(blocks=None, sampling='pagemap-unavailable')
    else:
        record['blocks'] = {stage: sorted(set().union(*group)) for stage, group in groups.items()}
    return record


def wait_for(child, path, seconds):
    deadline = time.monotonic() + seconds
    while not path.is_file():
        if child.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError(f'the generated player did not reach {path.name}')
        time.sleep(.015)


def scene(prefix, player, library, resources, fixtures, media, block, out):
    """Home and first Play of one saved song; the resident blocks at each."""
    out.mkdir()
    data = out / 'data'
    data.mkdir()
    config = data / 'config.toml'
    config.write_text(pgo.CONFIG + '[wifi]\nenabled = false\n[tailscale]\nenabled = false\n')
    runtime = out / 'runtime'
    runtime.mkdir(mode=0o700)
    marker = runtime / 'numeric-probe-enabled'
    marker.write_bytes(MARKER)
    marker.chmod(0o600)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(('LD_', 'QEMU_', 'Q2ND_', 'LLVM_PROFILE'))}
    env.update(Q2ND_RUNTIME=str(runtime), Q2ND_RESOURCE_ROOT=str(resources), Q2ND_DRM_INPUT='0',
               Q2ND_DRM_ALSA='0', Q2ND_STARTUP_PROFILE='1', Q2ND_NUMERIC_PROBE='1', Q2ND_PLAYTRACE='1',
               Q2ND_INSTALLED_STORAGE='0')
    # The player creates its own database; then the card is seeded.
    init = out / 'init.txt'
    init.write_text('wait 200\n')
    with (out / 'init.log').open('xb') as log:
        subprocess.run([*prefix, player, '--headless', '--data', data, '--script', init],
                       env=env, stdout=log, stderr=subprocess.STDOUT, timeout=60, check=True)
    pgo.seed(data, fixtures)
    with sqlite3.connect(data / 'library.db') as db:
        queue = json.loads(db.execute("SELECT value FROM kv WHERE key='queue'").fetchone()[0])
        queue['index'] = pgo.SONG_FILES.index(media)
        db.execute("UPDATE kv SET value=? WHERE key='queue'", [json.dumps(queue)])
    expected = queue['songs'][queue['index']]['title']
    blocks = {}
    evidence = {'config_sha256': sha(config), 'database_sha256': sha(data / 'library.db'),
                'media_sha256': sha(fixtures / media)}
    script = out / 'actions.txt'
    script.write_text(SCRIPT)
    command = [*prefix, '-E', f'LD_LIBRARY_PATH={library.parent}', '-E', f'LD_PRELOAD={library}',
               player, '--drm-test', '--data', data, '--script', script]
    with (out / 'player.log').open('xb') as log, subprocess.Popen(
            [str(part) for part in command], env=env, stdout=log, stderr=subprocess.STDOUT,
            start_new_session=True) as child:
        try:
            for name, state in STAGES:
                report = runtime / f'numeric-{name}.json'
                wait_for(child, report, 60)
                try:
                    blocks[name] = resident_blocks(child.pid, player, block)
                except PermissionError:
                    blocks[name] = None
                play = json.loads(report.read_text())['play']
                if play['state'] != state or play['song'] != expected or play['error']:
                    raise ValueError(f'generated saved playback did not reach {name}')
                evidence[name] = sha(report)
            trace, control = runtime / 'playtrace.dump', runtime / 'playtrace.ctl'
            if not stat.S_ISFIFO(control.lstat().st_mode):
                raise ValueError('the playback trace control is not a FIFO')
            subprocess.run(['sh', '-c', 'printf "dump\\n" > "$1"', 'sh', str(control)], check=True, timeout=3)
            wait_for(child, trace, 3)
            if not re.search(r'^E \d+ \d+ first-submit \d+ \d+ \d+$', trace.read_text(), re.M):
                raise ValueError('generated saved playback did not submit audio')
            if child.wait(timeout=30) != 0:
                raise RuntimeError('the generated player exited unsuccessfully')
            evidence['playtrace_sha256'] = sha(trace)
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)
    return {'blocks': blocks, 'evidence': evidence}


def generate(player, sdk, qemu_user, os_repo, out, fixtures):
    player = Path(player).resolve(strict=True)
    sdk, qemu_user, os_repo = (Path(p).resolve(strict=True) for p in (sdk, qemu_user, os_repo))
    out = Path(out).absolute()
    reader = consumer(os_repo)
    if os.sysconf('SC_PAGE_SIZE') != PAGE:
        raise SystemExit('the page sample needs 4 KiB host pages')
    cc = sdk / 'bin/mipsel-buildroot-linux-gnu-gcc'
    if not cc.is_file():
        raise SystemExit(f'{cc}: the SDK compiler is missing')
    out.mkdir()
    (ROOT / 'build').mkdir(exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix='startup-profile-', dir=ROOT / 'build'))
    record = {'schema': 1, 'passed': False, 'block_bytes': reader.BLOCK, 'app_sha256': sha(player),
              'app_bytes': player.stat().st_size, 'hardware': False}
    try:
        library = bridge(work / 'bridge', cc, os_repo / DISPLAY)
        record['producer_sha256'] = {name: sha(ROOT / name) for name in PRODUCERS}
        record['inputs_sha256'] = {'qemu_user': sha(qemu_user), 'display_bridge': sha(library),
                                   'display_source': sha(os_repo / DISPLAY), 'consumer': sha(os_repo / CONSUMER)}
        fixtures = pgo.fixtures(fixtures)
        resources = pgo.resources(work / 'resources')
        prefix = pgo.user_prefix(qemu_user, sdk)
        scenes = {media: scene(prefix, player, library, resources, fixtures, media, reader.BLOCK, work / media)
                  for media in MEDIA}
        combine(record, scenes)
        if sha(player) != record['app_sha256']:
            raise ValueError('the player changed while it was profiled')
        record['passed'] = True
        candidate = work / 'candidate.json'
        candidate.write_text(json.dumps(record, indent=2) + '\n')
        record['decision'] = reader.admit(candidate, player)['mode']
    except BaseException as error:
        record.update(passed=False, error=str(error), work=str(work))
        raise
    finally:
        with (out / ('profile.json' if record['passed'] else 'result.json')).open('x') as stream:
            json.dump(record, stream, indent=2)
            stream.write('\n')
    shutil.rmtree(work)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--player', type=Path, required=True, help='q2nd of a paired target build')
    parser.add_argument('--sdk', type=Path, required=True, help='the relocated Buildroot SDK of the build')
    parser.add_argument('--qemu-user', type=Path, required=True, help='qemu-mipsel')
    parser.add_argument('--os-repo', type=Path, required=True, help='the q2-os checkout of the build')
    parser.add_argument('--out', type=Path, help=f'new directory; default {SIDECAR}/ beside --player')
    parser.add_argument('--fixtures', type=Path, default=ROOT / 'build/perf/fixtures')
    args = parser.parse_args()
    out = args.out or args.player.parent / SIDECAR
    if out.exists() or out.is_symlink():
        parser.error(f'{out} exists; profile a fresh build')
    for name in ('player', 'sdk', 'qemu_user', 'os_repo'):
        if not getattr(args, name).exists():
            parser.error(f'--{name.replace("_", "-")} {getattr(args, name)} does not exist')
    record = generate(args.player, args.sdk, args.qemu_user, args.os_repo, out, args.fixtures)
    print(json.dumps({'decision': record['decision'], 'app_sha256': record['app_sha256'],
                      'blocks': None if record['blocks'] is None else {k: len(v) for k, v in record['blocks'].items()}}))


if __name__ == '__main__':
    main()
