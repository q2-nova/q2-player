#!/usr/bin/env python3
"""Generate an owned, bounded UI replay for a disposable full-system guest.

The script never contacts a physical device. The parent boot runner transfers
the generated shell text to its generated SSH guest. All media/config/DB inputs
are synthetic; no owner state is used.
"""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import sqlite3

ROOT = Path(__file__).resolve().parents[2]
GUEST = "/tmp/q2-ui-fixture"
SHOTS = ["playback", "default-speed", "downloads", "cache", "other", "free", "storage-parent", "music-menu", "shuffle-on", "repeat-one", "cancelled-menu"]


def sha(data):
    return hashlib.sha256(data).hexdigest()


def generate(out, app_sha):
    out.mkdir(parents=True, exist_ok=False)
    song = {"id": "generated-ui-song", "title": "Generated Music", "artist": "Generated Artist",
            "album": "Generated Album", "duration": 60, "suffix": "wav", "size": 3 * 1024**2}
    db_path = out / "generated.db"
    with sqlite3.connect(db_path) as db:
        # Fixture wire/storage inputs, never menu definitions. The actual app
        # creates the rest of its schema and consumes these canonical tables.
        db.executescript("""
CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE song_cache(song_id TEXT NOT NULL,variant TEXT NOT NULL,path TEXT NOT NULL,size INTEGER NOT NULL,used INTEGER NOT NULL,PRIMARY KEY(song_id,variant));
CREATE TABLE offline_songs(song_id TEXT PRIMARY KEY,path TEXT NOT NULL,size INTEGER NOT NULL,song TEXT NOT NULL);
CREATE TABLE spoken_downloads(id TEXT PRIMARY KEY,logical TEXT NOT NULL,path TEXT NOT NULL,size INTEGER NOT NULL);
""")
        # A queue-wide shuffle needs at least two items in the real engine.
        # A one-item queue cannot acknowledge On; preserve that failed run.
        second_song = {**song, "id": "generated-ui-song-2", "title": "Generated Music 2"}
        db.execute("INSERT INTO kv VALUES('queue',?)", (json.dumps({"songs": [song, second_song], "index": 0, "position_ms": 12000}),))
        db.execute("INSERT INTO song_cache VALUES(?,?,?,?,0)", (song["id"], "raw", GUEST + "/cache.wav", 3 * 1024**2))
        db.execute("INSERT INTO offline_songs VALUES(?,?,?,?)", ("generated-download", GUEST + "/download.wav", 4 * 1024**2, json.dumps({**song, "id": "generated-download"})))
        db.execute("INSERT INTO spoken_downloads VALUES(?,?,?,?)", ("generated-spoken", "generated-book", GUEST + "/spoken.m4b", 2 * 1024**2))
    inputs = f"""back
await 15000 screen:Q2
scroll -2147483648
scroll 4
center
await 15000 screen:Settings
scroll 1
center
await 15000 screen:Playback
shot {GUEST}/playback.png
scroll 3
center
await 15000 screen:Default Speed
shot {GUEST}/default-speed.png
back
await 15000 screen:Playback
back
await 15000 screen:Settings
scroll 3
center
await 15000 screen:Storage
center
await 30000 screen:Storage Usage
shot {GUEST}/downloads.png
scroll 1
shot {GUEST}/cache.png
scroll 1
shot {GUEST}/other.png
scroll 1
shot {GUEST}/free.png
back
await 15000 screen:Storage
shot {GUEST}/storage-parent.png
back
await 15000 screen:Settings
back
await 15000 screen:Q2
hold back
await 15000 screen:Now Playing
hold center
await 15000 overlay:Now Playing
shot {GUEST}/music-menu.png
scroll 3
center
await 15000 screen:Now Playing
await 15000 shuffle:on
hold center
await 15000 overlay:Now Playing
shot {GUEST}/shuffle-on.png
scroll 4
center
await 15000 screen:Now Playing
await 15000 repeat:one
hold center
await 15000 overlay:Now Playing
shot {GUEST}/repeat-one.png
back
await 15000 screen:Now Playing
shot {GUEST}/cancelled-menu.png
"""
    (out / "inputs.txt").write_text(inputs)
    database = base64.b64encode(db_path.read_bytes()).decode()
    encoded_inputs = base64.b64encode(inputs.encode()).decode()
    exports = "\n".join(f"if [ -f {GUEST}/{name}.png ]; then echo Q2_UI_PNG {name}; base64 {GUEST}/{name}.png; echo Q2_UI_PNG_END; fi" for name in SHOTS)
    script = f"""#!/bin/sh
set -eu
fixture_pid=
mounted=0
stopped=0
exported=0
export_artifacts() {{
    [ "$exported" = 0 ] || return 0
    if [ -f {GUEST}/app.log ]; then echo Q2_UI_APP_LOG_BEGIN; cat {GUEST}/app.log; echo Q2_UI_APP_LOG_END; fi
    {exports}
    exported=1
}}
cleanup() {{
    if [ -n "$fixture_pid" ]; then kill -TERM "$fixture_pid" 2>/dev/null || :; wait "$fixture_pid" 2>/dev/null || :; fi
    if [ "$mounted" = 1 ]; then export_artifacts; fi
    if [ "$mounted" = 1 ]; then umount {GUEST} || return 1; mounted=0; fi
    if [ "$stopped" = 1 ]; then /etc/init.d/S80q2-player start; stopped=0; fi
}}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
test ! -e {GUEST}
test "$(sha256sum /usr/bin/q2nd | cut -d' ' -f1)" = {app_sha}
/usr/bin/q2nd --help | grep -Fq 'await MS'
echo Q2_UI_APP_SHA256 {app_sha}
stopped=1
/etc/init.d/S80q2-player stop
mkdir {GUEST}
mount -t tmpfs -o size=16m tmpfs {GUEST}
mounted=1
echo '{database}' | base64 -d > {GUEST}/library.db
echo '{encoded_inputs}' | base64 -d > {GUEST}/inputs.txt
cat > {GUEST}/config.toml <<'Q2_CONFIG'
[display]
backlight_seconds=0
clock_in_title=false
clicker=false
[power]
sleep_minutes=0
Q2_CONFIG
dd if=/dev/zero of={GUEST}/cache.wav bs=1048576 count=3 2>/dev/null
dd if=/dev/zero of={GUEST}/download.wav bs=1048576 count=4 2>/dev/null
dd if=/dev/zero of={GUEST}/spoken.m4b bs=1048576 count=2 2>/dev/null
echo '{{"kind":"storage-render","scene":"legacy","iterations":1}}' > {GUEST}/legacy.json
echo '{{"kind":"storage-render","scene":"scanline","iterations":1}}' > {GUEST}/scanline.json
echo Q2_UI_RASTER_LEGACY_BEGIN
status=0
timeout 60 /usr/bin/q2nd --bench {GUEST}/legacy.json || status=$?
echo Q2_UI_RASTER_LEGACY_END "$status"
echo Q2_UI_RASTER_SCANLINE_BEGIN
timeout 20 /usr/bin/q2nd --bench {GUEST}/scanline.json
echo Q2_UI_RASTER_SCANLINE_END 0
unset Q2ND_HEALTH_SOCKET Q2ND_REQUIRE_HEALTH Q2ND_INSTALLED_STORAGE Q2ND_SD_READY
timeout 120 /usr/bin/q2nd --headless --data {GUEST} --script {GUEST}/inputs.txt > {GUEST}/app.log 2>&1 &
fixture_pid=$!
status=0
wait "$fixture_pid" || status=$?
fixture_pid=
if [ "$status" != 0 ]; then echo Q2_UI_APP_FAILED "$status"; exit "$status"; fi
! grep -q panicked {GUEST}/app.log
export_artifacts
echo Q2_UI_FIXTURE_PASS
cleanup
n=0
while :; do
    if [ -s /run/q2nd/healthy ] && [ "$(cat /run/q2nd/healthy)" = "$(cat /run/q2nd/player.pid)" ] && kill -0 "$(cat /run/q2nd/player.pid)"; then break; fi
    n=$((n+1)); [ "$n" -lt 100 ] || exit 1; sleep 0.2
done
test "$(cat /run/q2nd/healthy)" = "$(cat /run/q2nd/player.pid)"
kill -0 "$(cat /run/q2nd/player.pid)"
echo Q2_UI_PLAYER_RESTORED
"""
    (out / "guest.sh").write_text(script)
    (out / "fixture.json").write_text(json.dumps({"hardware": False, "generated_only": True,
        "expected_app_sha256": app_sha, "guest_script_sha256": sha(script.encode()),
        "input_script_sha256": sha(inputs.encode()), "generated_database_sha256": sha(db_path.read_bytes()),
        "screenshots": SHOTS, "scope": "full-system guest; actual app headless UI and filesystem; no DRM/evdev/physical-wheel claim"}, indent=2) + "\n")


def extract(log, out):
    from PIL import Image
    text = log.read_text()
    assert "Q2_UI_FIXTURE_PASS" in text and "Q2_UI_PLAYER_RESTORED" in text, "missing replay or restoration marker"
    timings = {}
    for line in text.splitlines():
        if line.startswith('{'):
            value = json.loads(line)
            if value.get('kind') == 'storage-render' and value.get('result'):
                timings[value['result']['rasterizer']] = value
    assert 'scanline' in timings and not timings['scanline']['error'], "missing actual optimized raster timing"
    if 'legacy' in timings:
        assert timings['legacy']['result']['pixel_md5'] == timings['scanline']['result']['pixel_md5'], "native raster pixels differ"
    else:
        assert 'Q2_UI_RASTER_LEGACY_END 124' in text, "baseline failed without a bounded timeout result"
    out.mkdir(parents=True, exist_ok=False)
    found = {}
    for match in re.finditer(r"^Q2_UI_PNG ([a-z-]+)\n([A-Za-z0-9+/=\n]+)^Q2_UI_PNG_END$", text, re.MULTILINE):
        name, encoded = match.groups()
        assert name in SHOTS and name not in found
        data = base64.b64decode(encoded)
        (out / f"{name}.png").write_bytes(data)
        with Image.open(out / f"{name}.png") as image:
            assert image.size == (375, 320)
            found[name] = sha(data)
    assert set(found) == set(SHOTS), sorted(found)
    for name, expected in [("downloads", "Downloads"), ("cache", "Song-Cache"), ("other", "Other-Files"), ("free", "Free")]:
        with Image.open(out / f"{name}.png") as actual, Image.open(ROOT / f"build/ui/storage/light-{expected}.png") as host:
            assert actual.crop((25, 248, 350, 274)).convert("RGB").tobytes() == host.crop((25, 248, 350, 274)).convert("RGB").tobytes(), f"selected wedge label differs for {name}"
            colors = set(actual.convert("RGB").getdata())
            assert all(color in colors for color in [(50, 161, 120), (56, 123, 212), (219, 151, 58), (166, 173, 183)]), "missing real storage wedge colors"
    assert len({found[name] for name in ["music-menu", "shuffle-on", "repeat-one"]}) == 3, "queue menu must reflect each actual player change"
    (out / "result.json").write_text(json.dumps({"passed": True, "hardware": False, "png_sha256": found,
        "selected_wedge_labels_match_host": True, "player_restored": True,
        "actual_raster_timings": timings, "legacy_timeout_lower_bound_s": None if 'legacy' in timings else 60}, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--app-sha256")
    action.add_argument("--extract", type=Path, metavar="GENERATED_GUEST_LOG")
    args = parser.parse_args()
    out = args.out.resolve()
    if not out.is_relative_to(ROOT / "build") or out == ROOT / "build": parser.error("use a new owned directory under build/")
    if args.extract:
        if not args.extract.resolve().is_relative_to(ROOT / "build"): parser.error("extract only generated build/ logs")
        extract(args.extract, out)
    else:
        if not re.fullmatch(r"[0-9a-f]{64}", args.app_sha256): parser.error("expected app SHA256 required")
        generate(out, args.app_sha256)


if __name__ == "__main__":
    main()
