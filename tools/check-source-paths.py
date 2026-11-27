#!/usr/bin/env python3
"""Check names in the player Git history for local account and deployment state."""
import os
from pathlib import PurePosixPath
import subprocess
import sys

# The vendored Cargo provenance and font attributes ship with upstream source.
FILES = {".gitignore", "assets/fonts/noto-emoji/.gitattributes"}
PROVIDER_FILES = {"hosting.json", "project.pbxuser", ".firebaserc", "credentials.json",
                  "service-account.json", "id_ed25519", "id_rsa", "terraform.tfstate"}


def allowed(path):
    parts = PurePosixPath(path).parts
    if not parts or any(part.lower() in PROVIDER_FILES for part in parts):
        return False
    hidden = [part for part in parts if part.startswith(".")]
    if not hidden or path in FILES:
        return True
    if parts[:2] == (".github", "workflows") and len(parts) == 3:
        return len(hidden) == 1 and parts[-1].endswith((".yml", ".yaml"))
    return (parts[:2] == ("crates", "vendor") and len(parts) == 4 and len(hidden) == 1
            and parts[-1] in (".cargo_vcs_info.json", ".gitignore"))


def main():
    cmd = ["git", "log", "--full-history", "-m", "--format=", "--no-renames", "--name-only", "-z", "HEAD"]
    historical = subprocess.check_output(cmd).split(b"\0")
    tracked = subprocess.check_output(["git", "ls-files", "-z"]).split(b"\0")
    rejected = sorted({os.fsdecode(name) for name in historical + tracked if name and not allowed(os.fsdecode(name))})
    if rejected:
        sys.exit("Review private or unexpected tracked paths:\n" + "\n".join(rejected))
    print("Tracked paths and reachable player history passed the deployment metadata check.")


if __name__ == "__main__":
    main()
