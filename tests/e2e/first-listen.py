#!/usr/bin/env python3
"""Exercise generated card playback through the native simulator."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
START = """\
await 10000 screen:Navidrome Server
back
await 10000 screen:Q2
"""
OFFLINE = """\
record-text
scroll 5
center
await 10000 screen:Settings
scroll 3
center
await 10000 screen:Connections
scroll 4
center
present 10000
text offline-mode
back
await 10000 screen:Settings
back
await 10000 screen:Q2
scroll -3
"""
LISTEN = """\
center
await 10000 screen:Books
scroll 13
center
await 10000 idle:On This Card
center
await 10000 idle:Book Info
center
await 10000 screen:Now Playing
await-play 10000 playing
probe playing-start
await-play 10000 playing 750
probe playing-end
play
await-play 10000 paused
probe paused-start
wait 500
probe paused-end
"""


def run(command, log, env, timeout):
    with log.open("xb") as output:
        try:
            subprocess.run(command, cwd=ROOT, env=env, stdout=output,
                           stderr=subprocess.STDOUT, timeout=timeout, check=True)
        except subprocess.SubprocessError as exc:
            raise ValueError(f"{command[0]} failed; see {log}") from exc


def read_probe(runtime, name):
    return json.loads((runtime / f"numeric-{name}.json").read_text())


def listen(player, fixture, out, offline):
    runtime = out / "runtime"
    runtime.mkdir(mode=0o700, parents=True)
    marker = runtime / "numeric-probe-enabled"
    marker.write_bytes(b"Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n")
    marker.chmod(0o600)
    card = out / "card"
    books = card / "audiobooks"
    books.mkdir(parents=True)
    shutil.copyfile(fixture, books / fixture.name)
    script = out / "script.txt"
    script.write_text(START + (OFFLINE if offline else "scroll 2\n") + LISTEN)

    # The player inherits no host configuration or credentials.
    env = {"Q2ND_NUMERIC_PROBE": "1", "Q2ND_INSTALLED_STORAGE": "0",
           "Q2ND_RUNTIME": str(runtime), "RUST_LOG": "q2nd=info"}
    run([player, "--headless", "--data", card, "--script", script],
        out / "player.log", env, timeout=100)
    if offline:
        mode = read_probe(runtime, "offline-mode")
        labels = mode["current"]
        if mode["title"] != "Connections" or ("On", "Offline Mode") not in zip(labels, labels[1:]):
            raise ValueError("Offline Mode did not turn on")

    start, end, paused, after_pause = (
        read_probe(runtime, name) for name in
        ("playing-start", "playing-end", "paused-start", "paused-end")
    )
    observations = (start, end, paused, after_pause)
    for observed, state in zip(observations, (1, 1, 2, 2), strict=True):
        if (observed["schema"] != 1 or observed["play_state"] != state
                or observed["error"] or observed["pause_admission_failed"]):
            raise ValueError("Unexpected playback state or player error")
        if any(observed[key] != start[key] for key in
               ("pid", "generation", "seek_revision", "queue_index", "selection_tag")):
            raise ValueError("Selection changed during the listen")
    advanced = end["position_ms"] - start["position_ms"]
    if start["selection_tag"] is None or advanced < 750 or not end["output_owned"]:
        raise ValueError("Local playback did not advance with output open")
    if paused["position_ms"] != after_pause["position_ms"]:
        raise ValueError("Playback continued while paused")
    return {"selection_tag": start["selection_tag"], "advanced_ms": advanced,
            "paused_at_ms": paused["position_ms"], "paused_for_ms": 500}


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True, help="fresh directory under build/")
    parser.add_argument("--player", type=Path, help="reuse an explicitly selected native q2nd binary")
    args = parser.parse_args()
    out = args.out.resolve()
    if not out.is_relative_to(ROOT / "build"):
        parser.error("--out must be under the checkout's build directory")
    out.mkdir(parents=True)

    player = args.player
    if player is None:
        temporary = out / "tmp"
        temporary.mkdir()
        target = ROOT / "build/first-listen-target"
        env = {**os.environ, "TMPDIR": str(temporary), "CARGO_PROFILE_DEV_DEBUG": "0"}
        run(["cargo", "build", "--locked", "-p", "q2nd", "-j", "2", "--target-dir", target],
            out / "build.log", env, timeout=1200)
        player = target / "debug/q2nd"
    player = player.resolve()

    fixture = out / "generated.m4b"
    run(["ffmpeg", "-v", "error", "-n", "-f", "lavfi", "-i", "sine=frequency=550:duration=15",
         "-metadata", "title=Generated First Listen", "-c:a", "aac", "-f", "mp4", fixture],
        out / "ffmpeg.log", os.environ, timeout=30)
    first = listen(player, fixture, out / "first", offline=False)
    replay = listen(player, fixture, out / "offline", offline=True)
    if replay["selection_tag"] != first["selection_tag"]:
        raise ValueError("Offline replay selected different media")
    result = {"player_sha256": sha256(player), "fixture_sha256": sha256(fixture),
              "first": first, "offline_replay": replay}
    (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"First listen advanced {first['advanced_ms']} ms, offline replay advanced "
          f"{replay['advanced_ms']} ms, and both paused. Receipts: {out}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as exc:
        raise SystemExit(f"First listen failed: {exc}") from exc
