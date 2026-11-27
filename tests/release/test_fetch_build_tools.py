"""Check release-tag selection and reject altered or malformed assets."""
import io
import json
from pathlib import Path
import runpy
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
API = runpy.run_path(str(ROOT / 'tools/fetch-build-tools.py'))


class ReleaseAssets(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.directory = Path(tmp.name)
        self.archive = self.directory / 'compiler.tar.gz'

    def test_checked_in_lock_uses_only_candidate_tags(self):
        data = json.loads((ROOT / 'release-lock.json').read_text())
        API['validate_versions'](data)
        data['q2-os'] = {'run_id': 1, 'sha256': '0' * 64}
        with self.assertRaisesRegex(ValueError, 'release candidate tag'):
            API['validate_versions'](data)

    def test_reject_changed_archive(self):
        self.archive.write_bytes(b'generated exact file')
        checksum = self.directory / 'compiler.tar.gz.sha256'
        checksum.write_text(f"{API['sha'](self.archive)}  {self.archive.name}\n")
        result = API['verify_sums'](self.directory, (self.archive.name,), checksum.name)
        self.assertEqual(result[self.archive.name], API['sha'](self.archive))
        self.archive.write_bytes(b'generated changed file')
        with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
            API['verify_sums'](self.directory, (self.archive.name,), checksum.name)

    def test_reject_duplicate_checksums(self):
        self.archive.write_bytes(b'generated file')
        checksum = self.directory / 'compiler.tar.gz.sha256'
        checksum.write_text(2 * f"{API['sha'](self.archive)}  {self.archive.name}\n")
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            API['verify_sums'](self.directory, (self.archive.name,), checksum.name)

    def test_reject_archive_escape(self):
        with tarfile.open(self.archive, 'w:gz') as archive:
            member = tarfile.TarInfo('install/../../generated-outside')
            member.size = 1
            archive.addfile(member, io.BytesIO(b'X'))
        with self.assertRaisesRegex(ValueError, 'unexpected archive member'):
            API['unpack'](self.archive, self.directory / 'out', {'install'})
        self.assertFalse((self.directory / 'generated-outside').exists())
