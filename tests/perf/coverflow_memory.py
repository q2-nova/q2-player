#!/usr/bin/env python3
"""What Cover Flow over a large generated library leaves resident (host).

Runs the headless player against ux_fault_fixture.py with ``--albums`` albums,
opens Cover Flow, scrolls through ``--covers`` of them, goes back to Music and
waits, then trims the heap. At each step the script's ``memory`` command logs
the memory telemetry sample (platform/memory.rs): RSS, the heap (mallinfo2)
and the decoded-artwork cache, and the trim's released bytes and time.

    python3 tests/perf/coverflow_memory.py --binary build/native-test-target/debug/q2nd [--albums 3000]

The result is a measurement of one run, not a comparison. Generated data only.
Host numbers: x86_64 pointers and allocator, not the Q2's MIPS32; the
decoded-art bytes are the same.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tests/ui'))
import ux_fault_fixture as fixture_module  # noqa: E402

MARKER = b'Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n'
LINE = re.compile(r'script memory (\S+): (\{.*\})')
TRIM = re.compile(r'script memory-trim (\S+): Trim \{ resident_kib: (\d+), available_kib: (\d+), heap_free_kib: (\d+), released_kib: (\d+) \} in (\d+) us')


def script(covers, step, interval_ms=400, reverse=False):
    text = ('await 15000 screen:Q2\nmemory boot\ncenter\nawait 15000 idle:Music\ncenter\n'
            'await 60000 idle:Cover Flow\nwait 2000\nmemory coverflow-open\n')
    for direction in ((step, -step) if reverse else (step,)):
        for _ in range(covers // step):
            text += f'scroll {direction}\nwait {interval_ms}\n'
    text += 'wait 3000\nmemory coverflow-scrolled\nback\nawait 5000 idle:Music\nwait 3000\nmemory after-exit\n'
    text += 'memory-trim trimmed\nmemory after-trim\nwait 500\n'
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, default=ROOT / 'build/native-test-target/debug/q2nd')
    parser.add_argument('--albums', type=int, default=3000)
    parser.add_argument('--covers', type=int, default=600, help='albums scrolled past')
    parser.add_argument('--step', type=int, default=3, help='detents per scroll')
    parser.add_argument('--interval-ms', type=int, default=400)
    parser.add_argument('--art-delay-ms', type=int, default=0)
    parser.add_argument('--reverse', action='store_true')
    parser.add_argument('--warm', action='store_true', help='prime the disk cache with the same journey in a separate process')
    parser.add_argument('--entries', action='store_true', help='require a nonempty on-entry artwork measurement')
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    out = (args.out or ROOT / 'build/perf' / f'coverflow-memory-{time.strftime("%Y%m%d-%H%M%S")}').resolve()
    if not out.is_relative_to(ROOT / 'build'):
        parser.error('--out must be below build/')
    out.mkdir(parents=True)
    fx = fixture_module.Fixture(albums=args.albums)
    if args.art_delay_ms:
        fx.nd.set('delay', seconds=args.art_delay_ms / 1000, paths=['/rest/getCoverArt'])
    card, runtime = out / 'card', out / 'runtime'
    card.mkdir()
    runtime.mkdir(mode=0o700)
    (runtime / 'numeric-probe-enabled').write_bytes(MARKER)
    (runtime / 'numeric-probe-enabled').chmod(0o600)
    (card / 'config.toml').write_text(fx.config())
    (out / 'script.txt').write_text(script(args.covers, args.step, args.interval_ms, args.reverse))
    env = {**os.environ, 'Q2ND_ALSA_DEVICE': 'q2nd-fake', 'Q2ND_NUMERIC_PROBE': '1',
           'Q2ND_INSTALLED_STORAGE': '0', 'Q2ND_RUNTIME': str(runtime)}
    command = [str(args.binary), '--headless', '--data', str(card), '--script', str(out / 'script.txt')]
    try:
        if args.warm:
            with (out / 'prime.log').open('x') as log:
                subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=600, check=True)
        with (out / 'player.log').open('x') as log:
            status = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=600).returncode
    finally:
        fx.close()
    text = (out / 'player.log').read_text(errors='replace')
    steps = {}
    for name, body in LINE.findall(text):
        sample = json.loads(body)
        steps[name] = {'vm_rss_kib': sample['player']['vm_rss_kib'], 'anonymous_kib': sample['player']['anonymous_kib'],
                       'private_dirty_kib': sample['player']['private_dirty_kib'], **sample['heap'], 'read_us': sample['read_us']}
    trims = [dict(zip(('resident_kib', 'available_kib', 'heap_free_kib', 'released_kib', 'trim_us'), map(int, m[1:])))
             for m in TRIM.findall(text)]
    result = {'albums': args.albums, 'covers_scrolled': args.covers, 'exit_status': status, 'steps': steps, 'trim': trims,
              'domain': 'host x86_64 debug build; not Q2 MIPS32'}
    entries = re.findall(r'art ready on entry (\d+)/(\d+)', text)
    with args.binary.open('rb') as binary:
        binary_sha256 = hashlib.file_digest(binary, 'sha256').hexdigest()
    result.update(binary_sha256=binary_sha256,
                  interval_ms=args.interval_ms, art_delay_ms=args.art_delay_ms, reverse=args.reverse, disk_warm=args.warm,
                  entry_ready=int(entries[-1][0]) if entries else None,
                  entry_count=int(entries[-1][1]) if entries else None)
    (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))
    shutil.rmtree(card, ignore_errors=True)
    return 0 if status == 0 and steps and (not args.entries or result["entry_count"]) else 1


if __name__ == '__main__':
    sys.exit(main())
