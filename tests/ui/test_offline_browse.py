"""Controls for the offline browsing journey (offline_browse.py).

The generated catalog answers every browse request consistently, a stopped
server also drops the connection the player keeps open, and each run's
markers are its own. The journey itself needs a built player:
`python3 tests/ui/offline_browse.py`.
"""
import http.client
import re
import socket
import unittest

import offline_browse


def get(port, path, connection=None):
    connection = connection or http.client.HTTPConnection('127.0.0.1', port, timeout=5)
    connection.request('GET', path)
    response = connection.getresponse()
    return response.status, response.read()


class GeneratedCatalog(unittest.TestCase):
    def setUp(self):
        self.library = offline_browse.CatalogLibrary()

    def answer(self, route, **query):
        return self.library.subsonic(route, {key: [value] for key, value in query.items()})['subsonic-response']

    def test_artists_albums_and_songs_agree(self):
        artists = self.answer('getArtists')['artists']['index'][0]['artist']
        self.assertEqual([a['name'] for a in artists],
                         ['Generated Artist Alpha', 'Generated Artist Beta', 'Generated Artist Gamma'])
        names = set()
        for artist in artists:
            albums = self.answer('getArtist', id=artist['id'])['artist']['album']
            self.assertEqual(len(albums), artist['albumCount'])
            for album in albums:
                songs = self.answer('getAlbum', id=album['id'])['album']['song']
                self.assertEqual([s['track'] for s in songs], [1, 2])
                self.assertTrue(all(s['albumId'] == album['id'] and s['artistId'] == artist['id'] for s in songs))
                self.assertTrue(all(s['size'] == len(self.library.audio[s['id']]) for s in songs))
                names.add(album['name'])
        self.assertTrue(set(offline_browse.DOWNLOADED) < names)
        listed = self.answer('getAlbumList2', type='alphabeticalByName', size='500', offset='0')['albumList2']['album']
        self.assertEqual([a['name'] for a in listed], sorted(names))
        self.assertEqual(self.answer('getAlbum', id='missing')['status'], 'failed')

    def test_download_rows_match_the_alphabetical_album_list(self):
        # Albums lists Shuffle Albums, then the catalog by name.
        for row, album in ((1, 'Alpha One'), (2, 'Alpha Two'), (3, 'Beta One')):
            self.assertEqual(self.library.albums[row - 1]['name'], album)
            self.assertIn(f'scroll {row}\ncenter\nawait 15000 idle:{album}\n', offline_browse.download(row, album))


class StoppedServer(unittest.TestCase):
    def test_stop_refuses_and_drops_kept_connections(self):
        service = offline_browse.StoppedService('nd', offline_browse.CatalogLibrary())
        self.addCleanup(service.close)
        kept = http.client.HTTPConnection('127.0.0.1', service.port, timeout=5)
        self.assertEqual(get(service.port, '/rest/ping', kept)[0], 200)
        service.set('down')
        with self.assertRaises(ConnectionRefusedError):
            socket.create_connection(('127.0.0.1', service.port), timeout=2)
        with self.assertRaises((ConnectionError, http.client.HTTPException, OSError)):
            get(service.port, '/rest/ping', kept)
        service.set('ok')
        self.assertEqual(get(service.port, '/rest/ping')[0], 200)


class Scripts(unittest.TestCase):
    def test_each_run_publishes_its_own_markers(self):
        seen = []
        for name, _, script, steps, offline_mode in offline_browse.runs():
            markers = re.findall(r'^text (\S+)$', script, re.M)
            self.assertIn(f'{name}-start', markers)
            self.assertIn(f'{name}-end', markers)
            self.assertEqual(offline_mode, f'{name}-offline-mode' in markers)
            for marker, _ in steps:
                self.assertIn(marker, markers)
            for marker in markers:
                # The player's numeric probe accepts these names only.
                self.assertRegex(marker, r'^[A-Za-z0-9_-]{1,32}$')
            seen += markers
        self.assertEqual(len(seen), len(set(seen)), 'a numeric marker is published once per runtime')

    def test_config_holds_only_generated_values(self):
        text = offline_browse.config(4533)
        self.assertIn('url="http://127.0.0.1:4533"', text)
        self.assertIn('user="generated"', text)


if __name__ == '__main__':
    unittest.main()
