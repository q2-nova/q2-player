"""Exercise release-input and physical-stock admission using generated data."""
import json
from pathlib import Path
import runpy
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
API = runpy.run_path(str(ROOT / 'tools/release-candidate.py'))
SOURCE = '1' * 40
PINS = {'linux': '2' * 40, 'q2-os': '3' * 40, 'tsrs': '4' * 40}


class Candidate(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.bundle = Path(temp.name)
        (self.bundle / 'image').mkdir()
        self.qualification = self.bundle / 'hardware-qualification.json'
        self.record = {'schema': 1, 'mode': 'stock-draft', 'player_commit': SOURCE,
                       'components': PINS, 'version': 'V0.02', 'installed_version': 'V0.01'}
        self.evidence = {'schema': 1, 'domain': 'physical-stock-q2', 'player_commit': SOURCE,
                         'components': PINS, 'device_model': 'Shanling Q2',
                         'version': 'V0.02', 'starting_version': 'V0.01',
                         'update_tar_sha256': 'a' * 64, 'stock_install_passed': True,
                         'generated_local_playback_passed': True, 'recovery_passed': True,
                         'conditions': 'synthetic example', 'procedure': 'synthetic example'}
        self.qualification.write_text(json.dumps(self.evidence))
        fixture_package = self.bundle / 'update.tar'
        fixture_package.write_text('synthetic archive marker')
        files = {self.qualification.name: API['sha'](self.qualification),
                 fixture_package.name: API['sha'](fixture_package)}
        for name in API['IMAGE_FILES']:
            path = self.bundle / 'image' / name
            path.write_text('generated fixture marker')
            files['image/' + name] = API['sha'](path)
        self.record['files'] = files
        self.manifest = self.bundle / 'release-inputs.json'
        self.manifest.write_text(json.dumps(self.record))
        self.manifest_sha = API['sha'](self.manifest)

    def admit(self, mode='stock-draft', source=SOURCE):
        return API['admit'](self.bundle, self.manifest_sha, mode, source, PINS)

    def test_admit_exact_manual_input_bundle(self):
        self.assertEqual(self.admit(), self.record)

    def test_qemu_input_cannot_enter_stock_gate(self):
        with self.assertRaisesRegex(ValueError, 'mode or locked source'):
            self.admit('qemu-rehearsal')

    def test_old_player_source_rejected(self):
        with self.assertRaisesRegex(ValueError, 'mode or locked source'):
            self.admit(source='5' * 40)

    def test_tampered_input_rejected(self):
        (self.bundle / 'image/rootfs.squashfs').write_text('changed bytes')
        with self.assertRaisesRegex(ValueError, 'rootfs.squashfs'):
            self.admit()

    def test_input_symlink_rejected(self):
        rootfs = self.bundle / 'image/rootfs.squashfs'
        relocated = self.bundle / 'relocated'
        rootfs.rename(relocated)
        rootfs.symlink_to(relocated)
        with self.assertRaisesRegex(ValueError, 'regular in-bundle'):
            self.admit()

    def test_hardware_evidence_matches_exact_package(self):
        qualify = API['stock_qualification']
        with self.assertRaisesRegex(ValueError, 'exact package and sources'):
            qualify(self.qualification, SOURCE, PINS, self.record, 'b' * 64)
        self.assertEqual(qualify(self.qualification, SOURCE, PINS, self.record, 'a' * 64), self.evidence)

    def test_emulator_boot_evidence_refused_for_stock(self):
        self.evidence['domain'] = 'direct kernel boot in QEMU'
        self.qualification.write_text(json.dumps(self.evidence))
        with self.assertRaisesRegex(ValueError, 'manual stock hardware'):
            API['stock_qualification'](self.qualification, SOURCE, PINS, self.record, 'a' * 64)
