/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Iatomic_queue/stub */
/* atomic_queue's AtomicQueue (see atomic_queue.h), the queue of atomic
   elements, with one slot, single-producer and single-consumer: a push
   waits for the slot to hold NIL and releases the element into it, and a
   pop waits for it to hold an element (acquire), then releases NIL into
   it. With -DMULTI it is the multi-producer, multi-consumer queue, whose
   push is an acq_rel compare-and-swap from NIL and whose pop an acq_rel
   exchange with NIL. t0 stores 42 in data, then pushes a value; t1 pops it
   and loads data. The condition is t1 getting the value but not 42.

   -DREUSE checks the release in pop instead: t0 tries to push three
   times, then stores 0 in data; t1 pops, loads data, and pops again. The
   condition is t1 popping twice but loading 0.

   By default the element is an int, 1, so t1's load of data depends on
   the element only by control. With -DPTR the element is the pointer
   &data, and t1 loads through it, an address dependency.

   -DOLD_2023 uses the code from before commit 2bc627d ("Fix missing
   acquire memory order on pop for atomic elements.", April 2023), whose
   single-consumer pop loaded the element relaxed rather than acquire. */
#include "atomic_queue.h"

int data;
#ifdef PTR
typedef int* Element;
#define NIL nullptr
#define VALUE (&data)
#define USE(e) (*(e))
#else
typedef int Element;
#define NIL 0
#define VALUE 1
#define USE(e) data
#endif
#ifdef MULTI
typedef atomic_queue::AtomicQueue<Element, 1, NIL> Queue;
#else
typedef atomic_queue::AtomicQueue<Element, 1, NIL, true, true, false, true> Queue;
#endif

alignas(Queue) char queue_memory[sizeof(Queue)];
Queue* const queue = reinterpret_cast<Queue*>(queue_memory);
int bad;

extern "C" void setup(void) {
	new (queue_memory) Queue();
}

#ifndef REUSE
extern "C" void t0(void) {
	data = 42;
	queue->push(VALUE);
}

extern "C" void t1(void) {
	Element e;
	if (queue->try_pop(e) && USE(e) != 42)
		bad = 1;
}
#else
/* The third push waits for the second pop to empty the slot, which t1
   does after it loads data, so t0's store of 0 comes after that load. */
extern "C" void t0(void) {
	data = 42;
	if (queue->try_push(VALUE) && queue->try_push(VALUE) && queue->try_push(VALUE))
		data = 0;
}

extern "C" void t1(void) {
	Element e;
	if (queue->try_pop(e)) {
		int r = USE(e);
		if (queue->try_pop(e) && r != 42)
			bad = 1;
	}
}
#endif
