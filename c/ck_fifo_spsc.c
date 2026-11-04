/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's single-producer, single-consumer FIFO (ck/include/
   ck_fifo.h, unchanged): a linked list whose head is always a stub
   node. The producer sends the values 1, 2, 3 using only two nodes, so
   it recycles each node (ck_fifo_spsc_recycle) once the consumer has
   moved past it, spinning while there is none. The values are sent as
   the pointers themselves: had the consumer read an item through each,
   that address dependency would order its read of the node before its
   next update of the head, even without the dequeue's fence.

   WEAK_ENQ drops the producer's store fence between filling a node and
   linking it; WEAK_DEQ drops the consumer's store fence between reading
   a node's value and moving the head past it, after which the producer
   may refill the node. */

#include <ck_pr.h>
#include <ck_fifo.h>

#define ITEMS 3

#if defined(WEAK_ENQ) || defined(WEAK_DEQ)
/* ck_fifo.h calls ck_pr_fence_store() once in each of enqueue and
   dequeue; replace the one in the function being weakened. */
static inline void weak_fence(void) { __asm__ __volatile__("" ::: "memory"); }
#endif

#ifdef WEAK_ENQ
#define ck_fifo_spsc_enqueue weak_enqueue
CK_CC_INLINE static void
weak_enqueue(struct ck_fifo_spsc *fifo, struct ck_fifo_spsc_entry *entry,
	     void *value)
{
	entry->value = value;
	entry->next = NULL;
	weak_fence();
	ck_pr_store_ptr(&fifo->tail->next, entry);
	fifo->tail = entry;
}
#endif

#ifdef WEAK_DEQ
#define ck_fifo_spsc_dequeue weak_dequeue
CK_CC_INLINE static bool
weak_dequeue(struct ck_fifo_spsc *fifo, void *value)
{
	struct ck_fifo_spsc_entry *entry;

	entry = ck_pr_load_ptr(&fifo->head->next);
	if (entry == NULL)
		return false;
	ck_pr_store_ptr_unsafe(value, entry->value);
	weak_fence();
	ck_pr_store_ptr(&fifo->head, entry);
	return true;
}
#endif

/* As ck_fifo_spsc_init(&fifo, &node[0]) leaves them. */
static ck_fifo_spsc_entry_t node[2];
ck_fifo_spsc_t fifo = {
	.head = &node[0], .tail = &node[0],
	.head_snapshot = &node[0], .garbage = &node[0],
};
unsigned long got[ITEMS];

void producer(void) {
	ck_fifo_spsc_enqueue(&fifo, &node[1], (void *)1);
	for (int i = 1; i < ITEMS; i++) {
		ck_fifo_spsc_entry_t *e;
		while ((e = ck_fifo_spsc_recycle(&fifo)) == NULL) {}
		ck_fifo_spsc_enqueue(&fifo, e, (void *)(unsigned long)(i + 1));
	}
}

void consumer(void) {
	for (int i = 0; i < ITEMS; i++) {
		void *v;
		while (!ck_fifo_spsc_dequeue(&fifo, &v)) {}
		got[i] = (unsigned long)v;
	}
}
