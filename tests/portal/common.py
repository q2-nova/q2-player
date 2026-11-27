"""Shared pieces of the generated portal journeys.

Every venue, certificate, account and track here is generated. The journeys
run inside an unprivileged bubblewrap sandbox with its own network, so the
helper's production paths (/usr/libexec/q2-network/q2-portal, /usr/sbin/wpa_cli,
/mnt/mmc) can be bound to fixtures without touching the host.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
HELPER = '/usr/libexec/q2-network/q2-portal'
IP = '192.0.2.10'
PEER = '198.51.100.7'
PORTAL = 'wifi.delta.com'
SIGNIN = 'signin.delta.com'
PROBE = 'detectportal.firefox.com'

# The only source change in the fixture helper: trust the generated root
# instead of the compiled-in public roots. Everything else is the shipped code.
TRUST_NEEDLE = '        let agent = AgentBuilder::new()\n'
TRUST_LINES = ('            .tls_config(subsonic::http::tls_trusting(include_bytes!("fixture-root.der").to_vec())\n'
               '                .expect("compile-time generated fixture trust root"))\n')
WORKSPACE_FILES = ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'clippy.toml')


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def run(argv, **kwargs):
    kwargs.setdefault('check', True)
    return subprocess.run([str(part) for part in argv], **kwargs)


def certificates(out):
    """A generated root and a venue certificate for both Delta origins."""
    out.mkdir(parents=True)
    ca_key, ca_pem, ca_der = out / 'root.key', out / 'root.pem', out / 'fixture-root.der'
    key, request, pem = out / 'server.key', out / 'server.csr', out / 'server.pem'
    extensions = out / 'server.ext'
    extensions.write_text('basicConstraints=critical,CA:FALSE\n'
                          'keyUsage=critical,digitalSignature\n'
                          'extendedKeyUsage=serverAuth\n'
                          f'subjectAltName=DNS:{PORTAL},DNS:{SIGNIN}\n')
    quiet = dict(stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    run(['openssl', 'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
         '-keyout', ca_key, '-out', ca_pem, '-days', '30', '-subj', '/CN=Generated portal fixture root',
         '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign'], **quiet)
    run(['openssl', 'x509', '-in', ca_pem, '-outform', 'DER', '-out', ca_der], **quiet)
    run(['openssl', 'req', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
         '-keyout', key, '-out', request, '-subj', f'/CN={PORTAL}'], **quiet)
    run(['openssl', 'x509', '-req', '-in', request, '-CA', ca_pem, '-CAkey', ca_key, '-CAcreateserial',
         '-out', pem, '-days', '30', '-extfile', extensions], **quiet)
    ca_key.unlink()
    return {'root_der': ca_der, 'certificate': pem, 'key': key}


def build_helpers(out, root_der, target_dir, log):
    """Build the production helper and the fixture-trust helper from this checkout.

    The fixture copy differs from the checkout only by TRUST_LINES and the
    generated root file; the build refuses any other difference.
    """
    out.mkdir(parents=True)
    env = dict(os.environ, CARGO_BUILD_JOBS=os.environ.get('CARGO_BUILD_JOBS', '2'))
    run(['cargo', 'build', '--locked', '-p', 'q2-portal', '--target-dir', target_dir],
        cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
    production = out / 'q2-portal-production'
    shutil.copy2(target_dir / 'debug/q2-portal', production)

    source = out / 'fixture-source'
    source.mkdir()
    for name in WORKSPACE_FILES:
        if (ROOT / name).is_file():
            shutil.copy2(ROOT / name, source / name)
    shutil.copytree(ROOT / 'crates', source / 'crates', symlinks=True,
                    ignore=shutil.ignore_patterns('target'))
    http = source / 'crates/portal/src/http.rs'
    original = http.read_text()
    if original.count(TRUST_NEEDLE) != 1:
        raise SystemExit('the fixture trust patch no longer applies to crates/portal/src/http.rs')
    http.write_text(original.replace(TRUST_NEEDLE, TRUST_NEEDLE + TRUST_LINES))
    shutil.copy2(root_der, source / 'crates/portal/src/fixture-root.der')
    changed = run(['diff', '-r', '-q', ROOT / 'crates', source / 'crates'], check=False,
                  capture_output=True, text=True).stdout.splitlines()
    expected = {f'Files {ROOT}/crates/portal/src/http.rs and {source}/crates/portal/src/http.rs differ',
                f'Only in {source}/crates/portal/src: fixture-root.der'}
    if set(changed) != expected:
        raise SystemExit('fixture helper source differs beyond its trust root:\n' + '\n'.join(changed))
    run(['cargo', 'build', '--locked', '--offline', '-p', 'q2-portal', '--target-dir', target_dir / 'portal-fixture'],
        cwd=source, env=env, stdout=log, stderr=subprocess.STDOUT)
    fixture = out / 'q2-portal-fixture'
    shutil.copy2(target_dir / 'portal-fixture/debug/q2-portal', fixture)
    (out / 'trust-only.diff').write_text(run(
        ['diff', '-u', ROOT / 'crates/portal/src/http.rs', http], check=False,
        capture_output=True, text=True).stdout)
    return {'production': production, 'fixture': fixture}


def player_binaries(target_dir, log):
    """The q2nd test executable and the headless player, built with the workspace defaults."""
    env = dict(os.environ, CARGO_BUILD_JOBS=os.environ.get('CARGO_BUILD_JOBS', '2'))
    messages = run(['cargo', 'test', '--locked', '-p', 'q2nd', '--no-run', '--message-format=json',
                    '--target-dir', target_dir], cwd=ROOT, env=env, capture_output=True, text=True)
    log.write(messages.stderr.encode())
    tests = [message['executable'] for message in map(json.loads, messages.stdout.splitlines())
             if message.get('reason') == 'compiler-artifact' and message.get('executable')
             and message['target']['name'] == 'q2nd' and message['profile']['test']]
    if len(tests) != 1:
        raise SystemExit(f'expected one q2nd test executable, found {tests}')
    run(['cargo', 'build', '--locked', '-p', 'q2nd', '--target-dir', target_dir],
        cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT)
    return {'player_test': Path(tests[0]), 'player': target_dir / 'debug/q2nd'}


def sandbox(*, caps=('CAP_NET_ADMIN', 'CAP_NET_RAW', 'CAP_NET_BIND_SERVICE')):
    """A bubblewrap root with a private network, PID space, /tmp and /run."""
    argv = ['bwrap', '--unshare-user', '--uid', '0', '--gid', '0', '--unshare-net', '--unshare-pid',
            '--die-with-parent']
    for cap in caps:
        argv += ['--cap-add', cap]
    argv += ['--ro-bind', '/usr', '/usr', '--symlink', 'usr/bin', '/bin', '--symlink', 'usr/lib', '/lib',
             '--symlink', 'usr/lib64', '/lib64', '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp',
             '--tmpfs', '/run', '--dir', '/etc', '--dir', '/fixture']
    # Debian and Ubuntu resolve /usr/bin/ld through /etc/alternatives; the
    # phone fixture compiles a small stand-in daemon inside the sandbox.
    if Path('/etc/alternatives').is_dir():
        argv += ['--ro-bind', '/etc/alternatives', '/etc/alternatives']
    return argv


def bind(argv, pairs, *, writable=()):
    for local, guest in pairs:
        argv += ['--ro-bind', str(Path(local).resolve(strict=True)), guest]
    for local, guest in writable:
        argv += ['--bind', str(Path(local).resolve(strict=True)), guest]
    return argv


def supplicant_status(out, ssid):
    """A wpa_cli stand-in that reports one completed association."""
    script = out / 'wpa_cli'
    script.write_text('#!/bin/sh\n'
                      f"printf 'wpa_state=COMPLETED\\nssid={ssid}\\n"
                      f"bssid=02:00:00:00:00:01\\nip_address={IP}\\n'\n")
    script.chmod(0o755)
    return script


def hosts(out, *names):
    path = out / 'hosts'
    path.write_text(f'{PEER} {" ".join(names)}\n')
    return path
