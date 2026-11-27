"""Controls for tools/startup-profile.py.

The format checks admit generated profiles with the q2-os consumer
(os/install/startup_read_ahead.py) of the checkout in Q2_OS_REPO, and the
bridge check builds os/platform/display.c from it with the host compiler.
Without Q2_OS_REPO those checks are skipped.
"""
import ctypes
import hashlib
import json
import mmap
import os
from pathlib import Path
import re
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / 'tools/startup-profile.py'
PROFILE = runpy.run_path(str(TOOL))
OS_REPO = os.environ.get('Q2_OS_REPO')
BLOCK = 128 * 1024


def scenes(*samples):
    return {f'scene-{i}': {'blocks': {'home': home, 'first_play': play}, 'evidence': {}}
            for i, (home, play) in enumerate(samples)}


def record(player, blocks_by_scene):
    data = player.read_bytes()
    value = {'schema': 1, 'passed': True, 'block_bytes': BLOCK, 'app_sha256': hashlib.sha256(data).hexdigest(),
             'app_bytes': len(data), 'hardware': False}
    return PROFILE['combine'](value, blocks_by_scene)


class Arguments(unittest.TestCase):
    def run_tool(self, *args):
        return subprocess.run([sys.executable, str(TOOL), *map(str, args)], capture_output=True, text=True)

    def test_refuses_an_existing_profile(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / 'q2nd').write_bytes(b'\x7fELF')
            (tmp / 'startup-read-ahead').mkdir()
            result = self.run_tool('--player', tmp / 'q2nd', '--sdk', tmp, '--qemu-user', tmp / 'q2nd',
                                   '--os-repo', tmp)
            self.assertEqual(result.returncode, 2)
            self.assertIn('exists; profile a fresh build', result.stderr)

    def test_refuses_missing_inputs(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / 'q2nd').write_bytes(b'\x7fELF')
            result = self.run_tool('--player', tmp / 'q2nd', '--sdk', tmp / 'sdk', '--qemu-user', tmp / 'q2nd',
                                   '--os-repo', tmp)
            self.assertEqual(result.returncode, 2)
            self.assertIn('--sdk', result.stderr)
            self.assertFalse((tmp / 'startup-read-ahead').exists())

    def test_refuses_an_os_checkout_without_the_consumer(self):
        with tempfile.TemporaryDirectory() as tmp, self.assertRaises(SystemExit) as refusal:
            PROFILE['consumer'](Path(tmp))
        self.assertIn('no startup read-ahead consumer', str(refusal.exception))

    def test_writes_beside_the_player_where_q2_os_reads_it(self):
        self.assertEqual(PROFILE['SIDECAR'] + '/profile.json', 'startup-read-ahead/profile.json')

    def test_script_uses_only_script_commands(self):
        script = (ROOT / 'crates/q2nd/src/script.rs').read_text()
        commands = set(re.findall(r'"([a-z-]+)" =>', script))
        lines = PROFILE['SCRIPT'].splitlines()
        for line in lines:
            self.assertIn(line.split()[0], commands, line)
        for name, _ in PROFILE['STAGES']:
            self.assertIn(f'text {name}', lines)

    def test_media_are_seeded_songs(self):
        self.assertLessEqual(set(PROFILE['MEDIA']), set(PROFILE['pgo'].SONG_FILES))


class Samples(unittest.TestCase):
    def test_union_per_stage(self):
        value = PROFILE['combine']({}, scenes(([0, 2], [0, 3]), ([0, 1], [0, 2, 3])))
        self.assertEqual(value['blocks'], {'home': [0, 1, 2], 'first_play': [0, 2, 3]})
        self.assertNotIn('sampling', value)

    def test_one_unavailable_sample_reads_the_whole_player(self):
        value = PROFILE['combine']({}, scenes(([0, 2], [0, 3]), (None, [0, 2, 3])))
        self.assertIsNone(value['blocks'])
        self.assertEqual(value['sampling'], 'pagemap-unavailable')

    def test_resident_blocks_of_a_mapped_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp).resolve() / 'mapped'
            # 64 blocks: page cache folios and fault-around map at most 2 MiB (16 blocks) per access.
            path.write_bytes(b'\0' * (64 * BLOCK))
            with path.open('rb') as stream, mmap.mmap(stream.fileno(), 0, prot=mmap.PROT_READ) as mapped:
                _ = mapped[0], mapped[40 * BLOCK + 5]
                try:
                    blocks = PROFILE['resident_blocks'](os.getpid(), path, BLOCK)
                except PermissionError:
                    self.skipTest('pagemap is not readable here')
            self.assertEqual(blocks, sorted(set(blocks)))
            self.assertLessEqual({0, 40}, set(blocks))
            self.assertFalse({20, 63} & set(blocks))

    def test_a_sample_without_the_header_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp).resolve() / 'mapped'
            path.write_bytes(b'\0' * (64 * BLOCK))
            with path.open('rb') as stream, mmap.mmap(stream.fileno(), 0, prot=mmap.PROT_READ) as mapped:
                _ = mapped[40 * BLOCK]
                try:
                    with self.assertRaisesRegex(ValueError, 'header'):
                        PROFILE['resident_blocks'](os.getpid(), path, BLOCK)
                except PermissionError:
                    self.skipTest('pagemap is not readable here')


@unittest.skipUnless(OS_REPO, 'set Q2_OS_REPO to a q2-os checkout to check the format against its consumer')
class Consumer(unittest.TestCase):
    def setUp(self):
        self.reader = PROFILE['consumer'](Path(OS_REPO))
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.player = Path(self.tmp.name).resolve() / 'q2nd'
        self.player.write_bytes(os.urandom(5 * BLOCK + 100))

    def admit(self, value):
        path = Path(self.tmp.name) / 'profile.json'
        path.write_text(json.dumps(value))
        return self.reader.admit(path, self.player)

    def test_block_size(self):
        self.assertEqual(self.reader.BLOCK, BLOCK)

    def test_selected_ranges(self):
        result = self.admit(record(self.player, scenes(([0, 1], [0, 1, 4]), ([0, 2], [0, 4]))))
        self.assertEqual(result['mode'], 'selected')
        self.assertEqual(result['ranges'], [(0, 3), (4, 1)])

    def test_every_block_reads_the_whole_player(self):
        result = self.admit(record(self.player, scenes(([0, 1, 2], [0, 3, 4, 5]))))
        self.assertEqual((result['mode'], result['reason']), ('full', 'all-blocks-observed'))

    def test_unavailable_pagemap_reads_the_whole_player(self):
        result = self.admit(record(self.player, scenes((None, [0]))))
        self.assertEqual((result['mode'], result['reason']), ('full', 'pagemap-unavailable'))

    def test_a_profile_of_another_player_is_refused(self):
        value = record(self.player, scenes(([0], [0])))
        self.player.write_bytes(b'other')
        with self.assertRaises(ValueError):
            self.admit(value)

    @unittest.skipUnless(shutil.which('cc'), 'needs a host C compiler')
    def test_bridge_presents_into_host_buffers(self):
        library = PROFILE['bridge'](Path(self.tmp.name) / 'bridge', 'cc', Path(OS_REPO) / PROFILE['DISPLAY'])
        bridge = ctypes.CDLL(str(library))
        bridge.q2_display_open.restype = ctypes.c_void_p
        for name in ('q2_display_close', 'q2_display_enable', 'q2_display_begin', 'q2_display_descriptor'):
            getattr(bridge, name).argtypes = [ctypes.c_void_p] + {
                'q2_display_enable': [ctypes.c_int],
                'q2_display_begin': [ctypes.POINTER(ctypes.c_uint32), ctypes.c_uint]}.get(name, [])
        display = bridge.q2_display_open()
        self.assertTrue(display)
        pixels = (ctypes.c_uint32 * (375 * 320))()
        self.assertEqual(bridge.q2_display_enable(display, 1), 0)
        self.assertEqual(bridge.q2_display_begin(display, pixels, 375 * 320), 0)
        self.assertEqual(bridge.q2_display_begin(display, pixels, 1), -1)
        self.assertEqual(bridge.q2_display_descriptor(display), -1)
        bridge.q2_display_close(display)


class Workflows(unittest.TestCase):
    def check_order(self, text, build, compose):
        profile = text.index('tools/startup-profile.py')
        self.assertLess(text.index(build), profile)
        self.assertLess(profile, text.index(compose))
        self.assertIn('--player "$PWD/build/paired-player/q2nd"', text[profile:])
        self.assertIn('.startup_read_ahead.app_sha256 == $sha', text[text.index(compose):])

    def test_release_candidate_profiles_the_paired_build_before_composing(self):
        workflow = (ROOT / '.github/workflows/player-rc.yml').read_text()
        self.check_order(workflow, 'tools/build-target.py', 'os/compose_ci.py')
        self.assertLess(workflow.index('tools/startup-profile.py'), workflow.index('name: q2-paired-player'))

    def test_stock_candidate_profiles_the_paired_build_before_composing(self):
        workflow = (ROOT / '.github/workflows/stock-rc.yml').read_text()
        script = (ROOT / 'tools/build-stock-rc.sh').read_text()
        self.assertLess(workflow.index('tools/build-target.py'), workflow.index('tools/startup-profile.py'))
        self.assertLess(workflow.index('tools/startup-profile.py'), workflow.index('tools/build-stock-rc.sh'))
        self.assertIn('.startup_read_ahead.app_sha256 == $sha', script[script.index('compose_stock.py" image'):])


if __name__ == '__main__':
    unittest.main()
