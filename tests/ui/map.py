#!/usr/bin/env python3
"""Regenerate or check the runtime UI map using the repository's exact test command."""
import argparse
import difflib
import os
from pathlib import Path
import subprocess
import sys
from map_log_guard import require_archived

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--write", action="store_true", help="write tests/ui/ui-hierarchy.md for deliberate UX review")
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(root / "tests/perf"))
from native_test_cache import check as check_native_cache
try:
    check_native_cache(root)
except ValueError as error:
    sys.exit(f"Native cache admission failed: {error}; run native_test_cache.py prepare and check first")
reviewed = root / "tests/ui/ui-hierarchy.md"
before = reviewed.read_text() if reviewed.exists() else ""
# Kept separate from the test command, as required by AGENTS.md.
(root / "build/perf").mkdir(parents=True, exist_ok=True)
try:
    require_archived(root)
except RuntimeError as error:
    sys.exit(str(error))
env = os.environ.copy()
if args.write:
    env["Q2_UI_MAP_UPDATE"] = "1"
else:
    env.pop("Q2_UI_MAP_UPDATE", None)
result = subprocess.run(
    "cargo test --workspace --locked --offline > build/perf/optimized-tests.log 2>&1",
    shell=True, cwd=root, env=env, check=False,
)
candidate = root / "build/ui/hierarchy.md"
if candidate.exists():
    difference = "".join(difflib.unified_diff(before.splitlines(keepends=True), candidate.read_text().splitlines(keepends=True),
        fromfile="reviewed/tests/ui/ui-hierarchy.md", tofile="generated/build/ui/hierarchy.md"))
    (root / "build/ui/hierarchy.diff").write_text(difference)
if result.returncode:
    print("Tests failed; see build/perf/optimized-tests.log and build/ui/hierarchy.md", file=sys.stderr)
else:
    print("Runtime UI map written to tests/ui/ui-hierarchy.md; inspect its diff and record the UX review."
          if args.write else "Runtime UI map matches the reviewed artifact; UX judgment remains required.")
sys.exit(result.returncode)
