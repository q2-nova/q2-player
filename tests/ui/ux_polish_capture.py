#!/usr/bin/env python3
"""Screen-by-screen headless captures for the UX polish audit (UX project, Phase 4).

Runs the actual player (`q2nd --headless --script`) against generated
Navidrome and Audiobookshelf servers built on ux_fault_fixture.py, and saves a
PNG of every reachable screen under build/ux-polish/<stamp>/<scenario>/:

- ``online``: a generated library with long titles, several albums, books,
  podcasts and stations; every browse, info, playback and settings screen;
- ``empty``: both servers answer with empty libraries;
- ``loading``: every answer is delayed 40 s; captures at 1 s and after the
  15 s page deadline;
- ``error``: both servers answer 503;
- ``offline``: both servers refuse connections (as when the network is down);
- ``synced``: online for the background library sync, then both servers
  refuse connections: Books from the card.

Each scenario also writes ``text.json``: every string drawn per capture.
``online`` and ``synced`` fail unless the player's first Audiobookshelf
library sync completes. Generated data only.

    python3 tests/ui/ux_polish_capture.py [--binary target/debug/q2nd] [--only NAME ...]
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import ux_fault_fixture as fx  # noqa: E402

MARKER = b'Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n'

LONG_ALBUM = 'An Extremely Long Generated Album Title That Cannot Fit On One Row (Deluxe Edition)'
LONG_ARTIST = 'The Generated Orchestra and Chorus of Synthetic Performers'
LONG_BOOK = 'A Generated Book With a Title Long Enough to Overflow Every Row It Appears In'


class RichLibrary(fx.Library):
    """Several albums, artists, playlists, books, podcasts and stations; ``empty`` answers nothing."""

    def __init__(self, empty=False):
        super().__init__()
        self.empty = empty
        names = [LONG_ALBUM, 'Generated Album', 'Blue Generated Hours', 'Copper Lines', 'Daylight Sampler',
                 'Echo Fields', 'Fourth Synthetic Album', 'Glass Garden', 'Harbour Lights', 'Indigo Tests']
        self.albums = []
        for n, name in enumerate(names):
            artist = LONG_ARTIST if n == 0 else 'Synthetic Artist'
            self.albums.append({'id': f'generated-album-{n}', 'name': name, 'artist': artist,
                                'artistId': 'generated-artist-0' if n == 0 else 'generated-artist',
                                'coverArt': f'generated-album-{n}', 'songCount': 3, 'duration': 3 * fx.SONG_SECONDS,
                                'year': 2020 + n % 6, 'genre': 'Generated Genre'})
        self.album = self.albums[1]
        for song in self.songs:
            song['albumId'] = 'generated-album-1'
            song['coverArt'] = 'generated-album-1'
        long_song = dict(self.songs[0], id='generated-song-long', title='A Generated Song With an Unreasonably Long Title For One Row',
                         artist=LONG_ARTIST, album=LONG_ALBUM, albumId='generated-album-0', coverArt='generated-album-0')
        self.long_songs = [long_song]
        self.audio['generated-song-long'] = self.audio[self.songs[0]['id']]
        self.books = []
        for n, title in enumerate([LONG_BOOK, 'Generated Book', 'Second Generated Book', 'Third Generated Book']):
            book = json.loads(json.dumps(self.book))
            book['id'] = 'generated-book' if n == 1 else f'generated-book-{n}'
            book['media']['metadata']['title'] = title
            book['media']['metadata']['series'] = [{'id': 'generated-series', 'name': 'Generated Series', 'sequence': str(n + 1)}]
            book['media']['metadata']['narrators'] = ['Synthetic Narrator']
            book['media']['metadata']['genres'] = ['Generated Genre']
            book['media']['metadata']['publishedYear'] = '2026'
            book['media']['metadata']['description'] = ('<p>A generated description, long enough to wrap onto several '
                                                       'lines of the info page and test its layout.</p>')
            self.books.append(book)
        self.episode = {'id': 'generated-episode', 'libraryItemId': 'generated-show', 'libraryId': 'generated-podcasts',
                        'title': 'Generated Episode With a Long Title About Synthetic Things', 'episode': '42',
                        'season': '7', 'publishedAt': 1767225600000, 'description': '<p>Generated episode notes.</p>',
                        'audioFile': {'duration': fx.BOOK_SECONDS, 'ino': 'generated-ino',
                                      'metadata': {'filename': 'episode.wav', 'ext': '.wav', 'size': len(self.book_audio)},
                                      'mimeType': 'audio/wav'},
                        'audioTrack': {'index': 1, 'contentUrl': '/api/items/generated-show/file/generated-ino',
                                       'duration': fx.BOOK_SECONDS, 'mimeType': 'audio/wav'}}
        self.show = {'id': 'generated-show', 'libraryId': 'generated-podcasts', 'mediaType': 'podcast',
                     'media': {'metadata': {'title': 'Generated Show', 'author': 'Synthetic Podcaster',
                                            'description': '<p>Generated show.</p>'},
                               'coverPath': 'generated-cover', 'numEpisodes': 1, 'episodes': [self.episode]}}

    # ---- Subsonic
    def subsonic(self, route, query):
        if self.empty and route in ('getAlbumList2', 'getArtists', 'getPlaylists', 'getStarred2', 'getGenres',
                                    'getRandomSongs', 'search3', 'getInternetRadioStations'):
            empty = {'getAlbumList2': {'albumList2': {}}, 'getArtists': {'artists': {}},
                     'getPlaylists': {'playlists': {}}, 'getStarred2': {'starred2': {}}, 'getGenres': {'genres': {}},
                     'getRandomSongs': {'randomSongs': {}}, 'search3': {'searchResult3': {}},
                     'getInternetRadioStations': {'internetRadioStations': {}}}[route]
            return {'subsonic-response': {'status': 'ok', 'version': '1.16.1', **empty}}
        ok = lambda payload: {'subsonic-response': {'status': 'ok', 'version': '1.16.1', 'type': 'generic',
                                                    'openSubsonic': False, **payload}}
        if route == 'getAlbumList2':
            offset = int(query.get('offset', ['0'])[0])
            size = int(query.get('size', ['50'])[0])
            return ok({'albumList2': {'album': self.albums[offset:offset + size]}})
        if route == 'getAlbum':
            wanted = query.get('id', [''])[0]
            album = next((a for a in self.albums if a['id'] == wanted), self.album)
            songs = self.long_songs + self.songs if album['id'] == 'generated-album-0' else self.songs
            return ok({'album': {**album, 'song': [dict(s, albumId=album['id'], album=album['name'],
                                                        coverArt=album['coverArt']) for s in songs]}})
        if route == 'getArtists':
            return ok({'artists': {'index': [
                {'name': 'S', 'artist': [{'id': 'generated-artist', 'name': 'Synthetic Artist', 'albumCount': 9}]},
                {'name': 'T', 'artist': [{'id': 'generated-artist-0', 'name': LONG_ARTIST, 'albumCount': 1}]}]}})
        if route == 'getArtist':
            wanted = query.get('id', [''])[0]
            albums = [a for a in self.albums if a['artistId'] == wanted]
            name = LONG_ARTIST if wanted == 'generated-artist-0' else 'Synthetic Artist'
            return ok({'artist': {'id': wanted, 'name': name, 'album': albums}})
        if route == 'getTopSongs':
            return ok({'topSongs': {'song': self.songs}})
        if route == 'getPlaylists':
            return ok({'playlists': {'playlist': [
                {'id': 'generated-playlist', 'name': 'Generated Playlist', 'songCount': 3, 'duration': 120,
                 'coverArt': 'generated-album-1'},
                {'id': 'generated-playlist-long', 'name': 'A Generated Playlist Whose Name Runs Past The Edge',
                 'songCount': 3, 'duration': 120, 'coverArt': 'generated-album-2'}]}})
        if route == 'getPlaylist':
            return ok({'playlist': {'id': query.get('id', [''])[0], 'name': 'Generated Playlist', 'songCount': 3,
                                    'entry': self.songs}})
        if route == 'getStarred2':
            return ok({'starred2': {'song': self.songs[:2], 'album': self.albums[:2]}})
        if route == 'getGenres':
            return ok({'genres': {'genre': [{'value': 'Generated Genre', 'songCount': 30, 'albumCount': 10},
                                            {'value': 'Synthetic Ambient', 'songCount': 3, 'albumCount': 1}]}})
        if route == 'getSongsByGenre':
            return ok({'songsByGenre': {'song': self.songs}})
        if route == 'search3':
            return ok({'searchResult3': {'album': self.albums[:2], 'song': self.songs,
                                         'artist': [{'id': 'generated-artist', 'name': 'Synthetic Artist'}]}})
        if route == 'getInternetRadioStations':
            return ok({'internetRadioStations': {'internetRadioStation': [
                {'id': 'generated-station', 'name': 'Generated Station', 'streamUrl': 'http://127.0.0.1:9/generated'}]}})
        if route == 'getAlbumInfo2':
            return ok({'albumInfo': {}})
        if route == 'getArtistInfo2':
            return ok({'artistInfo2': {}})
        return super().subsonic(route, query)

    # ---- Audiobookshelf
    def shelf(self, method, path):
        if method == 'GET':
            if path == '/api/libraries':
                return {'libraries': [{'id': 'generated-books', 'name': 'Generated Books', 'mediaType': 'book'},
                                      {'id': 'generated-podcasts', 'name': 'Generated Podcasts', 'mediaType': 'podcast'}]}
            for book in self.books:
                if path == f"/api/items/{book['id']}":
                    return book
            if path == '/api/items/generated-show':
                return self.show
            for library, items in (('generated-books', self.books), ('generated-podcasts', [self.show])):
                root = f'/api/libraries/{library}'
                if not path.startswith(root):
                    continue
                items = [] if self.empty else items
                rest = path[len(root):]
                if rest.startswith('/items'):
                    return {'results': self.window(items), 'page': self.query_page, 'limit': self.query_limit,
                            'total': len(items)}
                if rest == '/personalized':
                    shelves = []
                    if items:
                        for name in ('continue-listening', 'recently-added', 'discover', 'listen-again', 'newest-episodes'):
                            entities = [dict(i, recentEpisode=self.episode) if library == 'generated-podcasts' else i
                                        for i in items]
                            shelves.append({'id': name, 'total': len(entities), 'entities': entities})
                    return shelves
                if rest == '/filterdata':
                    return {'genres': [] if self.empty else ['Generated Genre']}
                if rest == '/narrators':
                    return {'narrators': [] if self.empty else [{'id': 'n', 'name': 'Synthetic Narrator', 'numBooks': 4}]}
                if rest == '/recent-episodes':
                    eps = [] if self.empty else [dict(self.episode, podcast=self.show['media'])]
                    return {'page': self.query_page, 'limit': self.query_limit, 'episodes': self.window(eps)}
                if rest == '/search':
                    return {'book': [{'libraryItem': i} for i in items], 'podcast': []}
                groups = {'/series': ('generated-series', 'Generated Series'),
                          '/collections': ('generated-collection', 'Generated Collection'),
                          '/playlists': ('generated-playlist', 'Generated Listening Playlist'),
                          '/authors': ('generated-author', 'Synthetic Author')}
                if rest in groups:
                    ident, name = groups[rest]
                    results = [] if self.empty or library != 'generated-books' else [
                        {'id': ident, 'name': name, 'numBooks': len(items), 'books': items}]
                    return {'results': self.window(results), 'page': self.query_page, 'limit': self.query_limit,
                            'total': len(results)}
            if path.startswith('/api/collections/'):
                return {'libraryId': 'generated-books', 'books': [] if self.empty else self.books}
            if path.startswith('/api/playlists/'):
                return {'libraryId': 'generated-books', 'items': [] if self.empty else [{'libraryItem': b} for b in self.books]}
        if method == 'POST' and path.startswith('/api/items/') and '/play' in path:
            item = path.split('/')[3]
            book = next((b for b in self.books if b['id'] == item), self.book)
            if item == 'generated-show':
                return {'id': 'generated-session', 'displayTitle': self.episode['title'], 'displayAuthor': 'Synthetic Podcaster',
                        'duration': fx.BOOK_SECONDS, 'currentTime': 0, 'chapters': [],
                        'audioTracks': [{'index': 1, 'contentUrl': '/api/items/generated-show/file/generated-ino',
                                         'startOffset': 0, 'duration': fx.BOOK_SECONDS, 'mimeType': 'audio/wav',
                                         'metadata': {'ext': '.wav', 'filename': 'episode.wav'}}]}
            return {'id': 'generated-session', 'displayTitle': book['media']['metadata']['title'],
                    'displayAuthor': 'Synthetic Author', 'duration': fx.BOOK_SECONDS, 'currentTime': 0,
                    'chapters': [{'id': 0, 'start': 0, 'end': fx.BOOK_SECONDS / 2, 'title': 'Generated Chapter One'},
                                 {'id': 1, 'start': fx.BOOK_SECONDS / 2, 'end': fx.BOOK_SECONDS, 'title': 'Chapter Two'}],
                    'audioTracks': [{'index': 1, 'contentUrl': f"/api/items/{item}/file/generated-ino",
                                     'startOffset': 0, 'duration': fx.BOOK_SECONDS, 'mimeType': 'audio/wav',
                                     'metadata': {'ext': '.wav', 'filename': 'book.wav'}}]}
        return super().shelf(method, path)

    # The page and limit of the request this thread is answering.
    _request = threading.local()

    @property
    def query_page(self):
        return getattr(self._request, 'page', 0)

    @property
    def query_limit(self):
        return getattr(self._request, 'limit', 0)

    def window(self, rows):
        """The rows of the requested page, as Audiobookshelf pages a list:
        ``limit`` rows from ``page * limit`` (0-indexed); no ``limit`` (or 0)
        answers every row and ignores ``page``."""
        limit = self.query_limit
        return rows[self.query_page * limit:(self.query_page + 1) * limit] if limit else rows

    def handler(self, service):
        base = super().handler(service)
        library = self

        class Handler(base):
            def handle_one(self, method):
                from urllib.parse import parse_qs, urlsplit
                url = urlsplit(self.path)
                query = parse_qs(url.query)
                library._request.page = int(query.get('page', ['0'])[0] or 0)
                library._request.limit = int(query.get('limit', ['0'])[0] or 0)
                path = url.path
                if service.name == 'abs' and '/file/' in path and not path.startswith('/api/items/generated-book/'):
                    self.media(library.book_audio, method == 'HEAD')
                    return
                if service.name == 'abs' and path.endswith('/cover'):
                    self.respond(library.cover, 'image/png', head=method == 'HEAD')
                    return
                super().handle_one(method)

        return Handler


class Fixture(fx.Fixture):
    def __init__(self, empty=False, host='127.0.0.1'):
        self.library = RichLibrary(empty=empty)
        self.nd = fx.Service('nd', self.library, host=host)
        # Paced so a book download shows its progress for several seconds.
        self.abs = fx.Service('abs', self.library, pace=192 * 1024, host=host)


# ---- script building
class Script:
    def __init__(self, out):
        self.out = out
        self.lines = ['record-text']
        self.shots = []

    def add(self, text):
        self.lines.extend(line for line in text.strip().splitlines())
        return self

    def shot(self, name, settle=400):
        self.shots.append(name)
        return self.add(f'wait {settle}\ntext {name}\nshot {self.out / (name + ".png")}')

    def open(self, scroll, title, wait='idle:'):
        """From the current list: move ``scroll`` rows, Center, wait for ``title``."""
        if scroll:
            self.add(f'scroll {scroll}')
        if wait is None:  # a page that may never settle: no readiness wait
            return self.add('center\nwait 300')
        return self.add(f'center\nawait 15000 {wait}{title}')

    def back(self, title, wait='idle:'):
        return self.add(f'back\nawait 15000 {wait}{title}')

    def text(self):
        return '\n'.join(self.lines) + '\n'


# Home rows (configured, both servers): Music 0, Books 1, Podcasts 2, Radios 3,
# Downloads 4, Settings 5, Shuffle Songs 6, Now Playing 7 (while a queue exists).
# Music rows: Cover Flow 0, Playlists 1, Artists 2, Albums 3, Genres 4,
# Favorites 5, Recently Added 6 … Random Albums 10, Search 11.

def online(s):
    s.add('await 15000 screen:Q2').shot('home')
    s.open(0, 'Music').shot('music')
    s.open(3, 'Albums').shot('albums')
    s.add('wait 1500').shot('albums-art', settle=200)
    s.open(1, LONG_ALBUM).shot('album-long')
    s.add('scroll 1\nhold center').shot('album-long-menu')
    s.add('back').back('Albums')
    s.open(1, 'Generated Album').shot('album')
    s.add('scroll 4').shot('album-download-row')
    s.back('Albums').back('Music')
    s.add('scroll -3').open(1, 'Playlists').shot('playlists')
    s.open(0, 'Generated Playlist').shot('playlist').back('Playlists').back('Music')
    s.open(1, 'Artists').shot('artists')
    s.open(0, 'Synthetic Artist').shot('artist').back('Artists').back('Music')
    s.open(2, 'Genres').shot('genres')
    s.open(0, 'Generated Genre').shot('genre').back('Genres').back('Music')
    s.open(1, 'Favorites').shot('favorites')
    s.open(0, 'Favorite Songs').shot('favorite-songs').back('Favorites').back('Music')
    s.open(5, 'Random Albums').shot('random-albums').back('Music')
    s.open(1, 'Search', wait='').shot('search-entry').add('back\nawait 15000 idle:Music')
    s.add('scroll -11').open(0, 'Cover Flow', wait='').add('wait 1500').shot('coverflow').back('Music')
    s.back('Q2', wait='screen:')
    # Books and podcasts
    s.open(1, 'Books').shot('books')
    s.open(6, 'Choose Library', wait='').add('wait 1500').shot('books-choose-library')
    s.add('center\nawait 15000 idle:All Books\nwait 1000').shot('books-list')
    s.open(0, 'Book Info', wait='').add('wait 1500').shot('book-info-long')
    s.add('scroll 3').shot('book-info-scrolled')
    s.add('back\nawait 15000 idle:All Books')
    s.add('hold center').shot('book-row-menu').add('back\nwait 300')
    s.back('Books')
    s.add('scroll -6')
    s.open(0, 'Continue Listening', wait='').add('wait 2500').shot('books-continue')
    s.add('back\nawait 15000 idle:Books')
    s.open(7, 'Genres', wait='').add('wait 2500').shot('books-genres').add('back\nawait 15000 idle:Books')
    s.open(1, 'Series', wait='').add('wait 2500').shot('books-series').add('back\nawait 15000 idle:Books')
    s.open(1, 'Collections', wait='').add('wait 2500').shot('books-collections').add('back\nawait 15000 idle:Books')
    s.open(-5, 'Search Books', wait='').shot('books-search-entry')
    s.add('scroll 6\ncenter\nwait 300\nhold center\nwait 2500').shot('books-search-results')
    s.add('back\nawait 15000 idle:Books')
    s.back('Q2', wait='screen:')
    s.open(1, 'Podcasts').shot('podcasts')
    s.open(3, 'Choose Library', wait='').add('wait 1500').shot('podcasts-choose-library')
    s.add('center\nwait 2500').shot('podcasts-library')
    s.add('center\nwait 2500').shot('podcast-show')
    s.add('back\nwait 500\nback\nawait 15000 idle:Podcasts')
    s.open(-2, 'Latest Episodes', wait='').add('wait 2500').shot('podcasts-latest').add('back\nawait 15000 idle:Podcasts')
    s.back('Q2', wait='screen:')
    s.open(1, 'Radios', wait='').add('wait 1500').shot('radios').back('Q2', wait='screen:')
    # Play a song, then Now Playing and its screens
    s.add('scroll -3').open(0, 'Music').open(3, 'Albums').open(2, 'Generated Album')
    s.add('scroll 1\ncenter\nawait 15000 screen:Now Playing\nwait 2500').shot('now-playing')
    s.add('center\nwait 400').shot('now-playing-center-1')
    s.add('center\nwait 400').shot('now-playing-center-2')
    s.add('center\nwait 400').shot('now-playing-center-3')
    s.add('hold center').shot('now-playing-menu')
    s.add('scroll 2\ncenter\nawait 5000 screen:Sleep Timer').shot('sleep-timer-choice')
    s.add('scroll 1\ncenter\nawait 5000 screen:Now Playing').shot('now-playing-sleep', settle=200)
    s.add('wait 2500').shot('now-playing-sleep-settled')
    s.add('hold center\nwait 300\nscroll 2\ncenter\nawait 5000 screen:Sleep Timer').shot('sleep-timer-choice-active')
    s.add('back\nawait 5000 screen:Now Playing')
    s.add('back\nawait 15000 idle:Generated Album').add('play\nwait 500')
    s.back('Albums').back('Music').back('Q2', wait='screen:')
    s.add('wait 500').shot('home-with-now-playing')
    s.open(4, 'Downloads', wait='screen:').add('wait 1000').shot('downloads').back('Q2', wait='screen:')
    # Settings
    s.open(1, 'Settings').shot('settings')
    for scroll, title, name in ((0, 'Sound', 'settings-sound'), (1, 'Playback', 'settings-playback'),
                                (1, 'Display', 'settings-display'), (1, 'Connections', 'settings-connections'),
                                (1, 'Bluetooth', 'settings-bluetooth'), (1, 'Storage', 'settings-storage'),
                                (1, 'System', 'settings-system')):
        s.open(scroll, title).add('wait 800').shot(name)
        if name == 'settings-playback':
            s.open(0, 'Auto Sleep Timer').shot('settings-auto-sleep').back('Playback')
            s.add('scroll 13').shot('settings-playback-end').add('scroll -13')
        if name == 'settings-storage':
            s.open(0, 'Storage Usage', wait='').add('wait 1500').shot('storage-usage')
            s.add('center\nwait 1500').shot('storage-drilldown-cache').back('Storage Usage')
            s.add('scroll 1').shot('storage-usage-2')
            s.add('center\nwait 1500').shot('storage-drilldown-other').back('Storage Usage')
            s.add('scroll 1').shot('storage-usage-3')
            s.add('center').shot('storage-drilldown-reserved', settle=200)
            s.add('back\nawait 15000 idle:Storage')
            s.add('scroll 2\ncenter\nwait 600').shot('storage-cache-limit').add('back\nwait 300')
        if name == 'settings-system':
            # About is the last System row.
            s.open(99, 'About').add('wait 800').shot('settings-about').back('System')
        if name == 'settings-connections':
            s.open(2, 'Audiobookshelf', wait='').add('wait 1500').shot('settings-abs').add('back\nwait 300')
            s.open(-1, 'Navidrome Server', wait='').add('wait 600').shot('settings-server').add('back\nwait 300\nscroll -1')
        s.back('Settings')
    # A book download in progress (UXI-008): its row and the Downloads page.
    s.back('Q2', wait='screen:')
    s.open(-4, 'Books').open(6, 'All Books', wait='').add('wait 1500\nscroll 1\nhold center\nwait 300')
    s.add('scroll 1\ncenter\nwait 3000').shot('books-list-downloading')
    s.add('back\nwait 800\nback\nawait 15000 screen:Q2')
    s.open(3, 'Downloads', wait='screen:').add('wait 1000').shot('downloads-with-book')
    return s


def loading(s):
    """Every answer waits 40 s: what each page shows while it waits and after its deadline."""
    s.add('await 15000 screen:Q2')
    s.open(0, 'Music').open(3, 'Albums', wait=None).add('wait 1000').shot('albums-loading')
    s.add('wait 15000').shot('albums-deadline').add('back\nawait 5000 idle:Music')
    s.open(-2, 'Playlists', wait=None).add('wait 1000').shot('playlists-loading')
    s.add('wait 15000').shot('playlists-deadline').add('back\nawait 5000 idle:Music')
    s.add('back\nawait 5000 screen:Q2')
    s.open(1, 'Books').open(6, 'Choose Library', wait=None).add('wait 1000').shot('books-loading')
    s.add('wait 15000').shot('books-deadline').add('back\nawait 5000 idle:Books')
    s.add('back\nawait 5000 screen:Q2')
    s.open(3, 'Downloads', wait=None).add('wait 1000').shot('downloads-while-servers-wait')
    return s


def failing(s):
    """Both servers answer 503 (error) or refuse connections (offline)."""
    s.add('await 15000 screen:Q2').shot('home')
    s.open(0, 'Music').shot('music')
    s.open(3, 'Albums', wait='screen:').add('wait 4000').shot('albums')
    s.add('back\nawait 5000 screen:Music')
    s.open(-2, 'Playlists', wait='screen:').add('wait 4000').shot('playlists')
    s.add('back\nawait 5000 screen:Music')
    s.open(1, 'Artists', wait='screen:').add('wait 4000').shot('artists')
    s.add('back\nawait 5000 screen:Music')
    s.open(9, 'Search', wait='').shot('search-entry').add('back\nawait 5000 screen:Music')
    s.add('back\nawait 5000 screen:Q2')
    s.open(1, 'Books', wait='screen:').shot('books')
    s.open(6, 'Choose Library', wait='screen:').add('wait 5000').shot('books-library')
    s.add('back\nawait 5000 screen:Books')
    s.open(-6, 'Continue Listening', wait=None).add('wait 5000').shot('books-continue')
    s.add('back\nawait 5000 screen:Books\nback\nawait 5000 screen:Q2')
    s.open(2, 'Radios', wait='screen:').add('wait 3000').shot('radios').add('back\nawait 5000 screen:Q2')
    s.open(1, 'Downloads', wait='screen:').add('wait 1500').shot('downloads').add('back\nawait 5000 screen:Q2')
    s.add('scroll 2\ncenter\nwait 4000').shot('shuffle-songs')
    s.add('back\nwait 500').open(-1, 'Settings').open(3, 'Connections').add('wait 3000').shot('connections')
    return s


def empty(s):
    s.add('await 15000 screen:Q2').shot('home')
    s.open(0, 'Music').open(3, 'Albums', wait='screen:').add('wait 2500').shot('albums').back('Music', wait='screen:')
    s.open(-2, 'Playlists', wait='screen:').add('wait 2500').shot('playlists').back('Music', wait='screen:')
    s.open(1, 'Artists', wait='screen:').add('wait 2500').shot('artists').back('Music', wait='screen:')
    s.open(2, 'Genres', wait='screen:').add('wait 2500').shot('genres').back('Music', wait='screen:')
    s.open(1, 'Favorites', wait='screen:').add('wait 1000')
    s.open(0, 'Favorite Songs', wait='screen:').add('wait 2500').shot('favorite-songs').back('Favorites', wait='screen:')
    s.back('Music', wait='screen:')
    s.open(-5, 'Cover Flow', wait='screen:').add('wait 2500').shot('coverflow').back('Music', wait='screen:')
    s.back('Q2', wait='screen:')
    s.open(1, 'Books').open(6, 'Choose Library', wait='screen:').add('wait 2500').shot('books-choose-library')
    s.add('center\nwait 2500').shot('books-list').add('back\nawait 5000 screen:Books')
    s.open(-6, 'Continue Listening', wait=None).add('wait 2500').shot('books-continue')
    s.add('back\nawait 5000 screen:Books\nback\nawait 5000 screen:Q2')
    s.open(2, 'Radios', wait='screen:').add('wait 2500').shot('radios').back('Q2', wait='screen:')
    s.open(1, 'Downloads', wait='screen:').add('wait 1500').shot('downloads').back('Q2', wait='screen:')
    s.add('scroll 2\ncenter\nwait 4000').shot('shuffle-songs')
    return s


def synced(s, settle='wait 16000'):
    """Online long enough for the background library sync, then both servers
    refuse connections: what Books shows from the card (UXI-016, UXI-017).
    ``settle`` covers the sync and the fault (here a 12 s timer in ``run``;
    ux_device_capture.py signals the host from the script instead)."""
    s.add('await 15000 screen:Q2').add(settle).shot('home-after-sync')
    s.open(1, 'Books').open(6, 'Choose Library', wait='').add('wait 2500').shot('books-choose-library')
    s.add('center\nwait 2500').shot('books-list')
    s.add('center\nwait 2500').shot('book-info')
    s.add('back\nwait 800\nback\nawait 15000 idle:Books')
    s.open(-6, 'Continue Listening', wait='').add('wait 2500').shot('books-continue').add('back\nawait 15000 idle:Books')
    s.open(4, 'Search Books', wait='').add('scroll 6\ncenter\nwait 300\nhold center\nwait 2500').shot('books-search-results')
    s.add('back\nawait 15000 idle:Books\nback\nawait 15000 screen:Q2')
    s.open(4, 'Settings').open(3, 'Connections').add('wait 1500').shot('connections')
    s.open(2, 'Audiobookshelf', wait='').add('wait 1500').shot('settings-abs')
    return s


# Scenarios whose Audiobookshelf library sync must complete before the fault.
SYNCED = ('online', 'synced')
SYNC_RESULT = re.compile(r'library sync measure: abs .* result=(\w+)')


def sync_results(logtext):
    """The result of each Audiobookshelf library sync the player logged."""
    return SYNC_RESULT.findall(logtext)


SCENARIOS = {
    'online': (online, None, {}),
    'synced': (synced, ('reset', {'delay_s': 12}), {}),
    'empty': (empty, None, {'empty': True}),
    'loading': (loading, ('delay', {'seconds': 40}), {}),
    'error': (failing, ('status', {'status': 503}), {}),
    'offline': (failing, ('down', {}), {}),
}


def run(name, binary, base):
    build, fault, options = SCENARIOS[name]
    out = base / name
    out.mkdir(parents=True)
    fixture = Fixture(**options)
    try:
        card = out / 'card'
        card.mkdir()
        runtime = out / 'runtime'
        runtime.mkdir(mode=0o700)
        (runtime / 'numeric-probe-enabled').write_bytes(MARKER)
        (runtime / 'numeric-probe-enabled').chmod(0o600)
        (card / 'config.toml').write_text(fixture.config())
        script = build(Script(out))
        (out / 'script.txt').write_text(script.text())
        timer = None
        if fault:
            kind, opts = fault
            opts = dict(opts)
            after = opts.pop('delay_s', 0)

            def apply():
                fixture.nd.set(kind, **opts)
                fixture.abs.set(kind, **opts)
            if after:
                timer = threading.Timer(after, apply)
                timer.start()
            else:
                # The first screen draws before the fault; the library's
                # first requests then meet it.
                apply()
        env = {**os.environ, 'Q2ND_ALSA_DEVICE': 'q2nd-fake', 'Q2ND_NUMERIC_PROBE': '1',
               'Q2ND_INSTALLED_STORAGE': '0', 'Q2ND_RUNTIME': str(runtime)}
        started = time.monotonic()
        with (out / 'player.log').open('w') as log:
            proc = subprocess.run([str(binary), '--headless', '--data', str(card), '--script', str(out / 'script.txt')],
                                  env=env, stdout=log, stderr=subprocess.STDOUT, timeout=600)
        if timer:
            timer.cancel()
        texts = {}
        for shot in script.shots:
            path = runtime / f'numeric-{shot}.json'
            if path.exists():
                texts[shot] = json.loads(path.read_text()).get('current', [])
        (out / 'text.json').write_text(json.dumps(texts, indent=1) + '\n')
        # Requests the generated servers do not answer: fixture gaps, not product faults.
        unknown = sorted({f"{e['service']} {e['method']} {e['path']}" for e in fixture.library.events if e['kind'] == 'unknown'})
        (out / 'unanswered.json').write_text(json.dumps(unknown, indent=1) + '\n')
        events = [e for e in fixture.library.events if e['kind'] != 'request'][-200:]
        (out / 'fixture-events.json').write_text(json.dumps(events, indent=1) + '\n')
        logtext = (out / 'player.log').read_text(errors='replace')
        failure = [line for line in logtext.splitlines() if 'headless script: ' in line]
        missing = [shot for shot in script.shots if not (out / f'{shot}.png').exists()]
        syncs = sync_results(logtext)
        return {'scenario': name, 'exit': proc.returncode, 'elapsed_s': round(time.monotonic() - started, 1),
                'shots': len(script.shots) - len(missing), 'missing': missing, 'syncs': syncs,
                'sync_ok': name not in SYNCED or (syncs[:1] or [None])[0] in ('complete', 'covers_missing'),
                'script_failure': failure[-1][:300] if failure else None}
    finally:
        fixture.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/q2nd')
    parser.add_argument('--only', action='append', default=[], choices=sorted(SCENARIOS))
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f'{binary} missing; build it with cargo build -p q2nd --locked --offline')
    base = (args.out or ROOT / 'build/ux-polish' / time.strftime('%Y%m%d-%H%M%S')).resolve()
    if not base.is_relative_to(ROOT / 'build'):
        parser.error('--out must be below build/')
    base.mkdir(parents=True, exist_ok=True)
    names = args.only or list(SCENARIOS)
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(names)) as pool:
        results = list(pool.map(lambda n: run(n, binary, base), names))
    (base / 'summary.json').write_text(json.dumps(results, indent=1) + '\n')
    for r in results:
        print(f"{r['scenario']:<8} exit {r['exit']} shots {r['shots']} {r['elapsed_s']} s"
              + (f" missing {r['missing']}" if r['missing'] else '')
              + ('' if r['sync_ok'] else f" library sync {r['syncs'] or 'not run'}")
              + (f" — {r['script_failure']}" if r['script_failure'] else ''))
    print(base)
    return 0 if all(r['exit'] == 0 and not r['missing'] and r['sync_ok'] for r in results) else 1


if __name__ == '__main__':
    sys.exit(main())
