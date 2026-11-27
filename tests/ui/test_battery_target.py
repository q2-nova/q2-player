import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import tomllib
import unittest
from unittest.mock import patch
import zlib
import subprocess
import runpy

spec=importlib.util.spec_from_file_location('battery_target',Path(__file__).with_name('battery_target.py'))
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

class Battery(unittest.TestCase):
    def test_same_driver_imports_from_actual_shallow_container_layout(self):
        with tempfile.TemporaryDirectory() as tmp:
            driver=Path(tmp)/'driver.py';driver.write_bytes(Path(m.__file__).read_bytes())
            loaded=runpy.run_path(str(driver),run_name='generated_battery_fixture')
            self.assertEqual(loaded['APP'],m.APP)
    def test_explicit_child_environment_resolves_chroot_without_inherited_settings(self):
        self.assertIn('/usr/sbin',m.CHILD_ENV['PATH'].split(':'))
        self.assertIsNotNone(m.shutil.which('chroot',path=m.CHILD_ENV['PATH']))
        self.assertFalse(any(k.startswith(('Q2ND_','LD_')) for k in m.CHILD_ENV))
    def test_typed_config_and_nondefault_cases(self):
        for name,case in m.CASES.items():
            config=tomllib.loads(m.config(case))
            self.assertIs(config['display']['night_warmth'],False)
            self.assertEqual(config['power'].get('charge_ceiling_percent'),case['ceiling'])
            self.assertEqual(config['power'].get('charge_speed'),case['speed'])
            self.assertFalse(config['wifi']['enabled']);self.assertFalse(config['tailscale']['enabled'])
            self.assertEqual(config['power']['sleep_minutes'],0)
    def test_cancel_script_requires_actual_destinations_and_never_commits_hover(self):
        for name,case in m.CASES.items():
            text=m.script(name,case)
            for title in ['Q2','Settings','System','Battery','Charge Ceiling','Charge Speed']:
                self.assertIn('idle:screen:'+title,text)
            for picker in ['Charge Ceiling','Charge Speed']:
                section=text.split('await 15000 idle:screen:'+picker+'\n',1)[1].split('back\n',1)[0]
                self.assertNotIn('\ncenter\n','\n'+section)
                self.assertIn('scroll ',section)
            self.assertEqual(text.count('\nshot '),len(m.SHOTS))
            self.assertNotIn('sleep',text.lower());self.assertNotIn('poweroff',text)
    def test_only_readonly_telemetry_paths_and_missing_arm(self):
        self.assertEqual(m.CASES['missing']['files'],{})
        for case in m.CASES.values():
            for relative in case['files']:
                self.assertTrue(relative=='run/q2/charge-status' or relative.startswith('sys/class/power_supply/axp20x-battery/'))
                self.assertNotIn('charge_behaviour',relative);self.assertNotIn('policy_control',relative)
        self.assertEqual(m.CASES['long-fault']['files']['run/q2/charge-status'].strip(),'charger-fault-or-unknown')
    def test_png_actual_pixels_crc_truncation_and_wrong_dimensions(self):
        def chunk(tag,data):return len(data).to_bytes(4,'big')+tag+data+zlib.crc32(tag+data).to_bytes(4,'big')
        valid=(b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',375,320,8,2,0,0,0))+
               chunk(b'IDAT',zlib.compress((b'\0'+bytes(375*3))*320))+chunk(b'IEND',b''))
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'image.png';path.write_bytes(valid);self.assertEqual(m.png(path),m.sha(path))
            for broken in [valid[:24],valid[:-1],valid[:20]+b'bad!'+valid[24:],valid[:-13]+b'bad!'+valid[-9:]]:
                path.write_bytes(broken)
                with self.assertRaises((ValueError,struct.error,zlib.error)):m.png(path)
    def test_missing_or_changed_plan_refuses_before_container_and_lease(self):
        with tempfile.TemporaryDirectory() as tmp:
            out=Path(tmp);(out/'plan.json').write_text('{}')
            with patch.object(m.subprocess,'run') as launch:
                with self.assertRaises(ValueError):m.run(out,'0'*64)
                launch.assert_not_called()
            (out/'result.json').write_text('{}')
            with self.assertRaises(ValueError):m.run(out,m.sha(out/'plan.json'))
    def test_changed_source_and_payload_refuse_before_container(self):
        with tempfile.TemporaryDirectory() as tmp:
            out=Path(tmp);source=out/'source';source.write_text('one')
            plan=out/'plan.json';plan.write_text(json.dumps({'inputs':{str(source):'0'*64},'files':{}}))
            with patch.object(m.subprocess,'run') as launch:
                with self.assertRaises(ValueError):m.run(out,m.sha(plan))
                launch.assert_not_called()
            plan.write_text(json.dumps({'inputs':{},'files':{'source':'0'*64}}))
            with self.assertRaises(ValueError):m.run(out,m.sha(plan))
    def test_lost_cid_ack_can_retire_exact_nonce_and_mount_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            fs=Path(tmp);cid=fs/'missing.cid';ident='a'*64
            info=[{'Id':ident,'Config':{'Labels':{'q2.test.nonce':'owned'}},'Mounts':[{'Source':str(fs),'Destination':'/fixture'}]}]
            seen=[]
            def run(args,**kw):
                seen.append(args)
                return subprocess.CompletedProcess(args,1 if args[-1]==ident and args[1:3]==['container','exists'] else 0)
            with patch.object(m.subprocess,'run',side_effect=run),patch.object(m.subprocess,'check_output',return_value=json.dumps(info)):
                self.assertTrue(m.retire(fs,'owned','owned-tag',cid))
            self.assertIn(['podman','rm','-f',ident],seen)
            info[0]['Config']['Labels']['q2.test.nonce']='foreign'
            with patch.object(m.subprocess,'run',return_value=subprocess.CompletedProcess([],0)) as run,patch.object(m.subprocess,'check_output',return_value=json.dumps(info)):
                with self.assertRaises(ValueError):m.retire(fs,'owned','owned-tag',cid)
                self.assertEqual(run.call_count,1)

if __name__=='__main__':unittest.main()
