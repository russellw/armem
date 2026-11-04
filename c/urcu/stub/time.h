/* Stand-in for the C library's time.h: what liburcu's headers use. */
#ifndef _STUB_TIME_H
#define _STUB_TIME_H
struct timespec { long tv_sec; long tv_nsec; };
#define CLOCK_MONOTONIC 1
int clock_gettime(int clock, struct timespec *ts);
#endif
