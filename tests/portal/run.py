#!/usr/bin/env python3
"""Build the player and portal helper and run every generated portal journey.

Journeys (all on generated venues inside an unprivileged sandbox):

guest-boingo       Helper detection, cancel, roam refusal and one consent POST on a
                   mixed free/account/paid page; the sponsored follow-up stays gated.
                   The player supervisor test drives the same venue, card traces are
                   checked, and the headless player streams only after consent.
guest-long-terms   An airport-length agreement yields only the fixed consent notice.
delta-ordinary     Ordinary Delta sign-in with a trusted and an untrusted certificate.
delta-video        Activation answered by a video page: delayed admission, still gated,
                   and a JSON acknowledgement.
wheel-supervisor   The player supervisor's cancel, challenge and success paths and the
                   helper's video handling (ui::portal generated_wheel_login_supervisor_fixture).
app-wheel          The App's Wi-Fi screen, a generated lease, wheel entry and an
                   uncached stream before and after admission (generated_delta_wheel_app_journey).
app-phone          The same App journey with both QR screens, a phone peer on the
                   temporary access point, a transient failure, Retry and the prepared
                   account. Needs a q2-os checkout for the access-point watcher.

The result is OUT/result.json; OUT/summary.md is a table of the journeys.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

JOURNEYS = ('guest-boingo', 'guest-long-terms', 'delta-ordinary', 'delta-video',
            'wheel-supervisor', 'app-wheel', 'app-phone')
GENERATED_WIFI = f'''#!/bin/sh
# Generated Wi-Fi helper: `up` associates, `dhcp` and `renew` lease {common.IP}.
set -eu
case "$1" in
up) : > /fixture/lifecycle/radio-up ;;
down) rm -f /fixture/lifecycle/radio-up /fixture/lifecycle/lease ;;
dhcp|renew) ip addr replace {common.IP}/32 dev wlan0; : > /fixture/lifecycle/lease ;;
esac
'''
PASS_MARKERS = {'wheel-supervisor': 'Q2_GENERATED_WHEEL_SUPERVISOR_PASS',
                'app-wheel': 'Q2_GENERATED_WHEEL_APP_PASS', 'app-phone': 'Q2_GENERATED_PHONE_APP_PASS'}
SCENARIOS = {'wheel-supervisor': ['cancel', 'challenge', 'success', 'video_admitted', 'video_gated'],
             'app-wheel': ['wheel-with-media'], 'app-phone': ['phone-prepared-retry-with-media']}


def guest(name, out, inputs):
    extra = ['--boingo-choice', '--trace-card', '--playback-player', inputs['player']] if name == 'guest-boingo' \
        else ['--long-agreement']
    rc = subprocess.run([sys.executable, common.HERE / 'guest_journey.py', '--binary', inputs['production'],
                         '--player-test', inputs['player_test'], *extra, '--out', out]).returncode
    result = json.loads((out / 'result.json').read_text())
    return rc == 0 and result['passed'], {'tested': result.get('tested', [])}


def delta(name, out, inputs):
    extra = ['--ordinary', '--production-helper', inputs['production']] if name == 'delta-ordinary' else []
    rc = subprocess.run([sys.executable, common.HERE / 'delta_journey.py', '--helper', inputs['fixture'],
                         '--certificate', inputs['certificate'], '--key', inputs['key'], '--verify-trace',
                         *extra, '--out', out]).returncode
    result = json.loads((out / 'result.json').read_text())
    return rc == 0 and result['passed'], {'cases': [case['case'] for case in result['iterations']]}


def venue(name, out, inputs):
    out.mkdir(parents=True)
    lifecycle = out / 'lifecycle'
    lifecycle.mkdir()
    (lifecycle / 'data').mkdir()
    wifi = out / 'generated-wifi'
    wifi.write_text(GENERATED_WIFI)
    media = out / 'media'
    sys.path.insert(0, str(common.HERE))
    import guest_journey
    guest_journey.generate_media(media)
    hosts = common.hosts(out, common.PORTAL, common.SIGNIN, common.PROBE)
    status = common.supplicant_status(out, 'DeltaWiFi.com')
    caps = ['CAP_NET_ADMIN', 'CAP_NET_RAW', 'CAP_NET_BIND_SERVICE']
    if name == 'app-phone':
        caps.append('CAP_SYS_ADMIN')
    argv = common.sandbox(caps=caps)
    pairs = [(hosts, '/etc/hosts'), (status, '/usr/sbin/wpa_cli'), (wifi, '/fixture/generated-wifi'),
             (common.HERE, '/fixture/portal'), (common.HERE / 'delta_venue.py', '/fixture/venue.py'),
             (inputs['certificate'], '/fixture/server.pem'), (inputs['key'], '/fixture/server.key'),
             (inputs['player_test'], '/fixture/player-test'), (media / 'tone.wav', '/fixture/tone.wav')]
    if name == 'app-phone':
        os_repo = inputs['os_repo']
        pairs += [(os_repo / 'os', '/fixture/os'),
                  (os_repo / 'os/install/integration/usr/libexec/q2-network/portal-phone-ap', '/fixture/portal-phone-ap'),
                  (inputs['fixture'], '/fixture/fixture-helper')]
    else:
        argv += ['--tmpfs', '/usr/libexec', '--dir', '/usr/libexec/q2-network']
        pairs.append((inputs['fixture'], common.HELPER))
    if Path('/usr/share/alsa/alsa.conf').is_file():
        argv += ['--setenv', 'ALSA_CONFIG_PATH', '/usr/share/alsa/alsa.conf']
    common.bind(argv, pairs, writable=[(lifecycle, '/fixture/lifecycle')])
    argv += ['--clearenv', '--setenv', 'PATH', '/usr/bin:/usr/sbin', '--setenv', 'PYTHONDONTWRITEBYTECODE', '1',
             '--setenv', 'Q2ND_WPA_CTRL', '/run/generated-wpa',
             '--setenv', 'Q2_GENERATED_TEST_ROOT', '/fixture/lifecycle/data',
             '/usr/bin/python3', '/fixture/venue.py', {'wheel-supervisor': 'supervisor', 'app-wheel': 'wheel',
                                                       'app-phone': 'phone'}[name]]
    log = out / 'run.log'
    with log.open('x') as stream:
        try:
            rc = subprocess.run(argv, stdout=stream, stderr=subprocess.STDOUT, timeout=150).returncode
        except subprocess.TimeoutExpired:
            stream.write('bounded sandbox timeout\n')
            rc = 124
    (lifecycle / 'qr-page').unlink(missing_ok=True)
    text = log.read_text()
    results = [json.loads(line) for line in text.splitlines() if line.startswith('{"scenario":')]
    passed = (rc == 0 and text.count(PASS_MARKERS[name] + '\n') == 1
              and [r['scenario'] for r in results] == SCENARIOS[name])
    (out / 'result.json').write_text(json.dumps({'passed': passed, 'exit_status': rc, 'scenarios': results},
                                                indent=2) + '\n')
    return passed, {'cases': [r['scenario'] for r in results]}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--out', type=Path, required=True, help='fresh directory under build/')
    parser.add_argument('--os-repo', type=Path, help='q2-os checkout for app-phone (skipped without it)')
    parser.add_argument('--target-dir', type=Path, default=common.ROOT / 'target')
    parser.add_argument('--only', choices=JOURNEYS, action='append', help='run only these journeys')
    args = parser.parse_args()
    out = args.out.resolve()
    if not out.is_relative_to(common.ROOT / 'build'):
        parser.error('--out belongs under build/')
    out.mkdir(parents=True, exist_ok=False)
    selected = args.only or [name for name in JOURNEYS if name != 'app-phone' or args.os_repo]
    if 'app-phone' in selected and not args.os_repo:
        parser.error('app-phone needs --os-repo')

    started = time.monotonic()
    with (out / 'build.log').open('wb') as log:
        tls = common.certificates(out / 'tls')
        helpers = common.build_helpers(out / 'helpers', tls['root_der'], args.target_dir.resolve(), log)
        players = common.player_binaries(args.target_dir.resolve(), log)
    inputs = {**tls, **helpers, **players, 'os_repo': args.os_repo.resolve() if args.os_repo else None}
    build_seconds = round(time.monotonic() - started, 1)

    journeys = {}
    for name in selected:
        print(f'== {name}', flush=True)
        begun = time.monotonic()
        runner = guest if name.startswith('guest') else delta if name.startswith('delta') else venue
        try:
            passed, detail = runner(name, out / name, inputs)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            passed, detail = False, {'error': f'{type(error).__name__}: {error}'}
        journeys[name] = {'passed': passed, 'seconds': round(time.monotonic() - begun, 1), **detail}
        print(f'{name}: {"passed" if passed else "FAILED"}', flush=True)

    identities = {name: common.sha(path) for name, path in inputs.items()
                  if name in ('production', 'fixture', 'player_test', 'player', 'root_der')}
    commit = subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=common.ROOT, capture_output=True,
                            text=True).stdout.strip()
    result = {'passed': all(j['passed'] for j in journeys.values()) and bool(journeys), 'hardware': False,
              'source_commit': commit, 'build_seconds': build_seconds, 'journeys': journeys,
              'identities': identities, 'trust_variant': 'fixture helper trusts only the generated root'}
    (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    rows = [f'| {name} | {"passed" if j["passed"] else "failed"} | {j["seconds"]} s |' for name, j in journeys.items()]
    (out / 'summary.md').write_text('## Generated portal journeys\n\n| Journey | Result | Time |\n| --- | --- | ---: |\n'
                                    + '\n'.join(rows) + '\n')
    print(json.dumps({name: j['passed'] for name, j in journeys.items()}), flush=True)
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    sys.exit(main())
