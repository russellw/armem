/* cflags: -ffreestanding -Iurcu/stub -Iurcu/include */
/* liburcu's QSBR flavor (urcu/include, unchanged): a reader announces a
   quiescent state while synchronize_rcu waits for it, and must not leave
   the grace period asleep with no one to wake it.

   The reader, t0, is online with the counter of the previous grace
   period; the grace period has begun (urcu_qsbr_gp.ctr has advanced).
   t0 calls urcu_qsbr_quiescent_state() (_urcu_qsbr_quiescent_state from
   urcu/static/urcu-qsbr.h), or with -DOFFLINE urcu_qsbr_thread_offline().
   The writer, t1, runs one pass of wait_for_readers from liburcu's
   src/urcu-qsbr.c, the pass that has run out of busy-wait attempts: it
   arms the futex, sets the reader's waiting flag, and checks the reader's
   counter; if the reader has not yet reported, it would call wait_gp,
   which sleeps in FUTEX_WAIT while the futex is still -1. "blocked" records
   that it would sleep; the stub futex records a FUTEX_WAKE in
   urcu_test_woken. A grace period that sleeps with no wakeup to come
   hangs synchronize_rcu.

   -DOLD_QSBR restores the code from before liburcu commit 9e6448d (see
   urcu_variants.h), whose store of the reader's counter is CMM_SEQ_CST:
   with a toolchain that supports C11 atomics, an STLR with no barrier
   after it, which a later load may pass. */
#define _LGPL_SOURCE
#include "urcu_variants.h"
#include <urcu/static/urcu-qsbr.h>

struct urcu_gp urcu_qsbr_gp = { .ctr = URCU_QSBR_GP_ONLINE + URCU_QSBR_GP_CTR };
struct urcu_qsbr_reader urcu_qsbr_reader = { .ctr = URCU_QSBR_GP_ONLINE, .registered = 1 };
unsigned long urcu_test_woken, blocked;

void t0(void) {
#ifdef OFFLINE
	_urcu_qsbr_thread_offline();
#else
	_urcu_qsbr_quiescent_state();
#endif
}

void t1(void) {
	struct urcu_qsbr_reader *index = &urcu_qsbr_reader;

	/* From wait_for_readers, with wait_loops >= RCU_QS_ACTIVE_ATTEMPTS. */
	uatomic_store(&urcu_qsbr_gp.futex, -1);
	cmm_smp_wmb();
	uatomic_store(&index->waiting, 1);
	cmm_smp_mb();
	if (urcu_qsbr_reader_state(&index->ctr, NULL) == URCU_READER_ACTIVE_OLD) {
		/* wait_gp */
		cmm_smp_rmb();
		if (uatomic_load(&urcu_qsbr_gp.futex) == -1)
			blocked = 1;
	} else {
		uatomic_store(&urcu_qsbr_gp.futex, 0, CMM_RELEASE);
	}
}
