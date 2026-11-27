"""Controls for the corresponding-source asset attached to Q2 releases."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('release_source', ROOT / 'tools/release-source.py')
SOURCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SOURCE)


class ReleaseSourceTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.repo = Path(temp.name)
        self.addCleanup(patch.stopall)
        patch.object(SOURCE, 'ROOT', self.repo).start()
        self.git('init', '-q')
        for name in SOURCE.NEEDED | {'docs/relink.md'}:
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('test content\n')
        self.components = {'q2-os': '1' * 40}
        (self.repo / 'component-lock.json').write_text(json.dumps(self.components))
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                 '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null',
                 'commit', '-qm', 'generated source')
        self.commit = self.git('rev-parse', 'HEAD')
        self.package = self.repo / 'update.tar'
        self.package.write_bytes(b'generated package')
        self.record = self.repo / 'result.json'
        self.record.write_text(json.dumps({
            'player_commit': self.commit, 'components': self.components,
            'update_tar_sha256': hashlib.sha256(self.package.read_bytes()).hexdigest(),
        }))
        self.output = self.repo / 'player-source.tar.gz'

    def git(self, *args):
        return subprocess.check_output(['git', '-C', self.repo, *args], text=True).strip()

    def test_archive_contains_committed_codec_and_exact_package_binding(self):
        source = self.repo / SOURCE.OPENAPTX / 'openaptx.c'
        source.write_text('uncommitted source must not appear')
        SOURCE.bundle(self.package, self.record, self.output)
        with tarfile.open(self.output) as archive:
            info = json.load(archive.extractfile('source-manifest.json'))
            self.assertEqual(info['player_commit'], self.commit)
            self.assertEqual(info['update_tar_sha256'], SOURCE.sha(self.package))
            self.assertEqual(archive.extractfile('q2-player/' + SOURCE.OPENAPTX + '/openaptx.c').read(),
                             b'test content\n')
            self.assertEqual(archive.extractfile('LIBOPENAPTX-LICENSE.txt').read(), b'test content\n')
            self.assertTrue(all(not (member.uid or member.gid or member.uname or member.gname)
                                for member in archive.getmembers()))
            self.assertNotIn('q2-player/update.tar', archive.getnames())
        with self.assertRaises(FileExistsError):
            SOURCE.bundle(self.package, self.record, self.output)

    def test_archive_refuses_an_unmatched_firmware(self):
        self.package.write_bytes(b'different package')
        with self.assertRaisesRegex(ValueError, 'not bound'):
            SOURCE.bundle(self.package, self.record, self.output)
        self.assertFalse(self.output.exists())
