#!/usr/bin/env python3
"""Print, or run on the Q2, a shell script that runs the timer wakeup probes.

    tools/tokio-timer/build.sh q2 build/tokio-timer/q2
    python3 tools/tokio-timer/q2_probe.py build/tokio-timer/q2 [--seconds 5] > probe.sh
    python3 tools/tokio-timer/q2_probe.py build/tokio-timer/q2 --device [--factory PATH]

Without ``--device`` it prints the script. With it, the script runs on the Q2
through the q2-factory checkout's device probe (tools/factory_device.py),
named by ``--factory`` or ``Q2_FACTORY_CHECKOUT``: the probe takes the device
lease, or joins the one of a ``rigs/device/lease.py run`` this runs under, and
exits 75 without running anything while a rig holds the device.

The script unpacks timer-wake-upstream and timer-wake-patched into a new
directory on /run (tmpfs), checks their SHA256, runs each scenario, prints
one JSON line per run prefixed with the binary's name, and removes the
directory on every exit path. It touches nothing else on the device. The
player keeps running, so the counts include no other process's work but the
timing shares the CPUs with it.
"""
import argparse
import base64
import hashlib
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import factory_device  # noqa: E402

NAMES = ("upstream", "patched")


def script(bin_dir, seconds):
    lines = ["set -eu", "umask 077", "D=$(mktemp -d /run/q2-timer-wake.XXXXXX)", "trap 'rm -rf \"$D\"' EXIT", "trap 'exit 1' HUP INT TERM"]
    for name in NAMES:
        data = (bin_dir / f"timer-wake-{name}").read_bytes()
        lines.append(f"base64 -d > \"$D/{name}\" <<'EOF'\n{base64.encodebytes(data).decode()}EOF")
        lines.append(f"echo '{hashlib.sha256(data).hexdigest()}  '\"$D/{name}\" | sha256sum -c - > /dev/null")
        lines.append(f"chmod 700 \"$D/{name}\"")
    scenarios = [f"sleep 250 {seconds}", f"sleep 1000 {seconds}", f"interval 250 {seconds}", f"many {seconds}",
                 f"cancel {seconds}", "timeout-ready 10000", f"timeout-wake {seconds}"]
    for scenario in scenarios:
        for name in NAMES:
            lines.append(f"printf '{name} ';\"$D/{name}\" {scenario}")
    return "\n".join(lines) + "\n"


def budget(seconds):
    """Seconds the whole script may take on the Q2: twelve timed runs, two
    10 s timeout-ready runs, and a minute to unpack and start."""
    return 12 * seconds + 2 * 10 + 60


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("bin_dir", type=pathlib.Path)
    ap.add_argument("--seconds", type=int, default=5)
    ap.add_argument("--device", action="store_true", help="run the script on the Q2 instead of printing it")
    ap.add_argument("--factory", type=pathlib.Path, metavar="PATH",
                    help=f"q2-factory checkout for --device (default: ${factory_device.ENV})")
    a = ap.parse_args()
    if a.factory and not a.device:
        ap.error("--factory is for --device")
    text = script(a.bin_dir, a.seconds)
    if not a.device:
        print(text, end="")
        return 0
    try:
        factory_device.probe_path(a.factory)
    except factory_device.NoFactory as error:
        ap.error(str(error))
    sys.stdout.flush()
    rc = factory_device.run(text.encode(), budget(a.seconds), a.factory)
    if rc == factory_device.BUSY:
        print("q2_probe: the device lease is held; nothing ran", file=sys.stderr)
    return rc


if __name__ == "__main__":
    sys.exit(main())
