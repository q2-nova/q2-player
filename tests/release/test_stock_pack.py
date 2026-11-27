"""Check the separation between the normal stock root and the simulator."""
import argparse
import json
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
VERIFY = runpy.run_path(str(ROOT / 'tools/verify-package.py'))
PINS = {'q2-os': '1' * 40, 'linux': '2' * 40}
PLAYER = '3' * 40


class StockPack(unittest.TestCase):
    def test_stock_smoke_observes_only_normal_root(self):
        root = b'Q2_INSTALLED_ROOT_READY'
        simulator = b'Q2_SIM_PLAYER_READY'
        self.assertTrue(VERIFY['installed_ready']([root], stock=True))
        self.assertFalse(VERIFY['installed_ready']([root], stock=False))
        self.assertTrue(VERIFY['installed_ready']([root, simulator], stock=False))
        with self.assertRaisesRegex(ValueError, 'unexpected emulator fixture'):
            VERIFY['installed_ready']([root, simulator], stock=True)

    def test_stock_root_rejects_the_simulator_probe(self):
        listing = b'squashfs-root/sbin/q2-installed-services\nsquashfs-root/sbin/q2-sim-ready\n'
        method = VERIFY['stock_root']
        with patch.dict(method.__globals__, {'filesystem_tool': lambda *_: listing}):
            with self.assertRaisesRegex(ValueError, 'without the QEMU probe'):
                method(Path('.'), PINS, PLAYER, 'V0.42')

    def test_stock_root_rejects_simulator_release_metadata(self):
        listing = b'squashfs-root/sbin/q2-installed-services\n'
        release = dict(qemu_fixture=True, private_configuration_imported=False,
                       release_tag='V0.42', app_source_commit=PLAYER,
                       kernel_source_commit=PINS['linux'])
        method = VERIFY['stock_root']
        def output(_tool, _cwd, option, *_args):
            return listing if option == '-ls' else json.dumps(release).encode()

        with patch.dict(method.__globals__, {'filesystem_tool': output}):
            with self.assertRaisesRegex(ValueError, 'release metadata differs'):
                method(Path('.'), PINS, PLAYER, 'V0.42')
            release['qemu_fixture'] = False
            method(Path('.'), PINS, PLAYER, 'V0.42')

    def test_stock_smoke_refuses_receipt_for_another_component_lock(self):
        with tempfile.TemporaryDirectory() as folder:
            receipt = Path(folder) / 'result.json'
            receipt.write_text(json.dumps(dict(profile='stock-qualification-candidate',
                                               hardware_qualified=False, components=PINS)))
            args = argparse.Namespace(pack_receipt=receipt)
            with self.assertRaisesRegex(ValueError, 'selected component lock'):
                VERIFY['boot_scope'](args, {})
            record, digest = VERIFY['boot_scope'](args, PINS)
            self.assertEqual(record['components'], PINS)
            self.assertEqual(digest, VERIFY['sha'](receipt.read_bytes()))
