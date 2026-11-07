/* Stand-in for the C library's signal.h (glibc, aarch64): the types liburing uses. */
#ifndef STUB_SIGNAL_H
#define STUB_SIGNAL_H
#include <sys/types.h>
#define _NSIG 65
typedef struct { unsigned long __val[1024 / (8 * sizeof(unsigned long))]; } sigset_t;
typedef struct { int __si_fields[32]; } siginfo_t;
#endif
