"""Encoded Home inputs/DB/media must share their declared generated namespace."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec=importlib.util.spec_from_file_location("home_visibility",Path(__file__).with_name("home_visibility.py"))
fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
ROOT=Path(__file__).resolve().parents[2]

class PayloadNamespace(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        (ROOT / "build/perf").mkdir(parents=True, exist_ok=True)

    def test_generated_payloads_and_indexed_media_are_coherent(self):
        with tempfile.TemporaryDirectory(dir=ROOT / "build/perf") as directory:
            out=Path(directory) / "generated"
            fixture.generate(out,Path("/tmp/q2-home-payload-test"),"0"*64,False)
            self.assertEqual(fixture.validate_generated(out)["cases"],4)

    def test_plaintext_namespace_rewrite_cannot_leave_encoded_paths_behind(self):
        with tempfile.TemporaryDirectory(dir=ROOT / "build/perf") as directory:
            out=Path(directory) / "generated"
            fixture.generate(out,Path("/tmp/q2-home-before"),"0"*64,False)
            p=out / "run.sh";p.write_text(p.read_text().replace("/tmp/q2-home-before","/tmp/q2-home-after"))
            m=out / "fixture.json";data=json.loads(m.read_text());data.update(runtime_root="/tmp/q2-home-after",script_sha256=hashlib.sha256(p.read_bytes()).hexdigest());m.write_text(json.dumps(data))
            with self.assertRaisesRegex(ValueError,"outside"):
                fixture.validate_generated(out)

if __name__=="__main__":unittest.main()
