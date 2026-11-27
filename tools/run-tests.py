#!/usr/bin/env python3
"""Run every Python unit test file under tests/ and tools/, each in its own process.

  tools/run-tests.py [--jobs N] [PATH ...]

Each test_*.py runs as `python3 -m unittest -v MODULE` from its own directory,
so it imports the modules beside it. A file passes when unittest exits 0.
Prints one line per file and every skip with its reason; prints the full
output of each failing file and exits 1 when any file fails.
"""
import argparse
import concurrent.futures
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def discover():
    return sorted(path for base in ('tests', 'tools') for path in (ROOT / base).rglob('test_*.py')
                  if 'build' not in path.relative_to(ROOT).parts)


def run(path):
    p = subprocess.run([sys.executable, '-m', 'unittest', '-v', path.stem], cwd=path.parent,
                       capture_output=True, text=True, timeout=900)
    out = p.stdout + p.stderr
    ran = re.search(r'^Ran (\d+) tests?', out, re.M)
    skips = re.findall(r'^\S+ \(([^)]+)\).*?\.\.\. skipped (.*)$', out, re.M)
    return path.relative_to(ROOT), p.returncode, int(ran.group(1)) if ran else 0, skips, out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--jobs', type=int, default=os.cpu_count() or 2)
    parser.add_argument('paths', nargs='*', type=Path, help='test files (default: all)')
    args = parser.parse_args()
    files = [path.resolve() for path in args.paths] or discover()
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        rows = sorted(pool.map(run, files))
    failed = [row for row in rows if row[1] != 0]
    for path, code, tests, skips, _ in rows:
        print(f'{"pass" if code == 0 else "FAIL"}  {path}  tests={tests} skipped={len(skips)}')
        for case, reason in skips:
            print(f'      skipped {case}: {reason}')
    for path, *_, out in failed:
        print(f'\n==== {path}\n{out.rstrip()}')
    print(f'{len(rows)} files, {sum(r[2] for r in rows)} tests, {len(failed)} files failed')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
