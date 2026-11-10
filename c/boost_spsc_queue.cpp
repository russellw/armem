/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Iboost/stub -Iboost/include */
/* Boost.Lockfree's spsc_queue (see boost_lockfree.h), a ring buffer of
   ints with room for CAP elements (default 1), sized at compile time.
   Its buffer has CAP + 1 slots: with CAP 1 or 3 a power of two, so the
   index wraps with a mask, and otherwise by a subtraction loop.

   t0, the producer, tries to push 1, 2, 3, ... three times, moving on to
   the next value only when a push succeeds. t1, the consumer, tries to
   pop three times. With CAP 1, the third push re-uses the first slot.
   The condition is that the consumer pops a value other than the next
   one pushed. It checks the values only after its last pop, since a
   branch on a value would order its read of the slot before its later
   stores, as a release does. */
#include "boost_lockfree.h"
#include <boost/lockfree/spsc_queue.hpp>

#ifndef CAP
#define CAP 1
#endif

typedef boost::lockfree::spsc_queue<int, boost::lockfree::capacity<CAP>> Queue;

alignas(Queue) char queue_memory[sizeof(Queue)];
Queue* const queue = reinterpret_cast<Queue*>(queue_memory);
int pushed, got, bad;
int vals[3];

extern "C" void setup(void) {
	new (queue_memory) Queue();
}

extern "C" void t0(void) {
	for (int i = 0; i < 3; i++)
		if (queue->push(pushed + 1))
			pushed++;
}

extern "C" void t1(void) {
	for (int i = 0; i < 3; i++)
		if (queue->pop(vals[got]))
			got++;
	for (int i = 0; i < got; i++)
		if (vals[i] != i + 1)
			bad = 1;
}
