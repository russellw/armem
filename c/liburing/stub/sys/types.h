/* Stand-in for the C library's sys/types.h (glibc, aarch64). */
#ifndef STUB_SYS_TYPES_H
#define STUB_SYS_TYPES_H
#include <stddef.h>
typedef long ssize_t;
typedef long off_t;
typedef unsigned int mode_t;
typedef unsigned int id_t;
typedef int pid_t;
#endif
