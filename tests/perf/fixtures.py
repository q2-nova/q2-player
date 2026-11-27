#!/usr/bin/env python3
"""Generate the deterministic audio and artwork fixtures for benchmarks and PGO training.

  python3 tests/perf/fixtures.py [--out build/perf/fixtures]

FFmpeg's noise and test-pattern sources generate every file; no recorded
media is used. The output's manifest.json lists each file's SHA-256 and the
FFmpeg version, so a profile receipt can name its exact inputs. Existing
files are kept; delete the directory to regenerate.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
DEFAULT = ROOT / 'build/perf/fixtures'
# Leave out encoder version tags and creation times.
BITEXACT = ['-map_metadata', '-1', '-fflags', '+bitexact', '-flags', '+bitexact']
NOISE = 'anoisesrc=color=pink:amplitude={amplitude}:sample_rate={rate}:duration=12:seed={seed}'

# The music encodings: 12 s of stereo pink noise.
MUSIC = [
    ('mp3.mp3', ['-c:a', 'libmp3lame', '-b:a', '192k']),
    ('no-xing.mp3', ['-c:a', 'libmp3lame', '-b:a', '192k', '-write_xing', '0']),
    ('flac.flac', ['-c:a', 'flac']),
    ('vorbis.ogg', ['-c:a', 'libvorbis', '-q:a', '5']),
    ('faststart.m4a', ['-c:a', 'aac', '-b:a', '192k', '-movflags', '+faststart']),
    ('tail-moov.m4a', ['-c:a', 'aac', '-b:a', '192k']),
    ('mp3-320.mp3', ['-c:a', 'libmp3lame', '-b:a', '320k']),
    ('podcast-mp3-128k-stereo.mp3', ['-c:a', 'libmp3lame', '-b:a', '128k']),
    ('aac-256.m4a', ['-c:a', 'aac', '-b:a', '256k', '-movflags', '+faststart']),
    ('alac.m4a', ['-c:a', 'alac']),
    ('wav.wav', ['-c:a', 'pcm_s16le', '-f', 'wav']),
]
# Audiobook and podcast encodings: band-limited noise with a 4 Hz envelope
# stands in for speech.
SPOKEN = [
    ('book-mp3-32k-mono-22k.mp3', ['-ar', '22050', '-c:a', 'libmp3lame', '-b:a', '32k']),
    ('book-mp3-64k-mono-22k.mp3', ['-ar', '22050', '-c:a', 'libmp3lame', '-b:a', '64k']),
    ('book-mp3-128k-mono-44k.mp3', ['-c:a', 'libmp3lame', '-b:a', '128k']),
    ('book-aac-32k-mono-22k.m4b', ['-ar', '22050', '-c:a', 'aac', '-b:a', '32k', '-movflags', '+faststart']),
    ('book-aac-64k-mono-44k.m4b', ['-c:a', 'aac', '-b:a', '64k', '-movflags', '+faststart']),
    ('book-aac-128k-stereo-44k.m4b', ['-ac', '2', '-c:a', 'aac', '-b:a', '128k', '-movflags', '+faststart']),
]
# Lossless encodings of two different channels (mid +/- 0.4 side), so FLAC and
# ALAC code a real side channel; FLAC also at compression level 8.
LOSSLESS = [
    ('flac-stereo.flac', 'stereo', ['-c:a', 'flac']),
    ('flac-stereo-l8.flac', 'stereo', ['-c:a', 'flac', '-compression_level', '8']),
    ('flac-24-96-stereo.flac', 'hires-stereo', ['-c:a', 'flac', '-sample_fmt', 's32']),
    ('flac-24-96-stereo-l8.flac', 'hires-stereo', ['-c:a', 'flac', '-sample_fmt', 's32', '-compression_level', '8']),
    ('alac-stereo.m4a', 'stereo', ['-c:a', 'alac']),
    ('alac-24-96-stereo.m4a', 'hires-stereo', ['-c:a', 'alac', '-sample_fmt', 's32p']),
    ('flac-24-96.flac', 'hires', ['-c:a', 'flac', '-sample_fmt', 's32']),
]
IMAGES = [('cover.jpg', 1200), ('cover.png', 1200), ('cover.webp', 1200),
          ('thumbnail.jpg', 50), ('thumbnail.png', 50), ('now-playing.jpg', 150)]


def ffmpeg(args, out):
    if not out.exists():
        part = out.with_name('partial-' + out.name)
        subprocess.run(['ffmpeg', '-v', 'error', '-y', *args, *BITEXACT, str(part)], check=True)
        part.rename(out)


def noise(amplitude, rate, seed):
    return ['-f', 'lavfi', '-i', NOISE.format(amplitude=amplitude, rate=rate, seed=seed)]


def generate(out):
    out.mkdir(parents=True, exist_ok=True)
    sources = {
        'source.wav': noise(0.12, 44100, 42) + ['-ac', '2', '-c:a', 'pcm_s16le'],
        'book-source.wav': noise(0.3, 44100, 7) + ['-af', 'lowpass=f=3800,highpass=f=90,tremolo=f=4:d=0.8',
                                                    '-ac', '1', '-c:a', 'pcm_s16le'],
        'hires-source.wav': noise(0.12, 96000, 43) + ['-ac', '2', '-c:a', 'pcm_s24le'],
    }
    mix = ['-filter_complex', '[0][1]amerge=inputs=2,pan=stereo|c0=c0+0.4*c1|c1=c0-0.4*c1']
    sources['stereo-source.wav'] = noise(0.12, 44100, 52) + noise(0.12, 44100, 152) + mix + ['-c:a', 'pcm_s16le']
    sources['hires-stereo-source.wav'] = noise(0.12, 96000, 53) + noise(0.12, 96000, 153) + mix + ['-c:a', 'pcm_s24le']
    for name, args in sources.items():
        ffmpeg(args, out / name)
    for name, opts in MUSIC:
        ffmpeg(['-i', str(out / 'source.wav'), *opts], out / name)
    for name, opts in SPOKEN:
        ffmpeg(['-i', str(out / 'book-source.wav'), *opts], out / name)
    for name, source, opts in LOSSLESS:
        ffmpeg(['-i', str(out / (source + '-source.wav')), *opts], out / name)
    for name, size in IMAGES:
        ffmpeg(['-f', 'lavfi', '-i', f'testsrc2=size={size}x{size}:rate=1', '-frames:v', '1'], out / name)
    files = {p.name: {'sha256': hashlib.sha256(p.read_bytes()).hexdigest(), 'bytes': p.stat().st_size}
             for p in sorted(out.iterdir()) if p.is_file() and p.name != 'manifest.json'}
    manifest = {'ffmpeg': subprocess.check_output(['ffmpeg', '-version'], text=True).splitlines()[0], 'files': files}
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--out', type=Path, default=DEFAULT)
    generate(parser.parse_args().out.resolve())
    print(parser.parse_args().out.resolve())


if __name__ == '__main__':
    main()
