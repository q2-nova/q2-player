#!/usr/bin/env python3
"""Heap and latency of browsing a large generated library (host).

Serves a generated Subsonic library (tests/ui/ux_fault_fixture.py's server
with --artists artists, a playlist of --playlist-songs songs, --starred
starred songs and --albums albums) and runs the headless player against it
with tests/perf/heapcount.c preloaded. The script opens Artists, the
playlist, Favorite Songs and Albums (the album catalog sync), --passes times
(later passes read the response cache), scrolls each list, and trims the heap
at the end. For each `memory` step it reports the live heap and the heap peak
since the previous step (heapcount, every malloc), the glibc arena
(mallinfo2), decoded artwork and RSS; for each wait, its wall time.

    cc -O2 -shared -fPIC -o build/heapcount.so tests/perf/heapcount.c -ldl
    python3 tests/perf/document_memory.py --binary build/native-test-target/debug/q2nd --label after

The result is a measurement of one run, not a comparison. Across runs of
the same binary and library the live heap at a step repeats within about
40 KiB and the peak within about 200 KiB (it depends on when concurrent
work allocates); timings vary more and are descriptive only. Host
numbers: x86_64 pointers and allocator, not the Q2's MIPS32 (a record's
pointers and lengths are half as large there). Generated data only.
"""
import argparse
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
import ux_fault_fixture as fixture  # noqa: E402

MEMORY = re.compile(r'script memory (\S+): (\{.*\})')
MARK = re.compile(r'script mark (\S+) boottime_us=(\d+)')
TIMING = re.compile(r'script timing (.+?): step ([\d.]+) ms, since input ([\d.]+) ms')
HEAP = re.compile(r'heapcount seq=(\d+) boottime_us=(\d+) live_kib=(\d+) peak_kib=(\d+) peak_all_kib=(\d+)')
PLAYLIST = 'Generated large playlist'


class AnyGenerated(dict):
    """Every generated song id streams the same generated WAV."""

    def __init__(self, data):
        super().__init__()
        self.data = data

    def get(self, key, default=None):
        return self.data if key.startswith('generated-song-') else default


class LargeLibrary(fixture.Library):
    """The fixture's catalog with large artist, playlist and starred documents."""

    def __init__(self, albums, artists, playlist_songs, starred):
        super().__init__(albums)
        self.audio = AnyGenerated(next(iter(self.audio.values())))
        self.artist_rows = [{'id': f'generated-artist-{n:05d}', 'name': f'Synthetic Artist {n:05d}',
                             'albumCount': 1 + n % 4, 'coverArt': f'generated-artist-{n:05d}'} for n in range(artists)]
        self.playlist_songs = [self.song(n) for n in range(playlist_songs)]
        self.starred_songs = [self.song(n, starred=True) for n in range(0, starred * 3, 3)]

    def song(self, n, starred=False):
        album = self.albums[n % len(self.albums)]
        row = {'id': f'generated-song-{n:06d}', 'parent': album['id'], 'isDir': False,
               'title': f'Generated Song {n:06d}', 'album': album['name'], 'albumId': album['id'],
               'artist': album['artist'], 'artistId': f'generated-artist-{n % max(1, len(self.artist_rows)):05d}',
               'track': 1 + n % 12, 'year': 2000 + n % 25, 'genre': 'Generated', 'coverArt': album['coverArt'],
               'size': len(self.audio.data), 'contentType': 'audio/wav', 'suffix': 'wav',
               'duration': fixture.SONG_SECONDS, 'bitRate': 352, 'path': f'generated/{n:06d}.wav',
               'created': '2026-01-01T00:00:00Z', 'type': 'music', 'mediaType': 'song',
               'replayGain': {'trackGain': -6.5}, 'channelCount': 1, 'samplingRate': fixture.RATE}
        if starred:
            row['starred'] = '2026-01-01T00:00:00Z'
        return row

    def subsonic(self, route, query):
        payload = None
        if route == 'getArtists':
            index = {}
            for artist in self.artist_rows:
                index.setdefault(artist['name'][0], []).append(artist)
            payload = {'artists': {'index': [{'name': k, 'artist': v} for k, v in sorted(index.items())]}}
        elif route == 'getArtist':
            wanted = query.get('id', [''])[0]
            artist = next((a for a in self.artist_rows if a['id'] == wanted), None)
            if artist is not None:
                payload = {'artist': {**artist, 'album': [a for a in self.albums[:artist['albumCount']]]}}
        elif route == 'getPlaylists':
            payload = {'playlists': {'playlist': [self.playlist(False)]}}
        elif route == 'getPlaylist':
            payload = {'playlist': self.playlist(True)}
        elif route == 'getStarred2':
            payload = {'starred2': {'song': self.starred_songs}}
        if payload is None:
            return super().subsonic(route, query)
        return {'subsonic-response': {'status': 'ok', 'version': '1.16.1', 'type': 'generic',
                                      'openSubsonic': False, **payload}}

    def playlist(self, entries):
        row = {'id': 'generated-playlist', 'name': PLAYLIST, 'songCount': len(self.playlist_songs),
               'duration': fixture.SONG_SECONDS * len(self.playlist_songs), 'public': False,
               'owner': 'generated', 'created': '2026-01-01T00:00:00Z', 'changed': '2026-01-01T00:00:00Z',
               'coverArt': 'generated-album'}
        return {**row, 'entry': self.playlist_songs} if entries else row


def script(passes: int, shots: Path, play: bool = False, screen_off: int = 0) -> str:
    # Music rows: Cover Flow 0, Playlists 1, Artists 2, Albums 3, Genres 4,
    # Favorites 5. `scroll N` moves the selection N rows.
    s = ['await 15000 screen:Q2', 'memory boot', 'center', 'await 15000 idle:Music']
    for n in range(passes):
        tag = f'p{n}'
        s += ['scroll 2', 'center', 'await 60000 idle:Artists', f'memory artists-{tag}', f'shot {shots}/artists-{tag}.png',
              'scroll 40', 'present 5000', 'scroll 40', 'present 5000', 'scroll -40', 'present 5000',
              'back', 'await 15000 idle:Music']
        s += ['scroll -1', 'center', 'await 60000 idle:Playlists', 'center', f'await 60000 idle:{PLAYLIST}',
              f'memory playlist-{tag}', f'shot {shots}/playlist-{tag}.png', 'scroll 40', 'present 5000', 'scroll 40', 'present 5000']
        if play:
            # Play the playlist from its top song: the queue takes the list's songs.
            s += ['scroll -200', 'present 5000', 'center', 'await 15000 idle:Now Playing', f'memory playing-{tag}',
                  'back', f'await 60000 idle:{PLAYLIST}']
        s += ['back', 'await 15000 idle:Playlists', 'back', 'await 15000 idle:Music']
        s += ['scroll 4', 'center', 'await 15000 idle:Favorites', 'center', 'await 60000 idle:Favorite Songs',
              f'memory starred-{tag}', f'shot {shots}/starred-{tag}.png', 'scroll 40', 'present 5000', 'scroll 40', 'present 5000',
              'back', 'await 15000 idle:Favorites', 'back', 'await 15000 idle:Music']
        s += ['scroll -2', 'center', 'await 60000 idle:Albums', f'memory albums-{tag}', f'shot {shots}/albums-{tag}.png',
              'scroll 40', 'present 5000', 'scroll 40', 'present 5000',
              'back', 'await 15000 idle:Music', 'scroll -3']
        s += [f'memory after-{tag}']
    if screen_off:
        # The backlight times out after the last input: the screen-off step.
        s += [f'wait {(screen_off + 3) * 1000}', 'memory screen-off']
    s += ['memory-trim end', 'memory after-trim', 'wait 300']
    return '\n'.join(s) + '\n'


def steps(text, heap_text):
    """Per `memory` step: heapcount's live heap and the peak since the previous step, beside the sample."""
    heap = [tuple(map(int, m)) for m in HEAP.findall(heap_text)]
    samples = {name: json.loads(body) for name, body in MEMORY.findall(text)}
    result, last = {}, 0
    for name, t in ((name, int(t)) for name, t in MARK.findall(text)):
        # The sample after the mark calls mallinfo2, which writes heapcount's report.
        own = next((i for i, h in enumerate(heap) if h[1] >= t), None)
        if own is None:
            continue
        window = heap[last:own + 1]
        last = own + 1
        sample = samples.get(name, {})
        result[name] = {'live_kib': heap[own][2], 'peak_kib': max(h[3] for h in window),
                        'arena_kib': sample.get('heap', {}).get('heap_arena_kib'),
                        'mmap_kib': sample.get('heap', {}).get('heap_mmap_kib'),
                        'art_kib': sample.get('heap', {}).get('art_decoded_kib'),
                        'rss_kib': sample.get('player', {}).get('vm_rss_kib'),
                        'hwm_kib': sample.get('player', {}).get('vm_hwm_kib')}
    return result, (heap[-1][4] if heap else None)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--binary', type=Path, default=ROOT / 'build/native-test-target/debug/q2nd')
    p.add_argument('--preload', type=Path, default=ROOT / 'build/heapcount.so')
    p.add_argument('--albums', type=int, default=3000)
    p.add_argument('--artists', type=int, default=2000)
    p.add_argument('--playlist-songs', type=int, default=5000)
    p.add_argument('--starred', type=int, default=2000)
    p.add_argument('--passes', type=int, default=2)
    p.add_argument('--label', default='run')
    p.add_argument('--out', type=Path, help='fresh directory under build/ (default build/perf/document-memory-LABEL-TIME)')
    p.add_argument('--play', action='store_true', help='play the playlist from its top song in each pass')
    p.add_argument('--screen-off', type=int, default=0, metavar='S',
                   help='backlight timeout in seconds; the run waits for it after browsing (memory step screen-off)')
    args = p.parse_args()
    if not args.preload.is_file():
        p.error(f'{args.preload} missing; build it with cc -O2 -shared -fPIC -o {args.preload} tests/perf/heapcount.c -ldl')
    out = (args.out or ROOT / 'build/perf' / f'document-memory-{args.label}-{time.strftime("%Y%m%d-%H%M%S")}').resolve()
    if not out.is_relative_to(ROOT / 'build'):
        p.error('--out must be below build/')
    out.mkdir(parents=True)
    library = LargeLibrary(args.albums, args.artists, args.playlist_songs, args.starred)
    nd = fixture.Service('nd', library)
    card = out / 'card'
    card.mkdir()
    (card / 'config.toml').write_text(
        f'[server]\nurl="http://127.0.0.1:{nd.port}"\nuser="generated"\npassword="synthetic-only"\n'
        f'[display]\nbacklight_seconds={args.screen_off}\nclock_in_title=false\nclicker=false\n'
        '[wifi]\nenabled=false\n[tailscale]\nenabled=false\n[power]\nsleep_minutes=0\n')
    (out / 'script.txt').write_text(script(args.passes, out, args.play, args.screen_off))
    heap_log = out / 'heap.log'
    env = {**os.environ, 'Q2ND_ALSA_DEVICE': 'q2nd-fake', 'Q2ND_INSTALLED_STORAGE': '0',
           'HEAPCOUNT_LOG': str(heap_log), 'LD_PRELOAD': str(args.preload.resolve()), 'RUST_LOG': 'info'}
    started = time.monotonic()
    try:
        with (out / 'player.log').open('x') as log:
            status = subprocess.run([str(args.binary), '--headless', '--data', str(card), '--script', str(out / 'script.txt')],
                                    env=env, stdout=log, stderr=subprocess.STDOUT, timeout=1800).returncode
    finally:
        nd.close()
    wall = time.monotonic() - started
    text = (out / 'player.log').read_text(errors='replace')
    measured, peak_all = steps(text, heap_log.read_text() if heap_log.exists() else '')
    timings = [{'step': s, 'step_ms': float(a), 'since_input_ms': float(b)} for s, a, b in TIMING.findall(text)]
    failed = [line for line in text.splitlines() if 'Script failed' in line or 'ScriptFailed' in line][:5]
    result = {'label': args.label, 'exit_status': status, 'wall_s': round(wall, 1),
              'library': {'albums': args.albums, 'artists': args.artists, 'playlist_songs': args.playlist_songs,
                          'starred_songs': args.starred},
              'peak_all_kib': peak_all, 'steps': measured, 'timings': timings, 'failed': failed,
              'domain': 'host x86_64; not Q2 MIPS32'}
    (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'timings'}, indent=1))
    print(out)
    shutil.rmtree(card, ignore_errors=True)
    return 0 if status == 0 and measured and not failed else 1


if __name__ == '__main__':
    sys.exit(main())
