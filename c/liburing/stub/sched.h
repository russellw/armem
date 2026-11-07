/* Stand-in for the C library's sched.h. */
#ifndef STUB_SCHED_H
#define STUB_SCHED_H
typedef struct { unsigned long __bits[1024 / (8 * sizeof(unsigned long))]; } cpu_set_t;
#endif
