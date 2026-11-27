#!/usr/bin/env python3
"""Download the Q2 SDK and compiler archives selected for a device build."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile
from urllib.parse import quote

ROOT = Path(__file__).resolve().parents[1]
REQUIRED = {"sdk": ("q2-os", "buildroot"), "compiler": ("q2-toolchain", "llvm-project", "rust")}


def validate(lock, components):
    if lock.get("format") != 1 or set(lock.get("tools", {})) != set(REQUIRED):
        raise ValueError("expected a format 1 lock with sdk and compiler entries")
    for name, required in REQUIRED.items():
        entry = lock["tools"][name]
        sources = entry["sources"]
        if set(sources) != set(required) or any(
            not re.fullmatch(r"[0-9a-f]{40}", sources[key]) or sources[key] != components[key]
            for key in required
        ):
            raise ValueError(f"{name}: source revisions differ from component-lock.json")
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", entry["repository"]):
            raise ValueError(f"{name}: expected a GitHub owner/repository")
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]*", entry["tag"]) or ".." in entry["tag"]:
            raise ValueError(f"{name}: expected a release tag")
        if not isinstance(entry["files"], dict) or not entry["files"]:
            raise ValueError(f"{name}: expected asset filenames and SHA-256 values")
        for filename, digest in entry["files"].items():
            if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", filename) or not re.fullmatch(r"[0-9a-f]{64}", digest):
                raise ValueError(f"{name}: invalid asset name or SHA-256")


def check_tag(entry, producer):
    repo = entry["repository"]
    tag = quote(entry["tag"], safe="/")
    endpoint = f"repos/{repo}/git/ref/tags/{tag}"
    while True:
        data = json.loads(subprocess.check_output(["gh", "api", "--hostname", "github.com", endpoint]))["object"]
        if data["type"] != "tag":
            break
        endpoint = f"repos/{repo}/git/tags/{data['sha']}"
    if data["type"] != "commit" or data["sha"] != entry["sources"][producer]:
        raise ValueError(f"{repo}: release tag does not point to the locked producer commit")


def verify_file(path, expected):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"{path.name}: expected a regular archive file")
    with path.open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if actual != expected:
        raise ValueError(f"{path.name}: SHA-256 mismatch")


def fetch(lock, out):
    if out.exists():
        for name, entry in lock["tools"].items():
            directory = out / name
            if directory.is_symlink() or {p.name for p in directory.iterdir()} != set(entry["files"]):
                raise ValueError(f"{name}: existing files differ from the lock")
            for filename, digest in entry["files"].items():
                verify_file(directory / filename, digest)
        return
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=out.parent) as work:
        stage = Path(work) / "downloads"
        for name, required in REQUIRED.items():
            entry = lock["tools"][name]
            check_tag(entry, required[0])
            directory = stage / name
            directory.mkdir(parents=True)
            for filename, digest in entry["files"].items():
                path = directory / filename
                subprocess.run(["gh", "release", "download", entry["tag"], "--repo", "github.com/" + entry["repository"],
                                "--pattern", filename, "--output", str(path)], check=True)
                verify_file(path, digest)
        stage.rename(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock", type=Path, default=ROOT / "build-tools.lock.json")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if not args.lock.is_file():
        parser.exit(1, "Missing build-tools lock. Select qualified component releases or use the source build.\n")
    try:
        lock = json.loads(args.lock.read_text())
        validate(lock, json.loads((ROOT / "component-lock.json").read_text()))
        fetch(lock, args.out)
    except (KeyError, TypeError, AttributeError, OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")
    print(f"Verified SDK and compiler archives in {args.out}")


if __name__ == "__main__":
    main()
