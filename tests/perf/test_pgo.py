"""Controls for tools/pgo.py and the release profile checks.

The training-set and receipt checks need no compiler or emulator. The
emulator output check runs a PGO and a plain build of the same source under
qemu-mipsel; it needs Q2_PGO_PLAIN_BUILD and Q2_PGO_BUILD (build-target.py
output directories), Q2_SDK and Q2_QEMU_USER.
"""
import json
import os
from pathlib import Path
import re
import runpy
import sqlite3
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools'))
import pgo  # noqa: E402

CANDIDATE = runpy.run_path(str(ROOT / 'tools/release-candidate.py'))
SOURCE = '1' * 40


class TrainingSet(unittest.TestCase):
    def test_training_version_two_covers_codecs_images_and_drawing(self):
        names = [name for name, _ in pgo.training_cases(Path('/fixtures'))]
        for prefix in ('decode/mp3', 'decode/flac', 'decode/vorbis', 'decode/faststart', 'decode/tail-moov',
                       'image/cover.jpg', 'image/cover.png', 'image/cover.webp', 'ui', 'storage-render/', 'eq',
                       'config-startup/'):
            self.assertTrue(any(n.startswith(prefix) for n in names), prefix)
        self.assertEqual(len(names), len(set(names)))
        self.assertEqual(len(names), 31)
        cases = dict(pgo.training_cases(Path('/fixtures')))
        self.assertEqual(cases['ui']['iterations'], 240)
        self.assertEqual(cases['image/cover.jpg@320']['iterations'], pgo.COVER_ITERATIONS)
        self.assertEqual(pgo.TRAINING_VERSION, 2)

    def test_fixtures_cover_every_training_input(self):
        generator = (ROOT / 'tests/perf/fixtures.py').read_text()
        needed = {Path(spec['fixture']).name for _, spec in pgo.training_cases(Path('/fx')) if 'fixture' in spec}
        needed |= set(pgo.SONG_FILES)
        for name in needed:
            self.assertIn(f"'{name}'", generator, name)

    def test_tours_use_only_script_commands(self):
        script = (ROOT / 'crates/q2nd/src/script.rs').read_text()
        commands = set(re.findall(r'"([a-z-]+)" =>', script))
        for name, lines in pgo.TOURS.items():
            for line in lines:
                self.assertIn(line.split()[0], commands, (name, line))
            self.assertIn('{dir}', '\n'.join(lines))

    def test_seed_uses_the_players_schema(self):
        schema = (ROOT / 'crates/q2nd/src/library/db.rs').read_text()
        tables = {name: re.search(r'CREATE TABLE IF NOT EXISTS ' + name + r' \(.*?\);', schema, re.S).group(0)
                  for name in ('offline_songs', 'offline_sets', 'kv')}
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            fx = tmp / 'fx'
            fx.mkdir()
            for name in pgo.SONG_FILES:
                (fx / name).write_bytes(b'generated')
            data = tmp / 'data'
            data.mkdir()
            with sqlite3.connect(data / 'library.db') as db:
                db.executescript('\n'.join(tables.values()))
            pgo.seed(data, fx)
            with sqlite3.connect(data / 'library.db') as db:
                self.assertEqual(db.execute('SELECT count(*) FROM offline_songs').fetchone()[0], len(pgo.SONG_FILES))
                self.assertEqual(db.execute("SELECT count(*) FROM offline_sets WHERE kind='album'").fetchone()[0], 2)
                queue = json.loads(db.execute("SELECT value FROM kv WHERE key='queue'").fetchone()[0])
            self.assertEqual(len(queue['songs']), len(pgo.SONG_FILES))

    def test_hot_paths_and_gaps(self):
        self.assertLessEqual(set(pgo.KNOWN_GAPS), set(pgo.HOT_PATHS))
        for pattern in pgo.HOT_PATHS.values():
            re.compile(pattern)
        self.assertRegex('symphonia_bundle_mp3::layer3::decode', pgo.HOT_PATHS['decoders'])
        self.assertRegex('q2nd::ui::screens::list::draw', pgo.HOT_PATHS['ui draw'])
        self.assertRegex('zune_jpeg::decoder::decode', pgo.HOT_PATHS['cover decode'])


class OutputEquality(unittest.TestCase):
    def test_hashes_collects_checksums_only(self):
        result = {'decode_us': 9, 'sample_md5': 'a', 'positions': [{'pcm_md5': 'b', 'wall_us': 3}],
                  'list_checksums': [1, 2], 'pixel_md5': ['c', 'd'], 'nested': {'checksum': 4, 'x': 5}}
        self.assertEqual(pgo.hashes(result), {'sample_md5': 'a', 'positions[0].pcm_md5': 'b', 'list_checksums': [1, 2],
                                              'pixel_md5': ['c', 'd'], 'nested.checksum': 4})

    def test_output_cases_decode_every_codec_fixture(self):
        with tempfile.TemporaryDirectory() as tmp:
            fx = Path(tmp)
            (fx / 'manifest.json').write_text(json.dumps({'files': {name: {} for name in (
                'mp3.mp3', 'flac.flac', 'vorbis.ogg', 'alac.m4a', 'book-aac-64k-mono-44k.m4b', 'wav.wav',
                'source.wav', 'cover.jpg')}}))
            names = [name for name, _ in pgo.output_cases(fx)]
        for expected in ('decode/mp3.mp3', 'decode/alac.m4a', 'decode/book-aac-64k-mono-44k.m4b', 'decode/wav.wav',
                         'eq', 'ui', 'image/cover.webp@320', 'storage-render/legacy'):
            self.assertIn(expected, names)
        self.assertNotIn('decode/source.wav', names)

    @unittest.skipUnless(all(os.environ.get(k) for k in ('Q2_PGO_PLAIN_BUILD', 'Q2_PGO_BUILD', 'Q2_SDK', 'Q2_QEMU_USER')),
                         'set Q2_PGO_PLAIN_BUILD, Q2_PGO_BUILD, Q2_SDK and Q2_QEMU_USER to compare builds in qemu-mipsel')
    def test_pgo_build_output_equals_plain_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            args = type('Args', (), dict(
                plain=Path(os.environ['Q2_PGO_PLAIN_BUILD']), pgo=Path(os.environ['Q2_PGO_BUILD']),
                sdk=Path(os.environ['Q2_SDK']), qemu_user=Path(os.environ['Q2_QEMU_USER']), cpu='P5600',
                fixtures=Path(os.environ.get('Q2_PGO_FIXTURES', ROOT / 'build/perf/fixtures')),
                out=Path(tmp) / 'compare'))
            report = pgo.compare(args)
        self.assertTrue(report['passed'], report['differences'])
        self.assertGreaterEqual(report['checksums'], 80)


def build_result(**changes):
    receipt = {'program': 'q2nd', 'source_commit': SOURCE, 'profile_sha256': 'p' * 64, 'feature_set_sha256': 'f' * 64,
               'compiler': {'compiler_receipt_sha256': 'c' * 64, 'rustc_sha256': 'r' * 64}, 'training_version': 2,
               'minutes': {'training': 5.0, 'total': 18.0}}
    result = {'source_commit': SOURCE, 'feature_set_sha256': 'f' * 64,
              'inputs_sha256': {'compiler_receipt': 'c' * 64, 'rustc': 'r' * 64, 'profile_use': 'p' * 64},
              'pgo': {'profile_sha256': 'p' * 64, 'receipt': receipt}}
    for path, value in changes.items():
        target = result
        *parents, key = path.split('.')
        for parent in parents:
            target = target[parent]
        target[key] = value
    return result


class ReleaseProfile(unittest.TestCase):
    check = staticmethod(CANDIDATE['check_profile'])

    def test_matching_receipt(self):
        self.assertEqual(self.check(build_result(), SOURCE)['training_version'], 2)

    def test_refusals(self):
        for path, value in (('pgo', None), ('source_commit', '2' * 40), ('pgo.receipt.source_commit', '2' * 40),
                            ('pgo.receipt.compiler.rustc_sha256', '0' * 64),
                            ('inputs_sha256.compiler_receipt', '0' * 64), ('pgo.receipt.feature_set_sha256', '0' * 64),
                            ('inputs_sha256.profile_use', '0' * 64), ('pgo.receipt.program', 'tsrsd')):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.check(build_result(**{path: value}), SOURCE)

    def test_release_workflow_profiles_before_building(self):
        workflow = (ROOT / '.github/workflows/player-rc.yml').read_text()
        profile = workflow.index('tools/pgo.py profile')
        build = workflow.index('tools/build-target.py')
        self.assertLess(workflow.index('cargo "+$nightly" fetch --locked'), profile)
        self.assertLess(profile, build)
        self.assertIn('--profile-use build/pgo/q2nd.profdata', workflow[build:])
        self.assertIn('release-candidate.py check-profile --build-result build/paired-player/result.json',
                      workflow[build:])
        self.assertIn('pgo: {q2nd: .[3].pgo.receipt, tsrsd: .[4].pgo.receipt}', workflow)

    def test_stock_workflow_profiles_before_building(self):
        workflow = (ROOT / '.github/workflows/stock-rc.yml').read_text()
        self.assertLess(workflow.index('tools/pgo.py profile'), workflow.index('tools/build-target.py'))
        self.assertIn('--profile-use build/pgo/q2nd.profdata', workflow)


if __name__ == '__main__':
    unittest.main()
