#!/usr/bin/env python3
"""Generate a synthetic UI replay. Caller owns the emulator/device lease.

Does not access a device, installed service, owner config or owner library.
Use --headless for MIPS emulation or --mainline for the coordinated DRM gate.
Set Q2ND_RUNTIME to the generated runtime/ subdirectory. Hash-check the candidate
against receipt.json before launch; service hold/restore belongs to the caller.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re


def generate(out, runtime, step, app_sha256):
    if step not in range(1, 5) or not re.fullmatch(r"[0-9a-f]{64}", app_sha256):
        raise ValueError("exact step 1–4 and SHA-256 required")
    runtime = Path(runtime)
    if not runtime.is_absolute() or not runtime.name.startswith("q2-custom-studio-"):
        raise ValueError("runtime must be an absolute owned q2-custom-studio-* directory")
    out.mkdir(parents=True, exist_ok=False)
    config = """[display]
backlight_seconds=0
clock_in_title=false
clicker=false
[power]
sleep_minutes=0
[wifi]
enabled=false
[tailscale]
enabled=false
"""
    # Actual human path, then actual wheel text entry. One await at each
    # asynchronous boundary; this does not inject profile/config selection.
    # Empty generated queue: Shuffle is last and Settings immediately before
    # it, independently of whether this ELF directory offers AirPlay.
    inputs = ["back", "await 15000 screen:Q2", "scroll 2147483647", "scroll -1", "center",
              "await 15000 screen:Settings", "center", "await 15000 screen:Sound", "center",
              "await 15000 screen:Equalizer", "scroll 5", "center", "await 15000 screen:Headphone EQ",
              "center", "await 15000 screen:Find Headphones"]
    cells = "___abcdefghijklmnopqrstuvwxyz0123456789"
    selected = 3  # TextEntry starts at a.
    for char in "custom studio":
        target = 2 if char == " " else cells.index(char)
        inputs += [f"scroll {target-selected}", "center"]
        selected = target
    inputs += ["hold center", "await 15000 screen:Headphone EQ", f"scroll {step}", "center",
               "await 15000 screen:Headphone EQ", "back", "await 15000 screen:Equalizer",
               "center", "await 15000 screen:Headphone EQ", "back", "await 15000 screen:Equalizer"]
    script = "\n".join(inputs) + "\n"
    (out / "config.toml").write_text(config)
    (out / "inputs.txt").write_text(script)
    receipt = {"schema": 1, "synthetic": True, "app_sha256": app_sha256,
               "runtime": str(runtime), "q2nd_runtime": str(runtime / "runtime"),
               "expected_profile": f"Beyerdynamic Custom Studio (step {step})",
               "expected_headphone_enabled": True, "expected_graphic_enabled": False,
               "config_sha256": hashlib.sha256(config.encode()).hexdigest(),
               "inputs_sha256": hashlib.sha256(script.encode()).hexdigest()}
    (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runtime", required=True)
    parser.add_argument("--step", type=int, choices=range(1, 5), required=True)
    parser.add_argument("--app-sha256", required=True)
    args = parser.parse_args()
    generate(args.output, args.runtime, args.step, args.app_sha256)
