/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's spinlocks (ck/include/spinlock, unchanged). Two
   threads each take the lock once and increment a plain counter inside
   it. -DLOCK=N picks the lock:
     1 CAS (compare-and-swap of a flag), 2 FAS (exchange of a flag),
     3 ticket, 4 MCS, 5 CLH, 6 Anderson (array), 7 decrement.
   Each lock acquires with ck_pr_fence_lock() and releases with
   ck_pr_fence_unlock() (both DMB SY on aarch64). -DWEAK_LOCK or
   -DWEAK_UNLOCK turns one of them into a compiler barrier only. */

#include <ck_pr.h>
#ifdef WEAK_LOCK
#define ck_pr_fence_lock ck_pr_barrier
#endif
#ifdef WEAK_UNLOCK
#define ck_pr_fence_unlock ck_pr_barrier
#endif
#include <ck_spinlock.h>

unsigned long count;

#if LOCK == 1
static ck_spinlock_cas_t lock = CK_SPINLOCK_CAS_INITIALIZER;
static void critical(void) {
  ck_spinlock_cas_lock(&lock);
  count++;
  ck_spinlock_cas_unlock(&lock);
}
#elif LOCK == 2
static ck_spinlock_fas_t lock = CK_SPINLOCK_FAS_INITIALIZER;
static void critical(void) {
  ck_spinlock_fas_lock(&lock);
  count++;
  ck_spinlock_fas_unlock(&lock);
}
#elif LOCK == 3
static ck_spinlock_ticket_t lock = CK_SPINLOCK_TICKET_INITIALIZER;
static void critical(void) {
  ck_spinlock_ticket_lock(&lock);
  count++;
  ck_spinlock_ticket_unlock(&lock);
}
#elif LOCK == 4
/* Each thread's queue node is static rather than on its stack, since
   armem's stacks are private to their thread. */
static ck_spinlock_mcs_t lock = CK_SPINLOCK_MCS_INITIALIZER;
static ck_spinlock_mcs_context_t nodes[2];
static void critical_node(ck_spinlock_mcs_context_t *node) {
  ck_spinlock_mcs_lock(&lock, node);
  count++;
  ck_spinlock_mcs_unlock(&lock, node);
}
#elif LOCK == 5
/* The lock starts with an unowned node; each thread brings its own and
   leaves with its predecessor's. */
static ck_spinlock_clh_t unowned = { .wait = false, .previous = 0 };
static ck_spinlock_clh_t *lock = &unowned;
static ck_spinlock_clh_t nodes[2];
static void critical_clh(ck_spinlock_clh_t *me) {
  ck_spinlock_clh_lock(&lock, me);
  count++;
  ck_spinlock_clh_unlock(&me);
}
#elif LOCK == 6
static ck_spinlock_anderson_thread_t slots[2] = { { false, 0 }, { true, 1 } };
static ck_spinlock_anderson_t lock = { .slots = slots, .count = 2, .wrap = 0, .mask = 1, .next = 0 };
static void critical(void) {
  ck_spinlock_anderson_thread_t *slot;
  ck_spinlock_anderson_lock(&lock, &slot);
  count++;
  ck_spinlock_anderson_unlock(&lock, slot);
}
#elif LOCK == 7
static ck_spinlock_dec_t lock = CK_SPINLOCK_DEC_INITIALIZER;
static void critical(void) {
  ck_spinlock_dec_lock(&lock);
  count++;
  ck_spinlock_dec_unlock(&lock);
}
#else
#error "define LOCK as 1 to 7"
#endif

#if LOCK == 4
void thread_a(void) { critical_node(&nodes[0]); }
void thread_b(void) { critical_node(&nodes[1]); }
#elif LOCK == 5
void thread_a(void) { critical_clh(&nodes[0]); }
void thread_b(void) { critical_clh(&nodes[1]); }
#else
void thread_a(void) { critical(); }
void thread_b(void) { critical(); }
#endif
