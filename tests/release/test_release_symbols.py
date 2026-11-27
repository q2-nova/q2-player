"""Pack and check release symbol bundles built from generated files."""
import hashlib
import json
from pathlib import Path
import runpy
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SYMBOLS = runpy.run_path(str(ROOT / 'tools/release-symbols.py'))
VERIFY = runpy.run_path(str(ROOT / 'tools/verify-package.py'))


def sha(data):
    return hashlib.sha256(data).hexdigest()


class ReleaseSymbols(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.build = self.root / 'build'
        self.result = {'passed': True, 'profile': 'release', 'release_eligible': True,
                       'features': {'q2nd': [], 'q2-portal': []}, 'artifacts': {}}
        for name, directory in SYMBOLS['COMPANIONS'].items():
            folder = self.build / directory
            folder.mkdir(parents=True)
            deployed = f'generated {name}'.encode()
            if name == 'q2nd':
                (self.build / 'q2nd').write_bytes(deployed)
            receipt = {'passed': True, 'artifacts': {'deployed': {'sha256': sha(deployed)}}}
            for kind in ('unstripped', 'debug'):
                data = f'generated {name} {kind}'.encode()
                (folder / f'{name}.{kind}').write_bytes(data)
                receipt['artifacts'][kind] = {'sha256': sha(data)}
            raw = json.dumps(receipt).encode()
            (folder / 'receipt.json').write_bytes(raw)
            self.result['artifacts'][name] = {'sha256': sha(deployed), 'symbols_receipt_sha256': sha(raw)}
        self.write_result()
        self.app = sha(b'generated q2nd')

    def write_result(self):
        (self.build / 'result.json').write_text(json.dumps(self.result))

    def test_pack_is_reproducible_and_checks_against_the_packaged_q2nd(self):
        first, second = self.root / 'first.tar.gz', self.root / 'second.tar.gz'
        self.assertEqual(SYMBOLS['pack'](self.build, first), SYMBOLS['pack'](self.build, second))
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(SYMBOLS['check'](first, self.app)['profile'], 'release')
        with self.assertRaisesRegex(ValueError, 'different q2nd'):
            SYMBOLS['check'](first, 'f' * 64)

    def test_refuses_emulator_diagnostic_build(self):
        self.result.update(profile='emulator-diagnostic', release_eligible=False)
        self.write_result()
        with self.assertRaisesRegex(ValueError, 'emulator diagnostic'):
            SYMBOLS['pack'](self.build, self.root / 'out.tar.gz')

    def test_refuses_companion_that_differs_from_its_receipt(self):
        (self.build / 'symbols/q2-portal/q2-portal.debug').write_bytes(b'changed bytes')
        with self.assertRaisesRegex(ValueError, 'q2-portal debug'):
            SYMBOLS['pack'](self.build, self.root / 'out.tar.gz')

    def test_refuses_unexpected_file(self):
        (self.build / 'symbols/extra').write_bytes(b'generated extra')
        with self.assertRaisesRegex(ValueError, 'unexpected'):
            SYMBOLS['pack'](self.build, self.root / 'out.tar.gz')


class DiagnosticBuild(unittest.TestCase):
    def test_package_gate_refuses_sampler_marker(self):
        VERIFY['refuse_diagnostic_build'](b'\x7fELF generated release bytes')
        with self.assertRaisesRegex(ValueError, 'emulator diagnostic build'):
            VERIFY['refuse_diagnostic_build'](b'\x7fELF ' + VERIFY['DIAGNOSTIC_MARKER'] + b': generated')

    def test_paired_receipt_of_diagnostic_build_is_refused(self):
        with tempfile.TemporaryDirectory() as temp:
            receipt = Path(temp) / 'result.json'
            receipt.write_text(json.dumps({'passed': True, 'profile': 'emulator-diagnostic',
                                           'release_eligible': False}))
            with self.assertRaisesRegex(ValueError, 'emulator diagnostic build'):
                VERIFY['paired_build'](receipt, '1' * 40, {}, {})

    def test_marker_matches_the_player_source(self):
        source = (ROOT / 'crates/q2nd/src/diagnostics.rs').read_text()
        self.assertIn(f'pub const DIAGNOSTIC_BUILD: &str = "{VERIFY["DIAGNOSTIC_MARKER"].decode()}:', source)


if __name__ == '__main__':
    unittest.main()
