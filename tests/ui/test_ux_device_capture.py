#!/usr/bin/env python3
"""Host checks for ux_device_capture.py: script generation, the base64 file
transport and its limits, events, and the device script run by sh on this
host with a generated stand-in for q2nd (no device, no q2nd build)."""
import base64
import hashlib
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import ux_device_capture as udc  # noqa: E402

HOST = '192.0.2.1'  # documentation address (RFC 5737)
ND, ABS = f'http://{HOST}:40001', f'http://{HOST}:40002'
NAME = re.compile(r'^[A-Za-z0-9_-]{1,32}$')   # numeric_probe::Output::at


def emitted(name, data):
    """What the device's emit() prints for one file."""
    lines = [f'Q2UX FILE {name} {len(data)} {hashlib.sha256(data).hexdigest()}']
    lines += base64.encodebytes(data).decode().splitlines()
    lines.append(f'Q2UX END {name}')
    return [(line + '\n').encode() for line in lines]


def heredocs(script):
    return [base64.b64decode(''.join(block.split()))
            for block in re.findall(r"<<'Q2UX_B64'\n(.*?)Q2UX_B64\n", script, re.S)]


class Runs(unittest.TestCase):
    def test_every_scenario_generates_names_the_binary_accepts(self):
        for name in udc.SCENARIOS:
            entries = udc.runs(name, ND, ABS)
            shots = [s for e in entries for s in e['shots']]
            self.assertEqual(len(shots), len(set(shots)), name)
            for e in entries:
                self.assertRegex(e['tag'], udc.TAG)
                for shot in e['shots']:
                    self.assertRegex(shot, NAME)
                    self.assertRegex(shot + '.png', udc.FILE_NAME)
                    self.assertIn(f'shot {udc.DEVICE_BASE}/{e["tag"]}/shots/{shot}.png\n', e['script'])
                for text in re.findall(r'^text (\S+)$', e['script'], re.M):
                    self.assertRegex(text, NAME)

    def test_events_in_scripts_are_handled(self):
        for name, (_b, _f, _o, events, _t) in udc.SCENARIOS.items():
            used = {t for e in udc.runs(name, ND, ABS) for t in re.findall(r'^text ev-(\S+)$', e['script'], re.M)}
            self.assertEqual(used, set(events), name)
        self.assertEqual(udc.SCENARIOS['synced'][3]['fault'][1], 'reset')

    def test_states_cover_both_looks_and_every_accent(self):
        entries = udc.runs('states', ND, ABS)
        self.assertEqual(len(entries), 14)
        seen = set()
        for e in entries:
            config = tomllib.loads(e['config'])
            seen.add((config['display']['look'], config['display']['accent']))
            self.assertTrue(all(s.startswith(e['tag'] + '-') for s in e['shots']))
        self.assertEqual(seen, {(look, accent) for look in udc.LOOKS for accent in udc.ACCENTS})

    def test_config_points_at_the_host_fixtures_with_reporting_off(self):
        config = tomllib.loads(udc.runs('online', ND, ABS)[0]['config'])
        self.assertEqual(config['server']['url'], ND)
        self.assertEqual(config['audiobookshelf']['url'], ABS)
        self.assertFalse(config['telemetry']['enabled'])
        self.assertFalse(config['wifi']['enabled'])
        self.assertEqual(config['power']['sleep_minutes'], 0)

    def test_synced_signals_the_fault_before_the_books_shots(self):
        script = udc.runs('synced', ND, ABS)[0]['script']
        self.assertLess(script.index('text ev-fault'), script.index('text home-after-sync'))


class DeviceScript(unittest.TestCase):
    def test_syntax_and_scope(self):
        for name in udc.SCENARIOS:
            entries = udc.runs(name, ND, ABS)
            for hold in (False, True):
                script = udc.device_script(name, entries, hold=hold)
                self.assertEqual(subprocess.run(['sh', '-n'], input=script.encode()).returncode, 0, name)
                for forbidden in ('/mnt/mmc', '/state', 'Q2Navidrome', 'config.toml >', '--mainline'):
                    self.assertNotIn(forbidden, script)
                self.assertEqual('q2-installed-services hold' in script, hold)
                self.assertIn('mount -t tmpfs -o size=', script)
                self.assertIn('Q2ND_INSTALLED_STORAGE=0', script)
                self.assertIn('echo 1000 > /proc/self/oom_score_adj', script)
                self.assertIn('env -i ', script)

    def test_heredocs_carry_the_config_and_script(self):
        entries = udc.runs('states', ND, ABS)
        blobs = heredocs(udc.device_script('states', entries))
        self.assertEqual(blobs, [x.encode() for e in entries for x in (e['config'], e['script'])])

    def test_refuses_unsafe_names(self):
        entry = dict(udc.runs('error', ND, ABS)[0], tag='../x')
        with self.assertRaises(AssertionError):
            udc.device_script('error', [entry])
        with self.assertRaises(AssertionError):
            udc.device_script('error', udc.runs('error', ND, ABS), binary='q2nd; reboot')

    def test_dry_run_prints_without_device_or_fixtures(self):
        r = subprocess.run([sys.executable, str(HERE / 'ux_device_capture.py'), '--dry-run', '--only', 'error', '--host', HOST],
                           capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('#### error:', r.stdout)
        self.assertIn('say START error', r.stdout)
        self.assertIn(f'http://{HOST}:NDPORT', ''.join(b.decode() for b in heredocs(r.stdout)))


class Transport(unittest.TestCase):
    def test_round_trip(self):
        events, receiver = [], None
        receiver = udc.Receiver(events.append)
        png = bytes(range(256)) * 300
        text = b'{"current":["Retry"]}\n'
        stream = [b'Q2UX BINARY /usr/bin/q2nd 0.122.0 abc\n', b'Q2UX EVENT nd-down\n', b'unrelated line\n']
        stream += emitted('albums.png', png) + emitted('text/numeric-albums.json', text)
        stream += [b'Q2UX EXIT error 0 31000\n']
        for line in stream:
            receiver.feed(line)
        self.assertEqual(receiver.errors, [])
        self.assertEqual(events, ['nd-down'])
        self.assertEqual(receiver.files, {'albums.png': png, 'text/numeric-albums.json': text})
        self.assertEqual(receiver.status('EXIT'), [['error', '0', '31000']])
        self.assertEqual(receiver.status('BINARY'), [['/usr/bin/q2nd', '0.122.0', 'abc']])

    def test_rejects_altered_unsafe_and_oversized_files(self):
        receiver = udc.Receiver()
        lines = emitted('a.png', b'0123456789')
        lines[1] = base64.encodebytes(b'0123456780').strip() + b'\n'
        for line in lines + emitted('../escape.png', b'x') + emitted('text/other.json', b'x'):
            receiver.feed(line)
        big = f'Q2UX FILE big.png {udc.MAX_FILE + 1} {"0" * 64}\n'.encode()
        receiver.feed(big)
        self.assertEqual(receiver.files, {})
        self.assertEqual(len(receiver.errors), 4, receiver.errors)

    def test_total_bound(self):
        receiver = udc.Receiver()
        chunk = b'x' * udc.MAX_FILE
        for n in range(udc.MAX_TOTAL // udc.MAX_FILE + 1):
            for line in emitted(f's{n}.png', chunk):
                receiver.feed(line)
        self.assertEqual(receiver.total, udc.MAX_TOTAL)
        self.assertEqual(len(receiver.errors), 1)


FAKE = r'''#!/bin/sh
# Stand-in for q2nd --headless --script: a PNG per shot, a text record per
# text step (ev-* ones are events), as the binary writes them.
[ "$1" = --version ] && { echo 0.0.0-generated; exit 0; }
script=$5
while IFS= read -r line; do
    case "$line" in
        "text "*) name=${line#text }; printf '{"current":["%s"]}\n' "$name" > "$Q2ND_RUNTIME/numeric-$name.json"
                  case "$name" in ev-*) sleep 1 ;; esac ;;
        "shot "*) printf 'PNG %s' "${line#shot }" > "${line#shot }" ;;
    esac
done < "$script"
echo "generated stand-in done"
'''


class LocalRun(unittest.TestCase):
    def test_device_script_on_this_host(self):
        with tempfile.TemporaryDirectory(dir=HERE.parents[1] / 'build' if (HERE.parents[1] / 'build').is_dir() else None) as d:
            d = Path(d)
            fake = d / 'q2nd'
            fake.write_text(FAKE)
            fake.chmod(0o755)
            base = str(d / 'device')
            entries = udc.runs('connections', ND, ABS, base)
            script = udc.device_script('connections', entries, binary=str(fake), base=base, mount=False,
                                       min_available_kib=0)
            events = []
            receiver = udc.Receiver(events.append)
            p = subprocess.run(['sh', '-s'], input=script.encode(), capture_output=True, timeout=120)
            for line in p.stdout.splitlines(keepends=True):
                receiver.feed(line)
            self.assertEqual(p.returncode, 0, p.stdout[-2000:])
            self.assertEqual(receiver.errors, [])
            self.assertEqual(events, ['nd-silent', 'nd-up', 'nd-down', 'nd-up', 'nd-down'])
            shots = entries[0]['shots']
            self.assertEqual(sorted(f for f in receiver.files if f.endswith('.png')), sorted(s + '.png' for s in shots))
            self.assertIn('text/numeric-connections-up.json', receiver.files)
            self.assertNotIn('text/numeric-ev-nd-down.json', receiver.files)
            self.assertIn(b'generated stand-in done', receiver.files['log/connections.log'])
            self.assertEqual(receiver.status('EXIT')[0][:2], ['connections', '0'])
            self.assertFalse(Path(base).exists())


if __name__ == '__main__':
    unittest.main()
