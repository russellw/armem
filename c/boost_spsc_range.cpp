/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Iboost/stub -Iboost/include */
/* Boost.Lockfree's spsc_queue (see boost_lockfree.h), pushing and popping
   ranges. The queue holds up to 2 ints, in 3 slots. The setup function
   pushes and pops two elements, so that the indices start at 2.

   t0, the producer, pushes as many of 1, 2, 3, 4 as fit, twice. t1, the
   consumer, pops as many as it can, twice. A first push or pop of two
   elements wraps around the end of the buffer, and the producer's
   second push of two re-uses the slot of the first element. The
   condition is that the consumer pops a value other than the next one
   pushed. It checks the values only after its last pop, since a branch
   on a value would order its read of the slot before its later stores,
   as a release does. */
#include "boost_lockfree.h"
#include <boost/lockfree/spsc_queue.hpp>

typedef boost::lockfree::spsc_queue<int, boost::lockfree::capacity<2>> Queue;

alignas(Queue) char queue_memory[sizeof(Queue)];
Queue* const queue = reinterpret_cast<Queue*>(queue_memory);
int items[4] = {1, 2, 3, 4};
int pushed, got, bad;
int vals[4];

extern "C" void setup(void) {
	new (queue_memory) Queue();
	int z[2] = {0, 0};
	queue->push(z, 2);
	queue->pop(z, 2);
}

extern "C" void t0(void) {
	for (int i = 0; i < 2; i++)
		pushed += queue->push(items + pushed, 4 - pushed);
}

extern "C" void t1(void) {
	for (int i = 0; i < 2; i++)
		got += queue->pop(vals + got, 4 - got);
	/* armem has no vector arithmetic, which clang -O2 would use here. */
#pragma clang loop vectorize(disable)
	for (int i = 0; i < got; i++)
		if (vals[i] != i + 1)
			bad = 1;
}
