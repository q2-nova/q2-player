"""Controls for tools/lastfm_authorize.py with a stand-in Last.fm API."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import tomllib
import unittest
import urllib.parse

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lastfm_authorize as tool  # noqa: E402

CONTACT = 'https://example.test/q2'


class Response(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class Api:
    """Answers auth.getToken and auth.getSession; records each query."""

    def __init__(self, session='generated-session', error=None):
        self.session, self.error, self.queries, self.agents = session, error, [], []

    def open(self, request, timeout):
        query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(request.full_url).query))
        self.queries.append(query)
        self.agents.append(request.get_header('User-agent'))
        if self.error:
            return Response(json.dumps({'error': self.error, 'message': 'refused'}).encode())
        if query['method'] == 'auth.getToken':
            return Response(b'{"token": "generated-token"}')
        return Response(json.dumps({'session': {'name': 'listener', 'key': self.session}}).encode())


def run(argv, api, answers=('generated-key', 'generated-secret')):
    links, replies = [], iter(answers)
    with contextlib.redirect_stdout(io.StringIO()):
        tool.main(argv, opener=api, ask=lambda _prompt: next(replies), confirm=links.append)
    return links


class Authorize(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.out = Path(self.tmp.name) / 'lastfm.toml'

    def test_writes_a_private_stanza_the_player_reads(self):
        api = Api()
        links = run(['--out', str(self.out), '--contact-url', CONTACT], api)
        self.assertEqual(stat.S_IMODE(self.out.stat().st_mode), 0o600)
        self.assertEqual(tomllib.loads(self.out.read_text())['lastfm'], {
            'enabled': True, 'api_key': 'generated-key', 'shared_secret': 'generated-secret',
            'session': 'generated-session', 'contact_url': CONTACT})
        self.assertEqual(links, [tool.AUTHORIZE + '?api_key=generated-key&token=generated-token'])
        self.assertEqual([q['method'] for q in api.queries], ['auth.getToken', 'auth.getSession'])
        self.assertEqual(api.queries[1]['token'], 'generated-token')
        self.assertEqual(set(api.agents), {f'q2nd setup ({CONTACT})'})

    def test_signature_is_md5_of_sorted_pairs_and_secret(self):
        query = dict(urllib.parse.parse_qsl(tool.signed('auth.getSession', 'k', 's', token='t')))
        expected = hashlib.md5(b'api_keykmethodauth.getSessiontokents').hexdigest()
        self.assertEqual(query, {'api_key': 'k', 'method': 'auth.getSession', 'token': 't',
                                 'api_sig': expected, 'format': 'json'})

    def test_contact_url_must_be_plain_https(self):
        self.assertIsNone(tool.contact_refusal(CONTACT))
        for url in ('http://example.test/q2', 'https://user:pw@example.test/', 'https://example.test/?q=1',
                    'https://example.test/#a', 'https://exämple.test/', 'https://example.test/ q2', 'https:///q2'):
            with self.subTest(url=url):
                self.assertIsNotNone(tool.contact_refusal(url))

    def test_refusals_write_nothing(self):
        for argv, answers in ((['--contact-url', 'http://example.test/'], None),
                              (['--contact-url', CONTACT], ('', 'generated-secret')),
                              (['--contact-url', CONTACT], ('generated key', 'generated-secret'))):
            with self.subTest(argv=argv, answers=answers), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as refusal:
                    run(['--out', str(self.out), *argv], Api(), answers or ('k', 's'))
                self.assertEqual(refusal.exception.code, 2)
                self.assertFalse(os.path.lexists(self.out))

    def test_keeps_an_existing_file(self):
        self.out.write_text('kept\n')
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            run(['--out', str(self.out), '--contact-url', CONTACT], Api())
        self.assertEqual(self.out.read_text(), 'kept\n')

    def test_api_errors_and_missing_sessions_write_nothing(self):
        for api in (Api(error=14), Api(session='')):
            with self.subTest(error=api.error), self.assertRaises(SystemExit) as refusal:
                run(['--out', str(self.out), '--contact-url', CONTACT], api)
            self.assertIsInstance(refusal.exception.code, str)
            self.assertNotIn('generated-secret', refusal.exception.code)
            self.assertFalse(os.path.lexists(self.out))


if __name__ == '__main__':
    unittest.main()
