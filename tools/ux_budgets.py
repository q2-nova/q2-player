#!/usr/bin/env python3
"""UX latency budgets from production telemetry, with a ratchet (UX-008).

Merges each metric's distribution reports for one build from the diagnostic
receiver (read-only), compares the configured quantile with
`docs/ux-budgets.json`, and exits 1 when a metric with enough samples misses
its target. `--ratchet` lowers a target to measured × headroom when that is
below the current target, and records the change; targets never rise.

  python3 tools/ux_budgets.py --store RECEIVER_OUT/production/reports.sqlite   # latest build
  python3 tools/ux_budgets.py --store STORE --build 5a734f41 --ratchet

STORE is a `reports.sqlite` under the --out directory of
tools/diagnostic_receiver.py.
"""
import argparse
import datetime
import json
import math
import sqlite3
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BUDGETS = ROOT / "docs/ux-budgets.json"
GAMMA = 1.01 / 0.99


def merged(rows, build):
    out = {}
    for row in rows:
        event = row.get("event") or {}
        if event.get("type") != "distribution" or not row["build"].startswith(build):
            continue
        sketch = event["sketch"]
        m = out.setdefault(event["metric"], {"bins": {}, "count": 0, "zero_count": 0, "max_us": 0})
        for index, n in sketch["bins"]:
            m["bins"][int(index)] = m["bins"].get(int(index), 0) + n
        m["count"] += sketch["count"]
        m["zero_count"] += sketch["zero_count"]
        m["max_us"] = max(m["max_us"], sketch["max_us"])
    return out


def quantile(m, q):
    target, seen = q * m["count"], m["zero_count"]
    if seen >= target:
        return 0.0
    for index in sorted(m["bins"]):
        seen += m["bins"][index]
        if seen >= target:
            return min(2 * GAMMA ** index / (GAMMA + 1), m["max_us"]) / 1000
    return m["max_us"] / 1000


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--store", type=Path, required=True, help="a receiver reports.sqlite")
    ap.add_argument("--build", help="build hash prefix (default: newest build with reports)")
    ap.add_argument("--ratchet", action="store_true", help="lower targets that the measurement beats")
    args = ap.parse_args()
    budgets = json.loads(BUDGETS.read_text())
    db = sqlite3.connect(f"file:{args.store}?mode=ro", uri=True)
    rows = [json.loads(b) for (b,) in db.execute("select body from reports order by rowid")]
    build = args.build or (rows[-1]["build"] if rows else "")
    dist = merged(rows, build)
    q, minimum, headroom = budgets["quantile"], budgets["min_samples"], budgets["headroom"]
    failed = False
    print(f"build {build[:8]}: p{int(q * 100)} against target (n ≥ {minimum} to gate)")
    for name, spec in budgets["metrics"].items():
        m = dist.get(name)
        if not m:
            print(f"  {name:26} no samples            target {spec['target_ms']:>6} ms")
            continue
        value = quantile(m, q)
        gated = m["count"] >= minimum
        verdict = "ok" if value <= spec["target_ms"] else ("MISS" if gated else "miss (too few)")
        failed |= gated and value > spec["target_ms"]
        print(f"  {name:26} n={m['count']:<5} p50={quantile(m, .5):8.1f} p{int(q*100)}={value:8.1f} ms  target {spec['target_ms']:>6} ms  {verdict}")
        lowered = math.ceil(value * headroom)
        if args.ratchet and gated and lowered < spec["target_ms"]:
            spec["history"].append({"date": datetime.date.today().isoformat(), "build": build[:16],
                                    "from_ms": spec["target_ms"], "to_ms": lowered, "measured_ms": round(value, 1), "samples": m["count"]})
            spec["target_ms"] = lowered
            print(f"    ratchet: target lowered to {lowered} ms")
    if args.ratchet:
        BUDGETS.write_text(json.dumps(budgets, indent=2, ensure_ascii=False) + "\n")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
