"""Exercise the player release download CLI with deterministic GitHub responses."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
COMPONENTS = json.loads((ROOT / "component-lock.json").read_text())
CONTENTS = b"synthetic archive\n"


class ReleaseTools(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.out = self.root / "downloads"
        self.lock = self.root / "lock.json"
        digest = hashlib.sha256(CONTENTS).hexdigest()

        def entry(keys):
            return {"repository": "example/" + keys[0], "tag": "tools/v1",
                    "sources": {key: COMPONENTS[key] for key in keys},
                    "files": {"tools.tar.gz": digest}}

        self.entries = {"sdk": entry(["q2-os", "buildroot"]),
                        "compiler": entry(["q2-toolchain", "llvm-project", "rust"])}
        cli = self.root / "gh"
        cli.write_text("""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
root = Path(__file__).parent
args = sys.argv[1:]
entries = json.loads((root / 'lock.json').read_text())['tools']
if args[:3] == ['api', '--hostname', 'github.com']:
    repo = args[3].split('/')[2]
    obj = {'type': 'commit', 'sha': entries['sdk' if repo == 'q2-os' else 'compiler']['sources'][repo]}
    if repo == 'q2-toolchain' and args[3].endswith('/git/ref/tags/tools/v1'):
        obj = {'type': 'tag', 'sha': 'a' * 40}
    if os.environ.get('WRONG_TAG'):
        obj = {'type': 'commit', 'sha': '0' * 40}
    print(json.dumps({'object': obj}))
elif args[:3] == ['release', 'download', 'tools/v1']:
    assert args[3:7] in (['--repo', 'github.com/example/' + r, '--pattern', 'tools.tar.gz']
                          for r in ('q2-os', 'q2-toolchain'))
    assert args[7] == '--output'
    Path(args[8]).write_bytes(b'synthetic archive\\n')
else:
    raise SystemExit(2)
""")
        cli.chmod(0o755)

    def call(self, success=True, **env):
        self.lock.write_text(json.dumps({"format": 1, "tools": self.entries}))
        process = subprocess.run([sys.executable, ROOT / "tools/fetch-build-tools.py",
                                  "--lock", self.lock, "--out", self.out],
                                 env={**os.environ, "PATH": str(self.root) + ":" + os.environ["PATH"], **env},
                                 capture_output=True, text=True)
        self.assertEqual(process.returncode == 0, success, process.stderr)
        return process.stderr

    def test_download_and_offline_reuse(self):
        self.call()
        self.assertEqual((self.out / "compiler/tools.tar.gz").read_bytes(), CONTENTS)
        self.call(PATH=str(self.root / "empty"))
        (self.out / "sdk/tools.tar.gz").write_bytes(b"changed")
        self.assertIn("SHA-256 mismatch", self.call(False))

    def test_bad_second_archive_does_not_publish_partial_pair(self):
        self.entries["compiler"]["files"]["tools.tar.gz"] = "1" * 64
        self.assertIn("SHA-256 mismatch", self.call(False))
        self.assertFalse(self.out.exists())

    def test_wrong_producer_tag(self):
        self.assertIn("locked producer commit", self.call(False, WRONG_TAG="1"))
        self.assertFalse(self.out.exists())

    def test_source_pin_drift(self):
        self.entries["sdk"]["sources"]["buildroot"] = "1" * 40
        self.assertIn("source revisions differ", self.call(False, PATH=""))
        self.assertFalse(self.out.exists())


if __name__ == "__main__":
    unittest.main()
