#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
int main(int argc,char **argv) {
    if (argc!=3) return 2;
    time_t t=(time_t)strtoll(argv[2],0,10);
    struct tm tm={0};
    localtime_r(&t,&tm); /* early logger primes libc before TZ selection */
    setenv("TZ",argv[1],1);
    if(getenv("APPLY_TZSET")) tzset();
    if (!localtime_r(&t,&tm)) return 3;
    printf("{\"time_t_bytes\":%u,\"hour\":%d,\"minute\":%d,\"second\":%d,\"offset\":%ld,\"isdst\":%d}\n",(unsigned)sizeof(t),tm.tm_hour,tm.tm_min,tm.tm_sec,tm.tm_gmtoff,tm.tm_isdst);
}
