#!/usr/bin/env python3
"""Bounded generated spoken sleep-timer UI replay.

This checks navigation, arm/cancel and persisted automatic settings. It does not
wait five minutes or establish panel-off, acoustic fade or rewind behavior.
"""
import argparse
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import tomllib
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]


def validate_expiry(probes, observations=None):
    before, fading, after = (probes[stage] for stage in ('before', 'fading', 'after'))
    assert all(not p['error'] and not p['pause_admission_failed'] and p['selection_tag'] is not None for p in probes.values())
    assert before['play_state'] == fading['play_state'] == 1 and after['play_state'] == 2, probes
    assert len({(p['pid'], p['generation'], p['selection_tag']) for p in probes.values()}) == 1, probes
    assert fading['position_ms'] - before['position_ms'] >= 40000, probes
    # Fade publication is around 50 s; expiry at 60 s rewinds 15 s.
    # Reject 10- or 20-second total rewinds, allowing scheduling jitter.
    assert -8000 <= after['position_ms'] - fading['position_ms'] <= -2500, probes
    assert after['seek_revision'] == fading['seek_revision'] + 1, probes
    if observations is not None:
        assert len(observations) == 2, observations
        rows = [line.split() for line in observations]
        assert [row[0] for row in rows] == ['fading', 'after'], rows
        assert all(len(row) == 4 and row[2] == '0' and row[3] == 'Off' for row in rows), rows
        assert float(rows[1][1]) > float(rows[0][1]), rows


def validate_visible_images(out):
    """Distinct real screen captures; a human still reviews readable captions."""
    crops = []
    for name in ('now-playing-active.png', 'now-playing-fading.png', 'now-playing-expired.png'):
        with Image.open(out / name) as image:
            image.load()
            if image.size != (375, 320): raise ValueError('unexpected screen size')
            crops.append(image.convert('RGB').crop((40, 299, 335, 320)).tobytes())
    if len(set(crops)) != 3: raise ValueError('timer status did not visibly change across phases')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--process-timeout', type=int, default=90)
    parser.add_argument('--execution-label', default='native host')
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--expiry', action='store_true', help='run the one-minute generated timer and numeric playback probes')
    modes.add_argument('--visible-expiry', action='store_true', help='capture the actual countdown and fade during one-minute automatic expiry')
    args = parser.parse_args()
    if not 1 <= args.process_timeout <= 220:
        parser.error('process timeout must be between 1 and 220 seconds')
    out = args.out.resolve()
    if not out.is_relative_to(ROOT / 'build') or out.exists():
        parser.error('use a fresh output directory under build/')
    out.mkdir(parents=True)
    card = out / 'generated-card'
    books = card / 'audiobooks'
    books.mkdir(parents=True)
    subprocess.run(['ffmpeg', '-v', 'error', '-n', '-f', 'lavfi', '-i',
                    'sine=frequency=317:sample_rate=16000:duration=120',
                    '-metadata', 'title=Generated Timer Book', '-c:a', 'aac', '-b:a', '48k',
                    '-movflags', '+faststart', str(books / 'timer.m4b')], check=True, timeout=30)
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args): pass
        def do_GET(self):
            path = self.path.split('?', 1)[0]
            if path == '/api/me':
                body = b'{"id":"generated-user"}'
            elif path == '/api/libraries':
                body = b'{"libraries":[]}'
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def do_POST(self):
            self.send_response(204)
            self.send_header('Content-Length', '0')
            self.end_headers()
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    runtime = out / 'generated-runtime'
    runtime.mkdir(mode=0o700)
    (runtime / 'numeric-probe-enabled').write_bytes(b'Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n')
    (runtime / 'numeric-probe-enabled').chmod(0o600)
    utc = datetime.now(timezone.utc)
    minute = utc.hour * 60 + utc.minute
    start, end = (minute + 720) % 1440, (minute + 660) % 1440
    expiry = args.expiry or args.visible_expiry
    playback = f'[playback]\nauto_sleep_minutes=1\nauto_sleep_start={start}\nauto_sleep_end={end}\n' if expiry else ''
    (card / 'config.toml').write_text(
        '[server]\nurl="http://127.0.0.1:1"\nuser="generated"\npassword="synthetic-only"\n'
        f'[audiobookshelf]\nurl="http://127.0.0.1:{server.server_port}"\ntoken="generated-ui-token"\n'
        f'[display]\nbacklight_seconds={1 if args.expiry else 0}\nclock_in_title=false\nclicker=false\n'
        '[wifi]\nenabled=false\n[tailscale]\nenabled=false\n[power]\nsleep_minutes=0\n' + playback)
    script = out / 'navigation.txt'
    script.write_text(f'''await 15000 screen:Q2
scroll 1
center
await 15000 screen:Books
scroll 13
center
await 15000 idle:On This Card
center
await 15000 idle:Book Info
center
await 15000 screen:Now Playing
hold center
await 15000 overlay:Audiobook
present 15000
shot {out / 'spoken-menu.png'}
scroll 1
center
await 15000 screen:Sleep Timer
scroll 1
center
await 15000 screen:Now Playing
present 15000
shot {out / 'now-playing-armed.png'}
center
present 15000
shot {out / 'now-playing-scrub.png'}
back
await 15000 screen:Now Playing
hold center
await 15000 overlay:Audiobook
scroll 1
center
await 15000 screen:Sleep Timer
present 15000
shot {out / 'timer-armed.png'}
scroll -1
center
await 15000 screen:Now Playing
hold center
await 15000 overlay:Audiobook
scroll 1
center
await 15000 screen:Sleep Timer
present 15000
shot {out / 'timer-cancelled.png'}
back
await 15000 screen:Now Playing
present 15000
shot {out / 'now-playing-cancelled.png'}
back
await 15000 screen:On This Card
back
await 15000 screen:Books
back
await 15000 screen:Q2
scroll -2147483648
scroll 5
center
await 15000 screen:Settings
scroll 1
center
await 15000 screen:Playback
present 15000
shot {out / 'playback-menu.png'}
center
await 15000 screen:Auto Sleep Timer
present 15000
shot {out / 'automatic-timer-group.png'}
center
await 15000 screen:Duration
scroll 4
center
await 15000 screen:Auto Sleep Timer
present 15000
shot {out / 'automatic-timer.png'}
back
await 15000 screen:Playback
wait 3500
''')
    if expiry:
        # Do not generate button input after playback starts. The hardware
        # adapter passively samples real panel sysfs when probe files appear.
        prefix = script.read_text().split('center\nawait 15000 screen:Now Playing', 1)[0]
        if args.visible_expiry:
            script.write_text(prefix + f'''present 15000
shot {out / 'expiry-book-info.png'}
center
await 15000 screen:Now Playing
wait 3000
present 15000
shot {out / 'now-playing-active.png'}
probe before
wait 48000
present 15000
shot {out / 'now-playing-fading.png'}
probe fading
wait 11000
probe after
present 15000
shot {out / 'now-playing-expired.png'}
''')
        else:
            script.write_text(prefix + f'''present 15000
shot {out / 'expiry-book-info.png'}
center
await 15000 screen:Now Playing
wait 3000
probe before
wait 50000
probe fading
wait 10000
probe after
wait 1000
back
await 15000 screen:Now Playing
back
await 15000 screen:On This Card
back
await 15000 screen:Books
back
await 15000 screen:Q2
scroll -2147483648
scroll 5
center
await 15000 screen:Settings
scroll 3
center
await 15000 screen:Connections
scroll 2
center
await 15000 screen:Audiobookshelf
present 15000
shot {out / 'audiobookshelf-status.png'}
''')
    try:
        with (out / 'generated-ui.log').open('wb') as log:
            subprocess.run([str(args.binary.resolve()), '--headless', '--data', str(card), '--script', str(script)],
                           env={**os.environ, 'Q2ND_ALSA_DEVICE':'q2nd-fake', 'Q2ND_NUMERIC_PROBE':'1',
                                'Q2ND_INSTALLED_STORAGE':'0', 'Q2ND_RUNTIME':str(runtime)}, stdout=log, stderr=log,
                           check=True, timeout=args.process_timeout)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
    config = tomllib.loads((card / 'config.toml').read_text())
    if expiry:
        assert config['playback']['auto_sleep_minutes'] == 1
        probes = {stage: json.loads((runtime / f'numeric-{stage}.json').read_text())
                  for stage in ('before', 'fading', 'after')}
        observations = ((runtime / 'panel-observations.txt').read_text().splitlines()
                        if args.execution_label == 'physical Q2 native mainline' else None)
        validate_expiry(probes, None if args.visible_expiry else observations)
        if args.visible_expiry:
            validate_visible_images(out)
            if observations is not None:
                rows = [line.split() for line in observations]
                assert len(rows) == 2 and [row[0] for row in rows] == ['fading', 'after']
                assert all(len(row) == 4 and int(row[2]) > 0 and row[3] == 'On' for row in rows), rows
    else:
        assert config['playback']['auto_sleep_minutes'] == 30
        assert config['playback']['auto_sleep_start'] == 1320
        assert config['playback']['auto_sleep_end'] == 420
        for name in ('spoken-menu.png', 'timer-armed.png', 'timer-cancelled.png', 'automatic-timer.png', 'automatic-timer-group.png', 'playback-menu.png', 'now-playing-armed.png', 'now-playing-scrub.png', 'now-playing-cancelled.png'):
            assert (out / name).is_file(), name
        # Off gains a checkmark only after cancellation; compare its rendered badge.
        with Image.open(out / 'timer-armed.png') as armed, Image.open(out / 'timer-cancelled.png') as cancelled:
            badge = (330, 30, 360, 64)
            assert armed.crop(badge).tobytes() != cancelled.crop(badge).tobytes(), 'Off checkmark did not change'
        with Image.open(out/'now-playing-armed.png') as armed, Image.open(out/'now-playing-cancelled.png') as cancelled:
            assert armed.size == cancelled.size == (375, 320)
            assert armed.crop((40, 299, 335, 320)).tobytes() != cancelled.crop((40, 299, 335, 320)).tobytes(), 'timer strip unchanged after cancellation'
        with Image.open(out/'now-playing-armed.png') as normal, Image.open(out/'now-playing-scrub.png') as scrub:
            assert normal.crop((24, 256, 351, 297)).tobytes() != scrub.crop((24, 256, 351, 297)).tobytes(), 'center did not expose the existing scrub control'
    def sha(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()
    artifacts = [script, card / 'config.toml', books / 'timer.m4b', out / 'generated-ui.log']
    artifacts += ([out / 'expiry-book-info.png'] + ([out / name for name in ('now-playing-active.png', 'now-playing-fading.png', 'now-playing-expired.png')] if args.visible_expiry else [out / 'audiobookshelf-status.png']) + [runtime / f'numeric-{stage}.json' for stage in ('before', 'fading', 'after')]
                  if expiry else [out / name for name in ('spoken-menu.png', 'timer-armed.png', 'timer-cancelled.png', 'automatic-timer.png', 'automatic-timer-group.png', 'playback-menu.png', 'now-playing-armed.png', 'now-playing-scrub.png', 'now-playing-cancelled.png')])
    if expiry and args.execution_label == 'physical Q2 native mainline':
        artifacts.append(runtime / 'panel-observations.txt')
    (out / 'result.json').write_text(json.dumps({
        'scope': args.execution_label,
        'mode': 'visible one-minute expiry' if args.visible_expiry else 'one-minute expiry' if args.expiry else 'navigation',
        'harness_sha256': sha(Path(__file__).resolve()),
        'invoked_binary_sha256': sha(args.binary.resolve()),
        'artifacts_sha256': {str(path.relative_to(out)): sha(path) for path in artifacts},
        'coverage': ('generated spoken countdown screenshots and numeric expiry; captions require visual review' if args.visible_expiry else 'generated spoken timer expiry and numeric rewind; physical panel only when panel receipt exists'
                     if expiry else 'navigation and persisted configuration only'),
        'not_covered': (['acoustic fade', 'battery impact'] if args.expiry and args.execution_label == 'physical Q2 native mainline'
                        else ['acoustic fade', 'physical screen-off', 'battery impact'] if args.expiry
                        else ['acoustic fade', 'physical screen-off', 'battery impact'] if args.visible_expiry else ['timer expiry', 'audio envelope', 'rewind', 'physical screen-off'])
    }, indent=2) + '\n')
    print('Generated timer UI replay passed; see result.json and screenshots; visual review required.')


if __name__ == '__main__':
    main()
