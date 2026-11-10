/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Iboost/stub -Iboost/include */
/* Boost.Lockfree's spsc_value (see boost_lockfree.h), a triple buffer
   holding an int. t0, the writer, writes 1 to 4; t1, the reader, reads
   twice. Each read returns the latest value written when it swaps its
   buffer with the one the writer last made available, so the values
   read only increase. The condition is a second read that returns less
   than the first.

   -DOLD uses boost/old/boost/lockfree/spsc_value.hpp, the file from
   before Boost.Lockfree commit fa6cbdf ("spsc_value: strengthen memory
   barriers", August 2026), which made the writer's exchange acq_rel
   rather than release, and the reader's exchange (and its unused
   compare-exchange) acq_rel rather than acquire. */
#include "boost_lockfree.h"
#ifdef OLD
#include "boost/old/boost/lockfree/spsc_value.hpp"
#else
#include <boost/lockfree/spsc_value.hpp>
#endif

typedef boost::lockfree::spsc_value<int> Value;

alignas(Value) char value_memory[sizeof(Value)];
Value* const value = reinterpret_cast<Value*>(value_memory);
int first, second, bad;

extern "C" void setup(void) {
	new (value_memory) Value();
}

extern "C" void t0(void) {
	for (int i = 1; i <= 4; i++)
		value->write(i);
}

extern "C" void t1(void) {
	if (value->read(first) && value->read(second) && second < first)
		bad = 1;
}
