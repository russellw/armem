/* Stand-in for liburcu's urcu/futex.h: futex_noasync and futex_async
   record a FUTEX_WAKE in urcu_test_woken, and do nothing else. (A
   FUTEX_WAIT would block, which the tests check for themselves.) */
#ifndef _URCU_FUTEX_H
#define _URCU_FUTEX_H
#include <stdint.h>
#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
extern unsigned long urcu_test_woken;
static inline int futex_noasync(int32_t *uaddr, int op, int32_t val,
		const void *timeout, int32_t *uaddr2, int32_t val3)
{
	(void) uaddr; (void) val; (void) timeout; (void) uaddr2; (void) val3;
	if (op == FUTEX_WAKE)
		__atomic_store_n(&urcu_test_woken, 1, __ATOMIC_RELAXED);
	return 0;
}
#define futex_async futex_noasync
#endif
