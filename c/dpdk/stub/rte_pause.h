/* rte_pause and rte_wait_until_equal_32, as in DPDK's generic
   rte_pause.h (lib/eal/include/generic) without WFE. */
#ifndef _RTE_PAUSE_H_
#define _RTE_PAUSE_H_
#include <rte_common.h>
#include <rte_stdatomic.h>
static inline void rte_pause(void) { __asm__ volatile("yield" ::: "memory"); }
static __rte_always_inline void
rte_wait_until_equal_32(volatile uint32_t *addr, uint32_t expected, rte_memory_order memorder)
{
	while (rte_atomic_load_explicit((volatile __rte_atomic uint32_t *)addr, memorder) != expected)
		rte_pause();
}
#endif
