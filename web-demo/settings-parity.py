#!/usr/bin/env python3
"""Compare shared settings pixels from native --headless and a built player.wasm."""

import argparse
from hashlib import file_digest
import json
import os
from pathlib import Path
import subprocess
import tomllib

from PIL import Image, ImageChops


def shot(name, screen, selection=None, *, regions=None):
    return {
        "shot": name,
        "screen": screen,
        "selection": selection,
        "regions": regions or [[0, 30, 375, 320]],
    }


# The native empty card includes Set Up Server, while the demo includes
# generated Books and Podcasts. The shared settings order is the same.
STEPS = [
    {"scroll": 4, "wasm_scroll": 5}, "center",
    shot("settings", "Settings", "Sound", regions=[[0, 30, 375, 138]]),
    {"scroll": 2}, "center",
    shot("display", "Display", "Brightness"),
    "center", shot("brightness-default", "Brightness"),
    {"scroll": 10}, shot("brightness-adjusted", "Brightness"),
    "back", shot("display-back", "Display", "Brightness"),
    {"scroll": 1}, "center",
    shot("backlight-picker", "Backlight Timeout", "30 Seconds"),
    {"scroll": 1}, shot("backlight-picker-next", "Backlight Timeout", "1 Minute"),
    "center", shot("display-applied", "Display", "Backlight Timeout"),
    "back", shot("settings-back", "Settings", "Display", regions=[[0, 30, 375, 138]]),
    {"scroll": -2}, "center",
    shot("sound", "Sound", "Equalizer"),
    {"scroll": 2}, "center",
    shot("volume-limit-default", "Volume Limit"),
    {"scroll": -25}, shot("volume-limit-adjusted", "Volume Limit"),
    "center", shot("sound-applied", "Sound", "Volume Limit"),
    "back", {"scroll": 6}, "center",
    {"scroll": 2}, "center", shot("date-time", "Date & Time", "Time Zone"),
    "center", shot("time-zone", "Time Zone", "GMT+0 UTC"),
    {"scroll": 1}, "center", shot("time-zone-confirm", "Change Time Zone?", "Cancel"),
    "back", shot("time-zone-cancelled", "Time Zone", "GMT+1 Algiers"),
    "back", shot("date-time-back", "Date & Time", "Time Zone"),
    "back", {"scroll": 1}, "center",
    # OS version and hardware actions vary deliberately on the About page.
    shot("about", "About", regions=[[0, 30, 375, 78]]),
    "back", "back", {"scroll": -1}, "center",
    shot("storage", "Storage", "Storage Usage"),
    {"scroll": 4}, "center", shot("clear-cache-confirm", "Clear Song Cache?", "Cancel"),
    {"scroll": 1}, shot("clear-cache-selected", "Clear Song Cache?", "Clear Cache"),
    "back", shot("storage-back", "Storage", "Clear Song Cache…"),
    "back", {"scroll": -4}, "center", {"scroll": 4}, "center",
    shot("default-speed", "Default Speed"),
    {"scroll": 1}, shot("default-speed-adjusted", "Default Speed"),
    "back",
    # Pause on Unplug reflects the available headphone switch in each target.
    shot("playback-speed-applied", "Playback", "Default Spoken Speed", regions=[[0, 174, 375, 320]]),
]


def sha256_file(path):
    with path.open("rb") as source:
        return file_digest(source, "sha256").hexdigest()


def render_native(binary, out, env):
    card = out / "card"
    card.mkdir()
    # Match demo settings independent of native defaults. Leave account
    # sections empty so the headless player has no network server to contact.
    (card / "config.toml").write_text(
        '[display]\nbrightness = 50\nbacklight_seconds = 30\nlook = "classic"\n'
        'accent = "blue"\nclock_in_title = false\nclock_24h = false\ntimezone = "Etc/UTC"\n'
        '[playback]\nmax_volume = 100\n'
    )
    script = out / "native-script.txt"
    lines = ["back", "await 5000 screen:Q2", "scroll -2147483648"]
    for step in STEPS:
        if isinstance(step, str):
            lines.append(step)
        elif "scroll" in step:
            lines.append(f"scroll {step['scroll']}")
        else:
            lines.extend((f"await 5000 screen:{step['screen']}", "present 5000",
                          f"shot {out / 'native' / step['shot']}.png"))
    script.write_text("\n".join(lines) + "\n")
    with (out / "native.log").open("x") as log:
        subprocess.run([binary, "--headless", "--data", card, "--script", script],
                       env=env, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=60)


def differences(native_path, wasm_path, regions, diff_path):
    with Image.open(native_path) as png:
        native = png.convert("RGB")
    wasm = Image.frombytes("RGBA", (375, 320), wasm_path.read_bytes()).convert("RGB")
    if native.size != wasm.size:
        raise ValueError(f"Expected {wasm.size} native frame, got {native.size}")
    diff = ImageChops.difference(native, wasm)
    red, green, blue = diff.split()
    changed = ImageChops.lighter(ImageChops.lighter(red, green), blue)

    def compare(rect):
        area = changed.crop(rect)
        bounds = area.getbbox()
        return {
            "rectangle": rect,
            "changed_pixels": area.width * area.height - area.histogram()[0],
            "difference_bounds": [bounds[i] + rect[i % 2] for i in range(4)] if bounds else None,
        }

    diff.save(diff_path)
    wasm.save(wasm_path.with_suffix(".png"))
    return {
        "whole_frame": compare([0, 0, 375, 320]),
        "whole_content": compare([0, 30, 375, 320]),
        "regions": [compare(rect) for rect in regions],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native", required=True, type=Path, help="host q2nd executable")
    parser.add_argument("--wasm", required=True, type=Path, help="compiled player.wasm")
    parser.add_argument("--out", required=True, type=Path, help="new directory for the native and WASM captures")
    parser.add_argument("--report-only", action="store_true",
                        help="record pixel and selection mismatches without failing")
    args = parser.parse_args()
    binary, wasm, out = args.native.resolve(), args.wasm.resolve(), args.out.resolve()
    out.mkdir()
    for name in ("native", "wasm", "diff", "home"):
        (out / name).mkdir()
    env = {
        "HOME": str(out / "home"), "PATH": os.environ.get("PATH", os.defpath),
        "TZ": "UTC", "RUST_LOG": "info", "Q2ND_INSTALLED_STORAGE": "0", "Q2ND_ALSA_DEVICE": "null",
    }
    plan = out / "plan.json"
    plan.write_text(json.dumps(STEPS, ensure_ascii=False, indent=2) + "\n")
    runner = Path(__file__).resolve().with_suffix(".cjs")
    inputs = {str(path): sha256_file(path) for path in (binary, wasm, plan, runner, Path(__file__).resolve())}
    render_native(binary, out, env)
    with (out / "card" / "config.toml").open("rb") as saved:
        state = tomllib.load(saved)
    native_values = {
        "brightness": state["display"]["brightness"],
        "backlight_seconds": state["display"]["backlight_seconds"],
        "max_volume": state["playback"]["max_volume"],
        "timezone": state["display"]["timezone"],
        "speed_percent": state["spoken"]["speed_percent"],
    }
    expected_values = {"brightness": 60, "backlight_seconds": 60, "max_volume": 75, "timezone": "Etc/UTC",
                       "speed_percent": 105}
    if native_values != expected_values:
        raise SystemExit(f"Unexpected native settings after input: {native_values!r}")
    with (out / "wasm.log").open("x") as log:
        subprocess.run(["node", runner, wasm, plan, out / "wasm"], env=env,
                       stdout=log, stderr=subprocess.STDOUT, check=True, timeout=20)
    for path, digest in inputs.items():
        if sha256_file(Path(path)) != digest:
            raise SystemExit(f"Input changed during capture: {path}")
    report = {
        "inputs": inputs,
        "native_settings": native_values,
        "screens": {},
    }
    wasm_states = json.loads((out / "wasm" / "labels.json").read_text())
    failed = []
    for step in STEPS:
        if not isinstance(step, dict) or "shot" not in step:
            continue
        name = step["shot"]
        data = differences(out / "native" / f"{name}.png", out / "wasm" / f"{name}.rgba",
                           step["regions"], out / "diff" / f"{name}.png")
        data["selection"] = {"expected": step["selection"], "wasm": wasm_states[name]}
        report["screens"][name] = data
        changed = sum(region["changed_pixels"] for region in data["regions"])
        expected, actual = step["selection"], wasm_states[name]
        wrong_selection = expected is not None and expected != actual
        selection_note = f", expected selection {expected!r} and saw {actual!r}" if wrong_selection else ""
        print(f"{name}: {changed} changed pixels in compared rectangles{selection_note}")
        if changed or wrong_selection:
            failed.append(name)
    report["parity_differences"] = failed
    (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    if failed and not args.report_only:
        raise SystemExit("Parity differences: " + ", ".join(failed))


if __name__ == "__main__":
    main()
