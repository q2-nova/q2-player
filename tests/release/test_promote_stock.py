from io import BytesIO
import json
from pathlib import Path
import runpy
import tarfile
import tempfile
import unittest
from unittest.mock import patch


PROMOTE = runpy.run_path(str(Path(__file__).resolve().parents[2] / 'tools/promote-stock.py'))
TAG = 'stock-v0.1.0-rc.4'
ROOTFS = {'recovery-update/rootfs.squashfs': {'sha256': 'd' * 64}}
COMMIT = 'a' * 40


class PromoteStockTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.qualified = self.directory / 'hardware.json'
        os_tag = 'v0.1.0-rc.5'
        os_sha = 'b' * 40
        legal_sha = 'c' * 64
        os_input = {'repository': 'q2-nova/q2-os', 'tag': os_tag, 'commit': os_sha,
                    'assets_sha256': {'buildroot-legal-info.tar.gz': legal_sha}}
        self.os_release = {'tagName': os_tag, 'isDraft': False, 'assets': [
            {'name': 'buildroot-legal-info.tar.gz', 'digest': 'sha256:' + legal_sha}]}
        archive = self.directory / 'update.tar'
        archive.write_bytes(b'generated updater')
        self.sha = PROMOTE['STOCK']['digest'](archive)
        (self.directory / 'q2nd-symbols.tar.gz').write_bytes(b'generated symbols')
        self.report = {
            'version': 'V0.10', 'starting_version': 'V1.32', 'update_tar_sha256': self.sha,
            'player_commit': COMMIT, 'player_tag': TAG, 'components': {'q2-os': os_sha},
            'release_inputs': {'q2-os': os_input}, 'profile': 'stock-qualification-candidate',
            'installed_root_ready': True, 'player_ready': None, 'hardware_qualified': False,
        }
        self.report['stock_pack'] = {key: self.report[key] for key in
                                     ('version', 'starting_version', 'update_tar_sha256')}
        self.report['stock_pack']['image_manifest_sha256'] = 'e' * 64
        radio = json.dumps(dict(schema=1, os_source_commit=os_sha, image_manifest_sha256='e' * 64,
                                rootfs_sha256=ROOTFS['recovery-update/rootfs.squashfs']['sha256'])).encode()
        with tarfile.open(self.directory / 'stock-radio-source.tar.gz', 'w:gz') as bundle:
            member = tarfile.TarInfo('./radio-source.json')
            member.size = len(radio)
            bundle.addfile(member, BytesIO(radio))
        (self.directory / 'result.json').write_text(json.dumps(self.report))
        physical = {key: self.report[key] for key in
                    ('version', 'starting_version', 'update_tar_sha256', 'player_commit', 'components')}
        physical.update(schema=1, domain='physical-stock-q2', device_model='Shanling Q2',
                        stock_install_passed=True, generated_local_playback_passed=True,
                        recovery_passed=True, conditions='generated', procedure='generated')
        self.qualified.write_text(json.dumps(physical))
        self.write_source(self.sha)
        admission = PROMOTE['STOCK']['admit'].__globals__
        patches = patch.dict(admission, read_archive=lambda *_: {'version': 'V0.10', 'files': ROOTFS},
                             symbols=lambda *_: None)
        patches.start()
        self.addCleanup(patches.stop)

    def write_source(self, updater_sha):
        manifest = {
            'player_commit': COMMIT, 'update_tar_sha256': updater_sha,
            'components': self.report['components'], 'tagged_inputs': self.report['release_inputs'],
        }
        with tarfile.open(self.directory / 'player-source.tar.gz', 'w:gz') as bundle:
            for name, data in [('source-manifest.json', json.dumps(manifest)),
                               ('RELINK.md', 'generated'),
                               ('LIBOPENAPTX-LICENSE.txt', 'generated'),
                               ('LIBOPENAPTX-NOTICE.txt', 'generated')]:
                raw = data.encode()
                member = tarfile.TarInfo(name)
                member.size = len(raw)
                bundle.addfile(member, BytesIO(raw))
        stock = PROMOTE['STOCK']
        (self.directory / 'SHA256SUMS').write_text(''.join(
            f'{stock["digest"](self.directory / name)}  {name}\n' for name in stock['PAYLOADS']))
        self.draft = {'isDraft': True, 'isPrerelease': True, 'tagName': TAG, 'assets': [
            {'name': name, 'digest': 'sha256:' + stock['digest'](self.directory / name)}
            for name in PROMOTE['ASSETS']]}

    def check(self):
        return PROMOTE['check'](self.draft, self.directory, TAG, COMMIT, self.directory,
                                self.qualified, self.os_release)

    def test_exact_checked_candidate(self):
        self.assertEqual(self.check()['update.tar'], 'sha256:' + self.sha)

    def test_modified_draft_rejected(self):
        (self.directory / 'update.tar').write_bytes(b'different')
        with self.assertRaisesRegex(ValueError, 'differ from the GitHub'):
            self.check()

    def test_unqualified_hardware_rejected(self):
        physical = json.loads(self.qualified.read_text())
        for change in ({'recovery_passed': False}, {'update_tar_sha256': '1' * 64}):
            self.qualified.write_text(json.dumps(physical | change))
            with self.assertRaisesRegex(ValueError, 'does not cover the exact package'):
                self.check()

    def test_source_for_other_archive_rejected(self):
        self.write_source('1' * 64)
        with self.assertRaisesRegex(ValueError, 'source does not match'):
            self.check()

    def test_corresponding_os_sources_must_still_match(self):
        self.os_release['assets'][0]['digest'] = 'sha256:' + '1' * 64
        with self.assertRaisesRegex(ValueError, 'source release is unavailable'):
            self.check()
