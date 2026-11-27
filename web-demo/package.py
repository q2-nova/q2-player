#!/usr/bin/env python3
"""Package the Q2 browser demo as a static site and a single offline HTML file."""

import argparse
import base64
import hashlib
import json
import re
from pathlib import Path
from shutil import rmtree
from subprocess import CalledProcessError, TimeoutExpired, run
from tempfile import NamedTemporaryFile, TemporaryDirectory, mkdtemp
from urllib.parse import urlsplit
from urllib.request import Request, urlopen


SITE = Path(__file__).resolve().parent / "site"
FONT_LICENSE = SITE.parent.parent / "assets" / "fonts" / "OpenSans-LICENSE.txt"
MUSIC_CACHE = SITE.parent.parent / "build" / "music-catalog"
HOSTED_DISCO_BITRATE = 224
AUDIO_KEYS = (
    "tidal", "northline", "afterhours", "sundial", "softsignal",
    "fieldnotes", "copper", "pinksky", "radio", *(f"spoken-{i}" for i in range(6)),
)
WASM_V1_HEADER = b"\x00asm\x01\x00\x00\x00"


def replace_one(source: str, marker: str, replacement: str) -> str:
    if source.count(marker) != 1:
        raise ValueError(f"expected one {marker!r} in the site HTML")
    return source.replace(marker, replacement, 1)


def music_asset(key: str, entry: dict, cache: Path) -> tuple[str, bytes]:
    suffix = Path(urlsplit(entry["url"]).path).suffix
    if not re.fullmatch(r"[a-z][a-z0-9-]*", key) or suffix not in {".m4a", ".mp3"}:
        raise ValueError(f"invalid music asset name: {key}")
    size = entry.get("bytes")
    digest = entry.get("sha256", "")
    if (type(size) is not int or size <= 0 or not isinstance(digest, str)
            or not re.fullmatch(r"[a-f0-9]{64}", digest)):
        raise ValueError(f"invalid music asset integrity data: {key}")
    path = cache / f"{key}{suffix}"
    try:
        with path.open("rb") as source:
            contents = source.read(size + 1)
        cached = True
    except FileNotFoundError:
        request = Request(entry["url"], headers={"User-Agent": "q2-browser-demo/1.0"})
        with urlopen(request, timeout=30) as source:
            contents = source.read(size + 1)
        cached = False
    if len(contents) != size or hashlib.sha256(contents).hexdigest() != digest:
        where = path if cached else entry["url"]
        raise ValueError(f"music asset {key} failed byte count or SHA-256 verification at {where}")
    if not cached:
        cache.mkdir(parents=True, exist_ok=True)
        with NamedTemporaryFile(dir=cache, delete=False) as pending:
            temporary = Path(pending.name)
            try:
                pending.write(contents)
                pending.close()
                temporary.replace(path)
            finally:
                temporary.unlink(missing_ok=True)
    return f"music-{key}{suffix}", contents


def catalog_html(template: str, catalog: dict) -> str:
    embedded = json.dumps(catalog, ensure_ascii=False, separators=(",", ":")).replace("<", "\\u003c")
    return replace_one(template, 'type="application/json">{}</script>',
                       f'type="application/json">{embedded}</script>')


def compact_disco_track(key: str, original: bytes) -> bytes:
    # Write a seekable MP3 so FFmpeg can include duration and gapless metadata.
    with TemporaryDirectory(prefix="q2-demo-music-") as scratch:
        source = Path(scratch) / "original.mp3"
        target = Path(scratch) / "hosted.mp3"
        source.write_bytes(original)
        try:
            run([
                "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-xerror",
                "-i", str(source), "-map", "0:a:0", "-map_metadata", "-1",
                "-c:a", "libmp3lame", "-b:a", f"{HOSTED_DISCO_BITRATE}k",
                "-threads", "1", "-fflags", "+bitexact", str(target),
            ], check=True, timeout=120)
        except FileNotFoundError as error:
            raise ValueError("hosted music requires FFmpeg with the libmp3lame encoder") from error
        except (CalledProcessError, TimeoutExpired) as error:
            raise ValueError(f"could not re-encode hosted music asset {key}: {error}") from error
        return target.read_bytes()


def distribution(wasm_path: Path, music_cache: Path | None = MUSIC_CACHE) -> dict[str, bytes]:
    wasm = wasm_path.read_bytes()
    if not wasm.startswith(WASM_V1_HEADER):
        raise ValueError(f"expected a version 1 WebAssembly file at {wasm_path}")

    catalog = {}
    hosted_music = {}
    music_files = {}
    for collection in ("music", "spoken", "radio"):
        entries = json.loads((SITE.parent / "catalog" / f"{collection}.json").read_text())
        for key, entry in entries.items():
            if key in catalog or not all(entry.get(field) for field in ("title", "artist", "url", "source")):
                raise ValueError(f"duplicate or incomplete media entry: {key}")
            if not all(entry[field].startswith("https://") for field in ("source", "url")):
                raise ValueError(f"insecure media entry: {key}")
            catalog[key] = entry
            if collection == "music" and music_cache is not None:
                name, contents = music_asset(key, entry, music_cache)
                hosted_entry = {**entry, "localUrl": f"./{name}"}
                if entry["album"] == "Disco Ultralounge":
                    contents = compact_disco_track(key, contents)
                    hosted_entry["credit"] = f"{entry['artist']} (hosted MP3 re-encoded)"
                music_files[name] = contents
                hosted_music[key] = hosted_entry
    template = (SITE / "index.html").read_text()
    html = catalog_html(template, {**catalog, **hosted_music}).encode("utf-8")
    css = (SITE / "style.css").read_bytes()
    scripts = {name: (SITE / name).read_bytes() for name in ("audio.js", "streams.js", "timezone.js", "demo.js")}
    previews = {
        f"audio-{key}.mp3": (SITE.parent / "audio" / f"{key}.mp3").read_bytes()
        for key in AUDIO_KEYS
    }
    license_bytes = FONT_LICENSE.read_bytes()
    license_notice = license_bytes.decode("utf-8").replace("\r\n", "\n")
    if any(b"</script" in js.lower() for js in scripts.values()) or b"</style" in css.lower():
        raise ValueError("site assets contain a closing HTML tag that cannot be inlined")
    if "-->" in license_notice:
        raise ValueError("font license cannot be inlined as a comment")

    standalone = replace_one(
        catalog_html(template, catalog),
        '<link rel="stylesheet" href="./style.css">',
        f"<style>\n{css.decode('utf-8')}</style>",
    )
    standalone = replace_one(standalone, '      <a class="offline-copy" href="./q2-player.html" download>Download offline demo</a>\n', "")
    encoded = base64.b64encode(wasm).decode("ascii")
    for name in scripts:
        standalone = replace_one(standalone, f'<script src="./{name}" defer></script>', "")
    embedded_previews = "\n".join(
        f'<script id="{Path(name).stem}" type="audio/mpeg">{base64.b64encode(data).decode("ascii")}</script>'
        for name, data in previews.items()
    )
    inline_scripts = "\n".join(f"<script>\n{js.decode('utf-8')}</script>" for js in scripts.values())
    standalone = replace_one(
        standalone,
        "</body>",
        f'<script id="player-wasm" type="application/wasm">{encoded}</script>\n'
        f"<!-- Open Sans font license\n{license_notice}-->\n"
        f"{embedded_previews}\n{inline_scripts}\n</body>",
    )
    return {
        "index.html": html,
        "style.css": css,
        **scripts,
        **previews,
        **music_files,
        "player.wasm": wasm,
        FONT_LICENSE.name: license_bytes,
        "q2-player.html": standalone.encode("utf-8"),
    }


def publish(files: dict[str, bytes], out: Path) -> None:
    if out.is_symlink() or (out.exists() and not out.is_dir()):
        raise ValueError(f"output must be a real directory: {out}")
    if out.exists():
        for path in out.iterdir():
            if path.name not in files or path.is_symlink() or not path.is_file():
                raise ValueError(f"output contains an unrelated entry: {path}")

    out.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(mkdtemp(prefix=".q2-demo-", dir=out.parent))
    staging = temporary / "new"
    previous = temporary / "previous"
    published = False
    try:
        staging.mkdir()
        for name, contents in files.items():
            (staging / name).write_bytes(contents)
        if out.exists():
            out.rename(previous)
        staging.rename(out)
        published = True
    except BaseException:
        if previous.exists() and not out.exists():
            try:
                previous.rename(out)
            except OSError as error:
                raise OSError(f"previous output remains at {previous}: {error}") from error
        raise
    finally:
        if published or not previous.exists():
            rmtree(temporary)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wasm", type=Path, help="compiled player.wasm")
    parser.add_argument("--out", required=True, type=Path, help="demo output directory")
    parser.add_argument("--music-cache", type=Path, default=MUSIC_CACHE,
                        help="cache of the original music recordings")
    parser.add_argument("--without-hosted-music", action="store_true",
                        help="use original URLs in the hosted demo instead of staging music files")
    args = parser.parse_args()
    try:
        publish(distribution(args.wasm, None if args.without_hosted_music else args.music_cache), args.out)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{parser.prog}: {error}\n")
    print(f"Static site and offline player written to {args.out}")


if __name__ == "__main__":
    main()
