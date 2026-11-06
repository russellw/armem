/* Stub libc headers for compiling libuv freestanding: just the types and
   declarations uv-common.c and unix/async.c need. */
#ifndef STUB_SYS_TYPES_H
#define STUB_SYS_TYPES_H
#include <stddef.h>
#include <stdint.h>
typedef long ssize_t;
typedef long off_t;
typedef unsigned mode_t;
typedef int pid_t;
typedef unsigned uid_t;
typedef unsigned gid_t;
#endif
