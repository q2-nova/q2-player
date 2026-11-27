#!/usr/bin/env python3
"""Run the generated mainline runner's cleanup against owned platform fakes."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("home_visibility", Path(__file__).with_name("home_visibility.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CleanupFaults(unittest.TestCase):
    def control(self, name, export_fails=False, resume_fails=False):
        import tempfile
        parent = ROOT / "build/ui/home-cleanup-regression"
        parent.mkdir(parents=True, exist_ok=True)
        out = Path(tempfile.mkdtemp(prefix=name + "-", dir=parent))
        # generate() requires a new output; reserve its parent instead.
        generated = out / "generated"
        runtime = out / "runtime"
        fixture.generate(generated, runtime, "0" * 64, True)
        script = (generated / "run.sh").read_text()
        subprocess.run(["sh", "-n", str(generated / "run.sh")], check=True)
        service, events, pid, healthy = (out / n for n in ("mock-service", "events", "pid", "healthy"))
        service.write_text(f'''#!/bin/sh
printf '%s\\n' "$1" >> '{events}'
if [ "$1" = resume ]; then
    {'exit 29' if resume_fails else ':'}
    echo "$Q2_HOME_TEST_PID" > '{pid}'
    echo "$Q2_HOME_TEST_PID" > '{healthy}'
fi
''')
        service.chmod(0o700)
        # Exercise the generated cleanup itself. Only platform endpoints are
        # replaced; no host/device service, config, PID file or logs are read.
        script = script.split('test "$(sha256sum "$app"')[0]
        script = script.replace("/sbin/q2-installed-services", str(service))
        script = script.replace("/run/q2nd/player.pid", str(pid)).replace("/run/q2nd/healthy", str(healthy))
        partial = runtime / "neither/home.png"
        partial.parent.mkdir(parents=True)
        partial.write_bytes(b"generated partial capture")
        script += f'''
export Q2_HOME_TEST_PID=$$
installed_sha=$(sha256sum /proc/$$/exe | cut -d' ' -f1)
held=1
'{service}' hold
'''
        if export_fails:
            script += "base64() { return 23; }\n"
        script += "cleanup\necho Q2_HOME_FIXTURE_PASS\n"
        control = out / "control.sh"
        control.write_text(script)
        result = subprocess.run(["sh", str(control), "/generated-unused-app"], capture_output=True, text=True, timeout=15,
                                env={**os.environ, "Q2_HOME_TEST_PID": ""})
        (out / "control.log").write_text(result.stdout + result.stderr)
        (out / "result.json").write_text(json.dumps({"exit": result.returncode, "export_failure": export_fails,
            "resume_failure": resume_fails, "service_calls": events.read_text().splitlines()}) + "\n")
        self.assertEqual(events.read_text().splitlines(), ["hold", "resume"])
        self.assertEqual(result.returncode == 0, not (export_fails or resume_fails))
        self.assertEqual("Q2_HOME_FIXTURE_PASS" in result.stdout, not (export_fails or resume_fails))
        self.assertEqual("Q2_HOME_PLAYER_RESTORED" in result.stdout, not resume_fails)
        if resume_fails:
            self.assertIn("Q2_HOME_RESUME_FAILED", result.stderr)

    def test_success_verifies_restored_identity(self):
        self.control("success")

    def test_export_failure_still_restores_and_reports_failure(self):
        self.control("export-failure", export_fails=True)

    def test_resume_failure_never_retries_or_claims_restoration(self):
        self.control("resume-failure", resume_fails=True)


if __name__ == "__main__":
    unittest.main()
