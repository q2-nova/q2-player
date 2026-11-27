#!/usr/bin/env python3
"""Generate an owned four-case Home replay for host, MIPS or coordinated hardware.

Inputs are synthetic. The generated runner verifies the app hash, holds/resumes
normal mainline supervision only with --mainline, and exports only its own PNGs.
The caller owns the canonical emulator/hardware lock for the complete experiment.
"""
import argparse
import base64
from contextlib import closing
import hashlib
import json
from pathlib import Path
import re
import shlex
import sqlite3
import shlex
import struct

ROOT = Path(__file__).resolve().parents[2]
CASES = {"neither": "Radios", "books": "Books", "podcasts": "Podcasts", "configured-offline": "Books"}


def encoded(data):
    return base64.b64encode(data).decode()


def generate(out, runtime, app_sha, mainline, qemu_user=None, sysroot=None):
    out.mkdir(parents=True, exist_ok=False)
    commands = []
    exports = []
    expected = {}
    for name, destination in CASES.items():
        target = runtime / name
        host = out / name
        host.mkdir()
        config = "[display]\nbacklight_seconds=0\nclock_in_title=false\nclicker=false\n[power]\nsleep_minutes=0\n"
        if name == "configured-offline":
            config += "[audiobookshelf]\nurl='http://127.0.0.1:9'\ntoken='generated-token'\n"
        files = {"config.toml": config.encode()}
        if name == "books":
            def atom(kind, data):
                return struct.pack(">I", len(data) + 8) + kind + data
            # The local catalog's production bounded metadata reader; audio
            # playback is outside this Home navigation gate.
            mvhd = bytes(12) + struct.pack(">II", 1000, 60000)
            files["audiobooks/generated.m4b"] = atom(b"moov", atom(b"mvhd", mvhd))
        database = host / "library.db"
        with closing(sqlite3.connect(database)) as db, db:
            db.executescript("CREATE TABLE spoken_catalog(id TEXT PRIMARY KEY,logical TEXT NOT NULL,song TEXT NOT NULL); CREATE TABLE spoken_downloads(id TEXT PRIMARY KEY,logical TEXT NOT NULL,path TEXT NOT NULL,size INTEGER NOT NULL);")
            # A stale remote-only book exists in every case. It must never
            # expose Books by itself; Podcasts-only is an independent control.
            book = {"id": "generated-remote-book", "title": "Generated remote-only book", "spoken": {"id": "generated-remote-book", "source": {"source": "audiobookshelf", "namespace": "generated-account", "item": "generated-book", "episode": None, "content_url": "generated.m4b"}}}
            db.execute("INSERT INTO spoken_catalog VALUES(?,?,?)", (book["id"], book["id"], json.dumps(book)))
            if name == "podcasts":
                episode = {"id": "generated-episode", "title": "Generated Episode", "spoken": {"id": "generated-episode", "source": {"source": "audiobookshelf", "namespace": "generated-account", "item": "generated-show", "episode": "generated-episode", "content_url": "generated.mp3"}}}
                db.execute("INSERT INTO spoken_catalog VALUES(?,?,?)", (episode["id"], episode["id"], json.dumps(episode)))
                data = b"Generated indexed media; navigation only\n"
                files["generated-episode.mp3"] = data
                db.execute("INSERT INTO spoken_downloads VALUES(?,?,?,?)", (episode["id"], episode["id"], str(target / "generated-episode.mp3"), len(data)))
        files["library.db"] = database.read_bytes()
        # Navidrome remains unconfigured, so startup opens setup. After Back,
        # row 2 is deliberately a semantic assertion of each conditional Home.
        inputs = f"back\nawait 15000 screen:Q2\nshot {target}/home.png\nscroll -2147483648\nscroll 2\ncenter\nawait 15000 screen:{destination}\nshot {target}/destination.png\nback\nawait 15000 screen:Q2\nshot {target}/returned.png\ncenter\nawait 15000 screen:{destination}\n"
        if name == "configured-offline":
            inputs += f"back\nawait 15000 screen:Q2\nscroll 1\ncenter\nawait 15000 screen:Podcasts\nshot {target}/podcasts.png\nback\nawait 15000 screen:Q2\n"
        files["inputs.txt"] = inputs.encode()
        commands.append(f"mkdir -p {shlex.quote(str(target / 'audiobooks'))}")
        for filename, data in files.items():
            (host / filename).parent.mkdir(parents=True, exist_ok=True)
            (host / filename).write_bytes(data)
            commands.append(f"echo '{encoded(data)}' | base64 -d > {shlex.quote(str(target / filename))}")
        mode = "--mainline" if mainline else "--headless"
        prefix = f"{shlex.quote(str(qemu_user))} -cpu P5600 -L {shlex.quote(str(sysroot))} " if qemu_user else ""
        commands.append(f'fixture_pid=; setsid timeout -k 3 60 {prefix}"$app" {mode} --data {shlex.quote(str(target))} --script {shlex.quote(str(target / "inputs.txt"))} > {shlex.quote(str(target / "app.log"))} 2>&1 &')
        commands += ['fixture_pid=$!', 'status=0', 'wait "$fixture_pid" || status=$?', 'fixture_pid=', f'if [ "$status" != 0 ]; then cat {shlex.quote(str(target / "app.log"))}; exit "$status"; fi', f'echo Q2_HOME_CASE_PASS {name}']
        images = ["home", "destination", "returned"] + (["podcasts"] if name == "configured-offline" else [])
        for image in images:
            path = shlex.quote(str(target / (image + '.png')))
            exports.append(f"if [ -f {path} ]; then echo Q2_HOME_PNG {name}-{image} || export_status=1; base64 {path} || export_status=1; echo Q2_HOME_PNG_END || export_status=1; fi")
        log = shlex.quote(str(target / "app.log"))
        exports.append(f"if [ -f {log} ]; then echo Q2_HOME_LOG {name} || export_status=1; cat {log} || export_status=1; echo Q2_HOME_LOG_END || export_status=1; fi")
        expected[name] = {"destination": destination, "pngs": images, "inputs_sha256": hashlib.sha256(inputs.encode()).hexdigest()}
    service_stop = 'installed_sha=$(sha256sum /usr/bin/q2nd | cut -d\' \' -f1)\nheld=1\n/sbin/q2-installed-services hold\n' if mainline else ""
    restore = '''if /sbin/q2-installed-services resume; then
            held=0
            n=0
            restored=0
            while [ "$n" -lt 100 ]; do
                pid=$(cat /run/q2nd/player.pid 2>/dev/null || :)
                healthy=$(cat /run/q2nd/healthy 2>/dev/null || :)
                if [ -n "$pid" ] && [ "$pid" = "$healthy" ] && kill -0 "$pid" 2>/dev/null; then restored=1; break; fi
                n=$((n+1)); sleep 0.2
            done
            if [ "$restored" = 1 ] && [ "$(sha256sum /proc/$pid/exe | cut -d' ' -f1)" = "$installed_sha" ]; then
                echo Q2_HOME_PLAYER_RESTORED || cleanup_status=1
            else cleanup_status=1; echo Q2_HOME_RESTORE_HEALTH_FAILED >&2; fi
        else cleanup_status=1; echo Q2_HOME_RESUME_FAILED >&2; fi''' if mainline else ":"
    script = f'''#!/bin/sh
set -eu
app=${{1:-/usr/bin/q2nd}}
held=0
fixture_pid=
exported=0
cleanup_ran=0
cleanup_status=0
export_artifacts() {{
    [ "$exported" = 0 ] || return 0
    export_status=0
    {chr(10).join(exports)}
    exported=1
    return "$export_status"
}}
cleanup() {{
    [ "$cleanup_ran" = 0 ] || return "$cleanup_status"
    cleanup_ran=1
    set +e
    if [ -n "$fixture_pid" ]; then
        kill -TERM -- "-$fixture_pid" 2>/dev/null || :
        sleep 1
        kill -KILL -- "-$fixture_pid" 2>/dev/null || :
        wait "$fixture_pid" 2>/dev/null || :
        fixture_pid=
    fi
    export_artifacts || cleanup_status=1
    if [ "$held" = 1 ]; then {restore}; fi
    set -e
    return "$cleanup_status"
}}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
test "$(sha256sum "$app" | cut -d' ' -f1)" = {app_sha}
test ! -e {shlex.quote(str(runtime))}
{service_stop}mkdir -p {shlex.quote(str(runtime))}
unset Q2ND_HEALTH_SOCKET Q2ND_REQUIRE_HEALTH Q2ND_INSTALLED_STORAGE Q2ND_SD_READY
''' + "\n".join(commands) + "\ncleanup\necho Q2_HOME_FIXTURE_PASS\n"
    (out / "run.sh").write_text(script)
    (out / "fixture.json").write_text(json.dumps({"generated_only": True, "mainline_service_coordination": mainline, "app_sha256": app_sha, "runtime_root": str(runtime), "script_sha256": hashlib.sha256(script.encode()).hexdigest(), "cases": expected, "scope": "actual Home disk discovery, semantic navigation and Back; generated metadata and indexed bytes, no audio/physical wheel qualification"}, indent=2) + "\n")



def validate_generated(out):
    """Decode owned payloads and verify one coherent runtime namespace."""
    metadata=json.loads((out / "fixture.json").read_text())
    runtime=Path(metadata["runtime_root"])
    script=(out / "run.sh").read_text()
    if hashlib.sha256(script.encode()).hexdigest()!=metadata["script_sha256"]:
        raise ValueError("generated runner hash changed")
    count=0
    for encoded,target in re.findall(r"^echo '([A-Za-z0-9+/=]+)' \| base64 -d > (.+)$",script,re.MULTILINE):
        target=Path(shlex.split(target)[0])
        if not target.is_relative_to(runtime):
            raise ValueError("encoded payload target outside declared runtime")
        relative=target.relative_to(runtime)
        if base64.b64decode(encoded,validate=True)!=(out / relative).read_bytes():
            raise ValueError("encoded payload differs from generated source")
        count+=1
    if count!=sum(p.is_file() for name in metadata["cases"] for p in (out / name).rglob("*")):
        raise ValueError("missing or duplicate encoded fixture payload")
    for name,expected in metadata["cases"].items():
        if expected["destination"]!=CASES[name]:
            raise ValueError("unexpected Home fixture destination")
        case=out / name
        inputs=(case / "inputs.txt").read_text()
        if f'await 15000 screen:{expected["destination"]}' not in inputs:
            raise ValueError("missing expected Home destination barrier")
        for line in inputs.splitlines():
            if line.startswith("shot ") and not Path(line[5:]).is_relative_to(runtime / name):
                raise ValueError("capture points outside its generated case")
        with closing(sqlite3.connect(case / "library.db")) as database, database:
            for (path,) in database.execute("SELECT path FROM spoken_downloads"):
                path=Path(path)
                if not path.is_relative_to(runtime / name) or not (case / path.relative_to(runtime / name)).is_file():
                    raise ValueError("indexed Home media points outside generated files")
    return dict(payloads=count,cases=len(metadata["cases"]),runtime_root=str(runtime))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--runtime-root", type=Path, help="owned absolute target path, default OUT/runtime")
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--app-sha256")
    action.add_argument("--extract", type=Path, help="extract only this fixture's generated runner log")
    parser.add_argument("--reference", type=Path, help="compare Home menu pixels with a previously reviewed host extraction")
    parser.add_argument("--mainline", action="store_true", help="use DRM and hold/resume installed services; caller must own full canonical device/emulator lease")
    parser.add_argument("--qemu-user", type=Path, help="optional actual-MIPS user emulator for generated headless replay")
    parser.add_argument("--sysroot", type=Path)
    args = parser.parse_args()
    out = args.out.resolve()
    if not out.is_relative_to(ROOT / "build") or out == ROOT / "build":
        parser.error("use a new owned directory under build/")
    if args.extract:
        from PIL import Image
        text = args.extract.read_text()
        assert "Q2_HOME_FIXTURE_PASS" in text
        assert all("Q2_HOME_CASE_PASS " + name in text for name in CASES)
        out.mkdir(parents=True, exist_ok=False)
        found = {}
        for name, data in re.findall(r"^Q2_HOME_PNG ([a-z-]+)\n([A-Za-z0-9+/=\n]+)^Q2_HOME_PNG_END$", text, re.MULTILINE):
            assert name not in found
            image = out / (name + ".png")
            image.write_bytes(base64.b64decode(data))
            with Image.open(image) as actual:
                assert actual.size == (375, 320)
                if args.reference and name.endswith(("-home", "-returned")):
                    with Image.open(args.reference / image.name) as expected:
                        assert actual.crop((0, 30, 187, 320)).convert("RGB").tobytes() == expected.crop((0, 30, 187, 320)).convert("RGB").tobytes(), name
            found[name] = hashlib.sha256(image.read_bytes()).hexdigest()
        expected_names = {f"{case}-{shot}" for case in CASES for shot in ["home", "destination", "returned"]} | {"configured-offline-podcasts"}
        assert set(found) == expected_names
        (out / "result.json").write_text(json.dumps({"passed": True, "generated_only": True, "semantic_navigation_and_back_barriers_passed": True, "home_menu_pixels_match_reference": bool(args.reference), "png_sha256": found}, indent=2) + "\n")
        return
    if len(args.app_sha256) != 64 or any(c not in "0123456789abcdef" for c in args.app_sha256):
        parser.error("expected SHA256 required")
    if bool(args.qemu_user) != bool(args.sysroot) or (args.mainline and args.qemu_user):
        parser.error("--qemu-user requires --sysroot and headless mode")
    runtime = args.runtime_root or out / "runtime"
    if not runtime.is_absolute() or len(runtime.parts) < 3 or ".." in runtime.parts:
        parser.error("use a dedicated absolute runtime directory")
    generate(out, runtime, args.app_sha256, args.mainline, args.qemu_user, args.sysroot)


if __name__ == "__main__":
    main()
