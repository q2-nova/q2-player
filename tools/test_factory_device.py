#!/usr/bin/env python3
"""Host checks for the device transport through a q2-factory checkout, with
a generated stand-in for the factory's device probe (no factory, no Q2)."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE))
import factory_device  # noqa: E402

# Records its arguments, stdin and lease variable, then exits FAKE_PROBE_EXIT.
FAKE_PROBE = r'''import json, os, sys
from pathlib import Path
record = Path(__file__).with_name('calls.jsonl')
with record.open('a') as out:
    out.write(json.dumps({'argv': sys.argv[1:], 'stdin': sys.stdin.read(),
                          'lease': os.environ.get('Q2_LEASE_HELD')}) + '\n')
print('generated probe output', flush=True)
sys.exit(int(os.environ.get('FAKE_PROBE_EXIT', '0')))
'''


class Factory:
    """A directory laid out as a q2-factory checkout, holding the stand-in probe."""

    def __init__(self, root):
        self.root = Path(root)
        probe = self.root / factory_device.PROBE
        probe.parent.mkdir(parents=True)
        probe.write_text(FAKE_PROBE)

    def calls(self):
        path = self.root / factory_device.PROBE.parent / 'calls.jsonl'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def environment(**extra):
    env = {k: v for k, v in os.environ.items() if k not in (factory_device.ENV, 'Q2_LEASE_HELD', 'FAKE_PROBE_EXIT')}
    env.update(extra)
    return env


class ProbePath(unittest.TestCase):
    def test_refuses_without_a_checkout(self):
        with self.assertRaisesRegex(factory_device.NoFactory, f'--factory PATH or set {factory_device.ENV}'):
            factory_device.probe_path(None, {})
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaisesRegex(factory_device.NoFactory, 'not a q2-factory checkout'):
                factory_device.probe_path(d, {})

    def test_explicit_path_wins_over_the_environment(self):
        with tempfile.TemporaryDirectory() as a, tempfile.TemporaryDirectory() as b:
            Factory(a), Factory(b)
            self.assertEqual(factory_device.probe_path(None, {factory_device.ENV: a}), Path(a).resolve() / factory_device.PROBE)
            self.assertEqual(factory_device.probe_path(b, {factory_device.ENV: a}), Path(b).resolve() / factory_device.PROBE)

    def test_argv_streams_with_the_budget(self):
        with tempfile.TemporaryDirectory() as d:
            Factory(d)
            argv = factory_device.argv(300.7, d, {})
            self.assertEqual(argv[1:], [str(Path(d).resolve() / factory_device.PROBE), '--stream', '--timeout', '300'])


class Run(unittest.TestCase):
    def test_script_goes_to_the_probe_and_its_exit_comes_back(self):
        with tempfile.TemporaryDirectory() as d:
            factory = Factory(d)
            for code in (0, factory_device.BUSY):
                r = subprocess.run([sys.executable, '-c',
                                    'import sys, factory_device; sys.exit(factory_device.run(b"echo generated\\n", 30))'],
                                   cwd=HERE, env=environment(**{factory_device.ENV: d, 'FAKE_PROBE_EXIT': str(code)}),
                                   capture_output=True, text=True, timeout=60)
                self.assertEqual(r.returncode, code, r.stderr)
                self.assertIn('generated probe output', r.stdout)
            calls = factory.calls()
            self.assertEqual([c['stdin'] for c in calls], ['echo generated\n'] * 2)
            self.assertEqual(calls[0]['argv'], ['--stream', '--timeout', '30'])

    def test_a_held_lease_reaches_the_probe(self):
        # Under `lease.py run`, the probe joins the runner's lease through
        # Q2_LEASE_HELD instead of taking its own.
        held = json.dumps({'dir': '/generated/locks', 'tokens': {'device': 'generated-token'}})
        with tempfile.TemporaryDirectory() as d:
            factory = Factory(d)
            r = subprocess.run([sys.executable, '-c', 'import sys, factory_device; sys.exit(factory_device.run(b"true\\n", 30))'],
                               cwd=HERE, env=environment(**{factory_device.ENV: d, 'Q2_LEASE_HELD': held}),
                               capture_output=True, timeout=60)
            self.assertEqual(r.returncode, 0)
            self.assertEqual(factory.calls()[0]['lease'], held)


class TimerProbe(unittest.TestCase):
    def binaries(self, d):
        bins = Path(d) / 'bins'
        bins.mkdir()
        for name in ('upstream', 'patched'):
            (bins / f'timer-wake-{name}').write_bytes(f'generated {name}'.encode())
        return bins

    def test_prints_without_device(self):
        with tempfile.TemporaryDirectory() as d:
            r = subprocess.run([sys.executable, str(HERE / 'tokio-timer/q2_probe.py'), str(self.binaries(d))],
                               env=environment(), capture_output=True, text=True, timeout=60)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertTrue(r.stdout.startswith('set -eu\n'))

    def test_device_refuses_without_a_checkout(self):
        with tempfile.TemporaryDirectory() as d:
            r = subprocess.run([sys.executable, str(HERE / 'tokio-timer/q2_probe.py'), str(self.binaries(d)), '--device'],
                               env=environment(), capture_output=True, text=True, timeout=60)
            self.assertEqual(r.returncode, 2)
            self.assertIn(factory_device.ENV, r.stderr)

    def test_device_runs_the_script_through_the_factory(self):
        with tempfile.TemporaryDirectory() as d:
            factory = Factory(Path(d) / 'factory')
            bins = self.binaries(d)
            printed = subprocess.run([sys.executable, str(HERE / 'tokio-timer/q2_probe.py'), str(bins), '--seconds', '2'],
                                     env=environment(), capture_output=True, text=True, timeout=60).stdout
            for code in (0, factory_device.BUSY):
                r = subprocess.run([sys.executable, str(HERE / 'tokio-timer/q2_probe.py'), str(bins), '--seconds', '2',
                                    '--device', '--factory', str(factory.root)],
                                   env=environment(FAKE_PROBE_EXIT=str(code)), capture_output=True, text=True, timeout=60)
                self.assertEqual(r.returncode, code, r.stderr)
            calls = factory.calls()
            self.assertEqual([c['stdin'] for c in calls], [printed] * 2)
            self.assertEqual(calls[0]['argv'], ['--stream', '--timeout', str(12 * 2 + 20 + 60)])
            self.assertIn('lease is held', r.stderr)


class UxDeviceCapture(unittest.TestCase):
    CAPTURE = ROOT / 'tests/ui/ux_device_capture.py'

    def test_device_mode_refuses_without_a_checkout(self):
        r = subprocess.run([sys.executable, str(self.CAPTURE), '--host', '127.0.0.1', '--only', 'error'],
                           env=environment(), capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 2)
        self.assertIn(factory_device.ENV, r.stderr)

    def test_scenario_script_goes_to_the_factory_probe_and_a_held_lease_stops_the_run(self):
        build = ROOT / 'build'
        build.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory(dir=build) as out:
            factory = Factory(d)
            r = subprocess.run([sys.executable, str(self.CAPTURE), '--host', '127.0.0.1', '--only', 'error', '--only', 'empty',
                                '--out', str(Path(out) / 'run')],
                               env=environment(**{factory_device.ENV: d, 'FAKE_PROBE_EXIT': str(factory_device.BUSY)}),
                               capture_output=True, text=True, timeout=120)
            self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
            self.assertIn('device lease held by a rig; stopping', r.stderr)
            calls = factory.calls()
            self.assertEqual(len(calls), 1)  # the second scenario never starts
            self.assertEqual(calls[0]['argv'][:2], ['--stream', '--timeout'])
            self.assertIn('say START error', calls[0]['stdin'])
            summary = json.loads((Path(out) / 'run/summary.json').read_text())
            self.assertEqual([(s['scenario'], s['exit']) for s in summary], [('error', factory_device.BUSY)])


if __name__ == '__main__':
    unittest.main()
