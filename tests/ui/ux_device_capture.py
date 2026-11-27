#!/usr/bin/env python3
"""UX Phase 4 device checks: ux_polish_capture.py's scenarios on the Q2.

The installed binary runs `q2nd --headless --script` on the Q2 as a separate
process: no display, input or ALSA device (headless renders into memory and
plays to the fake ALSA sink), data in a private size-limited tmpfs under
/run, runtime directory and numeric-probe marker of its own, `env -i`. It
does not read /mnt/mmc, /state, the installed player's runtime or the owner
config. The generated Navidrome and Audiobookshelf servers
(ux_polish_capture.Fixture) run on this host, bound to this host's address
on the USB network (``--host``).

Device access is only device_probe.py --stream (device lease), next to this
file. The probe is not part of this repository; without it, ``--dry-run``
and ``--local`` still run.
One probe call per scenario. The generated config and script go in as base64
heredocs; the PNGs, the text records and a bounded player-log tail come back
as base64 between ``Q2UX FILE`` / ``Q2UX END`` lines, checked by size and
SHA-256. A script step ``text ev-NAME`` makes the device print
``Q2UX EVENT NAME``, on which this host changes a server's fault (synced,
connections), so faults follow the script, not a host timer.

By default the installed player keeps running: headless mode shares no
device, socket or file with it. Memory is the one shared resource: the
capture process gets oom_score_adj 1000, its tmpfs is capped, and the run
refuses when MemAvailable is low. ``--hold`` holds the installed player
(``q2-installed-services hold``) and resumes it on exit instead.

Output: build/ux-device/<stamp>/<scenario>/ in ux_polish_capture's layout
(<shot>.png, text.json, script.txt, player.log, summary.json), comparable
with host captures such as build/ux-polish/20261119-phase4-items-after/.
Scenarios: the host's six plus ``connections`` (Checking…, Unavailable,
Connected as Navidrome stops, stops answering and returns; Radios with only
Navidrome down) and ``states`` (failed pages in both themes and every
accent). Generated data only.

    python3 tests/ui/ux_device_capture.py [--only NAME ...] [--hold]
    python3 tests/ui/ux_device_capture.py --dry-run --only online   # print the device script
    python3 tests/ui/ux_device_capture.py --local target/debug/q2nd  # transport test on this host
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shlex
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import ux_polish_capture as upc  # noqa: E402

DEVICE_BASE = '/run/q2-ux-device'
TMPFS_MIB = 48
TMPFS_HOLD_MIB = 96          # with --hold an app-image binary is copied in
MIN_AVAILABLE_KIB = 48 * 1024
MAX_FILE = 1 << 20           # one PNG, text record or log tail
MAX_TOTAL = 24 << 20         # everything one scenario sends back
LOG_TAIL = 64 * 1024
MARKER = upc.MARKER.decode()
LOOKS = ('classic', 'dark')
ACCENTS = ('blue', 'graphite', 'green', 'orange', 'pink', 'purple', 'red')
FILE_NAME = re.compile(r'^(?:[A-Za-z0-9-]{1,40}\.png|text/numeric-[A-Za-z0-9_-]{1,32}\.json|log/[a-z0-9-]{1,40}\.log)$')
TAG = re.compile(r'^[a-z0-9-]{1,40}$')


# ---- fixtures
class StoppedLibrary(upc.RichLibrary):
    """A ``down`` service also drops requests on connections it accepted
    before it stopped, as a stopped server does. ux_fault_fixture's ``down``
    only closes the listener, so a pooled keep-alive connection kept getting
    answers (Connections read "Connected" for a stopped Navidrome)."""

    def handler(self, service):
        base = super().handler(service)
        library = self

        class Handler(base):
            def handle_one(self, method):
                if service.current().kind == 'down':
                    library.event(service.name, 'faulted', path=self.path.split('?')[0], fault='down')
                    self.reset()
                    return
                super().handle_one(method)

        return Handler


class Fixture(upc.Fixture):
    def __init__(self, host, empty=False):
        self.library = StoppedLibrary(empty=empty)
        self.nd = upc.fx.Service('nd', self.library, host=host)
        # Paced as in ux_polish_capture, so a book download shows its progress.
        self.abs = upc.fx.Service('abs', self.library, pace=192 * 1024, host=host)


# ---- scripts
class PrefixScript(upc.Script):
    """Shot and text names carry ``prefix`` (states: one run per look and accent)."""

    def __init__(self, out, prefix=''):
        super().__init__(out)
        self.prefix = prefix

    def shot(self, name, settle=400):
        return super().shot(self.prefix + name, settle)


def event(s, name, settle=3000):
    """Ask the host to change a fault, then give it ``settle`` ms."""
    return s.add(f'text ev-{name}\nwait {settle}')


def synced(s):
    # The host's 16 s plus margin for the Q2's slower sync; the fault follows it.
    # On the host the sync's Audiobookshelf requests end within 2 s of start.
    return upc.synced(s, settle='wait 60000\ntext ev-fault\nwait 3000')


def connections(s):
    """Settings › Connections while Navidrome answers; after it stops answering
    (accepts, never replies: Checking… for up to two 8 s pings); after it
    returns; after it stops (refused, pooled connections dropped); after it
    returns again. Then Radios with only Navidrome down. Each change is made
    on the Settings page, so Connections opens with no request since."""
    s.add('await 15000 screen:Q2\nwait 3000')
    s.open(5, 'Settings').open(3, 'Connections', wait='screen:').shot('connections-up-early', settle=100)
    s.add('wait 3000').shot('connections-up')
    for name, fault, late in (('silent', 'nd-silent', 20000), ('recovered', 'nd-up', 4000),
                              ('down', 'nd-down', 4000), ('back', 'nd-up', 4000)):
        s.back('Settings')
        event(s, fault)
        s.open(0, 'Connections', wait='screen:').shot(f'connections-{name}-early', settle=100)
        s.add(f'wait {late}').shot(f'connections-{name}')
    s.back('Settings').back('Q2', wait='screen:')
    event(s, 'nd-down')
    s.open(-2, 'Radios', wait='screen:').add('wait 3000').shot('radios-nd-down')
    return s


def states(s):
    """A failed windowed list (Retry row) and Cover Flow's failed state."""
    s.add('await 15000 screen:Q2')
    s.open(0, 'Music').open(3, 'Albums', wait='screen:').add('wait 4000').shot('albums')
    s.add('back\nawait 5000 screen:Music')
    s.open(-3, 'Cover Flow', wait='screen:').add('wait 4000').shot('coverflow')
    return s


# name: (builder, immediate fault, fixture options, events, q2nd timeout s)
EVENTS_FAULT = {'fault': (('nd', 'abs'), 'reset', {})}
EVENTS_CONNECTIONS = {'nd-down': (('nd',), 'down', {}), 'nd-silent': (('nd',), 'delay', {'seconds': 60}),
                      'nd-up': (('nd',), 'ok', {})}
SCENARIOS = {
    'online': (upc.online, None, {}, {}, 480),
    'synced': (synced, None, {}, EVENTS_FAULT, 240),
    'offline': (upc.failing, ('down', {}), {}, {}, 240),
    'error': (upc.failing, ('status', {'status': 503}), {}, {}, 240),
    'connections': (connections, None, {}, EVENTS_CONNECTIONS, 240),
    'states': (states, ('down', {}), {}, {}, 60),
    'empty': (upc.empty, None, {'empty': True}, {}, 240),
    'loading': (upc.loading, ('delay', {'seconds': 40}), {}, {}, 240),
}


# The data directory is the "card": a 48 MiB tmpfs (96 MiB with --hold). The
# default [storage] reserve_mb (512) exceeds it, so the player would keep the
# card below its reserve the whole run: the library sync stops at its first
# cover ("Not enough card space to sync artwork", no completed sync, so Book
# Info has no synced fallback and Continue Listening has no stored rows), and
# the song cache is evicted (no Song Cache wedge in Storage Usage). On the
# host the reserve is a small fraction of the disk; this keeps it so here.
# tmpfs has no blocks reserved by the filesystem, so Storage Usage shows no
# Reserved Space wedge: online's storage-usage-3 selects Free instead.
RESERVE_MB = 4
assert RESERVE_MB * 8 <= TMPFS_MIB


def config_text(nd_url, abs_url, look=None, accent=None):
    """ux_fault_fixture's generated config; telemetry off; a reserve that fits
    the tmpfs; optional theme."""
    text = upc.Fixture.config(None, nd_url, abs_url,
                              f'[telemetry]\nenabled=false\n[storage]\nreserve_mb={RESERVE_MB}\n')
    if look or accent:
        theme = (f'look="{look}"\n' if look else '') + (f'accent="{accent}"\n' if accent else '')
        assert text.count('[display]\n') == 1
        text = text.replace('[display]\n', '[display]\n' + theme)
    return text


def runs(name, nd_url, abs_url, base=DEVICE_BASE):
    """One q2nd launch per entry: {tag, config, script, shots, timeout}."""
    build, _fault, _options, _events, timeout = SCENARIOS[name]
    combos = [(look, accent) for look in LOOKS for accent in ACCENTS] if name == 'states' else [(None, None)]
    out = []
    for look, accent in combos:
        tag = f'{look}-{accent}' if look else name
        prefix = f'{tag}-' if look else ''
        script = build(PrefixScript(PurePosixPath(base) / tag / 'shots', prefix))
        out.append({'tag': tag, 'config': config_text(nd_url, abs_url, look, accent), 'script': script.text(),
                    'shots': list(script.shots), 'timeout': timeout})
    return out


# ---- the device script
def heredoc(path, data):
    encoded = base64.encodebytes(data.encode() if isinstance(data, str) else data).decode()
    return f"base64 -d > {path} <<'Q2UX_B64'\n{encoded}Q2UX_B64\n"


def device_script(scenario, entries, binary='installed', hold=False, base=DEVICE_BASE, mount=True,
                  min_available_kib=MIN_AVAILABLE_KIB):
    """The busybox sh script for one scenario: every launch in ``entries``."""
    assert TAG.match(scenario) and all(TAG.match(e['tag']) for e in entries)
    assert binary == 'installed' or binary.startswith('/')
    size = TMPFS_HOLD_MIB if hold else TMPFS_MIB
    q = shlex.quote
    if hold:
        # An app-image binary is unmounted with the service: copy it first.
        hold_block = '''case "$BIN" in /usr/bin/*) ;; *) cp "$BIN" "$B/q2nd" && chmod 700 "$B/q2nd" && BIN=$B/q2nd || { say ERROR copy binary; exit 3; } ;; esac
HELD=1
/sbin/q2-installed-services hold || { say ERROR hold; exit 3; }
say HELD
'''
        resume_block = '''    if [ "$HELD" = 1 ]; then
        if /sbin/q2-installed-services resume; then say RESUMED; else say RESUME-FAILED; fi
    fi
'''
    else:
        hold_block = (f"if [ \"$(awk '/^MemAvailable:/{{print $2}}' /proc/meminfo)\" -lt {min_available_kib} ]; then\n"
                      f"    say ERROR MemAvailable below {min_available_kib} kB: run with --hold\n    exit 3\nfi\n")
        resume_block = ''
    head = f'''set -u
PATH=/bin:/sbin:/usr/bin:/usr/sbin
B={q(base)}
PIDF="$B.pid"
MOUNT={1 if mount else 0}
HOLD={1 if hold else 0}
HELD=0
W=
TOTAL=0
say() {{ echo "Q2UX $*"; }}
stale() {{
    [ -s "$PIDF" ] || return 0
    p=$(cat "$PIDF")
    case "$(tr '\\0' ' ' < /proc/$p/cmdline 2>/dev/null)" in
        *--headless*"$B/"*) kill -9 "$p" 2>/dev/null; say STALE-KILLED ;;
    esac
    rm -f "$PIDF"
}}
cleanup() {{
    trap - EXIT HUP INT TERM PIPE
    [ -z "$W" ] || kill "$W" 2>/dev/null
    stale
    cd /
    if [ "$MOUNT" = 1 ]; then
        if grep -q " $B tmpfs " /proc/mounts; then umount "$B" 2>/dev/null || umount -l "$B"; fi
        rmdir "$B" 2>/dev/null
    else
        rm -rf "$B"
    fi
{resume_block}}}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 141' PIPE
emit() {{
    [ -f "$2" ] || return 0
    s=$(wc -c < "$2")
    if [ "$s" -gt {MAX_FILE} ] || [ $((TOTAL + s)) -gt {MAX_TOTAL} ]; then say SKIP "$1" "$s"; return 0; fi
    TOTAL=$((TOTAL + s))
    say FILE "$1" "$s" "$(sha256sum < "$2" | cut -d' ' -f1)"
    base64 "$2"
    say END "$1"
}}
stale
if [ "$MOUNT" = 1 ]; then
    if grep -q " $B tmpfs " /proc/mounts; then umount "$B" || {{ say ERROR stale mount busy; exit 3; }}; fi
    mkdir -p "$B" && mount -t tmpfs -o size={size}m,mode=0700,nosuid,nodev q2-ux-device "$B" || {{ say ERROR tmpfs; exit 3; }}
else
    rm -rf "$B"; mkdir -p "$B" && chmod 700 "$B"
fi
if [ {q(binary)} = installed ]; then
    BIN=$(cat /run/q2nd/app-player 2>/dev/null)
    [ -n "$BIN" ] && [ -x "$BIN" ] || BIN=/usr/bin/q2nd
else
    BIN={q(binary)}
fi
[ -x "$BIN" ] || {{ say ERROR binary missing "$BIN"; exit 3; }}
say BINARY "$BIN" "$("$BIN" --version 2>/dev/null)" "$(sha256sum < "$BIN" | cut -d' ' -f1)"
IPID=$(cat /run/q2nd/player.pid 2>/dev/null)
say INSTALLED before "${{IPID:-none}}"
say MEMAVAILABLE before "$(awk '/^MemAvailable:/{{print $2}}' /proc/meminfo)"
{hold_block}FAILED=0
'''
    body = []
    for e in entries:
        tag, t = e['tag'], int(e['timeout'])
        r = f'$B/{tag}'
        body.append(f'''# ---- {tag}
R={r}
mkdir -p "$R/data" "$R/shots" "$R/runtime" && chmod 700 "$R/runtime"
printf '%s\\n' {q(MARKER.rstrip(chr(10)))} > "$R/runtime/numeric-probe-enabled"
chmod 600 "$R/runtime/numeric-probe-enabled"
{heredoc('"$R/data/config.toml"', e['config'])}{heredoc('"$R/script.txt"', e['script'])}say START {tag}
(
    echo 1000 > /proc/self/oom_score_adj
    exec env -i PATH=/bin:/sbin:/usr/bin:/usr/sbin HOME="$R" Q2ND_ALSA_DEVICE=q2nd-fake Q2ND_NUMERIC_PROBE=1 \\
        Q2ND_INSTALLED_STORAGE=0 Q2ND_RUNTIME="$R/runtime" \\
        timeout -k 5 {t} "$BIN" --headless --data "$R/data" --script "$R/script.txt"
) > "$R/player.log" 2>&1 &
P=$!
echo "$P" > "$PIDF"
(
    trap '' PIPE
    hwm=0; n=0
    while kill -0 "$P" 2>/dev/null; do
        for f in "$R"/runtime/numeric-ev-*.json; do
            [ -e "$f" ] || continue
            e=${{f##*/numeric-ev-}}; e=${{e%.json}}
            rm -f "$f"
            echo "Q2UX EVENT $e" || kill -9 "$P"
        done
        h=$(awk '/^VmHWM:/{{print $2}}' /proc/$P/status 2>/dev/null)
        [ -n "$h" ] && [ "$h" -gt "$hwm" ] && hwm=$h
        n=$((n + 1))
        if [ $((n % 50)) -eq 0 ]; then echo "Q2UX TICK {tag} $((n / 5)) s" || kill -9 "$P"; fi
        sleep 0.2
    done
    echo "$hwm" > "$R/hwm"
) &
W=$!
wait "$P"; RC=$?
wait "$W"; W=
rm -f "$PIDF"
say EXIT {tag} "$RC" "$(cat "$R/hwm" 2>/dev/null)"
[ "$RC" = 0 ] || FAILED=1
for f in "$R"/shots/*.png; do [ -e "$f" ] || continue; emit "${{f##*/}}" "$f"; done
for f in "$R"/runtime/numeric-*.json; do [ -e "$f" ] || continue; emit "text/${{f##*/}}" "$f"; done
tail -c {LOG_TAIL} "$R/player.log" > "$R/player.tail"
emit "log/{tag}.log" "$R/player.tail"
rm -rf "$R"
''')
    tail = '''say MEMAVAILABLE after "$(awk '/^MemAvailable:/{print $2}' /proc/meminfo)"
if [ "$HOLD" = 0 ]; then
    if [ -z "$IPID" ]; then
        say INSTALLED after "$(cat /run/q2nd/player.pid 2>/dev/null || echo none)" none-before
    elif kill -0 "$IPID" 2>/dev/null && [ "$(cat /run/q2nd/player.pid 2>/dev/null)" = "$IPID" ]; then
        say INSTALLED after "$IPID" running
    else
        say INSTALLED after "$(cat /run/q2nd/player.pid 2>/dev/null || echo none)" changed
    fi
fi
say DONE
exit "$FAILED"
'''
    return head + ''.join(body) + tail


# ---- the device output
class Receiver:
    """Parses the device stream: progress lines, events and base64 files."""

    def __init__(self, on_event=None, echo=None):
        self.on_event = on_event or (lambda name: None)
        self.echo = echo or (lambda line: None)
        self.files = {}
        self.errors = []
        self.lines = []        # every Q2UX status line, for device.json
        self.total = 0
        self._name = None
        self._expect = None
        self._chunks = []

    def feed(self, raw):
        line = raw.decode('utf-8', 'replace').rstrip('\r\n')
        if self._name is not None:
            if line == f'Q2UX END {self._name}':
                self._finish()
            elif len(self._chunks) * 76 > MAX_FILE * 2:
                self.errors.append(f'{self._name}: longer than announced')
                self._name = None
            else:
                self._chunks.append(line.strip())
            return
        if line.startswith('Q2UX FILE '):
            parts = line.split(' ')
            if len(parts) != 5 or not FILE_NAME.match(parts[2]) or not parts[3].isdigit():
                self.errors.append(f'bad file header: {line[:120]}')
                return
            size = int(parts[3])
            if size > MAX_FILE or self.total + size > MAX_TOTAL:
                self.errors.append(f'{parts[2]}: {size} bytes over the limit')
                return
            self._name, self._expect, self._chunks = parts[2], (size, parts[4]), []
            return
        if line.startswith('Q2UX EVENT '):
            name = line[len('Q2UX EVENT '):].strip()
            self.lines.append(line)
            self.echo(line)
            self.on_event(name)
            return
        if line.startswith('Q2UX '):
            self.lines.append(line)
        self.echo(line[:300])

    def _finish(self):
        name, (size, digest) = self._name, self._expect
        self._name = None
        try:
            data = base64.b64decode(''.join(self._chunks), validate=True)
        except ValueError:
            self.errors.append(f'{name}: not base64')
            return
        if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
            self.errors.append(f'{name}: size or SHA-256 differs')
            return
        self.total += size
        self.files[name] = data

    def status(self, word):
        return [line.split(' ')[2:] for line in self.lines if line.split(' ')[1:2] == [word]]


# ---- running
def apply_fault(fixture, services, kind, opts):
    for service in services:
        getattr(fixture, service).set(kind, **opts)


def run(name, out, args):
    build, fault, options, events, timeout = SCENARIOS[name]
    out.mkdir(parents=True)
    host = '127.0.0.1' if args.local else args.host
    fixture = Fixture(host=host, **options)
    started = time.monotonic()
    try:
        nd_url = f'http://{host}:{fixture.nd.port}'
        abs_url = f'http://{host}:{fixture.abs.port}'
        base = str((out / 'device').resolve()) if args.local else DEVICE_BASE
        entries = runs(name, nd_url, abs_url, base)
        script = device_script(name, entries, binary=str(args.local.resolve()) if args.local else args.device_binary,
                               hold=args.hold, base=base, mount=not args.local,
                               min_available_kib=0 if args.local else MIN_AVAILABLE_KIB)
        (out / 'device-script.sh').write_text(script)
        (out / 'script.txt').write_text(''.join((f'# ---- {e["tag"]}\n' if len(entries) > 1 else '') + e['script']
                                                for e in entries))
        if fault:
            apply_fault(fixture, ('nd', 'abs'), fault[0], fault[1])
        unknown_events = []

        def on_event(event):
            if event in events:
                services, kind, opts = events[event]
                apply_fault(fixture, services, kind, opts)
            else:
                unknown_events.append(event)
        receiver = Receiver(on_event, echo=lambda line: print(f'  {name}: {line}', flush=True))
        budget = sum(e['timeout'] + 30 for e in entries) + 120
        if args.local:
            argv = ['sh', '-s']
        else:
            argv = [sys.executable, str(HERE / 'device_probe.py'), '--stream', '--timeout', str(budget)]
        proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=None if not args.local else subprocess.STDOUT)
        proc.stdin.write(script.encode())
        proc.stdin.close()
        for raw in proc.stdout:
            receiver.feed(raw)
        rc = proc.wait(timeout=budget + 60)
        return collect(name, out, entries, receiver, fixture, rc, started, unknown_events)
    finally:
        fixture.close()


def collect(name, out, entries, receiver, fixture, rc, started, unknown_events):
    texts, logs = {}, []
    for file_name, data in sorted(receiver.files.items()):
        if file_name.endswith('.png'):
            (out / file_name).write_bytes(data)
        elif file_name.startswith('text/'):
            shot = file_name[len('text/numeric-'):-len('.json')]
            if not shot.startswith('ev-'):
                try:
                    texts[shot] = json.loads(data).get('current', [])
                except ValueError:
                    receiver.errors.append(f'{file_name}: not JSON')
        else:
            logs.append(f'==== {file_name}\n' + data.decode('utf-8', 'replace'))
    shots = [s for e in entries for s in e['shots']]
    (out / 'text.json').write_text(json.dumps({s: texts[s] for s in shots if s in texts}, indent=1) + '\n')
    (out / 'player.log').write_text('\n'.join(logs))
    unknown = sorted({f"{e['service']} {e['method']} {e['path']}" for e in fixture.library.events if e['kind'] == 'unknown'})
    (out / 'unanswered.json').write_text(json.dumps(unknown, indent=1) + '\n')
    events = [e for e in fixture.library.events if e['kind'] != 'request'][-200:]
    (out / 'fixture-events.json').write_text(json.dumps(events, indent=1) + '\n')
    failure = [line for log in logs for line in log.splitlines() if 'headless script: ' in line]
    missing = [s for s in shots if not (out / f'{s}.png').exists()]
    exits = {parts[0]: parts[1:] for parts in receiver.status('EXIT') if parts}
    binary = receiver.status('BINARY')
    device = {'binary': binary[0] if binary else None, 'installed': receiver.status('INSTALLED'),
              'mem_available_kib': receiver.status('MEMAVAILABLE'), 'held': bool(receiver.status('HELD')),
              'resumed': bool(receiver.status('RESUMED')), 'resume_failed': bool(receiver.status('RESUME-FAILED')),
              'exits': exits, 'errors': receiver.errors + [' '.join(p) for p in receiver.status('ERROR')],
              'skipped': receiver.status('SKIP'), 'unknown_events': unknown_events, 'probe_exit': rc,
              'bytes_received': receiver.total}
    after = [p for p in device['installed'] if p[:1] == ['after']]
    # Held: resumed. Running alongside: the same installed player PID before and after.
    device['installed_ok'] = (device['resumed'] if device['held']
                              else bool(after) and after[-1][-1] in ('running', 'none-before'))
    (out / 'device.json').write_text(json.dumps(device, indent=1) + '\n')
    exit_codes = [int(v[0]) for v in exits.values() if v and v[0].lstrip('-').isdigit()]
    code = rc if rc else (max(exit_codes, default=1) if len(exits) == len(entries) else 1)
    return {'scenario': name, 'exit': code, 'elapsed_s': round(time.monotonic() - started, 1),
            'shots': len(shots) - len(missing), 'missing': missing,
            'script_failure': failure[-1][:300] if failure else None, 'launched': len(exits),
            # Settings › Connections: the Navidrome row's value per shot (drawn before its label).
            'navidrome_row': {s: v[v.index('Navidrome Server') - 1] for s, v in texts.items()
                              if 'Navidrome Server' in v[1:] and s.startswith(('connections', 'settings-connections'))},
            'device_errors': device['errors'][:5], 'installed_ok': device['installed_ok'],
            'max_rss_kib': max((int(v[1]) for v in exits.values() if len(v) > 1 and v[1].isdigit()), default=None)}


def dry_run(names, args):
    nd_url, abs_url = f'http://{args.host}:NDPORT', f'http://{args.host}:ABSPORT'
    for name in names:
        script = device_script(name, runs(name, nd_url, abs_url), binary=args.device_binary, hold=args.hold)
        print(f'#### {name}: {len(script)} bytes; host fixtures on {args.host}, ports chosen at run time')
        print(script)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--only', action='append', default=[], choices=list(SCENARIOS))
    parser.add_argument('--out', type=Path)
    parser.add_argument('--hold', action='store_true', help='hold the installed player for the run and resume it')
    parser.add_argument('--device-binary', default='installed',
                        help='q2nd on the Q2: "installed" (what the service runs, else /usr/bin/q2nd) or a path')
    parser.add_argument('--host', help='host address the Q2 reaches over USB (required except with --local)')
    parser.add_argument('--dry-run', action='store_true', help='print the device scripts; no fixtures, no device')
    parser.add_argument('--local', type=Path, metavar='Q2ND',
                        help='run the device script with sh on this host against a host q2nd (transport test)')
    args = parser.parse_args()
    names = args.only or list(SCENARIOS)
    if not args.local and not args.host:
        parser.error('--host is required: the address of this host on the Q2 USB network')
    if args.dry_run:
        dry_run(names, args)
        return 0
    if args.local and not args.local.is_file():
        parser.error(f'{args.local} missing')
    if args.local and args.hold:
        parser.error('--hold is for the device')
    label = time.strftime('%Y%m%d-%H%M%S') + ('-local' if args.local else '')
    base = (args.out or ROOT / 'build/ux-device' / label).resolve()
    if not base.is_relative_to(ROOT / 'build'):
        parser.error('--out must be below build/')
    if not args.local:
        import socket
        try:
            socket.create_server((args.host, 0)).close()
        except OSError as error:
            parser.error(f'cannot listen on {args.host} ({error.strerror}): is the Q2 on the USB network?')
    base.mkdir(parents=True, exist_ok=True)
    results = []
    for name in names:
        print(f'{name}: starting', flush=True)
        results.append(run(name, base / name, args))
        r = results[-1]
        print(f"{name}: exit {r['exit']} shots {r['shots']} {r['elapsed_s']} s"
              + (f" missing {len(r['missing'])}" if r['missing'] else '')
              + (f" — {r['script_failure']}" if r['script_failure'] else ''), flush=True)
        (base / 'summary.json').write_text(json.dumps(results, indent=1) + '\n')
        if r['exit'] == 75:
            print('device lease held by a rig; stopping', file=sys.stderr)
            break
        if not r['launched']:
            print(f"{name}: nothing launched ({r['device_errors']}); stopping", file=sys.stderr)
            break
    for r in results:
        print(f"{r['scenario']:<12} exit {r['exit']} shots {r['shots']} {r['elapsed_s']} s"
              + (f" missing {r['missing']}" if r['missing'] else '')
              + (f" errors {r['device_errors']}" if r['device_errors'] else '')
              + ('' if r['installed_ok'] else ' INSTALLED PLAYER CHANGED OR NOT RESUMED'))
    print(base)
    return 0 if all(r['exit'] == 0 and not r['missing'] and not r['device_errors'] and r['installed_ok']
                    for r in results) else 1


if __name__ == '__main__':
    sys.exit(main())
