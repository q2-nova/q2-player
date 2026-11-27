"""Controls for the request and byte accounting of tests/perf/sync_host_measure.py.

A generated upstream on loopback records what reaches it; requests go through
the real proxy handler with phase marks as the Rust test sends them.
"""
import http.server
import json
import shutil
import subprocess
import tempfile
from pathlib import Path
import sys
import threading
import unittest
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sync_host_measure as measure  # noqa: E402

OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class Upstream(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    seen = []

    def log_message(self, *_):
        pass

    def do_GET(self):
        Upstream.seen.append((self.path, self.headers.get('Authorization')))
        if self.path.startswith('/missing'):
            body, status = b'{"error":"not found"}', 404
        else:
            size = int(urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query).get('size', ['100'])[0])
            body, status = b'x' * size, 200
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('ETag', '"generated"')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def get(base, path):
    try:
        with OPENER.open(base + path, timeout=5) as reply:
            return reply.status, reply.read(), reply.headers
    except urllib.error.HTTPError as e:
        with e:
            return e.code, e.read(), e.headers


class Accounting(unittest.TestCase):
    def setUp(self):
        Upstream.seen = []
        self.upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        threading.Thread(target=self.upstream.serve_forever, daemon=True).start()
        origin = f'http://127.0.0.1:{self.upstream.server_address[1]}'
        self.stats = measure.Stats()
        self.proxy, self.base = measure.start_proxy(
            measure.Upstream(origin, measure.subsonic_params('generated', 'synthetic-only'), origin, 'generated-token'),
            self.stats)

    def tearDown(self):
        self.proxy.shutdown()
        self.proxy.server_close()
        self.upstream.shutdown()
        self.upstream.server_close()

    def test_counts_requests_status_and_bytes_by_phase_and_endpoint(self):
        get(self.base, '/rest/ping.view?u=player&p=placeholder&size=10')
        get(self.base, '/__phase/catalog-full')
        for size in (1000, 2345):
            get(self.base, f'/rest/getAlbumList2?u=player&t=x&s=y&size={size}')
        get(self.base, '/__phase/abs-full')
        status, body, headers = get(self.base, '/api/libraries/lib-0001/items?size=77')
        self.assertEqual((status, len(body), headers['ETag']), (200, 77, '"generated"'))
        self.assertEqual(get(self.base, '/missing/item-9')[0], 404)
        get(self.base, '/__phase/end')
        get(self.base, '/rest/ping?size=5')
        summary = self.stats.summary({'catalog-full': {'albums': 3}})
        self.assertEqual(set(summary), {'catalog-full', 'abs-full'})
        catalog = summary['catalog-full']
        self.assertEqual((catalog['requests'], catalog['body_bytes'], catalog['status']), (2, 3345, {'200': 2}))
        self.assertEqual(catalog['by_endpoint'], {'rest/getAlbumList2': [2, 3345]})
        self.assertEqual(catalog['player'], {'albums': 3})
        self.assertIsNotNone(catalog['wall_s'])
        shelf = summary['abs-full']
        self.assertEqual((shelf['requests'], shelf['status']), (2, {'200': 1, '404': 1}))
        self.assertEqual(shelf['body_bytes'], 77 + len(b'{"error":"not found"}'))
        self.assertEqual(shelf['by_endpoint'], {'*/*': [1, 21], 'api/libraries/*/items': [1, 77]})

    def test_player_credentials_are_replaced_and_shelf_calls_carry_the_token(self):
        get(self.base, '/rest/getAlbumList2.view?u=player&p=placeholder&t=a&s=b&c=q2nd&apiKey=k&size=1')
        get(self.base, '/api/me')
        (subsonic, no_auth), (shelf, bearer) = Upstream.seen
        query = urllib.parse.parse_qs(urllib.parse.urlsplit(subsonic).query)
        self.assertEqual(query['u'], ['generated'])
        self.assertEqual(query['c'], ['q2measure'])
        self.assertNotIn('p', query)
        self.assertNotIn('apiKey', query)
        self.assertNotIn('player', subsonic)
        self.assertIsNone(no_auth)
        self.assertEqual((shelf, bearer), ('/api/me', 'Bearer generated-token'))

    def test_without_an_audiobookshelf_upstream_shelf_paths_are_refused(self):
        self.proxy.shutdown()
        self.proxy.server_close()
        origin = f'http://127.0.0.1:{self.upstream.server_address[1]}'
        self.proxy, self.base = measure.start_proxy(
            measure.Upstream(origin, measure.subsonic_params('generated', 'synthetic-only')), self.stats)
        self.assertEqual(get(self.base, '/api/me')[0], 404)
        self.assertEqual(Upstream.seen, [])


class Parsing(unittest.TestCase):
    def test_endpoint_kinds_drop_ids(self):
        self.assertEqual(measure.kind_of('/rest/getAlbumList2.view?size=500'), 'rest/getAlbumList2')
        self.assertEqual(measure.kind_of('/rest/'), 'rest/')
        self.assertEqual(measure.kind_of('/api/libraries/lib-x/personalized'), 'api/libraries/*/personalized')
        self.assertEqual(measure.kind_of('/api/items/li_123/cover?width=400'), 'api/items/*/cover')

    def test_player_lines(self):
        text = ('noise\nsync-measure catalog-full ms=12 rows=600 wal_bytes=4096 albums=600\n'
                'sync-measure abs-verify-series equal=3 reordered=0 other=0 not_stored=0 fetch_failed=0\n')
        self.assertEqual(measure.player_lines(text), {
            'catalog-full': {'ms': 12, 'rows': 600, 'wal_bytes': 4096, 'albums': 600},
            'abs-verify-series': {'equal': 3, 'reordered': 0, 'other': 0, 'not_stored': 0, 'fetch_failed': 0}})
        self.assertEqual(json.loads(json.dumps(measure.player_lines(''))), {})


@unittest.skipUnless(shutil.which('ffmpeg') and shutil.which('ffprobe'), 'needs FFmpeg')
class GeneratedMusic(unittest.TestCase):
    def test_tracks_carry_their_own_tags_and_decode(self):
        with tempfile.TemporaryDirectory() as tmp:
            music = Path(tmp) / 'music'
            measure.generate_music(music, 4, Path(tmp))
            files = sorted(music.rglob('*.flac'))
            self.assertEqual(len(files), 4 * measure.TRACKS)
            probe = json.loads(subprocess.run(
                ['ffprobe', '-v', 'error', '-show_entries', 'format_tags:format=duration', '-of', 'json', str(files[-1])],
                capture_output=True, text=True, check=True).stdout)['format']
            tags = {k.upper(): v for k, v in probe['tags'].items()}
            self.assertEqual((tags['ALBUM'], tags['ARTIST'], tags['TRACK'] if 'TRACK' in tags else tags['TRACKNUMBER']),
                             ('Generated Album 00003', 'Synthetic Artist 03', '3'))
            self.assertAlmostEqual(float(probe['duration']), 1.0, delta=0.05)
            subprocess.run(['ffmpeg', '-v', 'error', '-nostdin', '-i', str(files[0]), '-f', 'null', '-'], check=True)

    def test_refuses_a_file_that_is_not_flac(self):
        with self.assertRaises(ValueError):
            measure.tagged_flac(b'RIFF0000WAVE', {})


if __name__ == '__main__':
    unittest.main()
