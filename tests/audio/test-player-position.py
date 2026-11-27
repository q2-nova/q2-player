#!/usr/bin/env python3
"""Run actual player position/pause routines against a deterministic PCM model."""
import argparse
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]


def block(source, start):
    end = source.index('{', start) + 1
    depth = 1
    while depth:
        if source[end] == '{':
            depth += 1
        elif source[end] == '}':
            depth -= 1
        end += 1
    return source[start:end]


def function(source, name):
    for declaration in ['    fn ', '    pub fn ', 'pub fn ']:
        marker = declaration + name + '('
        if marker in source:
            return block(source, source.index(marker))
    raise ValueError(name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=ROOT / 'crates/q2nd/src/audio')
    parser.add_argument('--baseline', type=Path, required=True,
                        help='Directory containing unchanged baseline player.rs and alsa.rs')
    args = parser.parse_args()
    template = (ROOT / 'tests/audio/player-position-test.rs').read_text()
    out = ROOT / 'build/perf/player-position'
    out.mkdir(parents=True, exist_ok=True)
    for label, directory in [('baseline', args.baseline), ('fixed', args.source)]:
        player = (directory / 'player.rs').read_text()
        alsa = (directory / 'alsa.rs').read_text()
        engine = '\n'.join(function(player, name) for name in
                           ['pause', 'release_output', 'position_ms'])
        sink = '\n'.join(function(alsa, name) for name in ['pause', 'flush', 'delay'])
        resume = block(player, player.index('        if self.resume_at > 0 {'))
        source = template.replace('/* ACTUAL ENGINE */', engine)
        source = source.replace('/* ACTUAL SINK */', sink)
        source = source.replace('/* ACTUAL RESUME BRANCH */', resume)
        source = source.replace('/* ACTUAL SEEK LIMIT */', function(player, 'seek_position'))
        source = source.replace('/* FIXED */', str(label == 'fixed').lower())
        path = out / (label + '.rs')
        path.write_text(source)
        binary = out / label
        subprocess.run(['rustc', '--edition=2021', '-Dwarnings', str(path),
                        '-o', str(binary)], check=True)
        subprocess.run([str(binary)], check=True)


if __name__ == '__main__':
    main()
