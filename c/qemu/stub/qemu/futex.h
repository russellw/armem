/* Stand-in for QEMU's qemu/futex.h on Linux. qemu_futex_wake_all and
   qemu_futex_wake_single record a FUTEX_WAKE in qemu_test_woken.
   qemu_futex_wait does what FUTEX_WAIT does before it sleeps: a full
   barrier (smp_mb() (A) in the kernel's futex code), then a check that
   the futex word still holds val. If it does, the thread would sleep:
   the stub records that in qemu_test_blocked and ends the thread, by
   branching to qemu_test_exit, the address the test's thread function
   returns to. Otherwise FUTEX_WAIT fails with EWOULDBLOCK, and the stub
   returns. */
#ifndef QEMU_FUTEX_H
#define QEMU_FUTEX_H

#define HAVE_FUTEX

extern unsigned long qemu_test_woken, qemu_test_blocked;
extern void *qemu_test_exit;

static inline void qemu_futex_wake_all(void *f)
{
    (void) f;
    __atomic_store_n(&qemu_test_woken, 1, __ATOMIC_RELAXED);
}

static inline void qemu_futex_wake_single(void *f)
{
    qemu_futex_wake_all(f);
}

static inline void qemu_futex_wait(void *f, unsigned val)
{
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    if (__atomic_load_n((unsigned *) f, __ATOMIC_RELAXED) == val) {
        __atomic_store_n(&qemu_test_blocked, 1, __ATOMIC_RELAXED);
        asm volatile("br %0" :: "r"(qemu_test_exit));
        __builtin_unreachable();
    }
}
#endif
