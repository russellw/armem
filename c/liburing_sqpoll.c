/* cflags: -ffreestanding -nostdlibinc -Iliburing/stub -Iliburing/src/include -Iliburing/src */
/* liburing's submission to a ring with a kernel polling thread
   (IORING_SETUP_SQPOLL), with liburing/src/queue.c and its headers
   unchanged from liburing commit c63f4aa6 (2.16). The polling thread
   sleeps when it finds no work, after setting IORING_SQ_NEED_WAKEUP in
   the ring's flags. io_uring_submit must then see the flag and wake it
   with io_uring_enter(IORING_ENTER_SQ_WAKEUP), or the new entry is never
   submitted.

   t0 is the application: it gets an SQE, makes it a NOP, and calls
   io_uring_submit, whose system calls go to uring_test_syscall (see
   liburing/stub/arch/aarch64/syscall.h). That records a wakeup in
   uring_test_woken. t1 is the polling thread's check before it sleeps,
   from io_sq_thread in Linux's io_uring/sqpoll.c (Linux commit
   d4872371, November 2026), for one ring with nothing pending, with the
   kernel's arm64 atomic_or (LSE with -march=armv8.1-a), smp_mb (which
   smp_mb__after_atomic is on arm64) and smp_load_acquire. If it would
   sleep, it records that in uring_test_slept.

   -DOLD_SUBMIT uses liburing/old/src/queue.c, the current file with
   liburing commit 744f4156 ("Fix memory ordering in sq_ring_needs_enter",
   March 2022) undone. -DNO_KERNEL_MB removes the kernel's
   smp_mb__after_atomic. */
#ifdef OLD_SUBMIT
#include "liburing/old/src/queue.c"
#else
#include "liburing/src/queue.c"
#endif

int uring_test_slept;
int uring_test_woken;

long uring_test_syscall(long n, long a, long b, long c, long d, long e,
			long f) {
	(void) a; (void) c; (void) e; (void) f;
	if (n == __NR_io_uring_enter && (d & IORING_ENTER_SQ_WAKEUP))
		__atomic_store_n(&uring_test_woken, 1, __ATOMIC_RELAXED);
	return b;
}

/* The shared parts of the ring, as in the kernel's struct io_rings. */
struct {
	unsigned sq_head, sq_tail, cq_head, cq_tail;
	unsigned sq_flags, cq_flags, cq_overflow;
} rings;
unsigned sq_array[2];
struct io_uring_sqe sqes[2];
struct io_uring_cqe cqes[2];

/* As io_uring_queue_init leaves it for a 2-entry SQPOLL ring. */
struct io_uring ring = {
	.sq = {
		.khead = &rings.sq_head,
		.ktail = &rings.sq_tail,
		.kflags = &rings.sq_flags,
		.array = sq_array,
		.sqes = sqes,
		.ring_mask = 1,
		.ring_entries = 2,
	},
	.cq = {
		.khead = &rings.cq_head,
		.ktail = &rings.cq_tail,
		.kflags = &rings.cq_flags,
		.koverflow = &rings.cq_overflow,
		.cqes = cqes,
		.ring_mask = 1,
		.ring_entries = 2,
	},
	.flags = IORING_SETUP_SQPOLL,
	.ring_fd = 3,
	.enter_ring_fd = 3,
};

void t0(void) {
	struct io_uring_sqe *sqe = io_uring_get_sqe(&ring);
	io_uring_prep_nop(sqe);
	io_uring_submit(&ring);
}

/* The polling thread's own state for the ring. */
unsigned cached_sq_head;
unsigned sq_entries = 2;

/* The kernel's arm64 atomic_or (arch/arm64/include/asm/atomic_lse.h and
   atomic_ll_sc.h; the LL/SC version's PRFM, a prefetch hint, is left out),
   and its smp_mb and 32-bit smp_load_acquire (asm/barrier.h). */
static inline void atomic_or(unsigned i, unsigned *v) {
#ifdef __ARM_FEATURE_ATOMICS
	__asm__ volatile("stset %w[i], %[v]" : [v] "+Q" (*v) : [i] "r" (i));
#else
	unsigned long tmp;
	unsigned result;
	__asm__ volatile(
		"1:	ldxr	%w0, %2\n"
		"	orr	%w0, %w0, %w3\n"
		"	stxr	%w1, %w0, %2\n"
		"	cbnz	%w1, 1b\n"
		: "=&r" (result), "=&r" (tmp), "+Q" (*v)
		: "r" (i));
#endif
}
#define smp_mb() __asm__ volatile("dmb ish" ::: "memory")
static inline unsigned smp_load_acquire(unsigned *p) {
	unsigned v;
	__asm__ volatile("ldar %w0, %1" : "=r" (v) : "Q" (*p) : "memory");
	return v;
}

void t1(void) {
	unsigned entries;
	int needs_sched = 1;

	atomic_or(IORING_SQ_NEED_WAKEUP, &rings.sq_flags);
#ifndef NO_KERNEL_MB
	/* smp_mb__after_atomic() */
	smp_mb();
#endif
	/* io_sqring_entries(ctx) */
	entries = smp_load_acquire(&rings.sq_tail) - cached_sq_head;
	if ((entries < sq_entries ? entries : sq_entries) != 0)
		needs_sched = 0;
	if (needs_sched)
		__atomic_store_n(&uring_test_slept, 1, __ATOMIC_RELAXED);
}
