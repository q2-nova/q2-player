#!/usr/bin/env python3
"""Generate the picker offsets from TZif files compiled from the pinned tzdata."""
import argparse
from pathlib import Path
import re
import subprocess

TABLE = Path(__file__).with_name("zone.tab")
REFERENCE_YEAR = 2027


def standard_offset(zonefile):
    report = subprocess.run(
        ["zdump", "-i", "-c", f"{REFERENCE_YEAR},{REFERENCE_YEAR + 1}", str(zonefile)],
        check=True, capture_output=True, text=True, env={"LC_ALL": "C"},
    ).stdout
    standard = set()
    for line in report.splitlines():
        columns = line.split("\t")
        if len(columns) < 3 or (len(columns) >= 5 and columns[4] == "1"):
            continue
        match = re.fullmatch(r"([+-])(\d\d)(\d\d)?", columns[2])
        if not match:
            raise ValueError(f"Non-minute UTC offset for {zonefile}: {columns[2]}")
        sign, hour, minute = match.groups()
        standard.add((-1 if sign == "-" else 1) * (int(hour) * 60 + int(minute or 0)))
    if len(standard) != 1:
        raise ValueError(f"Expected one standard UTC offset for {zonefile}: {standard}")
    return standard.pop()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("zoneinfo", type=Path, help="directory of pinned compiled TZif data")
    args = parser.parse_args()
    zoneinfo = args.zoneinfo.resolve(strict=True)
    print(f"# IANA tzdata 2026e standard offsets in {REFERENCE_YEAR}, in minutes east of UTC")
    for line in TABLE.read_text().splitlines():
        if line and not line.startswith("#"):
            zone = line.split("\t")[2]
            zonefile = zoneinfo / zone
            with zonefile.open("rb") as binary:
                if binary.read(4) != b"TZif":
                    raise ValueError(f"Invalid TZif file: {zonefile}")
            print(f"{zone}\t{standard_offset(zonefile)}")
