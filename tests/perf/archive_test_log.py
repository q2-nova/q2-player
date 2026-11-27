#!/usr/bin/env python3
"""Preserve the required rolling Rust log; this does not certify test success.

Run immediately after the prescribed cargo command, before another test run.
An expected digest binds a previously recorded result: a replaced log is refused,
never silently associated with the old receipt. Only the fixed test-log path is
read. No configuration, app logs or repository-wide file collection is involved.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat


def archive(workspace, label, expected_sha256):
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,79}', label):
        raise ValueError('invalid archive label')
    if not re.fullmatch(r'[0-9a-f]{64}', expected_sha256):
        raise ValueError('expected SHA256 required')
    root = Path(workspace).resolve()
    perf = root / 'build/perf'
    if not perf.resolve().is_relative_to(root):
        raise ValueError('test artifacts must remain inside workspace')
    source = perf / 'optimized-tests.log'
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode):
            raise ValueError('regular test log required')
        data = stream.read()
        after = os.fstat(stream.fileno())
    signature = lambda s: (s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns)
    if signature(before) != signature(after):
        raise ValueError('test log changed while copying')
    digest = hashlib.sha256(data).hexdigest()
    if digest != expected_sha256:
        raise ValueError('test log digest differs; historical evidence unavailable')
    parent = perf / 'test-archive'
    parent.mkdir(exist_ok=True)
    if parent.is_symlink():
        raise ValueError('archive directory must not be a symlink')
    target = parent / label
    target.mkdir()  # Exclusive: never overwrite a prior successful or partial copy.
    # The manifest is the completion marker. A crash leaves an explicitly
    # incomplete directory; it cannot replace any previously completed archive.
    for name, payload in [
        ('optimized-tests.log', data),
        ('manifest.json', (json.dumps({
            'version': 1, 'scope': 'test-log-bytes-only',
            'source': 'build/perf/optimized-tests.log',
            'sha256': digest, 'bytes': len(data),
            'test_success_certified': False,
        }, indent=2) + '\n').encode()),
    ]:
        with (target / name).open('xb') as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
    for directory in (target, parent):
        fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    return target / 'manifest.json'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', type=Path,
                        default=Path(__file__).resolve().parents[2])
    parser.add_argument('--label', required=True)
    parser.add_argument('--sha256', required=True)
    args = parser.parse_args()
    print(archive(args.workspace, args.label, args.sha256))


if __name__ == '__main__':
    main()
