/* LD_PRELOAD shim: one line per epoll_wait with its timeout, result and the
 * time it slept. Shows where a runtime thread wakes between timer deadlines.
 *   cc -shared -fPIC -O2 -o epoll_trace.so epoll_trace.c -ldl
 *   LD_PRELOAD=./epoll_trace.so timer-wake-upstream sleep 250 1
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <time.h>

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000.0 + t.tv_nsec / 1e6;
}

int epoll_wait(int fd, struct epoll_event *events, int max, int timeout) {
    static int (*real)(int, struct epoll_event *, int, int);
    static double first;
    if (!real) real = dlsym(RTLD_NEXT, "epoll_wait");
    double before = now_ms();
    if (first == 0) first = before;
    int n = real(fd, events, max, timeout);
    fprintf(stderr, "t=%8.1f ms  epoll_wait(timeout=%d) = %d after %.1f ms\n", before - first, timeout, n, now_ms() - before);
    return n;
}
