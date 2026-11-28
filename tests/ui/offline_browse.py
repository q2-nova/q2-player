#!/usr/bin/env python3
"""Downloaded music browses and plays without Navidrome (MUSIC-003).

The headless player downloads two generated albums of one artist (the whole
artist) and one album of a second artist from a generated Navidrome
(Subsonic) server, then browses Music -> Artists -> artist -> album -> song
and Music -> Albums -> album -> song, and from Now Playing the long-Center
menu's Browse Album and Browse Artist, in four runs on the same card:

- warm-unreachable: the same run, after the server's listener closes;
- warm-offline-mode: a new run with the server up, after Offline Mode is
  turned on;
- cold-unreachable: a new run with the server's listener closed;
- cold-offline-mode: a new run with the server up and Offline Mode turned on
  before anything is opened.

Each run checks that the lists name every downloaded artist and album and
nothing that is not downloaded, that each page opens, and that the chosen
songs play. Offline Mode runs also check that browsing sends no request.
Generated data only. Results: OUT/result.json.

    python3 tests/ui/offline_browse.py [--binary target/debug/q2nd] [--out build/offline-browse/NAME]
"""
import argparse
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import ux_fault_fixture as fixture_module  # noqa: E402

MARKER = b'Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n'
SONG_SECONDS = 12

# Artist, album, year. Alpha is downloaded whole, Beta in part, Gamma not.
CATALOG = (
    ('Generated Artist Alpha', 'Alpha One', 2001),
    ('Generated Artist Alpha', 'Alpha Two', 2002),
    ('Generated Artist Beta', 'Beta One', 2003),
    ('Generated Artist Beta', 'Beta Two', 2004),
    ('Generated Artist Gamma', 'Gamma One', 2005),
)
DOWNLOADED = ('Alpha One', 'Alpha Two', 'Beta One')
BROWSE_ROUTES = ('getArtists', 'getArtist', 'getAlbum', 'getAlbumList2', 'search3')


def slug(text):
    return text.lower().replace(' ', '-')


class CatalogLibrary(fixture_module.Library):
    """The fault fixture's server with a generated catalog of several
    artists and albums, each album with its own two songs."""

    def __init__(self):
        super().__init__()
        self.artists, self.albums, self.songs, self.audio = {}, [], [], {}
        tracks = {}
        for n, (artist, name, year) in enumerate(CATALOG):
            artist_id, album_id = f'generated-{slug(artist)}', f'generated-{slug(name)}'
            self.artists.setdefault(artist_id, {'id': artist_id, 'name': artist, 'albumCount': 0})['albumCount'] += 1
            album = {'id': album_id, 'name': name, 'artist': artist, 'artistId': artist_id, 'coverArt': album_id,
                     'songCount': 2, 'duration': 2 * SONG_SECONDS, 'year': year}
            self.albums.append(album)
            tracks[album_id] = []
            for track in (1, 2):
                song = {'id': f'{album_id}-{track}', 'title': f'{name} Song {track}', 'album': name,
                        'albumId': album_id, 'artist': artist, 'artistId': artist_id, 'track': track, 'year': year,
                        'coverArt': album_id, 'contentType': 'audio/wav', 'suffix': 'wav',
                        'duration': SONG_SECONDS, 'isDir': False, 'type': 'music'}
                self.audio[song['id']] = fixture_module.generated_wav(SONG_SECONDS, 300 + 40 * (2 * n + track))
                song['size'] = len(self.audio[song['id']])
                tracks[album_id].append(song)
                self.songs.append(song)
        self.tracks = tracks
        self.albums.sort(key=lambda album: album['name'].lower())

    def subsonic(self, route, query):
        wanted = query.get('id', [''])[0]
        if route == 'getArtists':
            artists = sorted(self.artists.values(), key=lambda a: a['name'].lower())
            payload = {'artists': {'index': [{'name': 'G', 'artist': artists}]}}
        elif route == 'getArtist' and wanted in self.artists:
            payload = {'artist': {**self.artists[wanted], 'album': [a for a in self.albums if a['artistId'] == wanted]}}
        elif route == 'getAlbum' and wanted in self.tracks:
            album = next(a for a in self.albums if a['id'] == wanted)
            payload = {'album': {**album, 'song': self.tracks[wanted]}}
        elif route == 'getAlbumList2':
            offset, size = int(query.get('offset', ['0'])[0]), int(query.get('size', ['10'])[0])
            payload = {'albumList2': {'album': self.albums[offset:offset + size]}}
        elif route == 'getSong':
            song = next((s for s in self.songs if s['id'] == wanted), None)
            if song is None:
                return {'subsonic-response': {'status': 'failed', 'version': '1.16.1',
                                              'error': {'code': 70, 'message': 'Song not found'}}}
            payload = {'song': song}
        elif route == 'getRandomSongs':
            payload = {'randomSongs': {'song': self.songs}}
        elif route in ('getArtist', 'getAlbum'):
            return {'subsonic-response': {'status': 'failed', 'version': '1.16.1',
                                          'error': {'code': 70, 'message': 'Not found'}}}
        else:
            return super().subsonic(route, query)
        return {'subsonic-response': {'status': 'ok', 'version': '1.16.1', 'type': 'generic',
                                      'openSubsonic': False, **payload}}


def config(port):
    return (f'[server]\nurl="http://127.0.0.1:{port}"\nuser="generated"\npassword="synthetic-only"\n'
            '[display]\nbacklight_seconds=0\nclock_in_title=false\nclicker=false\n'
            '[wifi]\nenabled=false\n[tailscale]\nenabled=false\n[power]\nsleep_minutes=0\n'
            '[playback]\nscrobble=false\nsync_queue=false\n')


# ---- scripts. Home: Music, Radios, Downloads, Settings, Shuffle Songs.
# Music: Cover Flow, Playlists, Artists, Albums, ...
HOME = 'await 15000 idle:Q2\n'


def download(row, album):
    """Music -> Albums -> row -> Download Album (after Shuffle and two songs), back Home."""
    return (f'center\nawait 15000 idle:Music\nscroll 3\ncenter\nawait 15000 idle:Albums\nscroll {row}\ncenter\n'
            f'await 15000 idle:{album}\nscroll 3\ncenter\nwait 500\nback\nawait 5000 idle:Albums\nback\n'
            'await 5000 idle:Music\nback\n' + HOME)


DOWNLOADS = download(1, 'Alpha One') + download(2, 'Alpha Two') + download(3, 'Beta One')


def offline_mode(phase):
    """Settings -> Connections -> Offline Mode, back Home with Music selected."""
    return ('scroll 3\ncenter\nawait 15000 idle:Settings\nscroll 3\ncenter\nawait 15000 idle:Connections\n'
            f'scroll 4\ncenter\nwait 300\ntext {phase}-offline-mode\nback\nawait 5000 idle:Settings\nback\n' + HOME
            + 'scroll -3\n')


def browse(phase):
    """Artists -> Alpha -> Alpha Two -> its first song, then Now Playing's
    long-Center Browse Album and Browse Artist; Albums -> Beta One -> its
    first song."""
    mark = lambda name: f'text {phase}-{name}\n'
    return (mark('start') + 'center\nawait 15000 idle:Music\nscroll 2\ncenter\nawait 15000 idle:Artists\nwait 300\n'
            + mark('artists') + 'center\nawait 15000 idle:Generated Artist Alpha\nwait 300\n' + mark('artist')
            # The first album row is the newest: Alpha Two.
            + 'center\nawait 15000 idle:Alpha Two\nwait 300\n' + mark('artist-album')
            + 'scroll 1\ncenter\nawait 15000 screen:Now Playing\nawait-play 15000 playing 1000\n' + mark('artist-play')
            # Now Playing menu: Brightness, Equalizer, Sleep Timer, Up Next,
            # Shuffle, Repeat, Love, Browse Album, Browse Artist, ...
            + 'hold center\nawait 5000 overlay:Now Playing\n' + mark('np-menu')
            + 'scroll 7\ncenter\nawait 15000 idle:Alpha Two\nwait 300\n' + mark('np-album')
            + 'back\nawait 5000 screen:Now Playing\nhold center\nawait 5000 overlay:Now Playing\n'
            + 'scroll 8\ncenter\nawait 15000 idle:Generated Artist Alpha\nwait 300\n' + mark('np-artist')
            + 'back\nawait 5000 screen:Now Playing\n'
            + 'play\nawait-play 5000 paused\nback\nawait 5000 idle:Alpha Two\nback\nawait 5000 idle:Generated Artist Alpha\n'
            + 'back\nawait 5000 idle:Artists\nback\nawait 5000 idle:Music\n'
            + 'scroll 1\ncenter\nawait 15000 idle:Albums\nwait 300\n' + mark('albums')
            + 'scroll 3\ncenter\nawait 15000 idle:Beta One\nwait 300\n' + mark('album')
            + 'scroll 1\ncenter\nawait 15000 screen:Now Playing\nawait-play 15000 playing 1000\n' + mark('album-play')
            + 'play\nawait-play 5000 paused\nback\nawait 5000 idle:Beta One\nback\nawait 5000 idle:Albums\nback\n'
            + 'await 5000 idle:Music\nback\n' + HOME + mark('end'))


def runs():
    """(name, server up at start, script, (marker, server state) steps, offline mode)."""
    return (
        ('warm-unreachable', True, 'record-text\n' + HOME + DOWNLOADS + 'wait 6000\ntext downloaded\n'
         + browse('warm-unreachable'), (('downloaded', 'down'),), False),
        ('warm-offline-mode', True, 'record-text\n' + HOME + 'center\nawait 15000 idle:Music\nscroll 2\ncenter\n'
         'await 15000 idle:Artists\nback\nawait 5000 idle:Music\nback\n' + HOME + offline_mode('warm-offline-mode')
         + browse('warm-offline-mode'), (), True),
        ('cold-unreachable', False, 'record-text\n' + HOME + browse('cold-unreachable'), (), False),
        ('cold-offline-mode', True, 'record-text\n' + HOME + offline_mode('cold-offline-mode')
         + browse('cold-offline-mode'), (), True),
    )


class StoppedService(fixture_module.Service):
    """A server that, when stopped, also closes the connections it keeps
    open, as a stopped Navidrome does; the player's pooled connection then
    fails like a new one."""

    def start(self):
        open_requests = set()

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def process_request(self, request, client_address):
                open_requests.add(request)
                super().process_request(request, client_address)

        server = Server((self.host, self.port), self.library.handler(self))
        self.port = server.server_port
        self.server, self.open_requests = server, open_requests
        self.thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.05}, daemon=True)
        self.thread.start()

    def stop_listening(self):
        requests = getattr(self, 'open_requests', set())
        super().stop_listening()
        for request in list(requests):
            try:
                request.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


class Journey:
    def __init__(self, binary, out):
        self.binary, self.out = binary, out
        self.card = out / 'card'
        self.library = CatalogLibrary()
        self.nd = StoppedService('nd', self.library)
        self.runtime = out / 'runtime'

    def marker(self, name):
        try:
            return json.loads((self.runtime / f'numeric-{name}.json').read_text())
        except (OSError, ValueError):
            return None

    def run(self, name, up, script, steps):
        self.nd.set('ok' if up else 'down')
        directory = self.out / name
        directory.mkdir()
        (directory / 'script.txt').write_text(script)
        env = {**os.environ, 'Q2ND_ALSA_DEVICE': 'q2nd-fake', 'Q2ND_NUMERIC_PROBE': '1',
               'Q2ND_INSTALLED_STORAGE': '0', 'Q2ND_RUNTIME': str(self.runtime)}
        stop = threading.Event()

        def control():
            # The browse phase's start, in fixture time, then the server changes.
            for marker, action in ((f'{name}-start', lambda: self.library.event(name, 'phase')),
                                   *((marker, lambda state=state: self.nd.set(state)) for marker, state in steps)):
                while not stop.is_set() and self.marker(marker) is None:
                    time.sleep(0.02)
                if stop.is_set():
                    return
                action()
        controller = threading.Thread(target=control, daemon=True)
        with (directory / 'player.log').open('w') as log:
            proc = subprocess.Popen([str(self.binary), '--headless', '--data', str(self.card),
                                     '--script', str(directory / 'script.txt')],
                                    env=env, stdout=log, stderr=subprocess.STDOUT)
            controller.start()
            try:
                status = proc.wait(timeout=180)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
                status = 'timeout'
            finally:
                stop.set()
                controller.join(timeout=5)
        log = (directory / 'player.log').read_text(errors='replace')
        failure = [line.split('headless script: ', 1)[1] for line in log.splitlines() if 'headless script: ' in line]
        return status, failure[-1][:300] if failure else None

    def requests_since(self, since_s):
        with self.library.lock:
            return sorted({e['path'] for e in self.library.events if e['kind'] == 'request' and e['at_s'] >= since_s
                           and any(route in e['path'] for route in BROWSE_ROUTES)})

    def check(self, phase, offline_mode, since_s):
        failures = []
        shown = lambda name: (self.marker(f'{phase}-{name}') or {}).get('current', [])
        play = lambda name: (self.marker(f'{phase}-{name}') or {}).get('play') or {}
        artists = shown('artists')
        for artist in ('Generated Artist Alpha', 'Generated Artist Beta'):
            if artist not in artists:
                failures.append(f'Artists lacks downloaded {artist}: {artists}')
        if 'Generated Artist Gamma' in artists:
            failures.append('Artists lists an artist with nothing downloaded')
        artist = shown('artist')
        if not {'Alpha Two', 'Alpha One'} <= set(artist):
            failures.append(f'artist page lacks a downloaded album: {artist}')
        album = shown('artist-album')
        if not {'1. Alpha Two Song 1', '2. Alpha Two Song 2'} <= set(album):
            failures.append(f'album page lacks its songs: {album}')
        if not {'1. Alpha Two Song 1', '2. Alpha Two Song 2'} <= set(shown('np-album')):
            failures.append(f"Now Playing's Browse Album lacks the album's songs: {shown('np-album')}")
        if not {'Alpha Two', 'Alpha One'} <= set(shown('np-artist')):
            failures.append(f"Now Playing's Browse Artist lacks a downloaded album: {shown('np-artist')}")
        if 'Top Songs' in shown('np-artist') or 'Top Songs' in artist:
            failures.append('an artist page from the card lists Top Songs, which only the server has')
        albums = shown('albums')
        missing = [name for name in DOWNLOADED if name not in albums]
        if missing:
            failures.append(f'Albums lacks downloaded {missing}: {albums}')
        if {'Beta Two', 'Gamma One'} & set(albums):
            failures.append(f'Albums lists albums that are not downloaded: {albums}')
        if not {'1. Beta One Song 1', '2. Beta One Song 2'} <= set(shown('album')):
            failures.append(f"Albums' album page lacks its songs: {shown('album')}")
        for name, song in (('artist-play', 'Alpha Two Song 1'), ('album-play', 'Beta One Song 1')):
            if play(name).get('song') != song or play(name).get('state') != 'Playing':
                failures.append(f'{name}: {play(name)}, not {song} playing')
        requests = self.requests_since(since_s)
        if offline_mode and requests:
            failures.append(f'Offline Mode browsing sent requests: {requests}')
        return {'artists': artists, 'albums': albums, 'played': [play('artist-play').get('song'), play('album-play').get('song')],
                'browse_requests': requests, 'failures': failures}

    def execute(self):
        self.card.mkdir(parents=True)
        self.runtime.mkdir(mode=0o700)
        (self.runtime / 'numeric-probe-enabled').write_bytes(MARKER)
        (self.runtime / 'numeric-probe-enabled').chmod(0o600)
        (self.card / 'config.toml').write_text(config(self.nd.port))
        results = {}
        for name, up, script, steps, offline_mode in runs():
            status, failure = self.run(name, up, script, steps)
            if name == 'warm-unreachable':
                results['downloaded'] = self.downloaded()
            result = self.check(name, offline_mode, self.phase_started(name))
            if status != 0:
                result['failures'].insert(0, f'script: {failure or status}')
            results[name] = result
        return results

    def phase_started(self, name):
        """When the phase's browsing began, in fixture time."""
        with self.library.lock:
            steps = [e['at_s'] for e in self.library.events if e['kind'] == 'phase' and e['service'] == name]
        return steps[-1] if steps else float('inf')

    def downloaded(self):
        with sqlite3.connect(f'file:{self.card / "library.db"}?mode=ro', uri=True) as db:
            return sorted(row[0] for row in db.execute("SELECT name FROM offline_sets WHERE kind='album'"))

    def close(self):
        self.nd.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/q2nd')
    parser.add_argument('--out', type=Path, help='new directory below build/')
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f'{binary} missing; build it with cargo build -p q2nd --locked')
    out = (args.out or ROOT / 'build/offline-browse' / time.strftime('%Y%m%d-%H%M%S')).resolve()
    if not out.is_relative_to(ROOT / 'build'):
        parser.error('--out must be below build/')
    out.mkdir(parents=True)
    journey = Journey(binary, out)
    try:
        results = journey.execute()
    finally:
        journey.close()
    failures = [f'{name}: {failure}' for name, result in results.items() if isinstance(result, dict)
                for failure in result['failures']]
    if results.get('downloaded') != sorted(DOWNLOADED):
        failures.insert(0, f"downloads: {results.get('downloaded')}")
    summary = {'schema': 1, 'binary': str(binary), 'passed': not failures, 'failures': failures, 'runs': results}
    (out / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
    for name, _, _, _, _ in runs():
        print(f"{'PASS' if not results[name]['failures'] else 'FAIL'} {name:<18} {'; '.join(results[name]['failures'])}")
    print(out / 'result.json')
    return 0 if not failures else 1


if __name__ == '__main__':
    sys.exit(main())
