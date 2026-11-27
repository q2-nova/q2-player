#!/usr/bin/env python3
"""Fault-injecting generated Navidrome (Subsonic) and Audiobookshelf servers.

Two listeners on 127.0.0.1 (or ``host``), one per service, serving a generated library: one
album of three WAV songs and one single-file WAV book. Everything is
synthetic; no owner configuration, database rows or logs are read.

Faults are set per service while the player runs:

- ``ok``: normal answers.
- ``down``: the listener is closed (connection refused), as for a stopped server.
- ``status``: every API answer is the given HTTP status (5xx).
- ``delay``: response headers wait ``seconds`` (slow server).
- ``stall``: media bodies stop after ``after`` bytes and new requests wait
  without an answer for as long as the fault lasts, as when the link drops
  (no RST reaches the client).
- ``reset``: media bodies end with a TCP reset after ``after`` bytes and new
  connections are reset until the fault is cleared.
- ``garbage``: answers 200 with bytes that are not the expected content.
- ``slow``: a loaded proxy: every answer waits
  ``seconds`` before its headers, and of the requests whose path matches
  ``paths`` (all when empty) every ``drop_every``-th is never answered and
  reset after ``hold`` seconds; every request matching ``drop_paths`` is
  dropped. ``ping`` is slowed like the rest but never dropped unless
  ``paths`` or ``drop_paths`` names it.

``albums`` (Fixture) adds generated albums ``generated-album-N``, each with
its own cover, after the first, for Cover Flow over a large library.

``media_only`` limits a fault to stream/download/file requests; ``paths``
limits it to request paths containing one of the given strings.
"""
import io
import json
import math
import socket
import struct
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit
import wave
import zlib

SONG_SECONDS = 40
BOOK_SECONDS = 120
RATE = 22050


def generated_wav(seconds, frequency):
    """A quiet sine: decodable, non-silent, small."""
    stream = io.BytesIO()
    with wave.open(stream, 'wb') as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(RATE)
        period = RATE / frequency
        cycle = b''.join(struct.pack('<h', int(3000 * math.sin(2 * math.pi * i / period)))
                         for i in range(int(period * 50)))
        frames = seconds * RATE * 2
        audio.writeframes((cycle * (frames // len(cycle) + 1))[:frames])
    return stream.getvalue()


def generated_png(size=64, rgb=(70, 120, 180)):
    """A solid-colour PNG cover."""
    def chunk(kind, data):
        body = kind + data
        return struct.pack('>I', len(data)) + body + struct.pack('>I', zlib.crc32(body) & 0xffffffff)
    row = b'\x00' + bytes(rgb) * size
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', size, size, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', zlib.compress(row * size)) + chunk(b'IEND', b''))


class Fault:
    def __init__(self, kind='ok', **options):
        self.kind = kind
        self.status = options.get('status', 503)
        self.seconds = options.get('seconds', 0.0)
        self.after = options.get('after', 0)
        self.media_only = options.get('media_only', False)
        self.paths = tuple(options.get('paths', ()))
        self.drop_every = options.get('drop_every', 0)
        self.drop_paths = tuple(options.get('drop_paths', ()))
        self.hold = options.get('hold', 30.0)
        self.since = time.monotonic()
        self.matched = 0
        self.lock = threading.Lock()

    def drops(self, path):
        """For ``slow``: whether this request is one that is never answered."""
        if any(p in path for p in self.drop_paths):
            return True
        if not self.drop_every or (not self.paths and '/ping' in path):
            return False
        if self.paths and not any(p in path for p in self.paths):
            return False
        with self.lock:
            self.matched += 1
            return self.matched % self.drop_every == 0

    def applies(self, path, media):
        if self.kind == 'ok':
            return False
        if self.kind == 'slow':
            return True
        if self.media_only and not media:
            return False
        return not self.paths or any(p in path for p in self.paths)

    def describe(self):
        return {'fault': self.kind, 'status': self.status, 'seconds': self.seconds, 'after': self.after,
                'media_only': self.media_only, 'paths': list(self.paths), 'drop_every': self.drop_every}


class Service:
    """One fake server whose listener can be closed and reopened on its port."""

    def __init__(self, name, library, port=0, pace=0, host='127.0.0.1'):
        self.name = name
        self.host = host
        self.library = library
        self.pace = pace  # media bytes per second; 0 unpaced
        self.fault = Fault()
        self.cleared = threading.Event()
        self.cleared.set()
        self.lock = threading.Lock()
        self.server = None
        self.thread = None
        self.port = port
        self.start()

    def start(self):
        handler = self.library.handler(self)
        server = ThreadingHTTPServer((self.host, self.port), handler)
        server.daemon_threads = True
        self.port = server.server_port
        self.server = server
        self.thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.05}, daemon=True)
        self.thread.start()

    def stop_listening(self):
        if self.server:
            self.server.shutdown()
            self.server.server_close()
            self.thread.join(timeout=5)
            self.server = None

    def set(self, kind='ok', **options):
        with self.lock:
            previous = self.fault.kind
            self.fault = Fault(kind, **options)
            if kind in ('stall', 'reset'):
                self.cleared.clear()
            else:
                self.cleared.set()
        self.library.event(self.name, 'fault', **self.fault.describe())
        if kind == 'down' and previous != 'down':
            self.stop_listening()
        elif kind != 'down' and previous == 'down':
            self.start()

    def current(self):
        with self.lock:
            return self.fault

    def close(self):
        self.cleared.set()
        self.stop_listening()


class Library:
    """Generated catalog and the request handler shared by both services."""

    def __init__(self, albums=1):
        self.started = time.monotonic()
        self.songs = [{'id': f'generated-song-{n}', 'title': f'Generated Song {n}', 'album': 'Generated Album',
                       'albumId': 'generated-album', 'artist': 'Synthetic Artist', 'artistId': 'generated-artist',
                       'track': n, 'coverArt': 'generated-album', 'contentType': 'audio/wav', 'suffix': 'wav',
                       'duration': SONG_SECONDS, 'isDir': False, 'type': 'music'} for n in (1, 2, 3)]
        self.audio = {song['id']: generated_wav(SONG_SECONDS, 330 + 110 * n) for n, song in enumerate(self.songs)}
        for song in self.songs:
            song['size'] = len(self.audio[song['id']])
        self.album = {'id': 'generated-album', 'name': 'Generated Album', 'artist': 'Synthetic Artist',
                      'artistId': 'generated-artist', 'coverArt': 'generated-album', 'songCount': 3,
                      'duration': 3 * SONG_SECONDS}
        self.albums = [self.album] + [
            {'id': f'generated-album-{n:05d}', 'name': f'Generated Album {n:05d}', 'artist': f'Synthetic Artist {n % 97:02d}',
             'artistId': 'generated-artist', 'coverArt': f'generated-album-{n:05d}', 'songCount': 3,
             'duration': 3 * SONG_SECONDS} for n in range(1, albums)]
        self.covers = {}
        self.book_audio = generated_wav(BOOK_SECONDS, 250)
        self.book = {'id': 'generated-book', 'libraryId': 'generated-books', 'mediaType': 'book',
                     'media': {'metadata': {'title': 'Generated Book', 'authorName': 'Synthetic Author',
                                            'authors': [{'id': 'generated-author', 'name': 'Synthetic Author'}],
                                            'description': 'Generated fault-journey book.'},
                               'coverPath': 'generated-cover', 'duration': BOOK_SECONDS, 'numAudioFiles': 1,
                               'numTracks': 1, 'numChapters': 1,
                               'audioFiles': [{'ino': 'generated-ino', 'index': 1, 'duration': BOOK_SECONDS,
                                               'metadata': {'filename': 'book.wav', 'ext': '.wav',
                                                            'size': len(self.book_audio)},
                                               'mimeType': 'audio/wav'}],
                               'chapters': [{'id': 0, 'start': 0, 'end': BOOK_SECONDS, 'title': 'Generated Chapter'}]}}
        self.cover = generated_png()
        self.events = []
        self.lock = threading.Lock()

    def event(self, service, kind, **fields):
        with self.lock:
            self.events.append({'at_s': round(time.monotonic() - self.started, 3), 'service': service,
                                'kind': kind, **fields})

    def count(self, kind, service=None, path=None):
        with self.lock:
            return sum(1 for e in self.events if e['kind'] == kind and (service is None or e['service'] == service)
                       and (path is None or path in e.get('path', '')))

    def media_sent(self, service):
        with self.lock:
            return sum(e.get('bytes', 0) for e in self.events if e['service'] == service and e['kind'] == 'media')

    # ---- Subsonic
    def subsonic(self, route, query):
        if route in ('ping', 'scrobble', 'savePlayQueue', 'star', 'unstar', 'setRating', 'startScan'):
            payload = {}
        elif route == 'getScanStatus':
            payload = {'scanStatus': {'scanning': False, 'count': len(self.songs)}}
        elif route == 'getLicense':
            payload = {'license': {'valid': True}}
        elif route == 'getOpenSubsonicExtensions':
            payload = {'openSubsonicExtensions': []}
        elif route == 'getAlbumList2':
            offset, size = int(query.get('offset', ['0'])[0]), int(query.get('size', ['10'])[0])
            payload = {'albumList2': {'album': self.albums[offset:offset + size]}}
        elif route == 'getAlbum':
            wanted = query.get('id', ['generated-album'])[0]
            album = next((a for a in self.albums if a['id'] == wanted), self.album)
            payload = {'album': {**album, 'song': self.songs}}
        elif route == 'getSong':
            wanted = query.get('id', [''])[0]
            song = next((s for s in self.songs if s['id'] == wanted), None)
            if song is None:
                return {'subsonic-response': {'status': 'failed', 'version': '1.16.1',
                                              'error': {'code': 70, 'message': 'Song not found'}}}
            payload = {'song': song}
        elif route == 'getRandomSongs':
            payload = {'randomSongs': {'song': self.songs}}
        elif route == 'getArtists':
            payload = {'artists': {'index': [{'name': 'S', 'artist': [{'id': 'generated-artist', 'name': 'Synthetic Artist',
                                                                        'albumCount': 1}]}]}}
        elif route == 'getArtist':
            payload = {'artist': {'id': 'generated-artist', 'name': 'Synthetic Artist', 'album': [self.album]}}
        elif route == 'getPlaylists':
            payload = {'playlists': {'playlist': []}}
        elif route == 'getStarred2':
            payload = {'starred2': {}}
        elif route == 'getGenres':
            payload = {'genres': {'genre': []}}
        elif route == 'getPlayQueue':
            payload = {}
        elif route == 'getInternetRadioStations':
            payload = {'internetRadioStations': {}}
        elif route == 'search3':
            payload = {'searchResult3': {}}
        else:
            return None
        return {'subsonic-response': {'status': 'ok', 'version': '1.16.1', 'type': 'generic',
                                      'openSubsonic': False, **payload}}

    # ---- Audiobookshelf
    def shelf(self, method, path):
        if method == 'GET':
            if path == '/api/me':
                return {'id': 'generated-account', 'username': 'generated', 'mediaProgress': []}
            if path == '/api/libraries':
                return {'libraries': [{'id': 'generated-books', 'name': 'Generated Books', 'mediaType': 'book'}]}
            if path.startswith('/api/libraries/generated-books/items'):
                return {'results': [self.book], 'page': 0, 'limit': 50, 'total': 1}
            if path.startswith('/api/libraries/generated-books/'):
                return {'results': [], 'total': 0, 'personalized': []} if 'personalized' not in path else []
            if path == '/api/items/generated-book':
                return self.book
            if path.startswith('/api/me/progress/'):
                return {'currentTime': 0, 'lastUpdate': 0, 'progress': 0, 'isFinished': False}
            if path.startswith('/api/me/'):
                return {}
        if method == 'POST':
            if path == '/api/items/generated-book/play':
                return {'id': 'generated-session', 'displayTitle': 'Generated Book', 'displayAuthor': 'Synthetic Author',
                        'duration': BOOK_SECONDS, 'currentTime': 0,
                        'chapters': [{'id': 0, 'start': 0, 'end': BOOK_SECONDS, 'title': 'Generated Chapter'}],
                        'audioTracks': [{'index': 1, 'contentUrl': '/api/items/generated-book/file/generated-ino',
                                         'startOffset': 0, 'duration': BOOK_SECONDS, 'mimeType': 'audio/wav',
                                         'metadata': {'ext': '.wav', 'filename': 'book.wav'}}]}
            if path.startswith('/api/session/'):
                return {}
        return None

    def cover_for(self, cover_id):
        """A distinct solid colour per album cover."""
        if cover_id in ('', 'generated-album'):
            return self.cover
        with self.lock:
            if cover_id not in self.covers:
                h = zlib.crc32(cover_id.encode())
                self.covers[cover_id] = generated_png(rgb=(h & 0xff, h >> 8 & 0xff, h >> 16 & 0xff))
            return self.covers[cover_id]

    def handler(self, service):
        library = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *_args):
                pass

            def reset(self):
                try:
                    self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
                except OSError:
                    pass
                self.close_connection = True
                try:
                    self.connection.close()
                except OSError:
                    pass

            def hold(self, fault):
                """No answer while a link-loss fault lasts; then drop the request."""
                service.cleared.wait(timeout=600)
                self.reset()

            def respond(self, body, content='application/json', status=200, head=False, headers=()):
                self.send_response(status)
                self.send_header('Content-Type', content)
                self.send_header('Content-Length', str(len(body)))
                for key, value in headers:
                    self.send_header(key, value)
                self.end_headers()
                if not head:
                    self.wfile.write(body)

            def media(self, data, head):
                total = len(data)
                start, end, status = 0, total - 1, 200
                requested = self.headers.get('Range')
                if requested and requested.startswith('bytes='):
                    first, _, last = requested[6:].partition('-')
                    start = int(first or 0)
                    end = min(int(last), total - 1) if last else total - 1
                    if start >= total:
                        self.respond(b'', status=416, headers=[('Content-Range', f'bytes */{total}')])
                        return
                    status = 206
                self.send_response(status)
                self.send_header('Content-Type', 'audio/wav')
                self.send_header('Content-Length', str(end - start + 1))
                self.send_header('Accept-Ranges', 'bytes')
                if status == 206:
                    self.send_header('Content-Range', f'bytes {start}-{end}/{total}')
                self.end_headers()
                if head:
                    return
                sent = 0
                chunk = 16384
                began = time.monotonic()
                try:
                    at = start
                    while at <= end:
                        fault = service.current()
                        if fault.kind in ('stall', 'reset') and sent >= fault.after:
                            library.event(service.name, 'media-fault', path=self.path.split('?')[0], bytes=sent)
                            if fault.kind == 'stall':
                                self.hold(fault)
                            else:
                                self.reset()
                            return
                        piece = data[at:min(at + chunk, end + 1)]
                        self.wfile.write(piece)
                        at += len(piece)
                        sent += len(piece)
                        if service.pace:
                            ahead = sent / service.pace - (time.monotonic() - began)
                            if ahead > 0:
                                time.sleep(ahead)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
                finally:
                    library.event(service.name, 'media', path=self.path.split('?')[0], bytes=sent,
                                  range=requested or '', complete=sent == end - start + 1)

            def handle_one(self, method):
                url = urlsplit(self.path)
                path = url.path
                if path.startswith('/rest/') and path.endswith('.view'):
                    path = path[:-5]
                query = parse_qs(url.query)
                if method == 'POST':
                    length = int(self.headers.get('Content-Length', '0') or 0)
                    if length:
                        self.rfile.read(min(length, 1 << 20))
                media = path in ('/rest/stream', '/rest/download') or '/file/' in path
                fault = service.current()
                library.event(service.name, 'request', method=method, path=path)
                if fault.applies(path, media):
                    if fault.kind in ('stall', 'reset') and not media:
                        library.event(service.name, 'faulted', path=path, fault=fault.kind)
                        if fault.kind == 'stall':
                            self.hold(fault)
                        else:
                            self.reset()
                        return
                    if fault.kind in ('stall', 'reset') and media and fault.after == 0:
                        library.event(service.name, 'faulted', path=path, fault=fault.kind)
                        if fault.kind == 'stall':
                            self.hold(fault)
                        else:
                            self.reset()
                        return
                    if fault.kind == 'status':
                        library.event(service.name, 'faulted', path=path, fault='status')
                        self.respond(b'<html>Service Unavailable</html>', 'text/html', status=fault.status)
                        return
                    if fault.kind == 'garbage':
                        library.event(service.name, 'faulted', path=path, fault='garbage')
                        self.respond(b'generated bytes that are not an image', 'image/png')
                        return
                    if fault.kind == 'delay':
                        library.event(service.name, 'faulted', path=path, fault='delay')
                        time.sleep(fault.seconds)
                    if fault.kind == 'slow':
                        if fault.drops(path):
                            library.event(service.name, 'faulted', path=path, fault='dropped')
                            time.sleep(fault.hold)
                            self.reset()
                            return
                        time.sleep(fault.seconds)
                head = method == 'HEAD'
                if service.name == 'nd':
                    if path in ('/rest/stream', '/rest/download'):
                        data = library.audio.get(query.get('id', [''])[0])
                        if data is None:
                            self.respond(b'', status=404)
                        else:
                            self.media(data, head)
                        return
                    if path == '/rest/getCoverArt':
                        self.respond(library.cover_for(query.get('id', [''])[0]), 'image/png', head=head)
                        return
                    if path.startswith('/rest/'):
                        value = library.subsonic(path.rsplit('/', 1)[1], query)
                        if value is not None:
                            self.respond(json.dumps(value).encode(), head=head)
                            return
                else:
                    if path == '/api/items/generated-book/file/generated-ino':
                        self.media(library.book_audio, head)
                        return
                    if path.endswith('/cover'):
                        self.respond(library.cover, 'image/png', head=head)
                        return
                    value = library.shelf('GET' if head else method, path)
                    if value is not None:
                        self.respond(json.dumps(value).encode(), head=head)
                        return
                library.event(service.name, 'unknown', method=method, path=path)
                self.respond(b'{}', status=404, head=head)

            def do_GET(self):
                self.handle_one('GET')

            def do_HEAD(self):
                self.handle_one('HEAD')

            def do_POST(self):
                self.handle_one('POST')

            def do_PATCH(self):
                self.handle_one('POST')

        return Handler


class Fixture:
    """Both services; ``pace`` bounds media bytes per second (0: unpaced)."""

    def __init__(self, pace=0, albums=1, host='127.0.0.1'):
        self.library = Library(albums)
        self.nd = Service('nd', self.library, pace=pace, host=host)
        self.abs = Service('abs', self.library, pace=pace, host=host)

    def config(self, nd_url=None, abs_url=None, extra=''):
        nd_url = nd_url or f'http://{self.nd.host}:{self.nd.port}'
        abs_url = abs_url or f'http://{self.abs.host}:{self.abs.port}'
        return (f'[server]\nurl="{nd_url}"\nuser="generated"\npassword="synthetic-only"\n'
                f'[audiobookshelf]\nurl="{abs_url}"\ntoken="generated-ui-token"\n'
                '[display]\nbacklight_seconds=0\nclock_in_title=false\nclicker=false\n'
                '[wifi]\nenabled=false\n[tailscale]\nenabled=false\n[power]\nsleep_minutes=0\n' + extra)

    def close(self):
        self.nd.close()
        self.abs.close()
