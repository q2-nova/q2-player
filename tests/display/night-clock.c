#define _GNU_SOURCE
#include <time.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/syscall.h>
struct wide { int64_t sec,nsec; };
static int64_t epoch(void) { const char *s=getenv("Q2_TEST_EPOCH");return s?strtoll(s,0,10):1793691334; }
int64_t __time64(int64_t *p) { int64_t t=epoch();if(p)*p=t;return t; }
time_t time(time_t *p) { time_t t=epoch();if(p)*p=t;return t; }
int __clock_gettime64(clockid_t id,struct wide *out) {
    if (id==CLOCK_REALTIME) { out->sec=epoch();out->nsec=0;return 0; }
    int ret=syscall(SYS_clock_gettime64,id,out);
    if (!ret) { int64_t n=out->nsec*120;out->sec=out->sec*120+n/1000000000;out->nsec=n%1000000000; }
    return ret;
}
int clock_gettime(clockid_t id,struct timespec *out) {
    struct wide w;int ret=__clock_gettime64(id,&w);
    if(!ret){out->tv_sec=w.sec;out->tv_nsec=w.nsec;}
    return ret;
}
