"""Generated fixture integrity; these checks do not certify device playback."""
import base64
import gzip
import importlib.util
import json
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("offline_shuffle", Path(__file__).with_name("offline_shuffle.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class GeneratedShuffle(unittest.TestCase):
    def setUp(self):
        parent = ROOT / "build/ui/offline-shuffle-fixture-tests"
        parent.mkdir(parents=True, exist_ok=True)
        self.directory = Path(tempfile.mkdtemp(dir=parent))

    def test_embedded_payload_matches_owned_paths(self):
        out = self.directory / "generated"
        runtime = Path("/run/q2-offline-shuffle-generated-test")
        fixture.generate(out, runtime, "a" * 64)
        script = (out / "run.sh").read_text()
        subprocess.run(["sh", "-n", str(out / "run.sh")], check=True)
        payloads = re.findall(r"printf '%s' '([^']+)' \| base64 -d \| gzip -d > ([^\n]+)", script)
        self.assertEqual(len(payloads), 4)
        for blob, target in payloads:
            self.assertEqual(Path(target).parent, runtime)
            self.assertEqual(gzip.decompress(base64.b64decode(blob)), (out / Path(target).name).read_bytes())
        with sqlite3.connect(out / "library.db") as db:
            rows = db.execute("SELECT song_id,path FROM offline_songs").fetchall()
        self.assertEqual(len(rows), 4)
        self.assertTrue(all(Path(path).parent == runtime for _, path in rows))
        self.assertEqual(sum(Path(path).name == "missing.wav" for _, path in rows), 1)

    def test_reject_unowned_paths_before_creating_output(self):
        for runtime in ("relative/q2-offline-shuffle-test", "/run/ordinary", "/run/../q2-offline-shuffle-test"):
            out = self.directory / "rejected"
            with self.assertRaises(ValueError):
                fixture.generate(out, runtime, "a" * 64)
            self.assertFalse(out.exists())

    def test_queue_validator_rejects_missing_download_and_no_progress(self):
        path = self.directory / "generated-result.db"
        good = {"songs": [{"id": f"generated-shuffle-{i}"} for i in range(3)], "index": 0, "position_ms": 1800}
        with sqlite3.connect(path) as db:
            db.execute("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT)")
            db.execute("INSERT INTO kv VALUES('queue',?)", (json.dumps(good),))
        self.assertTrue(fixture.validate_database(path)["passed"])
        for bad in ({**good, "position_ms": 0}, {**good, "index": 3},
                    {**good, "songs": good["songs"] + [{"id": "generated-missing"}]}):
            with sqlite3.connect(path) as db:
                db.execute("UPDATE kv SET value=?", (json.dumps(bad),))
            with self.assertRaises(ValueError):
                fixture.validate_database(path)

    def test_export_requires_uncheckpointed_wal(self):
        original = self.directory / "live.db"
        exported = self.directory / "exported.db"
        with sqlite3.connect(original) as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA wal_autocheckpoint=0")
            db.execute("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT)")
            queue = {"songs": [{"id": f"generated-shuffle-{i}"} for i in range(3)],
                     "index": 1, "position_ms": 1800}
            db.execute("INSERT INTO kv VALUES('queue',?)", (json.dumps(queue),))
            db.commit()
            shutil.copyfile(original, exported)
            with self.assertRaises(sqlite3.OperationalError):
                fixture.validate_database(exported)
            shutil.copyfile(str(original) + "-wal", str(exported) + "-wal")
            self.assertTrue(fixture.validate_database(exported)["passed"])


if __name__ == "__main__":
    unittest.main()
