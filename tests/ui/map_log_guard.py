"""Refuse truncating a Rust log that has not been hash-archived.

This guards the map entry point, including the observed active-log collision.
Direct Cargo writers must still follow the repository's no-concurrent-writer rule.
"""
import hashlib,json

def require_archived(root):
    log=root/'build/perf/optimized-tests.log'
    if not log.exists():return
    digest=hashlib.sha256(log.read_bytes()).hexdigest()
    for manifest in (root/'build/perf/test-archive').glob('*/manifest.json'):
        try:
            receipt=json.loads(manifest.read_text());archived=manifest.parent/'optimized-tests.log'
            if receipt.get('scope')=='test-log-bytes-only' and receipt.get('sha256')==digest and archived.is_file() and hashlib.sha256(archived.read_bytes()).hexdigest()==digest:return
        except (OSError,ValueError):continue
    raise RuntimeError('Refusing to replace unarchived Rust output; wait for any active test and archive its exact digest with tests/perf/archive_test_log.py before running the UI map tool.')
