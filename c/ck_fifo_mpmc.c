/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's multi-producer, multi-consumer FIFO (ck/include/
   ck_fifo.h, unchanged): a Michael-Scott queue whose head, tail and
   links are (pointer, generation) pairs updated by a double-word
   compare-and-swap. As in ck's own test of re-use (added in commit
   e78c421), the queue starts with more entries than there are threads,
   and each thread dequeues an entry and at once enqueues it again in the
   node that the dequeue gave back. So the queue is never empty, and a
   dequeue that reports it empty is a bug, as is a value lost or
   duplicated.

   As written, the FIFO is wrong on ARM: a dequeue reads the value of the
   node it takes and then claims the node by a compare-and-swap, and
   nothing orders the read before the compare-and-swap. The other thread
   can then dequeue too, get the node as garbage and refill it before the
   read, which returns the refilled value. FENCE_DEQ adds a load-store
   fence between them. OLD_ENQ and OLD_DEQ use the enqueue and dequeue
   from before ck commits ed6bace and e78c421. (All in
   ck_fifo_mpmc_variants.c.) */

#include <ck_pr.h>
#include <ck_fifo.h>
#include "ck_fifo_mpmc_variants.c"

#ifndef ROUNDS
#define ROUNDS 1
#endif

/* The stub and the entries 1, 2, as ck_fifo_mpmc_init(&fifo, &node[0])
   and two enqueues leave them. */
static ck_fifo_mpmc_entry_t node[3] = {
	{ .next = { &node[1], 1 } },
	{ .value = (void *)1, .next = { &node[2], 1 } },
	{ .value = (void *)2, .next = { NULL, 1 } },
};
ck_fifo_mpmc_t fifo = {
	.head = { &node[0], 0 },
	.tail = { &node[2], 2 },
};
unsigned long got0[ROUNDS], got1[ROUNDS];

static void run(unsigned long *got) {
	for (int i = 0; i < ROUNDS; i++) {
		void *v;
		ck_fifo_mpmc_entry_t *garbage;
		if (!ck_fifo_mpmc_dequeue(&fifo, &v, &garbage)) {
			got[i] = 0;
			return;
		}
		got[i] = (unsigned long)v;
		ck_fifo_mpmc_enqueue(&fifo, garbage, v);
	}
}

void t0(void) { run(got0); }
void t1(void) { run(got1); }
