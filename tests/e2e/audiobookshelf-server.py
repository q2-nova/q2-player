#!/usr/bin/env python3
"""Run the player's spoken-word checks against the official Audiobookshelf image.

Starts the pinned Audiobookshelf container on its own internal Podman network
with generated data only: a two-file AAC book and a podcast whose feed, episode
and covers are served inside the container. The player's `--bench`
`spoken-server` check then lists, browses, streams, saves progress, downloads,
reopens offline and loads covers through the server. Only the container and
network this run creates are inspected or removed.
"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import socketserver
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tests/ui'))
from ux_fault_fixture import generated_png  # noqa: E402

# Audiobookshelf v2.37.2.
IMAGE = 'docker.io/advplyr/audiobookshelf@sha256:37c2057e150694970442579815c646fe6bdc01c95b9d1547c4c9068be8844ef3'
FEED_PORT = 9000
PUBLISHED = 'Tue, 03 Nov 2026 10:00:00 GMT'
FEED_SERVER = (
    "const http=require('http'),fs=require('fs');"
    "const files={'/episode.mp3':['generated-episode.mp3','audio/mpeg'],"
    "'/book.png':['generated-book-cover.png','image/png'],'/podcast.png':['generated-podcast-cover.png','image/png']};"
    "http.createServer((req,res)=>{const [name,type]=files[req.url]||['generated-feed.xml','application/rss+xml'];"
    "res.setHeader('Content-Type',type);res.end(fs.readFileSync('/fixture/'+name));})"
    f".listen({FEED_PORT},'127.0.0.1');")


def run(command, log, timeout, env=None):
    with log.open('ab') as output:
        try:
            subprocess.run([str(part) for part in command], cwd=ROOT, env=env, stdout=output,
                           stderr=subprocess.STDOUT, timeout=timeout, check=True)
        except subprocess.SubprocessError as exc:
            raise ValueError(f'{Path(str(command[0])).name} failed; see {log}') from exc


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def generate(out):
    """Book files, episode, feed and covers; returns the episode size."""
    book = out / 'books' / 'Synthetic Author' / 'Generated Book'
    book.mkdir(parents=True)
    for part, frequency in ((1, 550), (2, 660)):
        run(['ffmpeg', '-v', 'error', '-n', '-f', 'lavfi', '-i', f'sine=frequency={frequency}:duration=15',
             '-metadata', 'title=Generated Book', '-metadata', 'artist=Synthetic Author',
             '-c:a', 'aac', '-b:a', '64k', '-f', 'mp4', book / f'0{part}-Generated Book.m4b'],
            out / 'ffmpeg.log', 30)
    episode = out / 'generated-episode.mp3'
    run(['ffmpeg', '-v', 'error', '-n', '-f', 'lavfi', '-i', 'sine=frequency=733:sample_rate=16000:duration=12',
         '-c:a', 'libmp3lame', '-b:a', '48k', episode], out / 'ffmpeg.log', 30)
    (out / 'generated-book-cover.png').write_bytes(generated_png(128, (24, 140, 220)))
    (out / 'generated-podcast-cover.png').write_bytes(generated_png(128, (220, 90, 24)))
    size = episode.stat().st_size
    (out / 'generated-feed.xml').write_text(
        '<?xml version="1.0"?><rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">'
        '<channel><title>Generated Podcast</title><description>Synthetic episodes</description>'
        f'<link>http://127.0.0.1:{FEED_PORT}/</link><item><title>Generated Episode</title>'
        '<guid>generated-episode-one</guid><itunes:episode>42</itunes:episode><itunes:season>7</itunes:season>'
        f'<pubDate>{PUBLISHED}</pubDate><enclosure url="http://127.0.0.1:{FEED_PORT}/episode.mp3" '
        f'type="audio/mpeg" length="{size}"/></item></channel></rss>')
    (out / 'fixture-feed.cjs').write_text(FEED_SERVER)
    return size


class Socks(socketserver.BaseRequestHandler):
    """SOCKS5 CONNECT to the published server port only; counts connections."""

    def exact(self, count):
        data = b''
        while len(data) < count:
            chunk = self.request.recv(count - len(data))
            if not chunk:
                raise EOFError()
            data += chunk
        return data

    def handle(self):
        self.request.settimeout(20)
        try:
            version, count = self.exact(2)
            if version != 5 or 0 not in self.exact(count):
                return
            self.request.sendall(b'\x05\x00')
            version, command, reserved, kind = self.exact(4)
            if (version, command, reserved) != (5, 1, 0):
                return
            if kind == 1:
                address = socket.inet_ntoa(self.exact(4))
            elif kind == 3:
                address = self.exact(self.exact(1)[0]).decode()
            else:
                return
            port = struct.unpack('!H', self.exact(2))[0]
            if (address, port) != ('127.0.0.1', self.server.target_port):
                self.server.refused += 1
                self.request.sendall(b'\x05\x02\x00\x01\x00\x00\x00\x00\x00\x00')
                return
            with socket.create_connection((address, port), timeout=20) as remote:
                self.request.sendall(b'\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00')
                self.server.connections += 1
                sockets = [self.request, remote]
                while True:
                    ready, _, _ = select.select(sockets, [], [], 15)
                    if not ready:
                        return
                    for source in ready:
                        body = source.recv(32768)
                        if not body:
                            return
                        (remote if source is self.request else self.request).sendall(body)
        except (ConnectionError, EOFError, TimeoutError, OSError):
            return


class Proxy(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, target_port):
        super().__init__(('127.0.0.1', 0), Socks)
        self.target_port, self.connections, self.refused = target_port, 0, 0


class Server:
    """The JSON API of the published container, bypassing any host proxy."""

    def __init__(self, base):
        self.base = base
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.token = None

    def call(self, path, method='GET', body=None):
        headers = {'Content-Type': 'application/json', 'x-return-tokens': 'true'}
        if self.token:
            headers['Authorization'] = f'Bearer {self.token}'
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, headers=headers, method=method)
        with self.opener.open(request, timeout=15) as response:
            raw = response.read()
        try:
            return json.loads(raw)
        except ValueError:
            return raw.decode()

    def wait(self, check, what, seconds=40):
        deadline = time.monotonic() + seconds
        while True:
            try:
                value = check()
                if value:
                    return value
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                pass
            if time.monotonic() > deadline:
                raise ValueError(f'generated server: {what} timed out')
            time.sleep(0.2)


def populate(server, episode_size):
    """Root account, API key, both libraries, the scanned book and one downloaded episode."""
    password = 'generated-password-only'
    server.call('/init', 'POST', {'newRoot': {'username': 'generated', 'password': password}})
    user = server.call('/login', 'POST', {'username': 'generated', 'password': password})['user']
    server.token = user.get('accessToken') or user['token']
    key = server.call('/api/api-keys', 'POST', {'name': 'Generated Q2', 'userId': user['id'],
                                                'isActive': True})['apiKey']['apiKey']
    books = server.call('/api/libraries', 'POST', {'name': 'Generated Books', 'folders': [{'fullPath': '/books'}],
                                                   'mediaType': 'book', 'icon': 'audiobookshelf',
                                                   'provider': 'audible'})
    podcasts = server.call('/api/libraries', 'POST', {'name': 'Generated Podcasts',
                                                      'folders': [{'fullPath': '/podcasts'}],
                                                      'mediaType': 'podcast', 'icon': 'podcast',
                                                      'provider': 'itunes'})
    server.call(f'/api/libraries/{books["id"]}/scan', 'POST', {})
    podcast = server.call('/api/podcasts', 'POST', {
        'libraryId': podcasts['id'], 'folderId': podcasts['folders'][0]['id'],
        'path': '/podcasts/Generated Podcast',
        'media': {'metadata': {'title': 'Generated Podcast', 'feedUrl': f'http://127.0.0.1:{FEED_PORT}/feed.xml'},
                  'autoDownloadEpisodes': False}})
    server.call(f'/api/podcasts/{podcast["id"]}/download-episodes', 'POST', [{
        'title': 'Generated Episode', 'guid': 'generated-episode-one', 'episode': '42', 'season': '7',
        'pubDate': PUBLISHED, 'enclosure': {'url': f'http://127.0.0.1:{FEED_PORT}/episode.mp3',
                                            'type': 'audio/mpeg', 'length': episode_size}}])

    def scanned():
        items = server.call(f'/api/libraries/{books["id"]}/items')['results']
        episodes = server.call(f'/api/items/{podcast["id"]}?expanded=1')['media'].get('episodes')
        return (items, episodes) if items and episodes else None
    items, episodes = server.wait(scanned, 'book scan and episode download')
    if episodes[0].get('episode') != '42':
        raise ValueError('generated episode lost its episode number')
    book = items[0]
    # uploadCover takes a URL; both images come from the container's own feed server.
    for item, image in ((book['id'], 'book'), (podcast['id'], 'podcast')):
        uploaded = server.call(f'/api/items/{item}/cover', 'POST', {'url': f'http://127.0.0.1:{FEED_PORT}/{image}.png'})
        if uploaded.get('success') is not True:
            raise ValueError(f'{image} cover upload refused')
    server.call(f'/api/items/{book["id"]}/media', 'PATCH', {'metadata': {
        'description': '<p>Generated summary &amp; compatible metadata.</p>',
        'narrators': ['Generated Narrator'], 'series': [{'name': 'Generated Saga', 'sequence': '2'}],
        'publisher': 'Generated Press'}})
    return key, books['id'], podcasts['id'], {'episode': episodes[0].get('episode'),
                                              'season': episodes[0].get('season')}


def check(parsed):
    """The player's report against the generated libraries."""
    result = parsed['result']
    if parsed.get('error') is not None:
        raise ValueError(f'player check failed: {parsed["error"]}')
    if result['episodes'] != 1 or len(result['outcomes']) != 2:
        raise ValueError('player did not see one episode and two spoken items')
    for outcome in result['outcomes']:
        if not (outcome['book_info_cached_offline'] and outcome['reopened_offline'] and outcome['offline_matches']):
            raise ValueError('offline reopen or download check failed')
        if outcome['offline_cover_sizes'] != [50, 150, 320] or not outcome['book_info']['title']:
            raise ValueError('offline covers or title missing')
        if not outcome['podcast']:
            info = outcome['book_info']
            if outcome['physical_files'] != 2:
                raise ValueError('book did not open as two files')
            if (info['narrators'], info['series'], info['description']) != (
                    ['Generated Narrator'], ['Generated Saga #2'], 'Generated summary & compatible metadata.'):
                raise ValueError(f'book information differs: {info}')
    browser = result['browser']
    if len(browser) != 8 or not all(row['entries'] > 0 for row in browser if not row.get('empty_allowed')):
        raise ValueError('spoken browser pages missing')


def owned(container, nonce):
    """Whether this run's container exists; refuses one whose label changed."""
    exists = subprocess.run(['podman', 'container', 'exists', container], timeout=15).returncode
    if exists == 1:
        return False
    if exists != 0:
        raise ValueError('could not check the generated container')
    label = subprocess.check_output(['podman', 'inspect', '--format',
                                     '{{index .Config.Labels "q2.fixture.owner"}}', container],
                                    text=True, timeout=15).strip()
    if label != nonce:
        raise ValueError('generated container label changed; not touching it')
    return True


def cleanup(out, container, network, nonce, network_created):
    if owned(container, nonce):
        with (out / 'server.log').open('w') as log:
            subprocess.run(['podman', 'logs', container], stdout=log, stderr=log, timeout=15)
        subprocess.run(['podman', 'rm', '-f', container], check=True, timeout=30, stdout=subprocess.DEVNULL)
        if owned(container, nonce):
            raise ValueError('generated container was not removed')
    if network_created:
        subprocess.run(['podman', 'network', 'rm', network], check=True, timeout=20, stdout=subprocess.DEVNULL)
        if subprocess.run(['podman', 'network', 'exists', network], timeout=15).returncode != 1:
            raise ValueError('generated network was not removed')


def sha256(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


@contextlib.contextmanager
def generated_server(out):
    """The pinned server with generated libraries, on its own internal network.

    Yields the Server with `port`, `key` (an API key), `books` and `podcasts`
    (library ids), `episode` and `version`. Writes the generated data and
    `server.log` under OUT and removes the container and network on exit.
    """
    for name in ('config', 'metadata', 'podcasts'):
        (out / name).mkdir()
    episode_size = generate(out)
    port = free_port()
    nonce = uuid.uuid4().hex
    container, network = f'q2-abs-journey-{nonce}', f'q2-abs-journey-net-{nonce}'
    network_created = False
    try:
        subprocess.run(['podman', 'network', 'create', '--internal', network], check=True, timeout=30,
                       stdout=subprocess.DEVNULL)
        network_created = True
        # A port another process took makes this fail before any request.
        subprocess.run(['podman', 'run', '--rm', '-d', '--name', container, '--network', network,
                        '--publish', f'127.0.0.1:{port}:13378', '--label', f'q2.fixture.owner={nonce}',
                        '--userns=keep-id', '--user', f'{os.getuid()}:{os.getgid()}',
                        '-e', 'PORT=13378', '-e', 'HOST=0.0.0.0', '-e', 'SSRF_REQUEST_FILTER_WHITELIST=127.0.0.1',
                        '-v', f'{out}:/fixture:ro,Z',
                        *[arg for name in ('config', 'metadata', 'books', 'podcasts')
                          for arg in ('-v', f'{out / name}:/{name}:Z')], IMAGE],
                       check=True, timeout=300, stdout=subprocess.DEVNULL)
        subprocess.run(['podman', 'exec', '-d', container, 'node', '/fixture/fixture-feed.cjs'],
                       check=True, timeout=15, stdout=subprocess.DEVNULL)
        server = Server(f'http://127.0.0.1:{port}')
        server.port = port
        server.version = server.wait(lambda: server.call('/status'), 'start').get('serverVersion')
        server.key, server.books, server.podcasts, server.episode = populate(server, episode_size)
        yield server
    finally:
        cleanup(out, container, network, nonce, network_created)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True, help="fresh directory under the checkout's build/")
    parser.add_argument('--player', type=Path, help='an already built q2nd (default: build it)')
    parser.add_argument('--qemu-user', type=Path, help='run a MIPS q2nd under this qemu-mipsel')
    parser.add_argument('--sysroot', type=Path, help='the sysroot for --qemu-user')
    args = parser.parse_args()
    if bool(args.qemu_user) != bool(args.sysroot):
        parser.error('--qemu-user and --sysroot go together')
    if args.qemu_user and not args.player:
        parser.error('--qemu-user needs --player')
    out = args.out.resolve()
    if not out.is_relative_to(ROOT / 'build'):
        parser.error("--out must be under the checkout's build directory")
    out.mkdir(parents=True)

    player = args.player
    if player is None:
        temporary = out / 'tmp'
        temporary.mkdir()
        target = ROOT / 'build/first-listen-target'
        env = {**os.environ, 'TMPDIR': str(temporary), 'CARGO_PROFILE_DEV_DEBUG': '0'}
        run(['cargo', 'build', '--locked', '-p', 'q2nd', '-j', '2', '--target-dir', target],
            out / 'build.log', 1200, env)
        player = target / 'debug/q2nd'
    player = player.resolve()
    prefix = [str(args.qemu_user.resolve()), '-cpu', 'P5600', '-L', str(args.sysroot.resolve())] if args.qemu_user else []

    (out / 'player').mkdir()
    with generated_server(out) as server:
        proxy = Proxy(server.port)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        try:
            specification = out / 'player-fixture.json'
            specification.write_text(json.dumps({'kind': 'spoken-server', 'scratch': str(out / 'player'),
                                                 'url': server.base, 'api_token': server.key,
                                                 'proxy': f'socks5://127.0.0.1:{proxy.server_address[1]}'}))
            env = {'RUST_LOG': 'q2nd=info', 'PATH': os.environ.get('PATH', '/usr/bin:/bin')}
            with (out / 'player.log').open('w') as log:
                completed = subprocess.run([*prefix, str(player), '--bench', str(specification)],
                                           stdout=subprocess.PIPE, stderr=log, text=True, timeout=120, env=env)
            if completed.returncode:
                raise ValueError(f'player check exited {completed.returncode}; see {out / "player.log"}')
            parsed = json.loads(completed.stdout)
            check(parsed)
            wire = {name: server.call(path) for name, path in (
                ('books', f'/api/libraries/{server.books}/items?page=0&limit=50&minified=1'),
                ('podcasts', f'/api/libraries/{server.podcasts}/items?page=0&limit=50&minified=1'),
                ('recent_episodes', f'/api/libraries/{server.podcasts}/recent-episodes?page=0&limit=50'))}
            if not all(body.get('page') == 0 and body.get('limit') == 50 for body in wire.values()):
                raise ValueError('server pages are not numeric page/limit')
            if proxy.connections == 0 or proxy.refused:
                raise ValueError('player did not reach the server through the generated proxy only')
        finally:
            proxy.shutdown()
            proxy.server_close()
        parsed['server'] = {'image': IMAGE, 'version': server.version, 'episode': server.episode,
                            'proxy_connections': proxy.connections}
    parsed['player_sha256'] = sha256(player)
    (out / 'result.json').write_text(json.dumps(parsed, indent=2) + '\n')
    print(f'Audiobookshelf {parsed["server"]["version"]}: {len(parsed["result"]["outcomes"])} spoken items '
          f'streamed, downloaded and reopened offline. Receipts: {out}')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as exc:
        raise SystemExit(f'Audiobookshelf journey failed: {exc}') from exc
