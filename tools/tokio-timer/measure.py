#!/usr/bin/env python3
"""Run the timer wakeup probes and print one JSON document.

    tools/tokio-timer/measure.py BIN_DIR [--seconds N] [--runner CMD...]

BIN_DIR holds timer-wake-upstream and timer-wake-patched. `--runner` prefixes
each command, for example `qemu-mipsel-static -L SYSROOT` for the Q2 build.
The scenarios of one binary run one after another; the two binaries run in
parallel. `--gate` exits 1 unless the patched binary stays within the limits
in GATE and the upstream binary exceeds the sleep limit (the negative
control: the measurement can see a second wakeup). With a runner the
cross-thread scenario `timeout-wake` is reported and not gated.
"""
import argparse
import concurrent.futures
import json
import pathlib
import subprocess
import sys


def scenarios(seconds):
    return [["sleep", "50", str(seconds)], ["sleep", "250", str(seconds)], ["sleep", "1000", str(seconds)],
            ["interval", "250", str(seconds)], ["interval", "1000", str(seconds)], ["many", str(seconds)],
            ["cancel", str(seconds)], ["timeout-ready", "10000"], ["timeout-wake", str(seconds)]]


# Wakeups per expiry allowed for the patched runtime. One wakeup fires a
# timer; the margin (10 %, at least two wakeups) covers the end of the run and
# preemption by other work on the measuring host.
GATE = {"sleep": 1.1, "interval": 1.1, "many": 1.1, "cancel": 1.1, "timeout-wake": 1.1}
# timeout-ready: 10,000 timeouts armed and dropped, one 200 ms sleep.
GATE_TIMEOUT_READY_VOLUNTARY = 3


def run_all(runner, binary, seconds):
    rows = []
    for args in scenarios(seconds):
        out = subprocess.run([*runner, str(binary), *args], capture_output=True, text=True, timeout=seconds * 4 + 60)
        if out.returncode != 0:
            rows.append({"mode": args[0], "args": " ".join(args[1:]), "error": out.returncode, "stderr": out.stderr.strip()[-300:]})
        else:
            rows.append(json.loads(out.stdout.strip().splitlines()[-1]))
    return rows


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("bin_dir", type=pathlib.Path)
    ap.add_argument("--seconds", type=int, default=10)
    ap.add_argument("--gate", action="store_true")
    ap.add_argument("--runner", nargs=argparse.REMAINDER, default=[])
    a = ap.parse_args()
    names = ["upstream", "patched"]
    with concurrent.futures.ThreadPoolExecutor(2) as pool:
        futures = {n: pool.submit(run_all, a.runner, a.bin_dir / f"timer-wake-{n}", a.seconds) for n in names}
        result = {"seconds": a.seconds, "runner": [pathlib.Path(a.runner[0]).name] if a.runner else [],
                  "sizes": {n: (a.bin_dir / f"timer-wake-{n}").stat().st_size for n in names},
                  "runs": {n: futures[n].result() for n in names}}
    fails, skipped = [], []
    if a.gate:
        for row in result["runs"]["patched"]:
            if "error" in row:
                fails.append(f"patched {row['mode']} {row['args']}: exit {row['error']}")
            elif row["mode"] == "timeout-ready":
                if row["voluntary"] > GATE_TIMEOUT_READY_VOLUNTARY:
                    fails.append(f"patched timeout-ready: {row['voluntary']} wakeups for one expiry")
            elif row["mode"] == "timeout-wake" and a.runner:
                # Under user-mode emulation a wake from another thread costs
                # several host switches with either tokio; reported only.
                skipped.append("timeout-wake under a runner")
            elif row["voluntary"] > max(row["expiries"] * GATE[row["mode"]], row["expiries"] + 2):
                fails.append(f"patched {row['mode']} {row['args']}: {row['voluntary_per_expiry']} wakeups per expiry")
        control = [r for r in result["runs"]["upstream"] if r.get("mode") == "sleep" and r.get("args", "").startswith("250 ")]
        if not control or control[0].get("voluntary_per_expiry", 0) <= GATE["sleep"]:
            fails.append("negative control: upstream sleep 250 did not show a second wakeup")
    result["gate"] = {"checked": a.gate, "failures": fails, "skipped": skipped}
    json.dump(result, sys.stdout, indent=1)
    print()
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
