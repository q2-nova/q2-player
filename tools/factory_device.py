#!/usr/bin/env python3
"""Run a shell script on the Q2 through a q2-factory checkout.

The player repository has no device transport of its own. Host scripts that
run commands on the Q2 (tests/ui/ux_device_capture.py,
tools/tokio-timer/q2_probe.py) pipe their script to the factory's
``rigs/device/device_probe.py --stream``, which runs it with ``sh -s`` over
the USB SSH link under the device lease:

- started on its own, it takes the lease (``lease.device()``) for the one
  script and releases it after;
- started under ``rigs/device/lease.py run -- …`` (or a shell that exported a
  ``lease.py hold``'s ``Q2_LEASE_HELD``), it joins that lease, so several
  scripts run without another holder between them;
- while a rig holds the device it refuses with exit 75 and runs nothing.

The checkout is ``--factory PATH`` or, without it, ``Q2_FACTORY_CHECKOUT``.
"""
import os
from pathlib import Path
import subprocess
import sys

ENV = 'Q2_FACTORY_CHECKOUT'
PROBE = Path('rigs/device/device_probe.py')
BUSY = 75


class NoFactory(ValueError):
    """No usable q2-factory checkout was named."""


def probe_path(checkout=None, environ=None):
    """The factory's device_probe.py: ``checkout``, else ``Q2_FACTORY_CHECKOUT``."""
    environ = os.environ if environ is None else environ
    value = checkout or environ.get(ENV)
    if not value:
        raise NoFactory(f'the Q2 is reached through a q2-factory checkout: pass --factory PATH or set {ENV}')
    path = Path(value).expanduser().resolve() / PROBE
    if not path.is_file():
        raise NoFactory(f'{value} is not a q2-factory checkout: {PROBE} is missing')
    return path


def argv(timeout, checkout=None, environ=None):
    """The command that runs a script read from stdin on the Q2, streaming its output."""
    return [sys.executable, str(probe_path(checkout, environ)), '--stream', '--timeout', str(int(timeout))]


def popen(timeout, checkout=None, environ=None, **kwargs):
    """Start the probe with a stdin pipe; the caller writes the script and closes it."""
    return subprocess.Popen(argv(timeout, checkout, environ), stdin=subprocess.PIPE, **kwargs)


def run(script, timeout, checkout=None, environ=None):
    """Run ``script`` (bytes) on the Q2; its output goes to this process's
    stdout. Returns the probe's exit code: BUSY while a rig holds the device,
    124 when the probe outlives its own timeout by a minute."""
    proc = popen(timeout, checkout, environ)
    proc.stdin.write(script)
    proc.stdin.close()
    try:
        return proc.wait(timeout=timeout + 60)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        return 124
