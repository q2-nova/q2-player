#!/usr/bin/env python3
"""A generated Delta Sync venue for the player-driven sign-in journeys.

Runs inside the sandbox that `delta_app.py` builds. One argument selects the
journey:

supervisor  ui::portal::tests::generated_wheel_login_supervisor_fixture for
            cancel, challenge and success, then the helper alone for an
            activation answered with a video page (admitted and still gated).
wheel       ui::settings_tests::generated_delta_wheel_app_journey with wheel
            entry, a generated lease and an uncached stream before and after
            admission.
phone       The same App journey with phone entry over the temporary access
            point, a transient failure before the password prompt, Retry and
            the prepared account, then the uncached stream.
"""
import base64
from collections import Counter
import contextlib
import hashlib
import http.cookies
import http.server
import json
import re
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
import urllib.parse

sys.path.insert(0, '/fixture/portal')
import guest_journey as media  # noqa: E402  generated Subsonic stream

IP = '192.0.2.10'
PEER = '198.51.100.7'
INPUT = {'ssid': 'DeltaWiFi.com', 'bssid': '02:00:00:00:00:01', 'ip': IP}
USER = 'fixture-user'
PASSWORD = 'fixture-%&password'
SIGNIN = 'signin.delta.com'
PORTAL = 'wifi.delta.com'
URL_PATH = '/api/portableElectronicDevice/v1/flights/fixture-flight/devices/fixture-device/sessions'
HELPER = Path('/usr/libexec/q2-network/q2-portal')
LIFECYCLE = Path('/fixture/lifecycle')
SUPERVISOR_TEST = 'ui::portal::tests::generated_wheel_login_supervisor_fixture'
APP_TEST = 'ui::settings_tests::generated_delta_wheel_app_journey'
GATED = 'Delta showed a video. This player still needs Wi-Fi access.'

media.MEDIA_ID = 'fixture-uncached'
media.MEDIA_USER = 'fixture-media-user'
media.MEDIA_PASSWORD = 'fixture-media-password'


class Fixture:
    def __init__(self):
        self.reset()
        self.scenario = None
        self.transient_before_prompt = False

    def reset(self):
        self.stage = 0
        self.premium = False
        self.events = Counter()
        self.issues = []
        self.challenge = None
        self.prompt = 'USERNAME_PASSWORD_REQUIRED'
        self.account, self.password = USER, PASSWORD


fixture = Fixture()


class Venue(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, value=None, cookie=None, content_type='application/json', **headers):
        body = b'' if value is None else (value if isinstance(value, bytes) else json.dumps(value).encode())
        self.send_response(status)
        content_type = headers.pop('Content-Type', content_type)
        self.send_header('Content-Type', content_type)
        for name, header in headers.items():
            self.send_header(name, header)
        self.send_header('Content-Length', str(len(body)))
        if cookie:
            self.send_header('Set-Cookie', cookie)
        self.end_headers()
        if content_type != 'audio/wav':
            self.wfile.write(body)
            return
        # Pace the generated track so the player decodes while it arrives.
        transferred = 0
        try:
            for offset in range(0, len(body), 8192):
                transferred += self.wfile.write(body[offset:offset + 8192])
                time.sleep(0.025)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.server.media_transfers.append({'status': status, 'declared_bytes': len(body),
                                            'sent_bytes': transferred, 'range': headers.get('Content-Range')})

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path)
        if not isinstance(self.connection, ssl.SSLSocket) and path.path.startswith('/rest/'):
            try:
                if path.path not in ('/rest/stream', '/rest/stream.view'):
                    return self.reply(200, {'subsonic-response': {'status': 'ok', 'version': '1.16.1'}})
                self.server.admitted = fixture.premium
                return media.Venue.rest(self, path)
            except Exception as error:  # reported through fixture.issues
                fixture.issues.append(f'{type(error).__name__} serving generated media')
                return self.reply(500)
        self.dispatch('GET')

    def do_POST(self):
        self.dispatch('POST')

    def dispatch(self, method):
        try:
            assert self.client_address[0] == IP
            parsed = urllib.parse.urlsplit(self.path)
            host = self.headers['Host']
            if not isinstance(self.connection, ssl.SSLSocket):
                assert host == 'detectportal.firefox.com'
                assert (method, parsed.path) == ('GET', '/success.txt')
                fixture.events['http_probe_allow' if fixture.premium else 'http_probe_deny'] += 1
                return self.reply(200 if fixture.premium else 511,
                                  b'success\n' if fixture.premium else b'fixture gateway pending',
                                  content_type='text/plain')

            assert host in (PORTAL, SIGNIN)
            assert self.headers['Origin'] == 'https://wifi.delta.com'
            assert self.headers['Cache-Control'] == 'no-store'
            cookies = http.cookies.SimpleCookie(self.headers.get('Cookie', ''))
            if host == SIGNIN:
                assert 'XSRF-TOKEN' not in cookies
                assert self.headers['X-XSRF-Header'] == 'PingFederate'
            else:
                assert 'FLOW' not in cookies

            if (host, method, parsed.path) == (PORTAL, 'GET', '/api/device-details'):
                fixture.events['premium_policy' if fixture.premium else 'basic_policy'] += 1
                return self.reply(200, dict(deviceId='fixture-device', deviceLanIp=IP,
                                            devicePolicy='DalPedPremium' if fixture.premium else 'DalPedDefault'),
                                  cookie='XSRF-TOKEN=fixture-csrf; Domain=.delta.com; Secure; Path=/')
            if (host, method, parsed.path) == (PORTAL, 'GET', '/api/flight-data'):
                return self.reply(200, {'flightId': 'fixture-flight'})
            if (host, method, parsed.path) == (SIGNIN, 'GET', '/as/authorization.oauth2'):
                assert fixture.stage == 0
                query = urllib.parse.parse_qs(parsed.query)
                assert query['code_challenge_method'] == ['S256']
                fixture.challenge, = query['code_challenge']
                fixture.events['identity_starts'] += 1
                if fixture.transient_before_prompt:
                    assert (LIFECYCLE / 'phone-retired').is_file()
                    if fixture.events['identity_starts'] == 1:
                        assert not any(fixture.events[key] for key in (
                            'password_prompt', 'password_submissions', 'activation_submissions'))
                        fixture.events['temporary_before_prompt'] += 1
                        return self.reply(503, {'error': 'generated temporary outage'})
                fixture.stage = 1
                fixture.events['password_prompt'] += 1
                return self.reply(200, {'id': 'fixture-flow', 'status': fixture.prompt},
                                  cookie='FLOW=fixture-flow-cookie; Secure; HttpOnly; Path=/')
            data = self.rfile.read(int(self.headers.get('Content-Length', 0)))
            if (host, method, parsed.path) == (SIGNIN, 'POST', '/pf-ws/authn/flows/fixture-flow'):
                assert cookies['FLOW'].value == 'fixture-flow-cookie'
                action = (self.headers['Content-Type'].removeprefix('application/vnd.pingidentity.')
                          .removesuffix('+json'))
                action_by_stage = {1: 'checkUsernamePassword', 2: 'rememberDevice', 3: 'submitContinue'}
                assert action_by_stage[fixture.stage] == action
                payload = json.loads(data)
                fixture.stage += 1
                if action == 'checkUsernamePassword':
                    fixture.events['password_submissions'] += 1
                    if fixture.scenario == 'success':
                        # The player checks that the same helper process is
                        # still waiting on this exchange before it continues.
                        (LIFECYCLE / 'password-post').touch()
                        deadline = time.monotonic() + 5
                        while not (LIFECYCLE / 'continue').exists():
                            if time.monotonic() >= deadline:
                                raise AssertionError('player did not verify live helper')
                            time.sleep(.02)
                    if payload != {'username': fixture.account, 'password': fixture.password}:
                        return self.reply(401, {'status': 'USERNAME_PASSWORD_REQUIRED'})
                    return self.reply(200, {'id': 'fixture-flow', 'status': 'SHOULD_REMEMBER_USER'})
                if action == 'rememberDevice':
                    assert payload == {'rememberUser': False}
                    return self.reply(200, {'id': 'fixture-flow', 'status': 'REMEMBERED_DEVICE_DETAILS'})
                assert payload == {'continue': True}
                return self.reply(200, {'status': 'COMPLETED', 'authorizeResponse': {'code': 'fixture-code-&'}})
            if (host, method, parsed.path) == (SIGNIN, 'POST', '/as/token.oauth2'):
                assert fixture.stage == 4
                assert cookies['FLOW'].value == 'fixture-flow-cookie'
                fields = urllib.parse.parse_qs(data.decode())
                assert fields['code'] == ['fixture-code-&']
                assert fields['grant_type'] == ['authorization_code']
                verifier, = fields['code_verifier']
                digest = hashlib.sha256(verifier.encode()).digest()
                assert base64.urlsafe_b64encode(digest).decode().rstrip('=') == fixture.challenge
                fixture.stage = 5
                fixture.events['verified_pkce'] += 1
                return self.reply(200, {'expires_in': 3600, 'access_token': 'fixture-token'})
            if (host, method, parsed.path) == (PORTAL, 'POST', URL_PATH):
                assert fixture.stage == 5
                assert not fixture.premium
                assert self.headers['UserAuth'] == 'fixture-token'
                assert self.headers['X-XSRF-TOKEN'] == cookies['XSRF-TOKEN'].value == 'fixture-csrf'
                assert json.loads(data) == {'type': 'freeWithSkyMiles'}
                fixture.events['activation_submissions'] += 1
                fixture.premium = fixture.scenario != 'video_gated'
                fixture.stage = 6
                if fixture.scenario in ('video_admitted', 'video_gated'):
                    return self.reply(200, b'<video src="/fixture-video"></video>'
                                      b'<a href="/fixture-link">Test link</a>',
                                      content_type='text/html; charset=UTF-8')
                return self.reply(204)
            raise AssertionError('unexpected requested endpoint')
        except (AssertionError, KeyError, ValueError) as error:
            fixture.issues.append(f'{type(error).__name__} at {self.path.split("?")[0]}')
            self.reply(500, {'error': 'fixture validation failed'})


def until(predicate, seconds):
    end = time.monotonic() + seconds
    while not predicate():
        if time.monotonic() >= end:
            raise AssertionError('fixture condition timed out')
        time.sleep(0.05)


def link(static_address):
    steps = [['link', 'set', 'lo', 'name', 'wlan0'], ['link', 'set', 'wlan0', 'up'],
             ['addr', 'add', f'{PEER}/32', 'dev', 'wlan0']]
    if static_address:
        steps.append(['addr', 'add', f'{IP}/32', 'dev', 'wlan0'])
    for args in steps:
        subprocess.run(['ip', *args], check=True, timeout=4)


@contextlib.contextmanager
def venue():
    with contextlib.ExitStack() as stack:
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain('/fixture/server.pem', '/fixture/server.key')
        https = stack.enter_context(http.server.ThreadingHTTPServer((PEER, 443), Venue))
        https.socket = tls.wrap_socket(https.socket, server_side=True)
        web = stack.enter_context(http.server.ThreadingHTTPServer((PEER, 80), Venue))
        web.media = Path('/fixture/tone.wav').read_bytes() if Path('/fixture/tone.wav').is_file() else None
        web.media_requests = []
        web.media_transfers = []
        for server in (https, web):
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)
        yield web


class Supplicant:
    """wpa_supplicant's control socket: association after `up`, an address after `dhcp`."""

    def __init__(self, path='/run/generated-wpa'):
        self.path = path
        self.commands = Counter()
        self.ready = threading.Event()
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self.serve, daemon=True)

    def serve(self):
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as control:
            control.bind(self.path)
            self.ready.set()
            control.settimeout(.25)
            while not self.stopped.is_set():
                try:
                    request, sender = control.recvfrom(4096)
                except TimeoutError:
                    continue
                command = request.decode()
                self.commands[command] += 1
                if command == 'STATUS':
                    reply = 'wpa_state=DISCONNECTED\n'
                    if (LIFECYCLE / 'radio-up').is_file():
                        reply = f'wpa_state=COMPLETED\nssid=DeltaWiFi.com\nbssid={INPUT["bssid"]}\n'
                        if (LIFECYCLE / 'lease').is_file():
                            reply += f'ip_address={IP}\n'
                elif command == 'LIST_NETWORKS':
                    reply = 'network id / ssid / bssid / flags\n0\tDeltaWiFi.com\tany\t[CURRENT]\n'
                elif command == 'SIGNAL_POLL':
                    reply = 'RSSI=-50\n'
                elif command == 'SCAN_RESULTS':
                    reply = 'bssid / frequency / signal level / flags / ssid\n'
                else:
                    reply = 'OK\n'
                try:
                    control.sendto(reply.encode(), sender)
                except (FileNotFoundError, ConnectionRefusedError):
                    pass

    def __enter__(self):
        self.thread.start()
        assert self.ready.wait(4)
        return self

    def __exit__(self, *_):
        self.stopped.set()
        self.thread.join(2)
        assert not self.thread.is_alive()


def player(test, timeout):
    return subprocess.run(['/fixture/player-test', '--ignored', '--exact', test, '--nocapture'],
                          capture_output=True, text=True, timeout=timeout)


def report(name, **fields):
    print(json.dumps({'scenario': name, **fields}), flush=True)


def supervisor():
    link(static_address=True)
    with venue():
        for scenario in ('cancel', 'challenge', 'success'):
            (LIFECYCLE / 'case').write_text(scenario)
            for name in ('password-post', 'continue'):
                (LIFECYCLE / name).unlink(missing_ok=True)
            fixture.reset()
            fixture.scenario = scenario
            if scenario == 'challenge':
                fixture.prompt = 'MFA_REQUIRED'
            p = player(SUPERVISOR_TEST, 20)
            assert USER not in p.stdout + p.stderr and PASSWORD not in p.stdout + p.stderr
            print(scenario, p.stdout.strip(), p.stderr.strip() if p.returncode else '', sep='\n', flush=True)
            assert p.returncode == 0 and '1 passed;' in p.stdout, scenario
            assert not fixture.issues, fixture.issues
            assert fixture.events['http_probe_deny'] == 1 and fixture.events['password_prompt'] == 1
            expected = int(scenario == 'success')
            for key in ('password_submissions', 'activation_submissions', 'verified_pkce', 'http_probe_allow'):
                assert fixture.events[key] == expected, (scenario, key, dict(fixture.events))
            assert fixture.premium == bool(expected)
            report(scenario, counts=dict(fixture.events))
        for scenario in ('video_admitted', 'video_gated'):
            fixture.reset()
            fixture.scenario = scenario
            p = subprocess.run([str(HELPER)], input=json.dumps(
                {'v': 1, 'op': 'login', **INPUT, 'user': USER, 'password': PASSWORD}) + '\n',
                check=True, capture_output=True, text=True, timeout=45)
            assert USER not in p.stdout + p.stderr and PASSWORD not in p.stdout + p.stderr
            # The venue refuses every media and link request.
            assert not p.stderr and not fixture.issues, (p.stderr, fixture.issues)
            observed = json.loads(p.stdout)
            if scenario == 'video_admitted':
                assert observed == {'state': 'online'} and fixture.events['http_probe_allow'] == 1, observed
            else:
                assert observed == {'state': 'failed', 'message': GATED}, observed
                assert fixture.events['http_probe_allow'] == fixture.events['http_probe_deny'] == 0
            assert fixture.events['password_submissions'] == fixture.events['activation_submissions'] == 1
            assert fixture.events['verified_pkce'] == 1
            report(scenario, counts=dict(fixture.events))
    print('Q2_GENERATED_WHEEL_SUPERVISOR_PASS', flush=True)


def stream_evidence(web, denied_before_auth, decoded_ms):
    until(lambda: len(web.media_requests) > denied_before_auth and bool(web.media_transfers), 5)
    admitted = web.media_requests[denied_before_auth:]
    assert all(req['status'] in (200, 206) and req['bytes'] > 0 for req in admitted), admitted
    assert next(req for req in admitted if req['offset'] == 0)['bytes'] == len(web.media)
    assert any(req['sent_bytes'] >= 17684 for req in web.media_transfers), 'stream did not deliver 100 ms of PCM'
    assert decoded_ms >= 100
    return {'denied_streams': web.media_requests[:denied_before_auth], 'admitted_streams': web.media_transfers,
            'decoded_ms': decoded_ms}


def check_signin(supplicant, *, retried):
    assert not fixture.issues and not Path('/run/q2-portal-phone').exists(), fixture.issues
    assert fixture.premium and (LIFECYCLE / 'lease').exists()
    assert supplicant.commands['STATUS'] >= 2
    assert fixture.events['http_probe_deny'] >= 1
    assert fixture.events['identity_starts'] == (2 if retried else 1)
    assert fixture.events['temporary_before_prompt'] == int(retried)
    for key in ('password_prompt', 'password_submissions', 'activation_submissions', 'verified_pkce',
                'http_probe_allow'):
        assert fixture.events[key] == 1, (key, dict(fixture.events))


def decoded(stdout):
    advanced, = re.findall(r'generated same-queue stream advanced (\d+) ms', stdout)
    return int(advanced)


def wheel():
    link(static_address=False)
    (LIFECYCLE / 'streaming').touch()
    fixture.account, fixture.password = 'ab', 'ab'
    with Supplicant() as supplicant, venue() as web:
        p = player(APP_TEST, 45)
        print(p.stdout, p.stderr if p.returncode else '', sep='\n', flush=True)
        assert p.returncode == 0 and '1 passed;' in p.stdout
        denied_before_auth = sum(req['status'] == 403 for req in web.media_requests)
        assert denied_before_auth == 1 and web.media_requests[0] == {'status': 403, 'bytes': 0}
        check_signin(supplicant, retried=False)
        report('wheel-with-media', counts=dict(fixture.events), status_queries=supplicant.commands['STATUS'],
               **stream_evidence(web, denied_before_auth, decoded(p.stdout)))
    print('Q2_GENERATED_WHEEL_APP_PASS', flush=True)


def show_radio(mock, run):
    """The stand-in radio's requests and the watcher's state after a failed setup."""
    for path in [mock / 'radio-events', mock / 'station', *sorted(run.glob('*'))] if run.is_dir() else \
            [mock / 'radio-events', mock / 'station']:
        if path.is_file():
            print(f'-- {path}:', path.read_text(errors='replace')[-2000:], sep='\n', flush=True)
        elif path.exists():
            print(f'-- {path}: {sorted(p.name for p in path.iterdir()) if path.is_dir() else "special"}', flush=True)
    print(f'-- {run} exists: {run.exists()}', flush=True)


def phone():
    sys.path.insert(0, '/fixture/os')
    import test_portal_phone as radio  # q2-os: stand-in radio tools around the real AP watcher
    link(static_address=False)
    (LIFECYCLE / 'case').write_text('phone')
    (LIFECYCLE / 'streaming').touch()
    (LIFECYCLE / 'retry-prepared').touch()
    fixture.account, fixture.password = radio.ACCOUNT, radio.PASSWORD
    fixture.transient_before_prompt = True
    radio.SCRIPT = Path('/fixture/portal-phone-ap')
    with tempfile.TemporaryDirectory(prefix='q2-app-phone-', dir='/tmp') as temporary:
        mock = Path(temporary)
        radio.stage(mock)  # mounts a tmpfs on /run and the stand-ins on /usr/libexec
        (mock / 'station').write_text('2412\n')
        HELPER.touch()
        radio.run('mount', '--bind', '/fixture/fixture-helper', HELPER)
        try:
            with Supplicant() as supplicant, venue() as web:
                peer = radio.phone_namespace(mock)
                try:
                    with subprocess.Popen(['/fixture/player-test', '--ignored', '--exact', APP_TEST, '--nocapture'],
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as app:
                        try:
                            try:
                                until(lambda: (LIFECYCLE / 'scan-ready').is_file() or app.poll() is not None, 20)
                            except AssertionError:
                                app.kill()
                            if app.poll() is not None or not (LIFECYCLE / 'scan-ready').is_file():
                                print(*app.communicate(timeout=6), sep='\n', flush=True)
                                show_radio(mock, radio.RUN)
                            assert (LIFECYCLE / 'scan-ready').is_file(), 'App test ended before the QR rendezvous'
                            page = LIFECYCLE / 'qr-page'
                            assert page.stat().st_mode & 0o777 == 0o600
                            qr = urllib.parse.urlsplit(page.read_text().strip())
                            token = qr.fragment
                            assert qr.scheme == 'http' and len(token) == 64 and qr.path == '/'
                            url = qr._replace(fragment='').geturl()
                            pids = radio.owned_pids()
                            settings = (radio.RUN / 'hostapd.conf').read_text()
                            assert all(entry in settings for entry in
                                       ('channel=1\n', 'interface=wlan1\n', 'rsn_pairwise=CCMP\n'))
                            radio.peer_request(peer, url, 200)
                            radio.peer_request(peer, url, 200, token)
                            radio.check_cleanup(mock, pids, 0)
                            peer.kill()
                            peer.wait(timeout=6)
                            links = [x['ifname'] for x in json.loads(subprocess.check_output(['ip', '-j', 'link']))]
                            assert links == ['wlan0'], links
                            assert not fixture.events['password_submissions'] and not fixture.premium
                            assert web.media_requests and all(req == {'status': 403, 'bytes': 0}
                                                              for req in web.media_requests)
                            denied_before_auth = len(web.media_requests)
                            (LIFECYCLE / 'phone-retired').touch()
                            stdout, stderr = app.communicate(timeout=20)
                        finally:
                            if app.poll() is None:
                                app.kill()
                                app.wait(timeout=6)
                    assert all(secret not in stdout + stderr for secret in (radio.ACCOUNT, radio.PASSWORD, token))
                    print(stdout, stderr if app.returncode else '', sep='\n', flush=True)
                    assert app.returncode == 0 and '1 passed;' in stdout
                    assert not page.exists(), 'the QR token rendezvous should be retired'
                finally:
                    if peer.poll() is None:
                        peer.kill()
                        peer.wait(timeout=6)
                check_signin(supplicant, retried=True)
                report('phone-prepared-retry-with-media', counts=dict(fixture.events),
                       status_queries=supplicant.commands['STATUS'], phone_peer_retired_before_auth=True,
                       generated_station_channel=1, **stream_evidence(web, denied_before_auth, decoded(stdout)))
        finally:
            radio.run('umount', HELPER)
            radio.run('umount', '/usr/libexec')
            radio.run('umount', '/run')
    print('Q2_GENERATED_PHONE_APP_PASS', flush=True)


if __name__ == '__main__':
    {'supervisor': supervisor, 'wheel': wheel, 'phone': phone}[sys.argv[1]]()
