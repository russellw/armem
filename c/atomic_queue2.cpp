/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Iatomic_queue/stub */
/* atomic_queue's AtomicQueue2<int, 1> (see atomic_queue.h), the
   multi-producer, multi-consumer queue of elements that need not be
   atomic: each slot has a state, which a push takes from EMPTY to
   STORING with a compare-and-swap, then stores the element and releases
   STORED, and a pop takes from STORED to LOADING, then loads the element
   and releases EMPTY. With -DSINGLE it is the single-producer,
   single-consumer queue, whose push and pop instead wait for EMPTY and
   STORED (acquire). The queue has one slot. t0 pushes 1 and 2; t1 tries
   to pop twice. The condition is a pop that returns a value other than the
   one pushed.

   -DOLD_2020 uses the code from before commit 95bbd40 ("Bugfix. Use
   acquire semantics when loading states of non-atomic elements.",
   November 2020), whose compare-and-swaps in push and pop were relaxed
   rather than acquire (the single-producer, single-consumer code is the
   same in both). */
#include "atomic_queue.h"

#ifdef SINGLE
typedef atomic_queue::AtomicQueue2<int, 1, true, true, false, true> Queue;
#else
typedef atomic_queue::AtomicQueue2<int, 1> Queue;
#endif

alignas(Queue) char queue_memory[sizeof(Queue)];
Queue* const queue = reinterpret_cast<Queue*>(queue_memory);
int bad;

extern "C" void setup(void) {
	new (queue_memory) Queue();
}

extern "C" void t0(void) {
	queue->push(1);
	queue->push(2);
}

/* The values are checked only after the last pop: a branch on the first
   would order the second pop's accesses after its load. */
extern "C" void t1(void) {
	int a = 1, b = 2;
	bool got_a = queue->try_pop(a);
	bool got_b = got_a && queue->try_pop(b);
	if (a != 1 || b != 2)
		bad = 1;
	(void)got_b;
}
