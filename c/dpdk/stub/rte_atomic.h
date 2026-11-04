/* rte_atomic_thread_fence and rte_int128_t, as in DPDK's rte_atomic.h
   (aarch64 and generic). */
#ifndef _RTE_ATOMIC_H_
#define _RTE_ATOMIC_H_
#include <rte_stdatomic.h>
static __rte_always_inline void rte_atomic_thread_fence(rte_memory_order memorder)
{
	__rte_atomic_thread_fence(memorder);
}
typedef struct __rte_aligned(16) {
	union {
		uint64_t val[2];
		__extension__ __int128 int128;
	};
} rte_int128_t;
#endif
