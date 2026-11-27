#!/usr/bin/env python3
"""Host checks for ux_polish_capture.py's generated Audiobookshelf server:
list pages follow the server's ``limit`` and ``page``, and the capture reads
the player's library sync result. No q2nd build."""
import json
from pathlib import Path
import sys
import unittest
from urllib.request import Request, urlopen

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import ux_polish_capture as upc  # noqa: E402


class Paging(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = upc.Fixture()

    @classmethod
    def tearDownClass(cls):
        cls.fixture.close()

    def get(self, path):
        url = f'http://127.0.0.1:{self.fixture.abs.port}{path}'
        with urlopen(Request(url, headers={'Authorization': 'Bearer generated-ui-token'}), timeout=10) as response:
            return json.loads(response.read())

    def ids(self, body):
        return [row['id'] for row in body['results']]

    def test_items_answer_at_most_limit_rows(self):
        everything = [book['id'] for book in self.fixture.library.books]
        self.assertEqual(len(everything), 4)
        # The player's newest-item marker asks for one row.
        newest = self.get('/api/libraries/generated-books/items?limit=1&page=0&minified=1&sort=addedAt&desc=1')
        self.assertEqual((self.ids(newest), newest['limit'], newest['page'], newest['total']), (everything[:1], 1, 0, 4))
        second = self.get('/api/libraries/generated-books/items?limit=3&page=1')
        self.assertEqual(self.ids(second), everything[3:])
        self.assertEqual(self.ids(self.get('/api/libraries/generated-books/items?limit=3&page=2')), [])
        self.assertEqual(self.ids(self.get('/api/libraries/generated-books/items')), everything)

    def test_groups_and_episodes_page_the_same_way(self):
        self.assertEqual(len(self.get('/api/libraries/generated-books/series?limit=1&page=0')['results']), 1)
        self.assertEqual(self.get('/api/libraries/generated-books/series?limit=1&page=1')['results'], [])
        self.assertEqual(len(self.get('/api/libraries/generated-podcasts/recent-episodes?limit=1&page=0')['episodes']), 1)
        self.assertEqual(self.get('/api/libraries/generated-podcasts/recent-episodes?limit=1&page=1')['episodes'], [])


class SyncResult(unittest.TestCase):
    def test_reads_each_logged_result(self):
        log = ('04:51 INFO: library sync measure: catalog mode=browse ms=3 requests=1 albums=0 ok=true\n'
               '04:51 INFO: library sync measure: abs ms=537 pages_fetched=10 pages_unchanged=0 result=failed\n'
               '05:01 INFO: library sync measure: abs ms=900 pages_fetched=24 pages_unchanged=0 result=complete\n')
        self.assertEqual(upc.sync_results(log), ['failed', 'complete'])
        self.assertEqual(upc.sync_results(''), [])


if __name__ == '__main__':
    unittest.main()
