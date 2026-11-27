#!/usr/bin/env python3
"""Profile-guided optimization for release builds of q2nd.

  tools/pgo.py profile --out DIR BUILD-INPUTS --qemu-user QEMU
  tools/pgo.py coverage DIR
  tools/pgo.py compare --plain BUILD --pgo BUILD --sdk SDK --qemu-user QEMU --out DIR

`profile` builds an instrumented q2nd with tools/build-target.py and the
release build's features, trains it under qemu-mipsel on generated input,
merges the counts and writes DIR/q2nd.profdata and DIR/receipt.json. The
release build then passes `--profile-use DIR/q2nd.profdata` to
build-target.py, which refuses a receipt with another feature set or
compiler. BUILD-INPUTS are build-target.py's `--sdk`, `--toolchain-repo`,
`--compiler-work`, `--os-repo` and `--component-lock`.

The q2-toolchain driver (tools/q2_pgo.py) supplies the profile runtime,
the instrumentation flags, the qemu runs, the merge, the coverage report and
the receipt. This file owns the training set: training version 2, the
monorepo's set on the fixtures of tests/perf/fixtures.py.

`compare` runs the same output cases on two builds under qemu-mipsel and
requires equal decoded samples, pixels and UI checksums.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'tests/perf/fixtures.py'
# Bump when the training set changes, so receipts say which set made a profile.
TRAINING_VERSION = 2
COVER_ITERATIONS = 2  # training decodes of each full-size cover per size
SYSROOT = 'mipsel-buildroot-linux-gnu/sysroot'
BUILD_INPUTS = ('sdk', 'toolchain-repo', 'compiler-work', 'os-repo')

# Named hot paths: a regular expression over the `::`-joined defining path
# of each profiled function (q2_pgo.symbol_path). q2nd plays at the file's
# own rate, so its sample path is PCM conversion and EQ rather than a resampler.
HOT_PATHS = {
    'decoders': r'^symphonia_(bundle_mp3|bundle_flac|codec_vorbis|codec_aac|codec_alac)::',
    'pcm conversion and eq': r'^q2nd::audio::(pcm_convert|eq|gain|decoder)::',
    'network read path': r'^subsonic::http::(Body|Response|Reader|Stream)::',
    'ui draw': r'^q2nd::(gfx|ui)::',
    'cover decode': r'^(zune_jpeg|png|image_webp|fdeflate)::',
}
# Hot paths that training version 2 does not reach, with the reason.
KNOWN_GAPS = {
    'network read path': 'q2-player has no generated Navidrome, Audiobookshelf, radio, sync, scrobble '
                         'or portal fixtures for qemu-user training',
}
MODULE_CRATES = ('q2nd', 'subsonic')


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def driver(toolchain):
    path = Path(toolchain) / 'tools/q2_pgo.py'
    if not path.is_file():
        raise SystemExit(f'{path}: the q2-toolchain checkout has no PGO driver')
    return load(path, 'q2_pgo')


def fixtures(directory):
    directory = Path(directory)
    if not (directory / 'manifest.json').is_file():
        subprocess.run([sys.executable, str(FIXTURES), '--out', str(directory)], check=True)
    return directory


def training_cases(fx, cover_iterations=COVER_ITERATIONS):
    """Generated workloads for decoding, artwork and drawing (training version 2)."""
    cases = []
    for name in ['mp3.mp3', 'no-xing.mp3', 'flac.flac', 'vorbis.ogg', 'faststart.m4a', 'tail-moov.m4a']:
        cases.append((f'decode/{name}', {'kind': 'decode', 'fixture': str(fx / name)}))
    cases.append(('eq', {'kind': 'eq', 'iterations': 40}))
    for name in ['cover.jpg', 'cover.png', 'cover.webp', 'now-playing.jpg', 'thumbnail.jpg', 'thumbnail.png']:
        for size in [50, 150, 320]:
            iterations = cover_iterations if name.startswith('cover.') else 2
            cases.append((f'image/{name}@{size}', {'kind': 'image', 'fixture': str(fx / name),
                                                   'iterations': iterations, 'size': size}))
    # Lists, media lists, Cover Flow and night tint frames; then pie charts.
    cases.append(('ui', {'kind': 'ui', 'iterations': 240}))
    for scene in ['scanline', 'legacy']:
        cases.append((f'storage-render/{scene}', {'kind': 'storage-render', 'iterations': 10, 'scene': scene}))
    for scene in ['cold', 'warm', 'changed']:
        cases.append((f'config-startup/{scene}', {'kind': 'config-startup', 'iterations': 3, 'scene': scene}))
    return cases


# Headless tours through the real screens on a seeded card: the generated
# songs downloaded as two albums and queued, and a server that refuses
# connections (the offline paths). Each script line runs about 150 ms apart.
CONFIG = """[server]
url = "http://127.0.0.1:9"
user = "q2-fixture"
password = "synthetic-test-only"
[display]
timezone = "UTC"
[power]
sleep_minutes = 0
[playback]
scrobble = false
sync_queue = false
"""
SONG_FILES = ['mp3.mp3', 'flac.flac', 'vorbis.ogg', 'faststart.m4a', 'tail-moov.m4a', 'no-xing.mp3']


def browse(home_index, depth_items=7):
    """Open every entry of one Home submenu, scroll it, open its first row."""
    lines = ['scroll -20', f'scroll {home_index}', 'center', 'wait 500']
    for j in range(depth_items):
        lines += ['scroll -20', f'scroll {j}', 'center', 'wait 500', 'wheel-trace 30 16',
                  'scroll -20', 'center', 'wait 400', 'hold center', 'wait 300', 'back', 'back', 'back']
    return lines + ['back']


TOURS = {
    'browse': ['wait 800', 'play', 'wait 2000'] + browse(0) + browse(2) + browse(3, 9) + ['play', 'shot {dir}/browse.png'],
    'playing': ['wait 800', 'scroll -20', 'scroll 5', 'center', 'wait 3000', 'play', 'wait 4000', 'next', 'wait 2000',
                'prev', 'wait 2000', 'hold center', 'wait 300', 'back', 'scroll 3', 'scroll -6', 'back', 'back',
                'scroll -20', 'scroll 4', 'center', 'wait 4000', 'back', 'play', 'shot {dir}/playing.png'],
    'scroll': ['wait 800', 'scroll -20', 'center', 'wait 300'] + ['wheel-trace 200 8', 'wheel-trace 120 40'] * 3 +
              ['back', 'shot {dir}/scroll.png'],
}


def seed(data, fx):
    """Insert the generated songs as downloads, two album sets and a queue."""
    albums = [('pgo-album-a', 'Generated Alpha', 'Generated Artist One'),
              ('pgo-album-b', 'Generated Beta', 'Generated Artist Two')]
    songs = []
    with sqlite3.connect(data / 'library.db') as db:
        for i, name in enumerate(SONG_FILES):
            album_id, album, artist = albums[i % 2]
            dest = data / 'offline' / name
            dest.parent.mkdir(exist_ok=True)
            shutil.copy2(fx / name, dest)
            song = {'id': f'pgo-song-{i}', 'title': f'Generated Track {i + 1}', 'album': album, 'artist': artist,
                    'albumId': album_id, 'artistId': f'pgo-artist-{i % 2}', 'track': i // 2 + 1, 'duration': 12,
                    'suffix': name.rsplit('.', 1)[1], 'size': dest.stat().st_size}
            songs.append(song)
            db.execute('INSERT INTO offline_songs VALUES (?,?,?,?)', (song['id'], str(dest), song['size'], json.dumps(song)))
        for album_id, album, artist in albums:
            ids = [s['id'] for s in songs if s['albumId'] == album_id]
            db.execute('INSERT INTO offline_sets VALUES (?,?,?,?,?,?,?)',
                       ('album', album_id, album, artist, None, json.dumps(ids), 1))
        db.execute("INSERT OR REPLACE INTO kv VALUES ('queue',?)",
                   (json.dumps({'songs': songs, 'index': 0, 'position_ms': 0}),))


def resources(directory):
    directory.mkdir()
    (directory / 'fonts').symlink_to(ROOT / 'assets/fonts')
    (directory / 'autoeq').symlink_to(ROOT / 'crates/q2nd/assets/autoeq')
    return directory


def bench(run, name, binary, spec, work):
    """One `q2nd --bench` case; returns its JSON result."""
    work.mkdir(parents=True)
    (work / 'spec.json').write_text(json.dumps(dict(spec, scratch=str(work / 'scratch'))))
    process = run(name, [binary, '--bench', work / 'spec.json'], capture_output=True, check=False)
    lines = process.stdout.decode(errors='replace').strip().splitlines() or ['{}']
    result = json.loads(lines[-1]) if lines[-1].startswith('{') else {'error': 'no output'}
    shutil.rmtree(work)
    if process.returncode or result.get('error'):
        error = result.get('error') or process.stderr.decode(errors='replace')[-500:]
        raise SystemExit(f'{name} failed: {error}')
    return result


def train(training, binary, out, fx, cover_iterations):
    env = {'Q2ND_RESOURCE_ROOT': str(resources(out / 'resources')), 'Q2ND_ALSA_DEVICE': 'q2nd-fake'}
    training.env.update(env)
    for name, spec in training_cases(fx, cover_iterations):
        bench(training.user, name, binary, spec, out / 'work')

    def headless(name, data, script):
        with (script.parent / (script.stem + '.log')).open('wb') as log:
            training.user(name, [binary, '--data', data, '--headless', '--script', script],
                          stdout=log, stderr=subprocess.STDOUT)

    for name, lines in TOURS.items():
        directory = out / 'tours' / name
        data = directory / 'data'
        data.mkdir(parents=True)
        (data / 'config.toml').write_text(CONFIG)
        # The player creates its own database; then the card is seeded.
        (directory / 'init.txt').write_text('wait 500\n')
        headless('tour-init/' + name, data, directory / 'init.txt')
        seed(data, fx)
        (directory / 'tour.txt').write_text('\n'.join(lines).format(dir=directory) + '\n')
        headless('tour/' + name, data, directory / 'tour.txt')
        shutil.rmtree(data / 'offline')


def build_args(args):
    command = []
    for name in BUILD_INPUTS:
        command += ['--' + name, str(getattr(args, name.replace('-', '_')).resolve())]
    if args.component_lock:
        command += ['--component-lock', str(args.component_lock.resolve())]
    return command


def profile(args):
    started = time.monotonic()
    out = args.out.resolve()
    if out.exists():
        raise SystemExit(f'{out} exists')
    out.mkdir(parents=True)
    pgo = driver(args.toolchain_repo)
    fx = fixtures(args.fixtures.resolve())
    profiles = out / 'profiles'
    build_start = time.monotonic()
    subprocess.run([sys.executable, str(ROOT / 'tools/build-target.py'), *build_args(args),
                    '--profile-generate', str(profiles), '--out', str(out / 'instrumented')], check=True)
    build_minutes = round((time.monotonic() - build_start) / 60, 1)
    build = json.loads((out / 'instrumented/result.json').read_text())
    if build['profile'] != 'pgo-instrumented' or build['features'] != {'q2nd': [], 'q2-portal': []}:
        raise SystemExit('the instrumented build must have the release feature set')
    binary = out / 'instrumented/q2nd'
    compiler = args.compiler_work.resolve() / 'install'
    training = pgo.Training(profiles, args.qemu_user.resolve(), args.sdk.resolve() / SYSROOT)
    train(training, binary, out, fx, args.cover_iterations)
    profdata = compiler / pgo.PROFDATA
    merged, raw_count = pgo.merge(profdata, training, out / 'q2nd.profdata')
    report = pgo.coverage(profdata, merged, HOT_PATHS, MODULE_CRATES)
    pgo.write_json(out / 'coverage.json', report)
    manifest = json.loads((fx / 'manifest.json').read_text())
    receipt = pgo.make_receipt(
        program='q2nd', profile=merged, training_version=TRAINING_VERSION,
        compiler=pgo.compiler_identity(compiler), features=build['features'],
        coverage_summary=pgo.summary(report), training=training, started=started,
        source_commit=build['source_commit'], raw_profiles=raw_count, cover_iterations=args.cover_iterations,
        known_gaps={name: KNOWN_GAPS[name] for name in report['hot_paths_missing'] if name in KNOWN_GAPS},
        instrumented={'sha256': pgo.sha256(binary), 'build_result_sha256': pgo.sha256(out / 'instrumented/result.json'),
                      'build_minutes': build_minutes},
        fixtures={'ffmpeg': manifest['ffmpeg'], 'manifest_sha256': pgo.sha256(fx / 'manifest.json')},
        emulator={'qemu_user_sha256': pgo.sha256(args.qemu_user), 'cpu': training.cpu})
    pgo.write_json(out / 'receipt.json', receipt)
    shutil.rmtree(profiles)
    unexplained = [name for name in report['hot_paths_missing'] if name not in KNOWN_GAPS]
    if unexplained:
        raise SystemExit(f'hot paths without counts: {unexplained} (see {out / "coverage.json"})')
    print(json.dumps({k: receipt[k] for k in ('profile_sha256', 'minutes', 'raw_profiles')}, indent=1))
    return receipt


def coverage(args):
    directory = args.directory.resolve()
    receipt = json.loads((directory / 'receipt.json').read_text())
    pgo = driver(args.toolchain_repo)
    profdata = args.compiler_work.resolve() / 'install' / pgo.PROFDATA
    report = pgo.coverage(profdata, directory / receipt['profile'], HOT_PATHS, MODULE_CRATES)
    print(json.dumps(report, indent=1))


# Output equality

HASH_KEY = re.compile(r'(md5|checksum)')


def hashes(value, prefix=''):
    """Every md5 and checksum field of a bench result, by its JSON path."""
    found = {}
    if isinstance(value, dict):
        for key, item in value.items():
            path = f'{prefix}.{key}' if prefix else key
            if HASH_KEY.search(key) and not isinstance(item, dict):
                found[path] = item
            else:
                found.update(hashes(item, path))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            found.update(hashes(item, f'{prefix}[{index}]'))
    return found


def output_cases(fx):
    """Cases whose results carry decoded-sample, pixel or UI checksums."""
    cases = [(f'decode/{name}', {'kind': 'decode', 'fixture': str(fx / name)})
             for name in sorted(json.loads((fx / 'manifest.json').read_text())['files'])
             if name.rsplit('.', 1)[-1] in ('mp3', 'flac', 'ogg', 'm4a', 'm4b', 'wav') and 'source' not in name]
    cases.append(('eq', {'kind': 'eq', 'iterations': 4}))
    for name in ['cover.jpg', 'cover.png', 'cover.webp', 'now-playing.jpg', 'thumbnail.jpg', 'thumbnail.png']:
        for size in [50, 150, 320]:
            cases.append((f'image/{name}@{size}', {'kind': 'image', 'fixture': str(fx / name), 'iterations': 1,
                                                   'size': size}))
    cases.append(('ui', {'kind': 'ui', 'iterations': 4}))
    for scene in ['scanline', 'legacy']:
        cases.append((f'storage-render/{scene}', {'kind': 'storage-render', 'iterations': 1, 'scene': scene}))
    return cases


def compare(args):
    out = args.out.resolve()
    if out.exists():
        raise SystemExit(f'{out} exists')
    out.mkdir(parents=True)
    fx = fixtures(args.fixtures.resolve())
    prefix = [str(args.qemu_user.resolve()), '-cpu', args.cpu, '-L', str(args.sdk.resolve() / SYSROOT)]
    env = dict(os.environ, Q2ND_RESOURCE_ROOT=str(resources(out / 'resources')), Q2ND_ALSA_DEVICE='q2nd-fake')
    env.pop('LLVM_PROFILE_FILE', None)

    def run(name, argv, **kwargs):
        return subprocess.run([*prefix, *map(str, argv)], env=env, timeout=900, **kwargs)

    builds = {'plain': args.plain.resolve(), 'pgo': args.pgo.resolve()}
    records = {arm: json.loads((path / 'result.json').read_text()) for arm, path in builds.items()}
    if records['plain'].get('pgo') is not None or records['pgo'].get('pgo') is None:
        raise SystemExit('compare a build without a profile (--plain) with a profile-guided one (--pgo)')
    if records['plain']['source_commit'] != records['pgo']['source_commit']:
        raise SystemExit('the two builds come from different sources')
    results, differences, cases = {}, [], output_cases(fx)
    for case, spec in cases:
        for arm, path in builds.items():
            results.setdefault(case, {})[arm] = hashes(bench(run, case, path / 'q2nd', spec, out / 'work')['result'])
        if not results[case]['plain']:
            raise SystemExit(f'{case}: no checksums in the result')
        if results[case]['plain'] != results[case]['pgo']:
            differences.append(case)
    report = {'passed': not differences, 'differences': differences, 'cases': len(cases),
              'checksums': sum(len(r['plain']) for r in results.values()), 'source_commit': records['pgo']['source_commit'],
              'q2nd_sha256': {arm: records[arm]['artifacts']['q2nd']['sha256'] for arm in builds},
              'profile_sha256': records['pgo']['pgo']['profile_sha256'], 'cpu': args.cpu,
              'qemu_user_sha256': sha256(args.qemu_user), 'results': results}
    (out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({k: report[k] for k in ('passed', 'cases', 'checksums', 'differences')}))
    if differences:
        raise SystemExit(f'PGO output differs: {differences}')
    return report


def sha256(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='action', required=True)
    p = sub.add_parser('profile', help='instrumented build, training, merge and receipt')
    for name in BUILD_INPUTS:
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--component-lock', type=Path)
    p.add_argument('--qemu-user', type=Path, required=True)
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--fixtures', type=Path, default=ROOT / 'build/perf/fixtures')
    p.add_argument('--cover-iterations', type=int, default=COVER_ITERATIONS)
    c = sub.add_parser('coverage', help='the coverage report of a profile directory')
    c.add_argument('directory', type=Path)
    c.add_argument('--toolchain-repo', type=Path, required=True)
    c.add_argument('--compiler-work', type=Path, required=True)
    q = sub.add_parser('compare', help='output equality of a PGO and a plain build under qemu-mipsel')
    q.add_argument('--plain', type=Path, required=True, help='build-target.py output without a profile')
    q.add_argument('--pgo', type=Path, required=True, help='build-target.py output with --profile-use')
    q.add_argument('--sdk', type=Path, required=True)
    q.add_argument('--qemu-user', type=Path, required=True)
    q.add_argument('--cpu', default='P5600')
    q.add_argument('--fixtures', type=Path, default=ROOT / 'build/perf/fixtures')
    q.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    {'profile': profile, 'coverage': coverage, 'compare': compare}[args.action](args)


if __name__ == '__main__':
    main()
