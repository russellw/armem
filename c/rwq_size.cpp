/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Ireaderwriterqueue/stub -Ireaderwriterqueue */
/* moodycamel's ReaderWriterQueue (see rwq.h). t1 is the consumer: it
   calls size_approx, which walks the blocks from the front block through
   their next pointers, and records in bad a size above 2. A block whose
   next pointer it reads as null makes it access the null page, which
   armem reports as a fault. */
#include "rwq.h"

size_t size;

extern "C" void t1(void) {
	size = queue->size_approx();
	if (size > 2)
		bad = 1;
}
