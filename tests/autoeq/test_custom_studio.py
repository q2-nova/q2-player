"""Byte integrity of the pinned public Custom Studio archive (no network)."""
import hashlib
import json
from pathlib import Path
import unittest
import tempfile
from custom_studio_fixture import generate

ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / "crates/q2nd/assets/autoeq/custom-studio"


class CustomStudioArchive(unittest.TestCase):
    def test_original_exports_match_pinned_sha256_and_exact_model_paths(self):
        metadata = json.loads((ASSETS / "provenance.json").read_text())
        self.assertEqual(len(metadata["entries"]), 4)
        for n, entry in enumerate(metadata["entries"], 1):
            with self.subTest(step=n):
                self.assertEqual(entry["name"], f"Beyerdynamic Custom Studio (step {n})")
                self.assertEqual(hashlib.sha256((ASSETS / f"step-{n}.txt").read_bytes()).hexdigest(),
                                 entry["export_sha256"])
                self.assertTrue(entry["path"].endswith(f"Beyerdynamic%20Custom%20Studio%20(step%20{n})"))
                rows = (ASSETS / f"step-{n}-response.tsv").read_text().splitlines()
                self.assertEqual(rows[0], "frequency_hz\tparametric_eq_db")
                self.assertEqual(len(rows), 11)
                frequencies = [float(row.split("\t")[0]) for row in rows[1:]]
                self.assertEqual(frequencies, sorted(set(frequencies)))
        self.assertIn("step-1", metadata["measurement_caveat"])
        self.assertIn("586", metadata["shelf_export_correction"])
        self.assertEqual(hashlib.sha256((ASSETS / "LICENSE").read_bytes()).hexdigest(),
                         metadata["license_sha256"])


class GeneratedReplay(unittest.TestCase):
    def test_exact_hash_and_switch_are_required_before_creating_output(self):
        with tempfile.TemporaryDirectory(dir=ROOT / "build/perf") as temporary:
            out=Path(temporary) / "fixture"
            for step,digest,runtime in [(0,"a"*64,"/tmp/q2-custom-studio-test"),
                                        (4,"not-a-hash","/tmp/q2-custom-studio-test"),
                                        (4,"a"*64,"/etc")]:
                with self.assertRaises(ValueError):
                    generate(out,runtime,step,digest)
                self.assertFalse(out.exists())
            receipt=generate(out,"/tmp/q2-custom-studio-test",4,"a"*64)
            self.assertEqual(receipt["expected_profile"],"Beyerdynamic Custom Studio (step 4)")
            self.assertEqual(receipt["q2nd_runtime"],"/tmp/q2-custom-studio-test/runtime")
            self.assertEqual(hashlib.sha256((out / "inputs.txt").read_bytes()).hexdigest(),receipt["inputs_sha256"])
            self.assertIn("enabled=false",(out / "config.toml").read_text())
            self.assertNotIn("headphone",(out / "config.toml").read_text())
            self.assertIn("scroll 2147483647\nscroll -1\ncenter",(out / "inputs.txt").read_text())
            with self.assertRaises(FileExistsError):
                generate(out,"/tmp/q2-custom-studio-test",4,"a"*64)


if __name__ == "__main__":
    unittest.main()
