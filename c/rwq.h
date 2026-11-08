/* Shared by rwq_dequeue.cpp and rwq_size.cpp: moodycamel's
   ReaderWriterQueue (readerwriterqueue/readerwriterqueue.h, unchanged
   from readerwriterqueue commit 6867b564, September 2026), with an int
   element and room for one element, so that the producer's second
   enqueue allocates a second block.

   The setup function, run before the threads start, constructs the
   queue and enqueues 1, which fills the first block. t0 is the producer:
   it enqueues 2, which allocates a second block. t1, in each program, is
   the consumer.

   -DOLD uses readerwriterqueue/old/readerwriterqueue.h, the current file
   with readerwriterqueue commits 0a5aee4 ("fix: move release fence in
   inner_enqueue()", August 2026) and ba8c662 ("Fence and Comments") undone:
   its enqueue has one release fence, between linking the new block into
   the old one's next and making it the tail block. -DMID uses
   readerwriterqueue/mid/readerwriterqueue.h, with only ba8c662 undone:
   that fence comes before linking the new block instead. Both commits
   were merged in pull request #172. */
#if defined(OLD)
#include "readerwriterqueue/old/readerwriterqueue.h"
#elif defined(MID)
#include "readerwriterqueue/mid/readerwriterqueue.h"
#else
#include "readerwriterqueue/readerwriterqueue.h"
#endif

typedef moodycamel::ReaderWriterQueue<int> Queue;

/* malloc hands out zeroed memory, as fresh pages from the system are. */
alignas(16) char heap[1024];
size_t heap_used;

extern "C" void* malloc(size_t n) {
	void* p = heap + heap_used;
	heap_used += (n + 15) & ~(size_t) 15;
	if (heap_used > sizeof heap)
		__builtin_trap();
	return p;
}

extern "C" void free(void*) { __builtin_trap(); }
extern "C" void abort(void) { __builtin_trap(); }

alignas(Queue) char queue_memory[sizeof(Queue)];
Queue* const queue = reinterpret_cast<Queue*>(queue_memory);
int bad;

extern "C" void setup(void) {
	new (queue_memory) Queue(1);
	queue->enqueue(1);
}

extern "C" void t0(void) {
	queue->enqueue(2);
}
