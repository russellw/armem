/* cflags: -ffreestanding -Iqemu/stub -Iqemu/include */
/* QEMU's QemuEvent (qemu/util/event.c, unchanged) as util/rcu.c uses it:
   the call_rcu thread waits for work while call_rcu1 adds some, and must
   not sleep with no wakeup to come.

   The event is set, as call_rcu1 leaves it, and no work is pending. t0
   runs the loop at the top of call_rcu_thread: it resets the event, reads
   rcu_call_count, and if that is 0 waits on the event, and then tries
   again. t1 runs call_rcu1 without the queue: it increments
   rcu_call_count and sets the event. The stub futex (qemu/stub) records
   a FUTEX_WAKE in qemu_test_woken; FUTEX_WAIT, if it would sleep,
   records that in qemu_test_blocked and ends t0. A call_rcu thread that
   sleeps with no wakeup to come leaves the work, and every later
   call_rcu, waiting.

   -DOLD_EVENT uses qemu/old/util/event.c, the current file with QEMU
   commit 9586a132 ("qemu-thread-posix: cleanup, fix, document
   QemuEvent", March 2023) undone. Its qemu_event_reset makes its
   seq_cst OR (an LDAXR/STLXR loop, or an LDSETAL with LSE) with no
   barrier after it, so the caller's relaxed load of rcu_call_count may
   pass the store. */
#include "qemu/osdep.h"
#include "qemu/thread.h"
#ifdef OLD_EVENT
#include "qemu/old/util/event.c"
#else
#include "qemu/util/event.c"
#endif

QemuEvent rcu_call_ready_event = { .value = EV_SET, .initialized = true };
int rcu_call_count;
unsigned long qemu_test_woken, qemu_test_blocked;
void *qemu_test_exit;

void t0(void) {
	qemu_test_exit = __builtin_return_address(0);
	for (;;) {
		qemu_event_reset(&rcu_call_ready_event);
		if (qatomic_read(&rcu_call_count))
			break;
		qemu_event_wait(&rcu_call_ready_event);
	}
}

void t1(void) {
	qatomic_inc(&rcu_call_count);
	qemu_event_set(&rcu_call_ready_event);
}
