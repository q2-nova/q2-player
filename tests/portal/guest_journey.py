#!/usr/bin/env python3
"""Exercise the player portal supervisor and real helper in a generated venue."""
import argparse
import hashlib
import http.server
import json
import os
import sqlite3
import struct
import wave
from pathlib import Path
import subprocess
import sys
import threading
import time
import traceback
import urllib.parse
import uuid

ROOT = Path(__file__).resolve().parent.parent.parent
WIFI = dict(ssid='Generated Guest', bssid='02:00:00:00:00:01', ip='192.0.2.10')
PEER = '198.51.100.7'
PLAYER_TEST = 'ui::portal::tests::generated_guest_supervisor_fixture'
HELPER = '/usr/libexec/q2-network/q2-portal'
MEDIA_ROOT = Path('/fixture/media')
RESOURCES = Path('/fixture/resources')  # the player's fonts and EQ data
TRACE_CARD = Path('/mnt/mmc')
TRACE_CASES = Path('/fixture/wpa-status/trace-cases.json')
MEDIA_ID = 'generated-portal-track'
MEDIA_USER = 'generated-portal'
MEDIA_PASSWORD = 'generated-only'
TERMS = 'Guest Wi-Fi is for personal use. Please respect other travelers.'
FORM = '''<section id="terms">{terms}</section><form method="post" action="/admit">
<input type="hidden" name="csrf_token" value="fixture&amp;session">
<label><input type="checkbox" name="terms" required>I agree to the terms</label>
<label><input type="checkbox" name="marketing">Send offers</label>
<button type="submit">Agree and connect</button></form>'''


# LAWA's March 2021 public board report supplies the visible free choice and
# agreement wording. The forms, endpoints and session fields are generated.
BOINGO_FORM = """<main>
<h1>Free Airport Wi-Fi</h1>
<form method="post" action="/account">
<input type="text" name="username"><input type="password" name="password">
<button type="submit">Log In</button></form>
<form method="post" action="/free">
<input type="hidden" name="session" value="fixture&amp;session">
<input type="hidden" name="location" value="generated-airport">
<button type="submit" name="choice" value="free">Free Unlimited Wi-Fi</button>
<p>By selecting, you agree to the <a href="/policy.pdf">terms and conditions</a>.</p>
</form>
<form method="post" action="/paid">
<input type="hidden" name="product" value="day-pass">
<button type="submit">Buy Day Pass</button></form>
</main>"""
BOINGO_FIELDS = {'session': ['fixture&session'], 'location': ['generated-airport'],
                 'choice': ['free']}
FORBIDDEN_CHOICES = {'/policy.pdf', '/account', '/paid', '/sponsor-video'}


class Venue(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, code, data=b'', **headers):
        self.send_response(code)
        for key, value in headers.items():
            self.send_header(key, value)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def forbidden_choice(self):
        if urllib.parse.urlsplit(self.path).path not in FORBIDDEN_CHOICES:
            return False
        self.server.forbidden_requests.append((self.command, self.path))
        self.reply(403)
        return True

    def do_GET(self):
        if self.forbidden_choice():
            return
        path = urllib.parse.urlsplit(self.path)
        if self.server.media is not None and path.path.startswith('/rest/'):
            self.rest(path)
        elif self.path == '/success.txt':
            if self.server.admitted:
                self.reply(200, b'success\n', **{'Content-Type':'text/plain'})
            elif self.server.advertise_refresh:
                page = f'<head><meta http-equiv="refresh" content="0; url=http://{PEER}/guest"></head>'
                self.reply(511, page.encode(), **{'Content-Type': 'text/html'})
            else:
                self.reply(302, Location=f'http://{PEER}/guest')
        elif self.path == '/guest':
            self.reply(200, self.server.form, **{'Content-Type':'text/html','Set-Cookie':'visit=generated; Path=/'})
        else:
            self.reply(404)

    def rest(self, path):
        params = urllib.parse.parse_qs(path.query)
        if path.path in ('/rest/stream', '/rest/stream.view'):
            salt = params.get('s', [''])[0]
            token = hashlib.md5((MEDIA_PASSWORD + salt).encode()).hexdigest()
            assert (params.get('id') == [MEDIA_ID] and params.get('u') == [MEDIA_USER]
                    and salt and params.get('t') == [token]), 'incorrect generated Subsonic request'
            if not self.server.admitted:
                self.server.media_requests.append({'status': 403, 'bytes': 0})
                self.reply(403)
                return
            data = self.server.media
            offset = 0
            if self.headers.get('Range'):
                first, _, last = self.headers['Range'].removeprefix('bytes=').partition('-')
                offset = int(first)
                limit = min(len(data), int(last) + 1) if last else len(data)
                if not offset < limit:
                    self.reply(416, **{'Content-Range': f'bytes */{len(data)}'})
                    return
                payload = data[offset:limit]
                status = 206
                headers = {'Content-Range': f'bytes {offset}-{limit-1}/{len(data)}'}
            else:
                payload, status, headers = data, 200, {}
            self.reply(status, payload, **headers, **{'Content-Type': 'audio/wav', 'Accept-Ranges': 'bytes'})
            self.server.media_requests.append({'status': status, 'bytes': len(payload), 'offset': offset})
        else:
            status = ('ok' if self.server.admitted else 'failed')
            body = {'subsonic-response': {'status': status, 'version': '1.16.1'}}
            if status == 'failed':
                body['subsonic-response']['error'] = {'code': 50, 'message': 'Guest sign-in required'}
            self.reply(200 if self.server.admitted else 403, json.dumps(body).encode(),
                       **{'Content-Type': 'application/json'})

    def do_POST(self):
        if self.forbidden_choice():
            return
        encoded = self.rfile.read(int(self.headers['Content-Length'])).decode()
        body = urllib.parse.parse_qs(encoded, keep_blank_values=True, strict_parsing=True)
        ok = (self.path == self.server.free_path and self.client_address[0] == WIFI['ip']
              and self.headers.get('Cookie') == 'visit=generated'
              and body == self.server.free_fields)
        self.server.submissions += 1
        if ok and self.server.sponsored:
            self.server.form = b'<p>Watch Ad to Connect</p><video src="/sponsor-video"></video>'
        self.server.admitted = ok and not self.server.sponsored
        self.reply(200 if ok else 403)


def protocol(op, reply=None, wifi=WIFI, *, case):
    first = json.dumps(dict(v=1, op=op, **wifi)) + '\n'
    second = '' if not reply else json.dumps(dict(op=reply)) + '\n'
    env = None
    if TRACE_CARD.is_dir():
        attempt = str(uuid.uuid4())
        cases = json.loads(TRACE_CASES.read_text()) if TRACE_CASES.is_file() else {}
        assert case not in cases
        cases[case] = attempt
        TRACE_CASES.write_text(json.dumps(cases) + '\n')
        env = dict(os.environ, Q2_PORTAL_ATTEMPT=attempt)
    p = subprocess.run([HELPER], input=first+second, capture_output=True, env=env,
                       text=True, check=True, timeout=15)
    return [json.loads(line) for line in p.stdout.splitlines()]



def selection_tag():
    value = 2166136261
    for byte in MEDIA_ID.encode():
        value = ((value ^ byte) * 16777619) & 0xffffffff
    return value


def generate_media(directory):
    directory.mkdir()
    with wave.open(str(directory / 'tone.wav'), 'wb') as track:
        track.setparams((2, 2, 44100, 0, 'NONE', 'not compressed'))
        track.writeframes(b''.join(struct.pack('<hh', sample, -sample)
                                  for _ in range(44100 * 4 // 50)
                                  for sample in range(-2000, 2000, 80)))
    (directory / 'config.toml').write_text(f"""[server]
url = "http://{PEER}"
user = "{MEDIA_USER}"
password = "{MEDIA_PASSWORD}"
[wifi]
enabled = true
[tailscale]
enabled = false
[playback]
sync_queue = false
scrobble = false
[storage]
reserve_mb = 0
prefetch_songs = 0
""")


def playback(phase, qemu, consent=None):
    """Actual headless q2nd, saved queue, application decoder and software PCM."""
    directory = MEDIA_ROOT / phase
    directory.mkdir()
    runtime = directory / 'runtime'
    runtime.mkdir(mode=0o700)
    marker = runtime / 'numeric-probe-enabled'
    marker.write_text('Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n')
    marker.chmod(0o600)
    (directory / 'config.toml').write_bytes((MEDIA_ROOT / 'config.toml').read_bytes())
    song = {'id': MEDIA_ID, 'title': 'Generated portal track', 'suffix': 'wav', 'duration': 4}
    with sqlite3.connect(directory / 'library.db') as db:
        db.execute('CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT NOT NULL)')
        db.execute('INSERT INTO kv VALUES(?,?)', ('server', f'http://{PEER}|{MEDIA_USER}'))
        db.execute('INSERT INTO kv VALUES(?,?)',
                   ('queue', json.dumps({'songs': [song], 'index': 0, 'position_ms': 0})))
        if phase == 'local':
            db.execute('CREATE TABLE offline_songs(song_id TEXT PRIMARY KEY,path TEXT NOT NULL,'
                       'size INTEGER NOT NULL,song TEXT NOT NULL)')
            path = directory / 'offline' / 'tone.wav'
            path.parent.mkdir()
            path.write_bytes((MEDIA_ROOT / 'tone.wav').read_bytes())
            db.execute('INSERT INTO offline_songs VALUES(?,?,?,?)',
                       (MEDIA_ID, str(path), path.stat().st_size, json.dumps(song)))
    script = directory / 'inputs.txt'
    inputs = 'play\nawait-play 25000 playing 350\nprobe evidence\n'
    if consent:
        # The supervisor normally finishes in about a second. A finite retry
        # window leaves time for its real review and consent protocol. A retry
        # that races ahead of admission receives another 403 and fails.
        inputs = ('play\nawait-play 10000 stopped\nprobe denied\nwait 6000\n'
                  'play\nawait-play 12000 playing 350\nprobe evidence\n')
    script.write_text(inputs)
    env = dict(os.environ, Q2ND_NUMERIC_PROBE='1', Q2ND_INSTALLED_STORAGE='0',
               Q2ND_RUNTIME=str(runtime), Q2ND_RESOURCE_ROOT=str(RESOURCES))
    argv = ['/fixture/q2nd-playback']
    if qemu:
        argv = ['/fixture/qemu', '-L', '/fixture/sysroot', *argv]
    blocked = None
    with ((directory / 'app.log').open('x') as log):
        process = subprocess.Popen([*argv, '--headless', '--data', str(directory),
                                    '--script', str(script)],
                                   stdout=log, stderr=subprocess.STDOUT, env=env)
        try:
            if consent:
                marker = runtime / 'numeric-denied.json'
                deadline = time.monotonic() + 12
                while not marker.exists() and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.05)
                assert marker.exists(), 'player did not publish a denied stream snapshot'
                blocked = json.loads(marker.read_text())
                assert (blocked['selection_tag'] == selection_tag()
                        and blocked['position_ms'] == 0 and blocked['play_state'] == 0
                        and blocked['error']), blocked
                consent()
            assert process.wait(timeout=35) == 0, 'headless playback failed; see app.log'
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
    status = json.loads((runtime / 'numeric-evidence.json').read_text())
    assert status['selection_tag'] == selection_tag(), status
    trace = (directory / 'app.log').read_text()
    assert status['position_ms'] >= 350 and status['play_state'] == 1 and status['output_owned'], status
    assert not status['error'] and not status['pause_admission_failed'], status
    origin = 'Offline' if phase == 'local' else 'Streaming'
    assert f'playing "Generated portal track" (PCM 16/44.1, {origin})' in trace
    if blocked is not None:
        assert blocked['pid'] == status['pid'] == process.pid
        assert blocked['queue_index'] == status['queue_index']
        assert trace.index('playback: open: HTTP 403') < trace.index('PCM 16/44.1, Streaming')
    return status, blocked


def supervisor(venue, timeout=45):
    subprocess.run(['/fixture/q2nd-test', '--ignored', '--exact', PLAYER_TEST, '--nocapture'],
                   check=True, timeout=timeout)
    assert venue.submissions == 1 and venue.admitted, 'only stable-association consent may POST'
    print('player supervisor and venue consent counts: passed', flush=True)

def fixture(qemu=False, boingo=False):
    for args in [['link','set','lo','name','wlan0'], ['link','set','wlan0','up'],
                 ['addr','add',WIFI['ip']+'/32','dev','wlan0'], ['addr','add',PEER+'/32','dev','wlan0']]:
        subprocess.run(['ip', *args], check=True)
    with http.server.HTTPServer((PEER, 80), Venue) as venue:
        policy = Path('/fixture/long-agreement.txt')
        terms = policy.read_text() if policy.is_file() else TERMS
        venue.sponsored = False
        venue.form = (BOINGO_FORM if boingo else FORM.format(terms=terms)).encode()
        venue.free_path = '/free' if boingo else '/admit'
        venue.free_fields = BOINGO_FIELDS if boingo else {'csrf_token': ['fixture&session'], 'terms': ['on']}
        venue.forbidden_requests = []
        venue.admitted = False
        venue.submissions = 0
        venue.media = (MEDIA_ROOT / 'tone.wav').read_bytes() if MEDIA_ROOT.is_dir() else None
        venue.media_requests = []
        threading.Thread(target=venue.serve_forever, daemon=True).start()
        venue.advertise_refresh = False
        assert protocol('check', case='redirect') == [{'state': 'guest'}], 'HTTP redirect handoff failed'
        venue.advertise_refresh = True
        detected = protocol('check', case='refresh')
        assert detected == [{'state':'guest'}], f'guest detection returned {detected}'
        assert protocol('guest','cancel', case='cancel')[0] == {'state':'terms','text':'Terms & Conditions apply'}
        assert venue.submissions == 0 and not venue.admitted
        assert protocol('guest','accept',dict(WIFI, bssid='02:00:00:00:00:02'), case='initial-roam')[-1]['state']=='failed'
        assert venue.submissions == 0
        assert protocol('guest','accept', case='approved') == [{'state':'terms','text':'Terms & Conditions apply'},{'state':'online'}]
        assert venue.submissions == 1 and venue.admitted
        assert protocol('check', case='already-online') == [{'state':'online'}]
        print('standalone helper: passed', flush=True)

        if boingo:
            # Some venues present an ad only after the complimentary choice.
            venue.sponsored = True
            venue.admitted = False
            previous = venue.submissions
            states = protocol('guest', 'accept', case='sponsored')
            assert states == [
                {'state': 'terms', 'text': 'Terms & Conditions apply'},
                {'state': 'unsupported'},
            ], states
            assert venue.submissions == previous + 1 and not venue.admitted
            venue.sponsored = False
            venue.form = BOINGO_FORM.encode()
        venue.admitted = False
        venue.submissions = 0
        if venue.media is None:
            supervisor(venue)
        else:
            local, _ = playback('local', qemu)
            assert not venue.media_requests and not venue.admitted

            def consent():
                assert venue.media_requests == [{'status': 403, 'bytes': 0}]
                assert not venue.admitted and venue.submissions == 0
                supervisor(venue, timeout=5)
                assert venue.media_requests == [{'status': 403, 'bytes': 0}]

            links = json.loads(subprocess.check_output(['ip', '-j', 'link']))
            assert [link['ifname'] for link in links] == ['wlan0'], 'fixture has only the player link'
            opened, blocked = playback('uncached', qemu, consent)
            assert len(venue.media_requests) == 2
            approved = venue.media_requests[-1]
            assert approved.get('offset') == 0 and approved['status'] in (200, 206)
            assert approved['bytes'] > 0
            evidence = {'local': local, 'blocked': blocked, 'uncached': opened,
                        'media_requests': venue.media_requests, 'link_names': ['wlan0']}
            (MEDIA_ROOT / 'playback.json').write_text(json.dumps(evidence, indent=2) + '\n')
            print('generated local audio and same-player uncached retry: passed', flush=True)
        venue.shutdown()
        assert not venue.forbidden_requests, venue.forbidden_requests
        if boingo:
            print('mixed free choice and untouched alternative routes: passed', flush=True)



def sha(path):
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def trace_receipt(card, case_file):
    expected = {'redirect', 'refresh', 'cancel', 'initial-roam', 'approved',
                'already-online', 'sponsored'}
    cases = json.loads(case_file.read_text())
    assert set(cases) == expected and len(set(cases.values())) == len(expected)
    allowed_files = {'events.jsonl', 'player-outcome.json'}
    secrets = ('fixture&session', 'fixture&amp;session', 'fixture%26session',
               'generated-airport', 'visit=generated', 'value="day-pass"')
    traces = {}
    members = list((card / 'portal-debug').iterdir())
    assert len(members) >= len(expected) + 4
    for attempt in members:
        assert attempt.is_dir() and not attempt.is_symlink()
        identity = uuid.UUID(attempt.name)
        assert identity.version == 4 and str(identity) == attempt.name
        files = list(attempt.iterdir())
        assert all(f.is_file() and not f.is_symlink() and f.name in allowed_files for f in files)
        events = attempt / 'events.jsonl'
        assert events.stat().st_size < 2 * 1024 * 1024 + 1024
        raw = events.read_text()
        assert all(secret not in raw for secret in secrets), 'session material in card trace'
        lines = raw.splitlines()
        if not raw.endswith('\n'):
            lines.pop()
        rows = [json.loads(line) for line in lines]
        assert rows and rows[0]['kind'] == 'start' and rows[0]['attempt'] == attempt.name
        assert all(row['v'] == 1 and row['seq'] == index for index, row in enumerate(rows))
        requests, replies = {}, {}
        for row in rows:
            kind = row['kind']
            if kind == 'request':
                detail = row['detail']
                number = detail['n']
                assert number == len(requests) + 1
                requests[number] = detail['http']
            if kind in ('response', 'request_failed'):
                detail = row['detail']
                number = detail['n']
                assert number in requests and number not in replies
                replies[number] = row
        closed = rows[-1]['kind'] == 'close'
        assert not closed or (rows[-1]['dropped'] == 0 and requests.keys() == replies.keys())
        outcomes = attempt / 'player-outcome.json'
        player_outcome = None
        if outcomes.is_file():
            text = outcomes.read_text()
            assert all(secret not in text for secret in secrets)
            outcome = json.loads(text)
            assert outcome['attempt'] == attempt.name
            player_outcome = outcome['reason']
        posts = [(http, replies.get(number)) for number, http in requests.items()
                 if http['method'] == 'POST']
        traces[attempt.name] = {
            'files': {file.name: sha(file) for file in files},
            'op': rows[0]['op'],
            'states': [row['detail']['state'] for row in rows if row['kind'] == 'state'],
            'terminal': next((row['detail'] for row in rows if row['kind'] == 'terminal'), None),
            'records': len(rows), 'requests': len(requests), 'replies': len(replies),
            'closed': closed, 'posts': posts, 'player_outcome': player_outcome,
            'pages': [row['detail']['http']['body'] for row in replies.values()
                      if row['kind'] == 'response' and isinstance(row['detail']['http']['body'], dict)
                      and 'forms' in row['detail']['http']['body']],
        }

    def outcome(case):
        result = traces[cases[case]]
        assert result['closed'] and result['terminal']
        return result

    # A checked advertisement, ordinary selection, and local player
    # cancellation all see the same visible mixed portal form.
    for case in ('refresh', 'approved', 'cancel', 'sponsored'):
        attempt = outcome(case)
        mixed = next(page for page in attempt['pages'] if len(page['forms']) == 3)
        assert [f['action']['path_shape'] for f in mixed['forms']] == [['account'], ['free'], ['paid']]
        assert {c['name'] for c in mixed['controls'] if c['form'] == 1} == {'session', 'location', 'choice'}
        assert any('Free Airport Wi-Fi' == caption['text'] for caption in mixed['visible_labels'])
    for case in ('approved', 'sponsored'):
        attempt = outcome(case)
        assert attempt['terminal']['outcome'] == 'finished'
        assert attempt['states'] == ['terms', 'online' if case == 'approved' else 'unsupported']
        (http, response), = attempt['posts']
        assert http['url']['path_shape'] == ['free']
        assert set(http['body']['fields']) == {'session', 'location', 'choice'}
        assert response['kind'] == 'response' and response['detail']['http']['status'] == 200
    sponsor = outcome('sponsored')
    assert any(['video', 1] in page['landmarks'] for page in sponsor['pages'])
    assert outcome('cancel')['terminal']['reason'] == 'Sign-in cancelled'
    assert not outcome('cancel')['posts']
    assert 'Wi-Fi changed' in outcome('initial-roam')['terminal']['reason']
    assert not outcome('initial-roam')['posts']
    for case in ('redirect', 'already-online'):
        assert outcome(case)['terminal']['outcome'] == 'finished' and not outcome(case)['posts']

    supervisor_attempts = {id: trace for id, trace in traces.items() if id not in cases.values()}
    guest = [trace for trace in supervisor_attempts.values() if trace['op'] == 'guest']
    assert len(guest) == 3
    assert sum('online' in trace['states'] and trace['closed'] for trace in guest) == 1
    assert sum(trace['player_outcome'] == 'roam' and not trace['posts'] for trace in guest) == 1
    assert sum(len(trace['posts']) == 1 and trace['posts'][0][1]['kind'] == 'request_failed'
               and trace['posts'][0][1]['detail']['reason'] == 'Wi-Fi changed' for trace in guest) == 1
    # Preserve compact summaries and hash bindings. Raw captures live beside the
    # result and their projected fields were inspected above.
    return {'cases': cases, 'attempts': {
        attempt: {key: value for key, value in trace.items() if key not in ('pages', 'posts')}
                 | {'post_attempts': len(trace['posts'])} for attempt, trace in traces.items()}}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', type=Path, required=True, help='q2-portal build output')
    p.add_argument('--player-test', type=Path, required=True,
                   help='q2nd test executable from the exact workspace Rust test run')
    p.add_argument('--playback-player', type=Path,
                   help='optional q2nd executable for generated headless playback verification')
    p.add_argument('--qemu', type=Path, help='qemu-user for a MIPS q2-portal')
    p.add_argument('--sysroot', type=Path, help='runtime libraries for that MIPS build')
    venue = p.add_mutually_exclusive_group()
    venue.add_argument('--long-agreement', action='store_true', help='serve an airport-length policy and verify that the player gets only the short consent notice')
    venue.add_argument('--boingo-choice', action='store_true', help='serve a generated mixed free, account, and paid choice')
    p.add_argument('--trace-card', action='store_true', help='verify durable diagnostics on a generated card mount')
    p.add_argument('--out', type=Path, required=True)
    args = p.parse_args()
    if args.trace_card and not args.boingo_choice:
        p.error('--trace-card requires --boingo-choice')
    if bool(args.qemu) != bool(args.sysroot):
        p.error('--qemu and --sysroot are required together')
    out = args.out.resolve()
    if not out.is_relative_to(ROOT / 'build'):
        p.error('output belongs under build/')
    out.mkdir(parents=True, exist_ok=False)
    status = out / 'wpa-status'
    status.mkdir()
    (status / 'current').write_text('wpa_state=COMPLETED\nssid=Generated Guest\n'
                                   'bssid=02:00:00:00:00:01\nip_address=192.0.2.10\n')
    wpa = out / 'wpa_cli'
    wpa.write_text('#!/bin/sh\nexec cat /fixture/wpa-status/current\n')
    wpa.chmod(0o755)
    hosts = out / 'hosts'
    hosts.write_text(f'{PEER} detectportal.firefox.com\n')
    binary = args.binary.resolve(strict=True)
    player = args.player_test.resolve(strict=True)
    script = Path(__file__).resolve()
    sources = {'helper': binary, 'player_test': player, 'fixture': script,
               'player_supervisor_source': ROOT / 'crates/q2nd/src/ui/portal.rs'}
    cmd = ['bwrap', '--unshare-user', '--uid', '0', '--gid', '0', '--unshare-net', '--unshare-pid',
           '--die-with-parent', '--cap-add', 'CAP_NET_ADMIN', '--cap-add', 'CAP_NET_RAW',
           '--cap-add', 'CAP_NET_BIND_SERVICE', '--ro-bind', '/usr', '/usr', '--symlink', 'usr/bin', '/bin',
           '--symlink', 'usr/lib', '/lib', '--symlink', 'usr/lib64', '/lib64', '--proc', '/proc',
           '--dev', '/dev', '--tmpfs', '/tmp', '--dir', '/etc', '--ro-bind', str(hosts), '/etc/hosts',
           '--ro-bind', str(wpa), '/usr/sbin/wpa_cli', '--dir', '/fixture',
           '--bind', str(status), '/fixture/wpa-status',
           '--ro-bind', str(player), '/fixture/q2nd-test', '--ro-bind', str(script), '/fixture/run.py',
           '--tmpfs', '/usr/libexec', '--dir', '/usr/libexec/q2-network']
    if args.trace_card:
        card = out / 'card'
        card.mkdir()
        cmd += ['--dir', '/mnt', '--bind', str(card), str(TRACE_CARD)]
    if args.long_agreement:
        policy = out / 'long-agreement.txt'
        policy.write_text('Guest Wi-Fi is for "personal" use. ' * 400 + 'Final clause applies.')
        sources['agreement'] = policy
        cmd += ['--ro-bind', str(policy), '/fixture/long-agreement.txt']
    if args.playback_player:
        executable = args.playback_player.resolve(strict=True)
        media = out / 'media'
        generate_media(media)
        sources.update(playback_player=executable, generated_media=media / 'tone.wav',
                       generated_config=media / 'config.toml')
        cmd += ['--ro-bind', str(executable), '/fixture/q2nd-playback',
                '--bind', str(media), str(MEDIA_ROOT), '--tmpfs', str(RESOURCES),
                '--ro-bind', str(ROOT / 'assets/fonts'), str(RESOURCES / 'fonts'),
                '--ro-bind', str(ROOT / 'crates/q2nd/assets/autoeq'), str(RESOURCES / 'autoeq')]
    if args.qemu:
        qemu = args.qemu.resolve(strict=True)
        sysroot = args.sysroot.resolve(strict=True)
        if not sysroot.is_dir():
            p.error('--sysroot must name the target runtime directory')
        sources['qemu'] = qemu
        for directory in ('lib', 'lib64', 'usr/lib', 'usr/lib64'):
            for library in sorted((sysroot / directory).glob('*.so*')):
                if library.is_file():
                    sources['sysroot/' + str(library.relative_to(sysroot))] = library
        wrapper = out / 'q2-portal'
        wrapper.write_text('#!/bin/sh\nexec /fixture/qemu -L /fixture/sysroot /fixture/q2-portal-elf "$@"\n')
        wrapper.chmod(0o755)
        sources['qemu_launcher'] = wrapper
        cmd += ['--ro-bind', str(binary), '/fixture/q2-portal-elf',
                '--ro-bind', str(qemu), '/fixture/qemu', '--ro-bind', str(sysroot), '/fixture/sysroot',
                '--ro-bind', str(hosts), '/fixture/sysroot/etc/hosts',
                '--ro-bind', str(wrapper), HELPER]
    else:
        cmd += ['--ro-bind', str(binary), HELPER]
    cmd += ['--clearenv', '--setenv', 'PATH', '/usr/bin:/usr/sbin',
            '/usr/bin/python3', '/fixture/run.py', '--inner']
    if args.trace_card:
        invocation = cmd.index('/usr/bin/python3')
        cmd[invocation:invocation] = ['--setenv', 'Q2_PORTAL_TRACE_TEST', '1']
    if args.qemu:
        cmd += ['qemu']

    if args.boingo_choice:
        cmd.append('boingo')

    identities = {name: sha(path) for name, path in sources.items()}
    with (out / 'run.log').open('x') as log:
        try:
            rc = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, timeout=140 if args.playback_player else 75).returncode
        except subprocess.TimeoutExpired:
            log.write('isolated venue timed out\n')
            rc = 124
    text = (out / 'run.log').read_text()
    survived = identities == {name: sha(path) for name, path in sources.items()}
    passed = (rc == 0 and survived and 'running 1 test' in text
              and f'test {PLAYER_TEST} ... ok' in text
              and 'player supervisor and venue consent counts: passed' in text)
    if args.playback_player:
        passed = passed and 'generated local audio and same-player uncached retry: passed' in text
    if args.boingo_choice:
        passed = passed and 'mixed free choice and untouched alternative routes: passed' in text
    receipt = dict(passed=passed, hardware=False, exit_code=rc, identities=identities,
                   source_unchanged=survived, log_sha256=sha(out / 'run.log'), player_test=PLAYER_TEST,
                   helper_execution='qemu-user' if args.qemu else 'native',
                   sysroot=str(sysroot) if args.qemu else None,
                   tested=['helper detect and cancel', 'player observe, review, consent and online',
                           'no POST after observed BSSID change',
                           'no POST after BSSID change before player notification',
                           'one POST with the expected cookie and required fields'] if passed else [])
    if args.trace_card:
        try:
            receipt['trace_card'] = trace_receipt(card, status / 'trace-cases.json')
        except (OSError, ValueError, KeyError, TypeError, IndexError, StopIteration, AssertionError) as error:
            site = traceback.extract_tb(error.__traceback__)[-1]
            receipt['trace_card_error'] = {'type': type(error).__name__, 'line': site.lineno,
                                           'assertion': site.line}
            passed = receipt['passed'] = False
            receipt['tested'] = []
        if passed:
            receipt['tested'].append('distinct durable traces, paired HTTP records, redacted form and terminal evidence')
    if passed and args.boingo_choice:
        receipt['tested'] += ['only the free choice submitted its exact session and selection fields',
                              'zero requests to linked terms, account, or paid routes',
                              'subsequent sponsored page stayed gated with zero video requests']
    if passed and args.long_agreement:
        receipt['agreement_bytes'] = policy.stat().st_size
        receipt['tested'].append('airport-length terms produce only the fixed consent notice')
    if passed and args.playback_player:
        receipt['playback_sha256'] = sha(media / 'playback.json')
        receipt['playback_log_sha256'] = {phase: sha(media / phase / 'app.log')
                                          for phase in ('local', 'uncached')}
        receipt['tested'] += ['local audio advanced with sign-in pending', 'uncached stream blocked before consent',
                              'same player retried and decoded uncached stream after consent with only the player interface']
    with (out / 'result.json').open('x') as stream:
        stream.write(json.dumps(receipt, indent=2) + '\n')
    return 0 if passed else 1


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--inner':
        fixture(qemu='qemu' in sys.argv[2:], boingo='boingo' in sys.argv[2:])
    else:
        sys.exit(main())
