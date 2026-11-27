#!/usr/bin/env python3
"""Own a worktree-local native Cargo cache without changing the test command."""
import argparse
import importlib.util
import json
import os
from pathlib import Path


def paths(root):
    root = Path(root).resolve()
    if not (root / 'Cargo.toml').is_file():
        raise ValueError('workspace Cargo.toml required')
    for name in ('build', '.cargo'):
        if (root / name).is_symlink():
            raise ValueError('workspace-local directories required')
    target = root / 'build/native-test-target'
    if target.is_symlink() or target.resolve() != target:
        raise ValueError('cache must stay inside this worktree')
    return root / '.cargo/config.toml', root / 'build/native-test-cache.json', target


def expected(root):
    config, receipt, target = paths(root)
    text = '# Owned by tests/perf/native_test_cache.py\n[build]\njobs = 2\ntarget-dir = ' + json.dumps(str(target)) + '\n'
    return config, receipt, target, text


def check(root, environ=None):
    environ = os.environ if environ is None else environ
    if any(key in environ for key in ('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_JOBS')):
        raise ValueError('Cargo environment overrides bypass the owned cache configuration')
    config, receipt, target, text = expected(root)
    if (config.parent / 'config').exists() or (config.parent / 'config').is_symlink():
        raise ValueError('legacy Cargo config can override the owned configuration')
    if config.is_symlink() or receipt.is_symlink():
        raise ValueError('regular owned configuration required')
    if not receipt.is_file():
        raise ValueError('existing configuration has no owned cache receipt')
    if json.loads(receipt.read_text()) != {'target': str(target), 'config': text}:
        raise ValueError('cache ownership receipt changed')
    if config.read_text() != text:
        raise ValueError('Cargo configuration differs from owned isolated cache')
    # Both direct Cargo and the UI-map wrapper use this required admission.
    # Share the existing archival guard so a caller cannot accidentally replace
    # the map wrapper's unarchived output. This is not a concurrent-run lease.
    source = Path(__file__).resolve().parents[1] / 'ui/map_log_guard.py'
    spec = importlib.util.spec_from_file_location('q2_native_log_guard', source)
    guard = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(guard)
    try:
        guard.require_archived(Path(root).resolve())
    except RuntimeError as error:
        raise ValueError(str(error)) from error
    return target


def prepare(root):
    config, receipt, target, text = expected(root)
    if config.exists() or config.is_symlink() or receipt.exists() or receipt.is_symlink():
        # Never replace an existing Cargo configuration, including one using a
        # shared target. An already owned, unchanged setup is idempotent.
        return check(root)
    if (config.parent / 'config').exists() or (config.parent / 'config').is_symlink():
        raise ValueError('legacy Cargo config must be reconciled explicitly')
    if any(key in os.environ for key in ('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_JOBS')):
        raise ValueError('remove Cargo cache/job overrides before preparing')
    config.parent.mkdir(exist_ok=True)
    receipt.parent.mkdir(exist_ok=True)
    with config.open('x') as stream:
        stream.write(text)
    with receipt.open('x') as stream:
        json.dump({'target': str(target), 'config': text}, stream, indent=2)
        stream.write('\n')
    return check(root)


def cleanup(root):
    check(root)
    config, receipt, _ = paths(root)
    config.unlink()
    receipt.unlink()
    # Keep the isolated build cache for reuse; never remove compiled artifacts.


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('prepare', 'check', 'cleanup'))
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    result = {'prepare': prepare, 'check': check, 'cleanup': cleanup}[args.action](root)
    print(result if result is not None else 'Owned Cargo configuration removed; cache retained')
