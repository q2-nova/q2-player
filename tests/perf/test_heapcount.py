"""Controls for tests/perf/heapcount.c and the step attribution in document_memory.py.

Builds heapcount.so with the host C compiler and preloads it into a generated
C program whose allocations are known, then feeds the resulting log to
document_memory.steps().
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import document_memory  # noqa: E402

PROGRAM = r'''
#include <malloc.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static void mark(const char *name) {
    struct timespec ts;
    clock_gettime(CLOCK_BOOTTIME, &ts);
    printf("script mark %s boottime_us=%lld\n", name, (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000);
    fflush(stdout);
    (void)mallinfo2();
}
int main(void) {
    mark("start");
    char *big = malloc(4 << 20);          /* 4096 KiB live */
    big[0] = 1;
    mark("big");
    char *peak = malloc(2 << 20);         /* peak 6144 KiB, then freed */
    peak[0] = 1;
    free(peak);
    big = realloc(big, 1 << 20);          /* 1024 KiB live */
    mark("shrunk");
    free(big);
    void *aligned = NULL;
    if (posix_memalign(&aligned, 64, 512 << 10)) return 1;
    mark("aligned");
    free(aligned);
    mark("end");
    return 0;
}
'''


@unittest.skipUnless(shutil.which('cc'), 'needs a host C compiler (cc)')
class HeapCount(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp())
        cls.preload = cls.tmp / 'heapcount.so'
        subprocess.run(['cc', '-O2', '-Wall', '-Werror', '-shared', '-fPIC', '-o', str(cls.preload),
                        str(HERE / 'heapcount.c'), '-ldl'], check=True)
        (cls.tmp / 'program.c').write_text(PROGRAM)
        subprocess.run(['cc', '-O0', '-o', str(cls.tmp / 'program'), str(cls.tmp / 'program.c')], check=True)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp)

    def run_program(self, preload=True):
        log = self.tmp / f'heap-{preload}.log'
        log.unlink(missing_ok=True)
        env = {**os.environ, 'HEAPCOUNT_LOG': str(log)}
        if preload:
            env['LD_PRELOAD'] = str(self.preload)
        stdout = subprocess.run([str(self.tmp / 'program')], env=env, capture_output=True, text=True,
                                check=True).stdout
        return stdout, log.read_text() if log.exists() else ''

    def test_steps_report_live_heap_and_peak_since_previous_step(self):
        stdout, heap = self.run_program()
        self.assertEqual(len(document_memory.HEAP.findall(heap)), 5)
        steps, peak_all = document_memory.steps(stdout, heap)
        self.assertEqual(list(steps), ['start', 'big', 'shrunk', 'aligned', 'end'])
        base = steps['start']['live_kib']
        self.assertLess(base, 64)
        self.assertAlmostEqual(steps['big']['live_kib'] - base, 4096, delta=8)
        self.assertAlmostEqual(steps['shrunk']['live_kib'] - base, 1024, delta=8)
        # The 2 MiB block lived and was freed between the marks: only the peak shows it.
        self.assertAlmostEqual(steps['shrunk']['peak_kib'] - base, 6144, delta=8)
        self.assertAlmostEqual(steps['aligned']['live_kib'] - base, 512, delta=8)
        self.assertLess(steps['aligned']['peak_kib'] - base, 1024 + 8)
        self.assertAlmostEqual(steps['end']['live_kib'], base, delta=1)
        self.assertAlmostEqual(peak_all - base, 6144, delta=8)

    def test_without_preload_nothing_is_attributed(self):
        stdout, heap = self.run_program(preload=False)
        self.assertEqual(heap, '')
        self.assertEqual(document_memory.steps(stdout, heap), ({}, None))

    def test_journey_script_measures_every_screen(self):
        text = document_memory.script(2, Path('/shots'), play=True, screen_off=5)
        names = [line.split()[1] for line in text.splitlines() if line.split()[0] in ('memory', 'memory-trim')]
        for screen in ('artists', 'playlist', 'playing', 'starred', 'albums', 'after'):
            self.assertIn(f'{screen}-p0', names)
            self.assertIn(f'{screen}-p1', names)
        self.assertEqual(names[0], 'boot')
        self.assertEqual(names[-3:], ['screen-off', 'end', 'after-trim'])
        self.assertEqual(len(names), len(set(names)))


class GeneratedLibrary(unittest.TestCase):
    def test_documents_have_the_requested_sizes(self):
        library = document_memory.LargeLibrary(albums=40, artists=30, playlist_songs=120, starred=25)
        body = lambda route, **q: library.subsonic(route, {k: [v] for k, v in q.items()})['subsonic-response']
        self.assertEqual(sum(len(i['artist']) for i in body('getArtists')['artists']['index']), 30)
        playlist = body('getPlaylist', id='generated-playlist')['playlist']
        self.assertEqual((playlist['songCount'], len(playlist['entry'])), (120, 120))
        self.assertEqual(len({s['id'] for s in playlist['entry']}), 120)
        self.assertEqual(len(body('getStarred2')['starred2']['song']), 25)
        self.assertEqual(len(body('getAlbumList2', offset='0', size='500')['albumList2']['album']), 40)
        self.assertEqual(library.audio.get(playlist['entry'][77]['id']), library.audio.data)
        self.assertIsNone(library.audio.get('other'))


if __name__ == '__main__':
    unittest.main()
