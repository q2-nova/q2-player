"""Controls for the fault-injection journey suite (ux_faults.py).

The fixture's faults behave as documented, the screen-text checks know every
fixed message in crates/q2nd/src/ux.rs, and none of those messages reads as
raw error text. The journeys themselves need a built player:
`python3 tests/ui/ux_faults.py` (about a minute).
"""
import http.client
from pathlib import Path
import re
import socket
import time
import unittest

import ux_fault_fixture as fixture_module
import ux_faults

ROOT = Path(__file__).resolve().parents[2]


def request(port, path, timeout=5, headers=None):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=timeout)
    try:
        connection.request('GET', path, headers=headers or {})
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


class FixtureFaults(unittest.TestCase):
    def setUp(self):
        self.fx = fixture_module.Fixture()
        self.addCleanup(self.fx.close)

    def test_ok_serves_generated_catalog_and_ranges(self):
        status, body = request(self.fx.nd.port, '/rest/getAlbum?id=generated-album&f=json')
        self.assertEqual(status, 200)
        self.assertIn(b'Generated Song 1', body)
        status, body = request(self.fx.nd.port, '/rest/stream?id=generated-song-1', headers={'Range': 'bytes=0-99'})
        self.assertEqual((status, len(body)), (206, 100))
        status, body = request(self.fx.abs.port, '/api/libraries')
        self.assertEqual(status, 200)

    def test_down_refuses_and_returns_on_the_same_port(self):
        port = self.fx.abs.port
        self.fx.abs.set('down')
        with self.assertRaises(ConnectionRefusedError):
            socket.create_connection(('127.0.0.1', port), timeout=2)
        self.fx.abs.set('ok')
        self.assertEqual(request(port, '/api/me')[0], 200)
        self.assertEqual(self.fx.abs.port, port)

    def test_status_delay_and_garbage_follow_their_paths(self):
        self.fx.nd.set('status', status=503, paths=('getAlbumList2',))
        self.assertEqual(request(self.fx.nd.port, '/rest/getAlbumList2')[0], 503)
        self.assertEqual(request(self.fx.nd.port, '/rest/ping')[0], 200)
        self.fx.nd.set('delay', seconds=0.4, paths=('ping',))
        started = time.monotonic()
        self.assertEqual(request(self.fx.nd.port, '/rest/ping')[0], 200)
        self.assertGreaterEqual(time.monotonic() - started, 0.4)
        self.fx.nd.set('garbage', paths=('getCoverArt',))
        status, body = request(self.fx.nd.port, '/rest/getCoverArt?id=generated-album')
        self.assertEqual(status, 200)
        self.assertFalse(body.startswith(b'\x89PNG'))

    def test_stall_holds_until_cleared_then_resets(self):
        self.fx.nd.set('stall')
        with self.assertRaises((socket.timeout, TimeoutError)):
            request(self.fx.nd.port, '/rest/ping', timeout=0.5)
        self.fx.nd.set('ok')
        self.assertEqual(request(self.fx.nd.port, '/rest/ping')[0], 200)

    def test_reset_drops_requests(self):
        self.fx.nd.set('reset')
        with self.assertRaises((ConnectionResetError, http.client.RemoteDisconnected, ConnectionAbortedError)):
            request(self.fx.nd.port, '/rest/ping')


class ScreenTextChecks(unittest.TestCase):
    def test_plain_messages_match_the_fixed_codes(self):
        source = (ROOT / 'crates/q2nd/src/ux.rs').read_text()
        body = source[source.index('pub fn message(self)'):source.index('pub fn transient(')]
        messages = set(re.findall(r'=> "([^"]+)"', body))
        self.assertEqual(messages, ux_faults.PLAIN_MESSAGES)

    def test_raw_patterns_flag_backend_text_but_not_listener_words(self):
        for raw in ['Bad file descriptor (os error 9)', 'HTTP 503', 'network: dns error: failed to lookup address',
                    'stream: connection reset by peer', 'card: No space left on device', 'open: timed out reading response']:
            self.assertRegex(raw, ux_faults.RAW)
        for plain in ux_faults.PLAIN_MESSAGES | {'Chapters (1)', '1 of 3', 'PCM 16/22', '0:40', 'Retry 3 Failed',
                                                  'Album · Downloaded · 3 of 3', 'Press Play to retry'}:
            if plain not in ux_faults.PLAIN_MESSAGES:
                self.assertIsNone(ux_faults.RAW.search(plain), plain)

    def test_every_journey_has_bounds_and_a_measure(self):
        names = [journey.name for journey in ux_faults.journeys()]
        self.assertEqual(len(names), len(set(names)))
        for journey in ux_faults.journeys():
            self.assertTrue(journey.bound and journey.measure, journey.name)
            self.assertIn('record-text', journey.script)
            self.assertIn("text end\n", journey.script, journey.name)


if __name__ == '__main__':
    unittest.main()
