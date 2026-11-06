/* Stand-in for QEMU's qemu/osdep.h: what util/event.c uses. QEMU always
   builds with assertions; a failed one traps (and faults the test). */
#ifndef QEMU_OSDEP_H
#define QEMU_OSDEP_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <limits.h>
#define assert(x) do { if (!(x)) __builtin_trap(); } while (0)
/* As in QEMU: the compiler must prove the test true (here, the size
   checks in qemu/atomic.h). */
#define qemu_build_assert(test) _Static_assert(test, "qemu_build_assert")
#endif
