/* The parts of DPDK's rte_common.h (lib/eal/include) that the ring
   headers use, for a freestanding aarch64 build. */
#ifndef _RTE_COMMON_H_
#define _RTE_COMMON_H_
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <stdalign.h>
typedef long ssize_t;
#include <rte_config.h>
#define __rte_always_inline inline __attribute__((always_inline))
#define __rte_noinline __attribute__((noinline))
#define __rte_unused __attribute__((unused))
#define __rte_aligned(a) __attribute__((aligned(a)))
#define __rte_cache_aligned __rte_aligned(RTE_CACHE_LINE_SIZE)
#define __rte_malloc __attribute__((malloc))
#define __rte_dealloc(f, n)
#define __rte_experimental
#define __rte_internal
#define __rte_warn_unused_result __attribute__((warn_unused_result))
#define RTE_CACHE_GUARD
#define RTE_SET_USED(x) (void)(x)
#define RTE_MIN(a, b) ((a) < (b) ? (a) : (b))
#define RTE_MAX(a, b) ((a) > (b) ? (a) : (b))
#define RTE_BUILD_BUG_ON(c) _Static_assert(!(c), #c)
#define RTE_ASSERT(x) do { } while (0)
typedef uint64_t unaligned_uint64_t __rte_aligned(1);
typedef uint32_t unaligned_uint32_t __rte_aligned(1);
typedef uint16_t unaligned_uint16_t __rte_aligned(1);
#define RTE_PTR_ADD(p, x) ((void *)((uintptr_t)(p) + (x)))
#endif
