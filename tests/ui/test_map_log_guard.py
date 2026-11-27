import hashlib,json,subprocess,tempfile,unittest
from pathlib import Path
import map_log_guard as guard

class Guard(unittest.TestCase):
    def setUp(self):
        root=Path(__file__).resolve().parents[2]/'build/perf/map-guard-controls';root.mkdir(parents=True,exist_ok=True)
        self.temp=tempfile.TemporaryDirectory(dir=root);self.addCleanup(self.temp.cleanup);self.root=Path(self.temp.name);self.log=self.root/'build/perf/optimized-tests.log';self.log.parent.mkdir(parents=True)
    def test_active_or_unarchived_bytes_are_preserved(self):
        guard.require_archived(self.root)
        for data in (b'',b'active compiler output',b'completed but not archived'):
            self.log.write_bytes(data)
            with self.assertRaisesRegex(RuntimeError,'Refusing'):guard.require_archived(self.root)
            self.assertEqual(self.log.read_bytes(),data)
    def test_matching_manifest_alone_or_changed_archive_cannot_authorize_replacement(self):
        self.log.write_bytes(b'qualified bytes');directory=self.root/'build/perf/test-archive/fixture';directory.mkdir(parents=True)
        (directory/'manifest.json').write_text(json.dumps(dict(scope='test-log-bytes-only',sha256=hashlib.sha256(self.log.read_bytes()).hexdigest())))
        for content in (None,b'changed',b'qualified bytes'):
            if content is not None:(directory/'optimized-tests.log').write_bytes(content)
            if content==b'qualified bytes':guard.require_archived(self.root)
            else:
                with self.assertRaises(RuntimeError):guard.require_archived(self.root)
    def test_actual_cli_refuses_before_starting_cargo_or_truncating_log(self):
        scripts=self.root/'tests/ui';scripts.mkdir(parents=True)
        original=Path(__file__).resolve().parent
        for name in ('map.py','map_log_guard.py'):(scripts/name).write_bytes((original/name).read_bytes())
        perf=self.root/'tests/perf';perf.mkdir(parents=True)
        (perf/'native_test_cache.py').write_bytes((original.parent/'perf/native_test_cache.py').read_bytes())
        (self.root/'Cargo.toml').write_text('[workspace]\n')
        # Admit ownership before the simulated active writer creates its log.
        import importlib.util
        spec=importlib.util.spec_from_file_location('generated_native_cache',perf/'native_test_cache.py')
        cache=importlib.util.module_from_spec(spec);spec.loader.exec_module(cache)
        cache.prepare(self.root)
        (self.root/'docs').mkdir();(self.root/'docs/ui-hierarchy.md').write_text('generated fixture only')
        self.log.write_bytes(b'active compiler bytes')
        result=subprocess.run(['python3',str(scripts/'map.py')],capture_output=True,timeout=5)
        self.assertNotEqual(result.returncode,0);self.assertIn(b'Refusing',result.stderr);self.assertEqual(self.log.read_bytes(),b'active compiler bytes')

if __name__=='__main__':unittest.main()
