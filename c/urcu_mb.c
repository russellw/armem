/* cflags: -ffreestanding -Iurcu/stub -Iurcu/include */
/* liburcu's mb flavor (urcu/include, unchanged): a reader leaves its
   read-side critical section while synchronize_rcu waits for it, and must
   not leave the grace period asleep with no one to wake it.

   The reader, t0, is in a critical section begun in the previous phase;
   the grace period has flipped urcu_mb_gp.ctr's phase. t0 calls
   urcu_mb_read_unlock() (_urcu_mb_read_unlock from urcu/static/urcu-mb.h).
   The writer, t1, runs one pass of wait_for_readers from liburcu's
   src/urcu.c, the pass that has run out of busy-wait attempts: it
   decrements the futex to -1 and checks the reader's counter; if the
   reader is still in its critical section, it would call wait_gp, which
   sleeps in FUTEX_WAIT while the futex is still -1. "blocked" records that
   it would sleep; the stub futex records a FUTEX_WAKE in urcu_test_woken.

   -DOLD_MB restores the code from before liburcu commit 96ef712 (see
   urcu_variants.h), whose store of the reader's counter is CMM_SEQ_CST. */
#define _LGPL_SOURCE
#define RCU_MB
#include "urcu_variants.h"
#include <urcu/static/urcu-mb.h>

struct urcu_gp urcu_mb_gp = { .ctr = URCU_GP_COUNT | URCU_GP_CTR_PHASE };
struct urcu_reader urcu_mb_reader = { .ctr = URCU_GP_COUNT, .registered = 1 };
unsigned long urcu_test_woken, blocked;

void t0(void) {
	_urcu_mb_read_unlock();
}

void t1(void) {
	struct urcu_reader *index = &urcu_mb_reader;

	/* From wait_for_readers, with wait_loops >= RCU_QS_ACTIVE_ATTEMPTS
	   (smp_mb_master is cmm_smp_mb in this flavor). */
	uatomic_dec(&urcu_mb_gp.futex);
	cmm_smp_mb();
	if (urcu_common_reader_state(&urcu_mb_gp, &index->ctr, NULL) == URCU_READER_ACTIVE_OLD) {
		/* wait_gp */
		cmm_smp_mb();
		if (uatomic_load(&urcu_mb_gp.futex) == -1)
			blocked = 1;
	} else {
		cmm_smp_mb();
		uatomic_store(&urcu_mb_gp.futex, 0);
	}
}
