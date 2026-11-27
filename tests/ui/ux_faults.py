#!/usr/bin/env python3
"""Fault-injection journey suite for the headless player (UX project, Phase 2).

Each journey starts the actual player (`q2nd --headless --script`) against
the generated servers in ux_fault_fixture.py, injects one fault while the
script runs, and checks that the player reaches a usable state within a
stated bound:

- what is on screen (every string drawn is recorded by `record-text`);
- whether the action stays available (Back, retry, playback);
- whether playback resumes when the fault clears;
- that no error code or raw backend text is ever drawn (UX-006);
- that the fixed error code reaches the UX report (the `ux Error code=N`
  line written by the same call that admits the report to telemetry).

Generated data only. Results: build/ux-faults/<stamp>/summary.json.

    python3 tests/ui/ux_faults.py [--binary target/debug/q2nd] [--only NAME ...] [--jobs N]
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import ux_fault_fixture as fixture_module  # noqa: E402

MARKER = b'Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n'

# Fixed messages from ux::Code::message.
PLAIN_MESSAGES = {
    "Something went wrong", "You're offline", "No network connection", "Can't reach Navidrome",
    "Can't reach Audiobookshelf", "Library changed. Choose it again", "Set up Audiobookshelf in Settings",
    "Set up Navidrome in Settings", "The server had a problem", "The server took too long", "Stream interrupted",
    "Not downloaded", "File is missing from the card", "Insert a card", "The card is full", "Playback was interrupted",
    "Couldn't read the card", "Wrong user name or password", "Audiobookshelf had a problem", "No songs available", "No downloaded songs",
    "Playback changed. Try again", "Can't play this file", "Audio output problem", "Couldn't read the library",
    "Couldn't remove the cached song", "This is taking too long",
}
ANY_PLAIN = re.compile('|'.join(map(re.escape, sorted(PLAIN_MESSAGES))))

# Raw error text that must never be drawn (UX-006).
RAW = re.compile(
    r'(?i)os error|errno|\bhttp\b|\b[45]\d\d\b|status code|descriptor|refused|connection reset|broken pipe|'
    r'lookup|resolv|\bdns\b|timed out|panick|error:|failed to|unexpected eof|invalid response|\bjson\b|'
    r'\bcode\s*\d|\berror \d|sqlite|no such file|no space left|enospc|ebadf|io error|transport|ureq|tokio')

UX_LINE = re.compile(r'ux (Error|Recovered|Journey|Stop|Stuck) code=(\d+) journey=(\w+) outcome=(\w+) latency_ms=(\d+)')


class Journey:
    """A scripted journey, its fault and its bounds.

    ``bound`` maps measure names to limits: a number is a maximum in seconds,
    ``True`` a condition that must hold. ``measure(run)`` returns the measures;
    keys not in ``bound`` are reported only.
    """

    def __init__(self, name, fault, bound, script, steps=(), measure=None, card_mb=None, card_free_kb=0, pace=0,
                 nd_url=None, abs_url=None, expect_codes=(), closes=(), albums=1):
        self.name, self.fault, self.bound, self.script = name, fault, bound, script
        self.steps, self.measure, self.pace = list(steps), measure, pace
        self.card_mb, self.card_free_kb = card_mb, card_free_kb
        self.nd_url, self.abs_url = nd_url, abs_url
        self.expect_codes, self.closes = tuple(expect_codes), tuple(closes)
        self.albums = albums


# ---- navigation (generated fixture: one album of three songs, one book)
HOME = 'await 15000 screen:Q2\n'
TO_ALBUMS = 'center\nawait 15000 idle:Music\nscroll 3\ncenter\n'
OPEN_ALBUM = 'await 15000 idle:Albums\nscroll 1\ncenter\nawait 15000 idle:Generated Album\n'
PLAY_SONG_1 = 'scroll 1\ncenter\nawait 15000 screen:Now Playing\n'
DOWNLOAD_ALBUM_ROW = 'scroll 4\n'
TO_LIBRARIES = 'scroll 1\ncenter\nawait 15000 idle:Books\nscroll 6\ncenter\nawait 15000 idle:Choose Library\n'
TO_BOOKS = TO_LIBRARIES + 'center\nawait 15000 idle:All Books\n'
PLAY_BOOK = 'center\nawait 15000 idle:Book Info\ncenter\nawait 15000 screen:Now Playing\n'
TO_DOWNLOADS = 'scroll 4\ncenter\nawait 15000 screen:Downloads\n'
TO_COVERFLOW = 'center\nawait 15000 idle:Music\ncenter\nawait 30000 idle:Cover Flow\n'
TO_CONNECTIONS = 'scroll 5\ncenter\nawait 15000 idle:Settings\nscroll 3\ncenter\nawait 15000 idle:Connections\n'
HOME_FROM_MUSIC = 'back\nawait 5000 screen:Q2\n'


def mark(name):
    """Publishes runtime/numeric-NAME.json: strings on screen and playback state."""
    return f'text {name}\n'


def back_to(screen, name='back'):
    """Back, timed from `press` to the screen being shown and idle."""
    return mark('press') + f'back\nawait 3000 idle:{screen}\n' + mark(name)


def suspend(run, seconds):
    """Freeze the player as a suspend would, dropping its connections meanwhile."""
    pid = run.proc.pid
    os.kill(pid, signal.SIGSTOP)
    run.fx.library.event('harness', 'frozen')
    run.fx.nd.set('reset')
    time.sleep(seconds)
    run.fx.nd.set('ok')
    run.fx.library.event('harness', 'resumed')
    os.kill(pid, signal.SIGCONT)


def journeys():
    stall = 15

    def link_drop(back_screen):
        return ('await-play 15000 playing 2000\n' + mark('fault') + 'wait 1000\n' + back_to(back_screen)
                + f'wait {(stall - 2) * 1000}\n' + mark('cleared') + 'await-play 30000 playing\n' + mark('playing')
                + 'await-play 10000 playing 2000\n' + mark('advanced') + mark('end'))

    return [
        # resume_after_clear_s: the UI's 2 s auto-resume delay, then the heard
        # part of the song is fetched again (no range on transcoded streams).
        Journey('music-link-drop', 'Navidrome stream: no packets for 15 s mid-song (link drop), then back',
                {'back_s': 1, 'resume_after_clear_s': 6, 'same_song': True, 'kept_position': True},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + PLAY_SONG_1 + link_drop('Generated Album'),
                steps=[('marker', 'fault', lambda run: run.fx.nd.set('stall')),
                       ('marker', 'cleared', lambda run: run.fx.nd.set('ok'))],
                measure=measure_link_drop, pace=64 * 1024, closes=('UXI-003',)),
        Journey('spoken-link-drop', 'Audiobookshelf stream: no packets for 15 s mid-book (link drop), then back',
                {'back_s': 1, 'resume_after_clear_s': 5, 'same_song': True, 'kept_position': True},
                'record-text\n' + HOME + TO_BOOKS + PLAY_BOOK + link_drop('All Books'),
                steps=[('marker', 'fault', lambda run: run.fx.abs.set('stall')),
                       ('marker', 'cleared', lambda run: run.fx.abs.set('ok'))],
                measure=measure_link_drop, pace=192 * 1024, closes=('UXI-001', 'UXI-003')),
        Journey('abs-down', 'Audiobookshelf refuses connections, Navidrome up; then Audiobookshelf returns',
                {'albums_s': 3, 'abs_message_s': 5, 'retry_offered': True, 'back_s': 1, 'library_after_retry_s': 3},
                'record-text\n' + HOME + mark('start') + TO_ALBUMS + 'await 15000 idle:Albums\n' + mark('albums')
                + 'back\nback\nawait 5000 screen:Q2\n' + TO_LIBRARIES + mark('libraries') + 'wait 5000\n'
                + mark('failed') + back_to('Books') + 'center\nawait 15000 idle:Choose Library\n' + mark('abs-up')
                + 'center\nwait 4000\n' + mark('end'),
                steps=[('after', 0, lambda run: run.fx.abs.set('down')),
                       ('marker', 'abs-up', lambda run: run.fx.abs.set('ok'))],
                measure=measure_abs_down, expect_codes=(13,), closes=('UXI-002', 'UXI-003')),
        Journey('nd-5xx', 'Navidrome answers 503 to every request; then recovers',
                {'message_s': 3, 'truthful_message': True, 'retry_offered': True, 'back_s': 1, 'recovered_s': 3},
                'record-text\n' + HOME + mark('albums-open') + TO_ALBUMS + 'wait 4000\n' + mark('failed')
                + back_to('Music') + mark('cleared') + 'center\nwait 3000\n' + mark('end'),
                steps=[('after', 0, lambda run: run.fx.nd.set('status', status=503)),
                       ('marker', 'cleared', lambda run: run.fx.nd.set('ok'))],
                measure=measure_page_failure("The server had a problem"), expect_codes=(17,), closes=('UXI-022',)),
        Journey('stream-5xx', 'Navidrome answers 503 to stream requests; then recovers',
                {'message_s': 5, 'truthful_message': True, 'resume_after_retry_s': 3},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + mark('play-press') + PLAY_SONG_1 + 'wait 5000\n'
                + mark('failed') + mark('cleared') + 'play\nawait-play 10000 playing 1000\n' + mark('playing') + mark('end'),
                steps=[('after', 0, lambda run: run.fx.nd.set('status', status=503, paths=('/rest/stream', '/rest/download'))),
                       ('marker', 'cleared', lambda run: run.fx.nd.set('ok'))],
                measure=measure_stream_failure("The server had a problem"), expect_codes=(17,), closes=('UXI-022',)),
        # settled_s: the 15 s page deadline, measured from the marker 0.15 s
        # (one script step) before the press.
        Journey('slow-page', 'Navidrome delays the album list headers 40 s (past the read timeout)',
                {'back_s': 1, 'settled_s': 15.5, 'retry_offered': True},
                'record-text\n' + HOME + TO_ALBUMS + 'wait 2000\n' + back_to('Music') + mark('reopened')
                + 'center\nwait 17000\n' + mark('waited') + mark('end'),
                steps=[('after', 0, lambda run: run.fx.nd.set('delay', seconds=40, paths=('getAlbumList2',)))],
                measure=measure_slow_page, closes=('UXI-019',)),
        Journey('slow-stream', 'Navidrome delays stream headers 40 s (past the read timeout)',
                {'settled_s': 15.5, 'back_s': 1},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + 'scroll 1\n' + mark('play-press')
                + 'center\nawait 15000 screen:Now Playing\nwait 16000\n'
                + mark('waited') + back_to('Generated Album') + mark('end'),
                steps=[('after', 0, lambda run: run.fx.nd.set('delay', seconds=40, paths=('/rest/stream',)))],
                measure=measure_slow_stream),
        Journey('dns-failure', 'Navidrome host name does not resolve; Audiobookshelf up',
                {'message_s': 3, 'truthful_message': True, 'books_s': 3},
                'record-text\n' + HOME + mark('albums-open') + TO_ALBUMS + 'wait 4000\n' + mark('failed')
                + 'back\nback\nawait 5000 screen:Q2\n' + mark('books-open') + TO_BOOKS + mark('books') + mark('end'),
                nd_url='http://q2-fault-nd.invalid:4533',
                measure=measure_dns, expect_codes=(12,), closes=('UXI-022',)),
        Journey('suspend-mid-download', 'player frozen 10 s mid-download while the connection drops (suspend)',
                {'complete_after_resume_s': 30, 'all_downloaded': True, 'no_failure_shown': True},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + DOWNLOAD_ALBUM_ROW + 'center\n' + mark('download')
                + 'wait 1500\n' + mark('freeze') + 'wait 500\n' + mark('resumed')
                + 'back\nback\nback\nawait 5000 screen:Q2\n' + TO_DOWNLOADS + 'wait 28000\n' + mark('end'),
                steps=[('marker', 'freeze', lambda run: suspend(run, 10))],
                measure=measure_suspend, pace=256 * 1024, closes=('UXI-007',)),
        Journey('card-full', 'card has 1.5 MB free; download an album, then stream a song',
                {'message_s': 5, 'back_s': 1, 'stream_plays': True},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + DOWNLOAD_ALBUM_ROW + mark('download') + 'center\n'
                + 'wait 6000\n' + mark('failed') + back_to('Albums') + 'center\nawait 15000 idle:Generated Album\n'
                + 'scroll -4\n' + PLAY_SONG_1 + 'await-play 10000 playing 2000\n' + mark('playing') + mark('end'),
                card_mb=16, card_free_kb=1536, measure=measure_card_full, expect_codes=(23,)),
        Journey('bad-cover', 'the album cover is not an image; download the album',
                {'all_downloaded': True, 'no_failure_shown': True},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + DOWNLOAD_ALBUM_ROW + 'center\n' + mark('download')
                + 'back\nback\nback\nawait 5000 screen:Q2\n' + TO_DOWNLOADS + 'wait 6000\n' + mark('end'),
                steps=[('after', 0, lambda run: run.fx.nd.set('garbage', paths=('getCoverArt',)))],
                measure=measure_downloaded),
        # Cover Flow over 300 albums while a song streams, through a loaded
        # proxy: every answer waits 1 s, every third cover is never answered,
        # and so is the album list the listener opens next (Recently Added,
        # nothing cached). The server must stay online: one lost request is
        # not evidence that the server is down.
        Journey('slow-proxy', 'Navidrome answers 1 s late, drops every third cover during Cover Flow and a stream, '
                'then drops a page the listener waits for',
                {'stayed_online': True, 'no_unavailable_shown': True, 'covers_during_fault': True, 'back_s': 1,
                 'albums_s': 6, 'page_dropped': True},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + PLAY_SONG_1 + 'back\nback\nback\nback\nawait 5000 screen:Q2\n'
                + TO_COVERFLOW + 'wait 1000\n' + mark('loaded') + 'wait 500\n' + 'scroll 2\nwait 600\n' * 40
                + mark('scrolled') + back_to('Music') + mark('albums-open') + 'scroll 3\ncenter\nawait 30000 idle:Albums\n'
                + mark('albums') + 'back\nawait 5000 idle:Music\n' + mark('dropped-page') + 'scroll 6\ncenter\nwait 27000\n'
                + 'back\n' + HOME_FROM_MUSIC + TO_CONNECTIONS + 'wait 2500\n' + mark('end'),
                steps=[('marker', 'loaded', lambda run: run.fx.nd.set('slow', seconds=1.0, drop_every=3, hold=30,
                                                                     paths=('getCoverArt',))),
                       ('marker', 'dropped-page', lambda run: run.fx.nd.set('slow', seconds=1.0, drop_every=3, hold=30,
                                                                           paths=('getCoverArt',), drop_paths=('getAlbumList2',)))],
                measure=measure_slow_proxy, pace=64 * 1024, albums=300),
        # A server that stops answering anything: the page's request waits
        # out the kept connection's 4 s and a new connection's 20 s read
        # timeout, then the probe's two 8 s pings confirm it: 41 s, plus
        # opening Settings. The listener stays on the page: leaving cancels
        # the request, and a cancelled request is not evidence.
        Journey('dead-server', 'Navidrome stops answering (black hole) while a page loads',
                {'offline_s': 50, 'unavailable_shown': True, 'checking_before_offline': True},
                'record-text\n' + HOME + TO_ALBUMS + 'await 15000 idle:Albums\nback\nawait 5000 idle:Music\n'
                # Recently Added (row 6) has nothing cached: the listener waits.
                + mark('dead') + 'scroll 6\ncenter\nwait 27000\nback\nback\nawait 5000 screen:Q2\n'
                + TO_CONNECTIONS + mark('connections') + 'wait 22000\n' + mark('end'),
                steps=[('marker', 'dead', lambda run: run.fx.nd.set('stall'))],
                measure=measure_dead_server),
        Journey('bad-descriptor', 'the playing stream file descriptor becomes invalid (EBADF)',
                {'gap_s': 2, 'same_song': True, 'kept_position': True},
                'record-text\n' + HOME + TO_ALBUMS + OPEN_ALBUM + PLAY_SONG_1 + 'await-play 15000 playing 500\n'
                + mark('fault') + 'fault-descriptor\n' + mark('injected')
                + 'await-play 10000 playing 3000\n' + mark('advanced') + mark('end'),
                measure=measure_descriptor, pace=64 * 1024, expect_codes=(24,), closes=('UXI-006', 'UXI-022')),
    ]


# ---- measures
def measure_link_drop(run):
    fault, advanced = run.play('fault'), run.play('advanced')
    return {'back_s': run.span('press', 'back'), 'resume_after_clear_s': run.span('cleared', 'playing'),
            'same_song': bool(fault and advanced and fault['song'] == advanced['song']),
            'kept_position': bool(fault and advanced and advanced['position_ms'] >= fault['position_ms'] - 2000),
            'song_after': advanced and advanced['song'], 'state_at_clear': (run.play('cleared') or {}).get('state')}


def measure_abs_down(run):
    failed = run.current('failed')
    return {'albums_s': run.span('start', 'albums'),
            'abs_message_s': run.appear("Can't reach Audiobookshelf", 'libraries'),
            'shown_at_failure': failed, 'retry_offered': 'Retry' in failed,
            'back_s': run.span('press', 'back'), 'library_after_retry_s': run.appear('Generated Books', 'abs-up')}


def measure_page_failure(message):
    def measure(run):
        shown = run.current('failed')
        return {'message_s': run.appear(message, 'albums-open'), 'shown_at_failure': shown,
                'truthful_message': message in shown,
                'retry_offered': any('retry' in text.lower() for text in shown),
                'back_s': run.span('press', 'back'), 'recovered_s': run.appear('Generated Album', 'cleared')}
    return measure


def measure_stream_failure(message):
    def measure(run):
        shown = run.current('failed')
        return {'message_s': run.appear(message, 'play-press'), 'shown_at_failure': shown,
                'truthful_message': message in shown, 'resume_after_retry_s': run.span('cleared', 'playing'),
                'song_after': (run.play('playing') or {}).get('song')}
    return measure


def measure_slow_page(run):
    shown = run.current('waited')
    return {'back_s': run.span('press', 'back'), 'settled_s': run.appear(ANY_PLAIN, 'reopened'),
            'shown_at_end': shown, 'retry_offered': any('retry' in text.lower() for text in shown)}


def measure_slow_stream(run):
    return {'settled_s': run.appear(ANY_PLAIN, 'play-press'), 'state_after_16s': (run.play('waited') or {}).get('state'),
            'shown_after_16s': run.current('waited'), 'back_s': run.span('press', 'back')}


def measure_dns(run):
    shown = run.current('failed')
    return {'message_s': run.appear("Can't reach Navidrome", 'albums-open'), 'shown_at_failure': shown,
            'truthful_message': "Can't reach Navidrome" in shown, 'books_s': run.span('books-open', 'books')}


def measure_suspend(run):
    events = run.fx.library.events
    resumed = next((e['at_s'] for e in events if e['kind'] == 'resumed'), None)
    complete = sorted(e['at_s'] for e in events if e['kind'] == 'media' and e['service'] == 'nd' and e['complete'])
    songs = run.downloaded_songs()
    shown = run.current('end')
    return {'complete_after_resume_s': round(complete[-1] - resumed, 2) if resumed and len(complete) >= 3 else None,
            'all_downloaded': songs == 3, 'downloaded_songs': songs, 'shown_at_end': shown,
            'no_failure_shown': not any('fail' in text.lower() for text in shown)}


def measure_downloaded(run):
    songs = run.downloaded_songs()
    shown = run.current('end')
    return {'all_downloaded': songs == 3, 'downloaded_songs': songs, 'shown_at_end': shown,
            'no_failure_shown': not any('fail' in text.lower() for text in shown)}


def measure_card_full(run):
    return {'message_s': run.appear('The card is full', 'download'), 'shown_at_failure': run.current('failed'),
            'back_s': run.span('press', 'back'), 'stream_plays': run.marker('playing') is not None}


def measure_slow_proxy(run):
    log = (run.out / 'player.log').read_text(errors='replace')
    shown = run.current('end')
    seen = {row['text'] for data in run.markers() for row in data.get('seen', ())}
    dropped = sum(1 for e in run.fx.library.events if e['kind'] == 'faulted' and e.get('fault') == 'dropped')
    page = sum(1 for e in run.fx.library.events if e['kind'] == 'faulted' and e.get('fault') == 'dropped'
               and e.get('path') == '/rest/getAlbumList2')
    return {'stayed_online': 'server unreachable' not in log, 'checked': log.count('server checking'),
            'page_dropped': page > 0, 'pages_dropped': page,
            'no_unavailable_shown': 'Unavailable' not in shown and "Can't reach Navidrome" not in seen,
            'covers_during_fault': run.fx.library.count('request', 'nd', path='getCoverArt') >= 20 and dropped > 0,
            'covers_requested': run.fx.library.count('request', 'nd', path='getCoverArt'), 'covers_dropped': dropped,
            'back_s': run.span('press', 'back'), 'albums_s': run.span('albums-open', 'albums'), 'shown_at_end': shown}


def measure_dead_server(run):
    shown = run.current('end')
    log = (run.out / 'player.log').read_text(errors='replace')
    checking, offline = log.find('server checking'), log.find('server unreachable')
    return {'offline_s': run.appear('Unavailable', 'dead'), 'unavailable_shown': 'Unavailable' in shown,
            'checking_before_offline': 0 <= checking < offline, 'shown_at_end': shown}


def measure_descriptor(run):
    fault, advanced = run.play('fault'), run.play('advanced')
    # gap_s: time to hear 3 s of audio after the fault, less those 3 s.
    span = run.span('injected', 'advanced')
    return {'gap_s': round(span - 3.0, 2) if span is not None else None,
            'same_song': bool(fault and advanced and fault['song'] == advanced['song']),
            'kept_position': bool(fault and advanced and advanced['position_ms'] >= fault['position_ms'] - 2000)}


class Run:
    def __init__(self, journey, binary, base):
        self.journey, self.binary = journey, binary
        self.out = base / journey.name
        self.out.mkdir(parents=True)
        self.fx = fixture_module.Fixture(pace=journey.pace, albums=journey.albums)
        self.proc = None
        self.result = {'journey': journey.name, 'fault': journey.fault, 'bound': journey.bound,
                       'closes': list(journey.closes), 'passed': False}

    # ---- observations (marker files written by the script's `text` command)
    def marker_path(self, name):
        return self.out / 'runtime' / f'numeric-{name}.json'

    def marker(self, name):
        try:
            return json.loads(self.marker_path(name).read_text())
        except (OSError, ValueError):
            return None

    def markers(self):
        return [data for data in (json.loads(p.read_text()) for p in (self.out / 'runtime').glob('numeric-*.json'))
                if 'seen' in data]

    def span(self, first, second):
        a, b = self.marker(first), self.marker(second)
        if not a or not b:
            return None
        return round((b['ms'] - a['ms']) / 1000, 2)

    def play(self, name):
        data = self.marker(name)
        return data and data.get('play')

    def current(self, name):
        data = self.marker(name)
        return data['current'] if data else []

    def appear(self, pattern, after):
        """Seconds from marker `after` until `pattern` is on screen (0 if already)."""
        start = self.marker(after)
        if not start:
            return None
        if isinstance(pattern, re.Pattern):
            match = lambda text: bool(pattern.search(text))
        else:
            match = lambda text: text == pattern
        if any(match(text) for text in start['current']):
            return 0.0
        last = max(self.markers(), key=lambda data: data['ms'])
        times = [row['ms'] for row in last['seen'] if row['ms'] >= start['ms'] and match(row['text'])]
        return round((min(times) - start['ms']) / 1000, 2) if times else None

    def downloaded_songs(self):
        path = self.out / 'card' / 'library.db'
        if not path.exists():
            return None
        try:
            with sqlite3.connect(f'file:{path}?mode=ro', uri=True) as db:
                return db.execute('SELECT COUNT(*) FROM offline_songs').fetchone()[0]
        except sqlite3.Error:
            return None

    # ---- execution
    def control(self, stop):
        for trigger, *rest in self.journey.steps:
            if trigger == 'marker':
                name, action = rest
                while not stop.is_set() and not self.marker_path(name).exists():
                    time.sleep(0.02)
            elif trigger == 'after':
                seconds, action = rest
                stop.wait(seconds)
            if stop.is_set():
                return
            self.fx.library.event('harness', 'step', path=str(trigger))
            action(self)

    def execute(self):
        card = self.out / 'card'
        card.mkdir()
        runtime = self.out / 'runtime'
        runtime.mkdir(mode=0o700)
        (runtime / 'numeric-probe-enabled').write_bytes(MARKER)
        (runtime / 'numeric-probe-enabled').chmod(0o600)
        (self.out / 'config.toml').write_text(self.fx.config(self.journey.nd_url, self.journey.abs_url))
        (self.out / 'script.txt').write_text(self.journey.script)
        env = {**os.environ, 'Q2ND_ALSA_DEVICE': 'q2nd-fake', 'Q2ND_NUMERIC_PROBE': '1',
               'Q2ND_INSTALLED_STORAGE': '0', 'Q2ND_RUNTIME': str(runtime)}
        command = [str(self.binary), '--headless', '--data', str(card), '--script', str(self.out / 'script.txt')]
        if self.journey.card_mb:
            # A size-limited card without root: tmpfs in an unprivileged user
            # and mount namespace (host network), filled to leave card_free_kb.
            fill = self.out / 'card.sh'
            fill.write_text(
                'set -e\nmount -t tmpfs -o size=%dk tmpfs "$1"\ncp "$2" "$1/config.toml"\n'
                'dd if=/dev/zero of="$1/filler" bs=1k count=%d 2>/dev/null\nshift 2\nexec "$@"\n'
                % (self.journey.card_mb * 1024, self.journey.card_mb * 1024 - self.journey.card_free_kb))
            command = ['unshare', '-rm', 'sh', str(fill), str(card), str(self.out / 'config.toml')] + command
        else:
            shutil.copy2(self.out / 'config.toml', card / 'config.toml')
        stop = threading.Event()
        controller = threading.Thread(target=self.control, args=(stop,), daemon=True)
        started = time.monotonic()
        with (self.out / 'player.log').open('w') as log:
            self.proc = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
            controller.start()
            try:
                self.result['exit_status'] = self.proc.wait(timeout=240)
            except subprocess.TimeoutExpired:
                self.result['exit_status'] = 'timeout'
            finally:
                stop.set()
                if self.proc.poll() is None:
                    self.proc.kill()
                    self.proc.wait()
                self.fx.close()
                controller.join(timeout=15)
        self.result['elapsed_s'] = round(time.monotonic() - started, 1)
        self.evaluate()
        return self.result

    def evaluate(self):
        log = (self.out / 'player.log').read_text(errors='replace')
        failures = []
        if self.result['exit_status'] != 0:
            script = [line.split('headless script: ', 1)[1] for line in log.splitlines()
                      if 'headless script: ' in line]
            failures.append('script: ' + (script[-1][:300] if script else f"exit {self.result['exit_status']}"))
        if 'panicked' in log:
            failures.append('panic')
        reports = [{'what': m[1], 'code': int(m[2]), 'journey': m[3], 'outcome': m[4], 'latency_ms': int(m[5])}
                   for m in UX_LINE.finditer(log)]
        self.result['ux_reports'] = reports
        codes = sorted({r['code'] for r in reports if r['what'] in ('Error', 'Recovered')})
        self.result['ux_codes'] = codes
        for code in self.journey.expect_codes:
            if code not in codes:
                failures.append(f'ux code {code} not reported')
        # Every string drawn during the run (UX-006).
        seen = {row['text'] for data in self.markers() for row in data.get('seen', ())}
        raw = sorted(text for text in seen if RAW.search(text) and text not in PLAIN_MESSAGES)
        self.result['raw_text'] = raw
        if raw:
            failures.append('raw error text drawn')
        self.result['messages'] = sorted(text for text in seen if text in PLAIN_MESSAGES)
        if self.journey.measure:
            try:
                measured = self.journey.measure(self)
            except Exception as error:  # a measuring bug fails the journey, with its reason
                measured = {'measure_error': repr(error)}
            self.result['measured'] = measured
            for key, limit in self.journey.bound.items():
                value = measured.get(key)
                if limit is True:
                    if value is not True:
                        failures.append(f'{key}: {value}')
                elif value is None or value > limit:
                    failures.append(f'{key}: {value} > {limit}')
        self.result['failures'] = failures
        self.result['passed'] = not failures
        self.result['fixture_events'] = [e for e in self.fx.library.events if e['kind'] != 'request'][-80:]
        self.result['requests'] = self.fx.library.count('request')
        (self.out / 'result.json').write_text(json.dumps(self.result, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/q2nd')
    parser.add_argument('--only', action='append', default=[], help='journey name; repeatable')
    parser.add_argument('--jobs', type=int, default=6, help='journeys run at once')
    parser.add_argument('--out', type=Path, help='output directory below build/')
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f'{binary} missing; build it with cargo build -p q2nd --locked --offline')
    names = [j.name for j in journeys()]
    unknown = [name for name in args.only if name not in names]
    if unknown:
        parser.error(f'unknown journey {unknown}; choose from {names}')
    selected = [j for j in journeys() if not args.only or j.name in args.only]
    base = (args.out or ROOT / 'build/ux-faults' / time.strftime('%Y%m%d-%H%M%S')).resolve()
    if not base.is_relative_to(ROOT / 'build'):
        parser.error('--out must be below build/')
    base.mkdir(parents=True)
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        results = list(pool.map(lambda j: Run(j, binary, base).execute(), selected))
    summary = {'schema': 1, 'binary': str(binary), 'elapsed_s': round(time.monotonic() - started, 1),
               'passed': all(r['passed'] for r in results), 'journeys': [
                   {k: r.get(k) for k in ('journey', 'fault', 'bound', 'measured', 'passed', 'failures', 'ux_codes',
                                          'messages', 'raw_text', 'closes', 'elapsed_s')} for r in results]}
    (base / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    for r in results:
        print(f"{'PASS' if r['passed'] else 'FAIL'} {r['journey']:<22} {'; '.join(r['failures'])}")
    print(f"{base / 'summary.json'} ({summary['elapsed_s']} s)")
    return 0 if summary['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
