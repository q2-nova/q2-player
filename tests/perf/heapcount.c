// Host-only heap accounting for q2nd (LD_PRELOAD). Counts live heap bytes
// (malloc_usable_size of every live block) and the peak since the previous
// report. A report is written to $HEAPCOUNT_LOG whenever the process calls
// mallinfo2, which the headless script's `memory NAME` step does, so each
// script step gets a line: seq, CLOCK_BOOTTIME, live KiB, peak KiB since the
// last report and peak KiB since start. The peak since the last report is
// then reset. tests/perf/document_memory.py reads the lines; the counts are
// the host allocator's (x86_64), not the Q2's.
//
//   cc -O2 -shared -fPIC -o build/heapcount.so tests/perf/heapcount.c -ldl
//   HEAPCOUNT_LOG=build/x/heap.log LD_PRELOAD=build/heapcount.so q2nd --headless ...
#define _GNU_SOURCE
#include <malloc.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>

extern void *__libc_malloc(size_t);
extern void *__libc_calloc(size_t, size_t);
extern void *__libc_realloc(void *, size_t);
extern void *__libc_memalign(size_t, size_t);
extern void __libc_free(void *);

static size_t live, peak, peak_all, seq;

static void grow(void *p) {
    if (!p) return;
    size_t n = __atomic_add_fetch(&live, malloc_usable_size(p), __ATOMIC_RELAXED);
    size_t old = __atomic_load_n(&peak, __ATOMIC_RELAXED);
    while (n > old && !__atomic_compare_exchange_n(&peak, &old, n, 1, __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {}
    old = __atomic_load_n(&peak_all, __ATOMIC_RELAXED);
    while (n > old && !__atomic_compare_exchange_n(&peak_all, &old, n, 1, __ATOMIC_RELAXED, __ATOMIC_RELAXED)) {}
}
static void shrink(void *p) {
    if (p) __atomic_sub_fetch(&live, malloc_usable_size(p), __ATOMIC_RELAXED);
}

void *malloc(size_t n) { void *p = __libc_malloc(n); grow(p); return p; }
void *calloc(size_t a, size_t b) { void *p = __libc_calloc(a, b); grow(p); return p; }
void free(void *p) { shrink(p); __libc_free(p); }
void *realloc(void *p, size_t n) {
    size_t before = p ? malloc_usable_size(p) : 0;
    void *q = __libc_realloc(p, n);
    if (!q) return q;
    __atomic_sub_fetch(&live, before, __ATOMIC_RELAXED);
    grow(q);
    return q;
}
void *memalign(size_t a, size_t n) { void *p = __libc_memalign(a, n); grow(p); return p; }
void *aligned_alloc(size_t a, size_t n) { return memalign(a, n); }
int posix_memalign(void **out, size_t a, size_t n) {
    void *p = __libc_memalign(a, n);
    if (!p) return ENOMEM;
    grow(p);
    *out = p;
    return 0;
}

struct mallinfo2 mallinfo2(void) {
    struct mallinfo2 info;
    // glibc's own figures, through the real symbol.
    static struct mallinfo2 (*real)(void);
    if (!real) {
        extern void *dlsym(void *, const char *);
        real = (struct mallinfo2 (*)(void))dlsym((void *)-1l /* RTLD_NEXT */, "mallinfo2");
    }
    if (real) info = real(); else memset(&info, 0, sizeof info);
    const char *path = getenv("HEAPCOUNT_LOG");
    if (path) {
        char line[160];
        size_t now = __atomic_load_n(&live, __ATOMIC_RELAXED);
        size_t top = __atomic_exchange_n(&peak, now, __ATOMIC_RELAXED);
        struct timespec ts;
        clock_gettime(CLOCK_BOOTTIME, &ts);
        int len = snprintf(line, sizeof line, "heapcount seq=%zu boottime_us=%lld live_kib=%zu peak_kib=%zu peak_all_kib=%zu\n",
                           ++seq, (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000, now / 1024, top / 1024,
                           __atomic_load_n(&peak_all, __ATOMIC_RELAXED) / 1024);
        int fd = open(path, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644);
        if (fd >= 0) { if (write(fd, line, (size_t)len) < 0) {} close(fd); }
    }
    return info;
}
