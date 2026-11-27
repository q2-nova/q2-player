#!/usr/bin/env python3
"""Owned generated offline Shuffle Songs replay; caller holds whole-device lease.

Run the generated script only inside an already held/stopped normal service
fixture. It neither reads owner configuration nor owns supervision restoration.
"""
import argparse, base64, gzip, hashlib, io, json, shlex, sqlite3, wave
from contextlib import closing
from pathlib import Path


def generate(out, runtime, app_sha):
    runtime = Path(runtime)
    if (not runtime.is_absolute() or '..' in runtime.parts
            or not runtime.name.startswith('q2-offline-shuffle-')):
        raise ValueError('owned absolute runtime required')
    if len(app_sha) != 64 or any(c not in '0123456789abcdef' for c in app_sha):
        raise ValueError('exact executable hash required')
    out.mkdir(parents=True, exist_ok=False)
    config = '''[display]
backlight_seconds=0
clock_in_title=false
clicker=false
[power]
sleep_minutes=0
[wifi]
enabled=false
[tailscale]
enabled=false
[playback]
volume=0
scrobble=false
sync_queue=false
'''
    pcm = io.BytesIO()
    with wave.open(pcm, 'wb') as w:
        w.setnchannels(2); w.setsampwidth(2); w.setframerate(44100)
        w.writeframes(bytes(44100 * 4 * 10))
    wave_data = pcm.getvalue()
    database = out/'library.db'
    with closing(sqlite3.connect(database)) as db, db:
        db.execute('CREATE TABLE offline_songs(song_id TEXT PRIMARY KEY,path TEXT NOT NULL,size INTEGER NOT NULL,song TEXT NOT NULL)')
        for i in range(3):
            song = dict(id=f'generated-shuffle-{i}', title=f'Generated silent track {i}', suffix='wav', duration=10)
            db.execute('INSERT INTO offline_songs VALUES(?,?,?,?)', (song['id'], str(runtime/'generated.wav'), len(wave_data), json.dumps(song)))
        # A missing download must not enter the queue.
        missing = dict(id='generated-missing', title='Missing generated track', suffix='wav', duration=10)
        db.execute('INSERT INTO offline_songs VALUES(?,?,?,?)', (missing['id'], str(runtime/'missing.wav'), len(wave_data), json.dumps(missing)))
    inputs = '\n'.join(['back','await 15000 screen:Q2','scroll 2147483647','center','await 15000 screen:Now Playing','wait 1800','play','wait 200'])+'\n'
    files = {'config.toml':config.encode(),'library.db':database.read_bytes(),'inputs.txt':inputs.encode(),'generated.wav':wave_data}
    commands=[]
    for name,data in files.items():
        (out/name).write_bytes(data)
        blob=base64.b64encode(gzip.compress(data,mtime=0)).decode()
        commands.append(f"printf '%s' '{blob}' | base64 -d | gzip -d > {shlex.quote(str(runtime/name))}")
    rt=shlex.quote(str(runtime))
    script=f'''#!/bin/sh
set -eu
umask 077
app=${{1:-/usr/bin/q2nd}}
[ "$(sha256sum "$app" | cut -d' ' -f1)" = '{app_sha}' ]
[ ! -e {rt} ] && [ ! -L {rt} ]
mkdir -p {rt}/runtime
{chr(10).join(commands)}
unset Q2ND_HEALTH_SOCKET Q2ND_REQUIRE_HEALTH Q2ND_INSTALLED_STORAGE Q2ND_SD_READY Q2ND_LEGACY_MEDIA Q2ND_TLS_CA Q2ND_DATA Q2_EMULATOR_TEST
Q2ND_RUNTIME={rt}/runtime timeout -k 3 45 "$app" --mainline --data {rt} --script {rt}/inputs.txt > {rt}/app.log 2>&1
awk '$6=="first-frame" {{found++}} END{{exit found==1?0:1}}' {rt}/runtime/release-metrics
echo Q2_OFFLINE_SHUFFLE_DB_BEGIN
base64 {rt}/library.db
echo Q2_OFFLINE_SHUFFLE_DB_END
if [ -f {rt}/library.db-wal ]; then
    echo Q2_OFFLINE_SHUFFLE_WAL_BEGIN
    base64 {rt}/library.db-wal
    echo Q2_OFFLINE_SHUFFLE_WAL_END
fi
echo Q2_OFFLINE_SHUFFLE_RUN_PASS
'''
    (out/'run.sh').write_text(script)
    receipt={'synthetic':True,'runtime':str(runtime),'app_sha256':app_sha,'expected_ids':[f'generated-shuffle-{i}' for i in range(3)],'files':{name:hashlib.sha256(data).hexdigest() for name,data in files.items()}}
    (out/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
    return receipt


def validate_database(path):
    with closing(sqlite3.connect(f'file:{Path(path).resolve()}?mode=ro',uri=True)) as db:
        row=db.execute("SELECT value FROM kv WHERE key='queue'").fetchone()
        if row is None: raise ValueError('generated playback queue missing')
        queue=json.loads(row[0]); ids=sorted(song['id'] for song in queue['songs'])
        if ids != [f'generated-shuffle-{i}' for i in range(3)]: raise ValueError('wrong generated queue')
        if not 0 <= queue['index'] < 3 or not 500 <= queue['position_ms'] < 10000: raise ValueError('generated playback did not progress')
    return dict(passed=True,queue_count=3,position_ms=queue['position_ms'],index=queue['index'])


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True);p.add_argument('--runtime',required=True);p.add_argument('--app-sha256',required=True)
    a=p.parse_args();generate(a.output,a.runtime,a.app_sha256)
