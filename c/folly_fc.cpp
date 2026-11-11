/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -DNDEBUG -Dthrow= -Ifolly/stub -Ifolly/include */
/* folly's FlatCombining (folly/include/folly/synchronization, unchanged
   from folly commit 4a2f75a, November 2026; Apache License 2.0, see
   folly/LICENSE) with a dedicated combiner, which sleeps on the
   semaphore pending_ until a requester posts it. Stand-ins for the rest
   of folly and the standard library are in folly/stub; -Dthrow= lets
   the throws that the test never reaches compile without exceptions.

   std::thread discards the combiner's thread function, and t0 runs it
   instead: dedicatedCombining() waits for pending_, resets it, checks
   shutdown_, then under the lock m_ scans the records for valid
   requests, and goes back to waiting. The stub futex ends t0 if it would
   sleep, and so does the stub mutex if t0 would sleep on m_ (see
   folly/stub/README). m_ is contended only while t1 holds it to apply
   its request itself, so no execution that ends there meets the
   condition.

   In setup, pending_ is posted, as by an earlier requester whose post
   the combiner has yet to see, so the combiner wakes, resets pending_
   and takes the lock. t1 makes one asynchronous request, which adds 1 to
   applied, on a record allocated in setup. If it gets the lock, it
   applies the request itself. Otherwise it marks the record valid and
   posts pending_; the record is new, so it then links it into the list
   and posts pending_ again. The condition is a lost wakeup: the combiner
   sleeps, no post wakes it, and the request is never applied. If the
   second post finds pending_ already set, it only fences and reloads it,
   which is safe only if the combiner fences between resetting pending_
   and scanning the records.

   With -DSHUTDOWN, t1 instead shuts the combiner down, as its
   destructor does: it stores shutdown_ and posts pending_, which may
   find pending_ still set by the earlier post. applied stays 0, so the
   condition is then that the combiner sleeps with no post to wake it,
   and the destructor's join() would hang.

   -DOLD uses folly/old/folly/synchronization/FlatCombining.h, the file
   from before folly commit 6d16b1d ("fix lost FlatCombining
   notifications", October 2026), which added a seq_cst fence after the
   combiner resets pending_. */
#include <new>
#ifdef OLD
#include "folly/old/folly/synchronization/FlatCombining.h"
#else
#include <folly/synchronization/FlatCombining.h>
#endif

int applied;

struct Counter : folly::FlatCombining<Counter, std::mutex, std::atomic, int> {
	Counter() : FlatCombining(true) {}
	void combinedOp(int& n) { applied += n; }
	void combine() { dedicatedCombining(); }
	void stop() { shutdown(); }
	void post() { setPending(); }
};

alignas(Counter) char counter_memory[sizeof(Counter)];
Counter* const counter = reinterpret_cast<Counter*>(counter_memory);
Counter::Rec* rec;
extern "C" {
unsigned long folly_test_woken, folly_test_blocked, folly_test_mutex_blocked;
void* folly_test_exit;
}

extern "C" void setup(void) {
	new (counter_memory) Counter();
	rec = counter->allocRec();
	counter->post();
}

extern "C" void t0(void) {
	folly_test_exit = __builtin_return_address(0);
	counter->combine();
}

extern "C" void t1(void) {
#ifdef SHUTDOWN
	counter->stop();
#else
	counter->requestFC([] { applied += 1; }, [](int& n) { n = 1; }, rec, false);
#endif
}
