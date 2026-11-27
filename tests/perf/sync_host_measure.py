#!/usr/bin/env python3
"""Requests, bytes and time of the library syncs, measured from the host.

Runs a local proxy in front of generated servers and the ignored test
`measure_sync_through_proxy`
(crates/q2nd/src/library/music_catalog/owner_measure.rs) against it. The
test builds the album catalog twice (a full build, then an unchanged
library) and, with --abs, syncs every Audiobookshelf library twice (full,
then unchanged). The proxy counts each request, its status and the body
bytes on the wire, by phase and by endpoint.

    python3 tests/perf/sync_host_measure.py --generated 300 --abs --out build/sync/NAME.json
    python3 tests/perf/sync_host_measure.py --url http://127.0.0.1:4533 --user q2 --password q2test

`--generated N` starts the pinned official Navidrome image with N generated
albums of three tagged FLAC tracks, on its own internal Podman network;
`--abs` adds the generated Audiobookshelf of
tests/e2e/audiobookshelf-server.py. `--url`, `--user` and `--password` name
a Navidrome that already holds a generated library (tests/e2e/README.md).
The player under test gets placeholder credentials; the proxy adds the
server's. Request and byte counts are exact for the server version and
library used; times are descriptive. A run is a measurement, not a
comparison: to compare two source trees, run each with `--tree` against the
same server. Output is numbers only.
"""
import argparse
import collections
import contextlib
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import struct
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]
TEST = 'library::music_catalog::owner_measure::measure_sync_through_proxy'
WORDS = {'api', 'rest', 'libraries', 'items', 'cover', 'personalized', 'series', 'collections', 'playlists', 'authors',
         'narrators', 'filterdata', 'recent-episodes', 'search', 'me', 'ping'}
LINE = re.compile(r'^sync-measure (\S+) (.*)$')
SUBSONIC_AUTH = ('u', 'p', 't', 's', 'apiKey', 'c')
FORWARDED_HEADERS = ('accept', 'accept-encoding', 'content-type', 'if-none-match', 'range', 'if-range')
RETURNED_HEADERS = ('Content-Type', 'Content-Encoding', 'ETag', 'Content-Range', 'Accept-Ranges', 'Last-Modified')
# Navidrome 0.64.2.
NAVIDROME = 'docker.io/deluan/navidrome@sha256:38dc2727bfcfd5ede290f8ada114fc90368146f265ae4701ddddbcbe2a44ee52'
GENERATED_USER, GENERATED_PASSWORD = 'generated', 'generated-password-only'
TRACKS = 3


class Upstream:
    """Where requests go and how they are authenticated (values never printed)."""

    def __init__(self, nd_url, nd_params, abs_url=None, abs_token=None):
        self.nd_url, self.nd_params = nd_url.rstrip('/'), nd_params
        self.abs_url, self.abs_token = abs_url and abs_url.rstrip('/'), abs_token


def subsonic_params(user, password):
    """Fresh token authentication per request."""
    def params():
        salt = secrets.token_hex(8)
        return dict(u=user, t=hashlib.md5((password + salt).encode()).hexdigest(), s=salt, v='1.16.1',
                    c='q2measure', f='json')
    return params


def kind_of(path: str) -> str:
    """The endpoint without ids: `rest/getAlbumList2`, `api/libraries/*/items`."""
    parts = [p for p in path.split('?')[0].split('/') if p]
    if parts[:1] == ['rest']:
        return 'rest/' + (parts[1].removesuffix('.view') if len(parts) > 1 else '')
    return '/'.join(p if p in WORDS else '*' for p in parts)


def new_phase():
    return {'requests': 0, 'status': collections.Counter(), 'body_bytes': 0, 'upstream_s': 0.0,
            'kinds': collections.Counter(), 'kind_bytes': collections.Counter()}


class Stats:
    """Counts by phase; the test moves the phase with GET /__phase/NAME."""

    def __init__(self):
        self.lock = threading.Lock()
        self.phase = 'setup'
        self.marks = []
        self.phases = collections.defaultdict(new_phase)

    def mark(self, name):
        with self.lock:
            self.phase = name
            self.marks.append((name, time.monotonic()))

    def count(self, path, status, size, elapsed):
        with self.lock:
            row = self.phases[self.phase]
            row['requests'] += 1
            row['status'][str(status)] += 1
            row['body_bytes'] += size
            row['upstream_s'] += elapsed
            row['kinds'][kind_of(path)] += 1
            row['kind_bytes'][kind_of(path)] += size

    def summary(self, player=None):
        """Per phase between marks; the `setup` and `end` phases are left out."""
        player = player or {}
        with self.lock:
            walls = {name: round(end - start, 2) for (name, start), (_, end) in zip(self.marks, self.marks[1:])}
            return {name: {'requests': row['requests'], 'status': dict(row['status']), 'body_bytes': row['body_bytes'],
                           'upstream_s': round(row['upstream_s'], 2), 'wall_s': walls.get(name),
                           'by_endpoint': {k: [row['kinds'][k], row['kind_bytes'][k]] for k in sorted(row['kinds'])},
                           'player': player.get(name, {})}
                    for name, row in self.phases.items() if name not in ('setup', 'end')}


def make_handler(upstream: Upstream, stats: Stats):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    class Proxy(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def log_message(self, *_):
            pass

        def do_GET(self):
            self.forward('GET')

        def do_POST(self):
            self.forward('POST')

        def do_PATCH(self):
            self.forward('PATCH')

        def empty(self, status):
            self.send_response(status)
            self.send_header('Content-Length', '0')
            self.end_headers()

        def forward(self, method):
            if self.path.startswith('/__phase/'):
                stats.mark(self.path.split('/')[2])
                self.empty(200)
                return
            length = int(self.headers.get('Content-Length') or 0)
            body = self.rfile.read(length) if length else None
            parts = urllib.parse.urlsplit(self.path)
            headers = {k: v for k, v in self.headers.items() if k.lower() in FORWARDED_HEADERS}
            if parts.path.startswith('/rest/'):
                query = [(k, v) for k, v in urllib.parse.parse_qsl(parts.query, keep_blank_values=True)
                         if k not in SUBSONIC_AUTH]
                query += list(upstream.nd_params().items())
                url = upstream.nd_url + parts.path + '?' + urllib.parse.urlencode(query)
            elif upstream.abs_url:
                url = upstream.abs_url + parts.path + (('?' + parts.query) if parts.query else '')
                headers['Authorization'] = 'Bearer ' + upstream.abs_token
            else:
                self.empty(404)
                return
            started = time.monotonic()
            request = urllib.request.Request(url, data=body, method=method, headers=headers)
            try:
                with opener.open(request, timeout=120) as reply:
                    status, returned, data = reply.status, reply.headers, reply.read()
            except urllib.error.HTTPError as e:
                status, returned, data = e.code, e.headers, e.read()
            except (OSError, urllib.error.URLError) as e:
                status, returned, data = 502, {}, b''
                print(f'proxy: upstream request failed ({type(e).__name__})', file=sys.stderr)
            stats.count(parts.path, status, len(data), time.monotonic() - started)
            self.send_response(status)
            for k in RETURNED_HEADERS:
                if returned and returned.get(k):
                    self.send_header(k, returned.get(k))
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
    return Proxy


def start_proxy(upstream, stats):
    proxy = http.server.ThreadingHTTPServer(('127.0.0.1', 0), make_handler(upstream, stats))
    proxy.daemon_threads = True
    threading.Thread(target=proxy.serve_forever, daemon=True).start()
    return proxy, f'http://127.0.0.1:{proxy.server_address[1]}'


def player_lines(text):
    """The test's `sync-measure NAME key=value ...` lines."""
    player = {}
    for line in text.splitlines():
        m = LINE.match(line)
        if m:
            player[m.group(1)] = {k: int(v) for k, v in (kv.split('=') for kv in m.group(2).split())}
    return player


def load_journey():
    """tests/e2e/audiobookshelf-server.py: the generated Audiobookshelf and the owned-container cleanup."""
    spec = importlib.util.spec_from_file_location('audiobookshelf_server', ROOT / 'tests/e2e/audiobookshelf-server.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def tagged_flac(base: bytes, tags: dict) -> bytes:
    """BASE (a FLAC file) with its metadata replaced by STREAMINFO and these Vorbis comments."""
    if base[:4] != b'fLaC':
        raise ValueError('not a FLAC file')
    at, streaminfo = 4, None
    while True:
        header = base[at]
        length = int.from_bytes(base[at + 1:at + 4], 'big')
        if header & 0x7f == 0:
            streaminfo = base[at + 4:at + 4 + length]
        at += 4 + length
        if header & 0x80:
            break
    vendor = b'q2 generated'
    entries = [f'{key}={value}'.encode() for key, value in tags.items()]
    comment = (struct.pack('<I', len(vendor)) + vendor + struct.pack('<I', len(entries))
               + b''.join(struct.pack('<I', len(e)) + e for e in entries))
    return (b'fLaC' + bytes([0]) + len(streaminfo).to_bytes(3, 'big') + streaminfo
            + bytes([0x84]) + len(comment).to_bytes(3, 'big') + comment + base[at:])


def generate_music(music: Path, albums: int, scratch: Path):
    """ALBUMS albums of TRACKS one-second tracks, 97 artists, tagged per file."""
    base = scratch / 'base.flac'
    subprocess.run(['ffmpeg', '-v', 'error', '-nostdin', '-n', '-f', 'lavfi', '-i', 'sine=frequency=440:duration=1',
                    '-ac', '1', '-ar', '8000', '-c:a', 'flac', str(base)], check=True, timeout=30)
    data = base.read_bytes()
    for n in range(albums):
        artist, album = f'Synthetic Artist {n % 97:02d}', f'Generated Album {n:05d}'
        folder = music / artist / album
        folder.mkdir(parents=True)
        for track in range(1, TRACKS + 1):
            (folder / f'{track:02d} Generated Song {n:05d}-{track}.flac').write_bytes(tagged_flac(data, {
                'ARTIST': artist, 'ALBUMARTIST': artist, 'ALBUM': album, 'TITLE': f'Generated Song {n:05d}-{track}',
                'TRACKNUMBER': track, 'DATE': 2000 + n % 25, 'GENRE': 'Generated'}))


@contextlib.contextmanager
def generated_navidrome(out: Path, albums: int):
    """The pinned Navidrome scanning ALBUMS generated albums; yields its URL."""
    journey = load_journey()
    music, data = out / 'music', out / 'data'
    data.mkdir(parents=True)
    generate_music(music, albums, out)
    port = journey.free_port()
    nonce = uuid.uuid4().hex
    container, network = f'q2-sync-nd-{nonce}', f'q2-sync-nd-net-{nonce}'
    network_created = False
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    url = f'http://127.0.0.1:{port}'

    def api(endpoint):
        query = urllib.parse.urlencode({**subsonic_params(GENERATED_USER, GENERATED_PASSWORD)(), 'f': 'json'})
        with opener.open(f'{url}/rest/{endpoint}?{query}', timeout=10) as reply:
            return json.load(reply)['subsonic-response']
    try:
        subprocess.run(['podman', 'network', 'create', '--internal', network], check=True, timeout=30,
                       stdout=subprocess.DEVNULL)
        network_created = True
        subprocess.run(['podman', 'run', '--rm', '-d', '--name', container, '--network', network,
                        '--publish', f'127.0.0.1:{port}:4533', '--label', f'q2.fixture.owner={nonce}',
                        '--userns=keep-id', '--user', f'{os.getuid()}:{os.getgid()}',
                        '-e', 'ND_SCANSCHEDULE=0', '-e', 'ND_ENABLEINSIGHTS=false',
                        '-e', 'ND_ENABLEEXTERNALSERVICES=false',
                        '-v', f'{music}:/music:ro,Z', '-v', f'{data}:/data:Z', NAVIDROME],
                       check=True, timeout=300, stdout=subprocess.DEVNULL)
        deadline = time.monotonic() + 60
        while True:
            try:
                request = urllib.request.Request(f'{url}/auth/createAdmin', method='POST', data=json.dumps(
                    {'username': GENERATED_USER, 'password': GENERATED_PASSWORD}).encode(),
                    headers={'Content-Type': 'application/json'})
                with opener.open(request, timeout=5) as reply:
                    reply.read()
                break
            except (OSError, urllib.error.URLError):
                if time.monotonic() > deadline:
                    raise ValueError('generated Navidrome did not start')
                time.sleep(0.2)
        api('startScan')
        deadline = time.monotonic() + 300
        while True:
            status = api('getScanStatus')['scanStatus']
            if not status['scanning'] and status.get('count') == albums * TRACKS:
                break
            if time.monotonic() > deadline:
                raise ValueError(f'generated Navidrome scanned {status.get("count")} of {albums * TRACKS} tracks')
            time.sleep(0.5)
        yield url
    finally:
        journey.cleanup(out, container, network, nonce, network_created)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--generated', type=int, metavar='ALBUMS', help='start Navidrome with this many generated albums')
    ap.add_argument('--abs', action='store_true', help='also start the generated Audiobookshelf and sync it')
    ap.add_argument('--url', help='a Navidrome that already holds a generated library')
    ap.add_argument('--user')
    ap.add_argument('--password')
    ap.add_argument('--tree', type=Path, default=ROOT, help='the source tree whose test runs')
    ap.add_argument('--label', default='run')
    ap.add_argument('--out', type=Path, help='write the summary JSON here')
    args = ap.parse_args()
    if bool(args.generated) == bool(args.url and args.user and args.password):
        ap.error('give --generated ALBUMS, or --url, --user and --password of a generated server')
    tree = args.tree.resolve()
    work = tree / 'build' / 'sync' / f'servers-{args.label}-{uuid.uuid4().hex[:8]}'
    with contextlib.ExitStack() as stack:
        if args.generated:
            nd = stack.enter_context(generated_navidrome(work / 'navidrome', args.generated))
            upstream = Upstream(nd, subsonic_params(GENERATED_USER, GENERATED_PASSWORD))
        else:
            upstream = Upstream(args.url, subsonic_params(args.user, args.password))
        if args.abs:
            (work / 'audiobookshelf').mkdir(parents=True)
            shelf = stack.enter_context(load_journey().generated_server(work / 'audiobookshelf'))
            upstream.abs_url, upstream.abs_token = shelf.base, shelf.key
        code, summary = measure(tree, upstream, args)
    if code == 0:
        shutil.rmtree(work, ignore_errors=True)
    else:
        summary['servers'] = str(work)
    text = json.dumps(summary, indent=1)
    print(text)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + '\n')
    return 0 if code == 0 else 1


def measure(tree, upstream, args):
    """Runs the test through the proxy; returns its exit status and the summary."""
    stats = Stats()
    proxy, base = start_proxy(upstream, stats)
    env = {**os.environ, 'Q2ND_SYNC_PROXY': base, 'Q2ND_SYNC_ABS': '1' if args.abs else '0'}
    log = tree / 'build' / 'sync' / f'host-measure-{args.label}.log'
    log.parent.mkdir(parents=True, exist_ok=True)
    command = ['cargo', 'test', '-p', 'q2nd', '--locked', '--offline', '--', '--ignored', '--exact', '--nocapture', TEST]
    try:
        with log.open('w') as out:
            code = subprocess.run(command, cwd=tree, env=env, stdout=out, stderr=subprocess.STDOUT).returncode
    finally:
        proxy.shutdown()
        proxy.server_close()
        # The test removes its card database when it ends; a stopped run leaves it.
        for left in (tree / 'build' / 'perf').glob('sync-measure-*'):
            shutil.rmtree(left, ignore_errors=True)
    player = player_lines(log.read_text(errors='replace'))
    summary = {'label': args.label, 'test_exit': code,
               'navidrome': f'{NAVIDROME}, {args.generated} generated albums' if args.generated else 'generated library at --url',
               'audiobookshelf': 'generated (tests/e2e/audiobookshelf-server.py)' if args.abs else None,
               'phases': stats.summary(player)}
    # The verification lines (abs-verify-KIND) compare stored pages with the server's.
    summary['checks'] = {name: values for name, values in player.items() if name not in summary['phases']}
    if code == 0 and not player:
        summary['test_exit'] = code = 1
        summary['error'] = f'the test printed no sync-measure lines; see {log}'
    return code, summary


if __name__ == '__main__':
    sys.exit(main())
