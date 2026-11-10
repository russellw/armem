/* Shared by the atomic_queue_* programs: freestanding atomic_queue
   (atomic_queue/include, unchanged from atomic_queue commit b7f1bef,
   October 2026; MIT License, see atomic_queue/LICENSE), with stand-ins
   for the standard headers it includes (atomic_queue/stub).
   -DOLD_2020 uses the headers from before commit 95bbd40 (November 2020),
   in atomic_queue/old_2020, and -DOLD_2023 those from before commit
   2bc627d (April 2023), in atomic_queue/old_2023. Each test constructs
   its queue in its setup function, run before the threads start, so that
   the queue needs no static constructor or destructor. */
#include <new>
/* The old headers write Base::template f(...) without a template argument
   list, which clang 19 and later reject by default. */
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wunknown-warning-option"
#pragma clang diagnostic ignored "-Wmissing-template-arg-list-after-template-kw"
#if defined(OLD_2020)
#include "atomic_queue/old_2020/atomic_queue/atomic_queue.h"
#elif defined(OLD_2023)
#include "atomic_queue/old_2023/atomic_queue/atomic_queue.h"
#else
#include "atomic_queue/include/atomic_queue/atomic_queue.h"
#endif
#pragma clang diagnostic pop

/* At -O0 and -Os, clang zeroes AtomicQueue2's arrays in its constructor
   with a call to memset. */
extern "C" __attribute__((no_builtin)) void* memset(void* d, int c, std::size_t n) {
	unsigned char* p = static_cast<unsigned char*>(d);
	while (n--)
		*p++ = static_cast<unsigned char>(c);
	return d;
}
