#!/usr/bin/env python3
"""Locked code of a q2nd build, as platform/resident.rs computes it (static).

  tools/locked_size.py --binary B/q2nd --readelf SDK/bin/...-readelf [--json OUT]

The player mlocks every file-backed r-x mapping. For its own executable that is
the R-E LOAD segment, page-rounded, minus the sections resident.rs skips
(`.q2tools`; `.eh_frame_hdr` and `.eh_frame` in a `q2_unlock_unwind_tables`
build), merged where adjacent and shrunk to whole 4 KiB pages. Shared
libraries are not counted. Static only: no process was measured.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess

PAGE = 4096
TOOLS = ['.q2tools']
UNWIND = ['.eh_frame_hdr', '.eh_frame']


def sections(binary, readelf):
    raw = subprocess.check_output([str(readelf), '--wide', '--sections', str(binary)], text=True)
    out = {}
    for m in re.finditer(r'^\s*\[\s*\d+\]\s+(\S+)\s+(\S+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+([0-9a-f]+)', raw, re.M):
        name, kind, addr, off, size = m.groups()
        out[name] = dict(type=kind, address=int(addr, 16), offset=int(off, 16), bytes=int(size, 16))
    return out


def code_segment(binary, readelf):
    raw = subprocess.check_output([str(readelf), '--wide', '--program-headers', str(binary)], text=True)
    loads = [m.groups() for m in re.finditer(
        r'^\s*LOAD\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+0x[0-9a-f]+\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+(.*?)\s+0x[0-9a-f]+$', raw, re.M)]
    code = [l for l in loads if 'E' in l[4]]
    if len(code) != 1:
        raise SystemExit(f'expected one executable LOAD segment, found {len(code)}')
    offset, vaddr, filesz, memsz = (int(v, 16) for v in code[0][:4])
    return dict(offset=offset, vaddr=vaddr, filesz=filesz, memsz=memsz)


def skipped(segment, found, names):
    """resident.rs Mapping::skipped on the segment's mapping."""
    start = segment['vaddr'] // PAGE * PAGE
    end = -(-(segment['vaddr'] + segment['memsz']) // PAGE) * PAGE
    file_start = segment['offset'] - (segment['vaddr'] - start)
    file_end = file_start + (end - start)
    ranges = []
    for name in names:
        s = found.get(name)
        if not s or s['type'] == 'NOBITS' or not s['bytes']:
            continue
        a, b = max(s['offset'], file_start), min(s['offset'] + s['bytes'], file_end)
        if a < b:
            ranges.append((start + a - file_start, start + b - file_start))
    merged = []
    for a, b in sorted(ranges):
        if merged and a <= merged[-1][1] + 16:
            merged[-1][1] = max(merged[-1][1], b)
        else:
            merged.append([a, b])
    pages = [(-(-a // PAGE) * PAGE, b // PAGE * PAGE) for a, b in merged]
    return end - start, sum(b - a for a, b in pages if a < b)


def measure(binary, readelf):
    found = sections(binary, readelf)
    segment = code_segment(binary, readelf)
    mapping, tools = skipped(segment, found, TOOLS)
    _, both = skipped(segment, found, TOOLS + UNWIND)
    size = lambda *names: sum(found[n]['bytes'] for n in names if n in found)
    return {
        'binary': str(binary),
        'text': size('.text'), 'q2tools': size('.q2tools'), 'rodata': size('.rodata'),
        'eh_frame': size('.eh_frame'), 'eh_frame_hdr': size('.eh_frame_hdr'),
        're_load_memsz': segment['memsz'], 're_mapping': mapping,
        'locked': mapping - tools, 'locked_unwind_unlocked': mapping - both,
        'unlocked_tools': tools, 'unlocked_tools_and_unwind': both,
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--binary', type=Path, required=True)
    ap.add_argument('--readelf', type=Path, required=True)
    ap.add_argument('--json', type=Path)
    a = ap.parse_args()
    result = measure(a.binary.resolve(), a.readelf)
    text = json.dumps(result, indent=1) + '\n'
    if a.json:
        with a.json.open('x') as out:
            out.write(text)
    print(text, end='')


if __name__ == '__main__':
    main()
