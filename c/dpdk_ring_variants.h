/* Earlier versions of parts of DPDK's ring (dpdk/include), for
   dpdk_ring.c. Included before <rte_ring.h>.

   OLD_HEAD: the multi-thread head move of the C11 ring from before DPDK
   commit a4ad0eba9d ("ring: establish safe partial order in default
   mode", November 2025), copied from rte_ring_c11_pvt.h at its parent
   (its single-thread case dropped, and named as the current code calls
   it). It reads the head relaxed and orders the read of the other side's
   tail after it with an acquire fence, and its compare-and-swap of the
   head is relaxed. The commit gives it acquire and release instead.

   OLD_GENERIC: the generic ring (RTE_GENERIC_RING, rte_ring_gcc_pvt.h)
   without the acquire fence between the reads of the head and of the
   other side's tail, as before DPDK commit 9bc2cbb007 ("ring: guarantee
   load/load order in enqueue and dequeue", November 2017).

   OLD_HTS and OLD_RTS: the head/tail sync and relaxed tail sync modes
   from before DPDK commits 66d5f96278 and 36b69b5f95 (November 2025, the
   same series as a4ad0eba9d): dpdk/old holds their rte_ring_hts_elem_pvt.h
   and rte_ring_rts_elem_pvt.h, unchanged from the commits' parents.
   Included first, they define the include guards of the current files,
   which rte_ring.h then skips. */

#if defined(OLD_HTS) || defined(OLD_RTS)
#include <rte_common.h>
#include <rte_ring_core.h>
#include <rte_ring_elem_pvt.h>
#endif
#ifdef OLD_HTS
#include "dpdk/old/rte_ring_hts_elem_pvt.h"
#endif
#ifdef OLD_RTS
#include "dpdk/old/rte_ring_rts_elem_pvt.h"
#endif

#ifdef OLD_GENERIC
#define __atomic_thread_fence(m) ((void)0)
#endif

#ifdef OLD_HEAD
#include <rte_ring_core.h>
#include <rte_atomic.h>
#define _RTE_RING_C11_PVT_H_

static __rte_always_inline unsigned int
__rte_ring_headtail_move_head_st(struct rte_ring_headtail *d,
		const struct rte_ring_headtail *s, uint32_t capacity,
		unsigned int n,
		enum rte_ring_queue_behavior behavior,
		uint32_t *old_head, uint32_t *new_head, uint32_t *entries);

static __rte_always_inline unsigned int
__rte_ring_headtail_move_head_mt(struct rte_ring_headtail *d,
		const struct rte_ring_headtail *s, uint32_t capacity,
		unsigned int n,
		enum rte_ring_queue_behavior behavior,
		uint32_t *old_head, uint32_t *new_head, uint32_t *entries)
{
	uint32_t stail;
	int success;
	unsigned int max = n;

	*old_head = rte_atomic_load_explicit(&d->head,
			rte_memory_order_relaxed);
	do {
		/* Reset n to the initial burst count */
		n = max;

		/* Ensure the head is read before tail */
		rte_atomic_thread_fence(rte_memory_order_acquire);

		/* load-acquire synchronize with store-release of ht->tail
		 * in update_tail.
		 */
		stail = rte_atomic_load_explicit(&s->tail,
					rte_memory_order_acquire);

		/* The subtraction is done between two unsigned 32bits value
		 * (the result is always modulo 32 bits even if we have
		 * *old_head > s->tail). So 'entries' is always between 0
		 * and capacity (which is < size).
		 */
		*entries = (capacity + stail - *old_head);

		/* check that we have enough room in ring */
		if (unlikely(n > *entries))
			n = (behavior == RTE_RING_QUEUE_FIXED) ?
					0 : *entries;

		if (n == 0)
			return 0;

		*new_head = *old_head + n;
		/* on failure, *old_head is updated */
		success = rte_atomic_compare_exchange_strong_explicit(
				&d->head, old_head, *new_head,
				rte_memory_order_relaxed,
				rte_memory_order_relaxed);
	} while (unlikely(success == 0));
	return n;
}

/* Never called: these rings are multi-thread. */
static __rte_always_inline unsigned int
__rte_ring_headtail_move_head_st(struct rte_ring_headtail *d,
		const struct rte_ring_headtail *s, uint32_t capacity,
		unsigned int n,
		enum rte_ring_queue_behavior behavior,
		uint32_t *old_head, uint32_t *new_head, uint32_t *entries)
{
	return __rte_ring_headtail_move_head_mt(d, s, capacity, n, behavior,
			old_head, new_head, entries);
}
#endif
