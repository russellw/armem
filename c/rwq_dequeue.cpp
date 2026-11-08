/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Ireaderwriterqueue/stub -Ireaderwriterqueue */
/* moodycamel's ReaderWriterQueue (see rwq.h). t1 is the consumer: it
   calls try_dequeue twice, and records in bad an element other than the
   next one the producer enqueued. Built with NDEBUG, as a release build
   is, so that the queue's own assertions do not trap. */
#include "rwq.h"

int got;

extern "C" void t1(void) {
	for (int i = 0; i < 2; i++) {
		int v;
		if (queue->try_dequeue(v)) {
			if (v != got + 1)
				bad = 1;
			got++;
		}
	}
}
