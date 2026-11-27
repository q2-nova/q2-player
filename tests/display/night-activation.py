#!/usr/bin/env python3
"""Exact-app/timezone activation regression in generated QEMU-user chroots.

Frozen synthetic wall time, accelerated monotonic fade, actual Settings input
and actual libc conversion/rendering. This is component integration, not a full
KMS/hardware or comprehensive DST pass. No owner data is imported.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import random
import shutil
import subprocess
from zoneinfo import ZoneInfo
from PIL import Image

ROOT=Path(__file__).resolve().parents[2]


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    for key in ['sdk','installed-root','tzdata','before-app','app','qemu-user']:
        ap.add_argument('--'+key,type=Path,required=True)
    ap.add_argument('--out',type=Path,default=ROOT/'build/perf/night-activation')
    ap.add_argument('--before-settings-scroll',type=int,default=3,help='generated configured Home layout of the pre-books baseline')
    ap.add_argument('--settings-scroll',type=int,default=4,help='generated configured Home layout of the candidate; use 3 for V0.19')
    args=ap.parse_args();out=args.out.resolve()
    assert out.is_relative_to(ROOT/'build'), 'generated output must stay under build/'
    out.mkdir(parents=True,exist_ok=True)
    cc=args.sdk/'bin/mipsel-buildroot-linux-gnu-gcc'
    flags=['-O2','-mabi=32','-march=mips32r2','-mfp64','-mnan=2008']
    probe=out/'probe';clock=out/'clock.so'
    subprocess.run([str(cc),*flags,str(ROOT/'tests/display/timezone-probe.c'),'-o',str(probe)],check=True)
    subprocess.run([str(cc),*flags,'-shared','-fPIC',str(ROOT/'tests/display/night-clock.c'),'-o',str(clock)],check=True)
    container=subprocess.check_output(['podman','image','inspect','--format','{{.Id}}','q2-qemu-build'],text=True).strip()
    prefix=['podman','run','--rm','--network','none','--security-opt','label=disable','-v',f'{ROOT}:/w','-w','/w',container]
    def make_root(name,app,zones,backlight=0):
        root=out/name
        if root.exists():shutil.rmtree(root)
        (root/'lib').mkdir(parents=True);(root/'data').mkdir();(root/'tmp').mkdir();(root/'dev').mkdir()
        for name in ['libc.so.6','libm.so.6','libgcc_s.so.1','ld-linux-mipsn8.so.1']:
            shutil.copy2(args.installed_root/'lib'/name,root/'lib'/name)
        for src,dest in [(app,'q2nd'),(args.qemu_user,'qemu'),(clock,'clock.so'),(probe,'probe')]:shutil.copy2(src,root/dest)
        if zones:shutil.copytree(args.tzdata,root/'usr/share/zoneinfo',symlinks=True)
        (root/'data/config.toml').write_text('[server]\nurl="http://127.0.0.1:9"\nuser="fixture"\npassword="synthetic-test-only"\n'
            '[wifi]\nenabled=false\n[display]\ntimezone="America/Chicago"\nnight_warmth=false\n'
            f'backlight_seconds={backlight}\nclock_in_title=true\n')
        return root
    def guest(root,arguments,env=()):
        envargs=['-E','TZ=UTC']
        for item in env:envargs+=['-E',item]
        command=prefix+['chroot','/w/'+str(root.relative_to(ROOT)),'/qemu',*envargs,*arguments]
        return subprocess.check_output(command,stderr=subprocess.STDOUT,text=True,timeout=30)
    summary={'scope':'exact app + libc + generated chroot component integration; hardware final step pending',
             'container':container,'cases':[],'clock_properties':[]}
    # Four-way negative control distinguishes missing data from stale libc TZ.
    cases=[('before-no-data',args.before_app,False,1793691334,0,(255,255,255)),
           ('before-with-data',args.before_app,True,1793691334,0,(255,255,255)),
           ('fixed-no-data',args.app,False,1793691334,0,(255,255,255)),
           ('fixed-night',args.app,True,1793691334,0,(255,209,140)),
           ('fixed-day',args.app,True,int(datetime(2026,11,3,19,35,34,tzinfo=timezone.utc).timestamp()),0,(255,255,255)),
           ('fixed-half-ramp',args.app,True,int(datetime(2026,11,4,3,30,tzinfo=timezone.utc).timestamp()),0,(255,231,197)),
           ('fixed-dark-wake',args.app,True,1793691334,120,(255,209,140))]
    for name,app,zones,epoch,backlight,expected in cases:
        root=make_root(name,app,zones,backlight)
        wake='back\n' if backlight else ''
        home_scroll=args.before_settings_scroll if name.startswith('before-') else args.settings_scroll
        (root/'script').write_text(f'scroll {home_scroll}\ncenter\nscroll 2\ncenter\nscroll 2\nshot /data/off.png\n'
            'center\nwait 1200\nshot /data/tick-on.png\n'+wake+'shot /data/on.png\ncenter\nwait 1200\nshot /data/tick-off.png\n'+wake+'shot /data/restored.png\n')
        log=guest(root,['/q2nd','--headless','--data','/data','--script','/script'],['LD_PRELOAD=/clock.so',f'Q2_TEST_EPOCH={epoch}'])
        (root/'fixture.log').write_text(log)
        pixels={label:Image.open(root/f'data/{label}.png').convert('RGB').getpixel((180,280)) for label in ['off','on','restored']}
        assert pixels['off']==(255,255,255),(name,pixels)
        assert pixels['on']==expected,(name,pixels,expected)
        assert pixels['restored']==(255,255,255),(name,pixels)
        summary['cases'].append({'name':name,'epoch':epoch,'pixel':pixels,'app_sha256':hashlib.sha256(app.read_bytes()).hexdigest()})
    assert Image.open(out/'fixed-dark-wake/data/on.png').crop((0,100,375,320)).tobytes()==Image.open(out/'fixed-night/data/on.png').crop((0,100,375,320)).tobytes(), 'wake must retain the Display page rather than pop it'
    # Deterministically generated calendar/timezone conversions use the pinned
    # TZif files as the independent oracle, not the host zone database.
    root=out/'fixed-night';rng=random.Random(57)
    for index in range(32):
        zone=rng.choice(['America/Chicago','Europe/London','Australia/Lord_Howe','Asia/Kathmandu','Pacific/Chatham'])
        epoch=rng.randrange(1735689600,1924992000)
        with (args.tzdata/zone).open('rb') as f:tz=ZoneInfo.from_file(f,key=zone)
        oracle=datetime.fromtimestamp(epoch,timezone.utc).astimezone(tz)
        result=json.loads(guest(root,['/probe',zone,str(epoch)],['APPLY_TZSET=1']))
        assert (result['hour'],result['minute'],result['second'],result['offset'])==(oracle.hour,oracle.minute,oracle.second,int(oracle.utcoffset().total_seconds())),(zone,epoch,result,oracle)
        summary['clock_properties'].append({'seed':57,'index':index,'zone':zone,'epoch':epoch,'actual':result})
    inputs=[args.app,args.before_app,args.installed_root/'lib/libc.so.6',args.tzdata/'America/Chicago',
            ROOT/'tests/display/night-clock.c',ROOT/'tests/display/timezone-probe.c']
    summary['hashes']={str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in inputs}
    summary['passed']=True;(out/'result.json').write_text(json.dumps(summary,indent=2)+'\n')
    print(json.dumps({'passed':True,'activation_cases':len(cases),'seeded_clock_conversions':32,'result':str(out/'result.json')}))


if __name__=='__main__':main()
