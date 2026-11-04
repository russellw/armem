/* cflags: -ffreestanding -Idpdk/include -Idpdk/stub */
/* DPDK's ring (dpdk/include, unchanged), multi-producer and
   multi-consumer, with 4 slots. It starts holding the item 1, and the
   other slots hold 99, which no one enqueues. Thread t0 acts first as a
   producer and then as a consumer: it enqueues 2, then dequeues two
   items at once. Thread t1 dequeues one item. So the items received
   must be distinct, from 1 and 2, with t0 receiving both or neither.

   DPDK commit a4ad0eba9d (November 2025) says that before it, a thread
   that is first a producer and then a consumer can let another consumer
   see the consumer head it moved without the producer tail it read; the
   other consumer then computes a negative number of entries, which wraps
   to a huge one, and dequeues an empty slot. It says this needs an
   acquire load with RCpc semantics: LDAPR, which clang emits for acquire
   loads with -march=armv8.3-a, rather than LDAR. -DOLD_HEAD restores the
   code from before that commit, and -DRTE_GENERIC_RING -DOLD_GENERIC
   tests the bug of DPDK commit 9bc2cbb007 (see dpdk_ring_variants.h).

   -DHTS and -DRTS use the ring's head/tail sync and relaxed tail sync
   modes instead (RTS with the default maximum distance between head and
   tail, capacity / 8, which is 0 here, unless -DHTD_MAX=N), and -DOLD_HTS and -DOLD_RTS their
   code from before DPDK commits 66d5f96278 and 36b69b5f95, the same
   series as a4ad0eba9d. */

#include "dpdk_ring_variants.h"
#include <rte_ring.h>

#define SLOTS 4

#ifndef HTD_MAX
#define HTD_MAX ((SLOTS - 1) / 8)
#endif

/* The state rte_ring_init and one enqueue of 1 leave. */
static struct {
	struct rte_ring r;
	void *slot[SLOTS];
} ring = {
	.r = {
		.size = SLOTS,
		.mask = SLOTS - 1,
		.capacity = SLOTS - 1,
#if defined(HTS)
		.hts_prod = { .ht.pos = { .head = 1, .tail = 1 }, .sync_type = RTE_RING_SYNC_MT_HTS },
		.hts_cons = { .ht.pos = { .head = 0, .tail = 0 }, .sync_type = RTE_RING_SYNC_MT_HTS },
#elif defined(RTS)
		.rts_prod = { .head.val = { .cnt = 1, .pos = 1 }, .tail.val = { .cnt = 1, .pos = 1 },
			      .sync_type = RTE_RING_SYNC_MT_RTS, .htd_max = HTD_MAX },
		.rts_cons = { .sync_type = RTE_RING_SYNC_MT_RTS, .htd_max = HTD_MAX },
#else
		.prod = { .head = 1, .tail = 1, .sync_type = RTE_RING_SYNC_MT },
		.cons = { .head = 0, .tail = 0, .sync_type = RTE_RING_SYNC_MT },
#endif
	},
	.slot = { (void *)1, (void *)99, (void *)99, (void *)99 },
};

unsigned long got0a, got0b, got1;

void t0(void) {
	void *obj[2] = { 0, 0 };
#ifndef PRODUCER
	rte_ring_enqueue(&ring.r, (void *)2);
#endif
	if (rte_ring_dequeue_bulk(&ring.r, obj, 2, NULL) == 2) {
		got0a = (unsigned long)obj[0];
		got0b = (unsigned long)obj[1];
	}
}

void t1(void) {
	void *obj = 0;
	if (rte_ring_dequeue(&ring.r, &obj) == 0)
		got1 = (unsigned long)obj;
}

#ifdef PRODUCER
void t2(void) {
	rte_ring_enqueue(&ring.r, (void *)2);
}
#endif
/* For -O0, where the copies of entries call it. Like a libc memcpy, it
   copies an aligned word with one load and one store. */
__attribute__((no_builtin)) void *memcpy(void *d, const void *s, size_t n) {
  if (((unsigned long)d | (unsigned long)s | n) % 8 == 0) {
    unsigned long *dw = d;
    const unsigned long *sw = s;
    for (; n; n -= 8)
      *dw++ = *sw++;
  } else {
    char *dc = d;
    const char *sc = s;
    while (n--)
      *dc++ = *sc++;
  }
  return d;
}
