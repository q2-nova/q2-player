#!/usr/bin/env python3
"""Print a shell script that runs the timer wakeup probes on the Q2.

    tools/tokio-timer/build.sh q2 build/tokio-timer/q2
    python3 tools/tokio-timer/q2_probe.py build/tokio-timer/q2 [--seconds 5] > probe.sh

Run probe.sh with sh on the Q2.

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


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("bin_dir", type=pathlib.Path)
    ap.add_argument("--seconds", type=int, default=5)
    a = ap.parse_args()
    print(script(a.bin_dir, a.seconds), end="")


if __name__ == "__main__":
    main()
