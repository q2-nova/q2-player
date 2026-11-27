#!/usr/bin/env python3
"""Generate the player-facing files for a listening cadence test."""
import argparse
from contextlib import closing
import json
from pathlib import Path
import shutil
import sqlite3


def seed(directory, fixture, scene):
    directory.mkdir()
    media = directory / "generated.m4b"
    shutil.copy2(fixture, media)
    backlight = 1 if scene == "dark" else 0
    (directory / "config.toml").write_text(f"""[server]
url="http://127.0.0.1:9"
user="generated"
password="synthetic-only"
[display]
timezone="UTC"
clock_in_title=false
backlight_seconds={backlight}
clicker=false
night_warmth=false
[power]
sleep_minutes=0
[wifi]
enabled=false
[tailscale]
enabled=false
""")
    song = dict(id="generated-cadence", title="Generated Cadence", artist="Synthetic",
                album="Fixture", duration=120, suffix="m4b")
    with closing(sqlite3.connect(directory / "library.db")) as db, db:
        db.execute("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT NOT NULL)")
        db.execute("CREATE TABLE offline_songs(song_id TEXT PRIMARY KEY,path TEXT NOT NULL,size INTEGER NOT NULL,song TEXT NOT NULL)")
        db.executemany("INSERT INTO kv VALUES (?,?)", (
            ("server", "http://127.0.0.1:9|generated"),
            ("queue", json.dumps(dict(songs=[song], index=0, position_ms=0))),
        ))
        db.execute("INSERT INTO offline_songs VALUES (?,?,?,?)",
                   (song["id"], str(media), media.stat().st_size, json.dumps(song)))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("media", type=Path)
    parser.add_argument("scene", choices=("dark", "visible"))
    args = parser.parse_args()
    seed(args.directory, args.media, args.scene)
