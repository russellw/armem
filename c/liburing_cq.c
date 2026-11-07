/* cflags: -ffreestanding -nostdlibinc -Iliburing/stub -Iliburing/src/include -Iliburing/src */
/* liburing's completion queue (liburing/src/include/liburing.h and
   liburing/src/queue.c, unchanged from liburing commit c63f4aa6, 2.16),
   with one entry, so that the kernel reuses its slot for each completion.

   t0 is the application: it calls io_uring_peek_cqe twice, and for each
   CQE it gets, checks the CQE's res (the kernel posts 1 then 2), then
   calls io_uring_cqe_seen. It records a wrong res in bad. t1 is the
   kernel posting two completions, as io_uring/io_uring.c in Linux commit
   d4872371 (November 2026) does: io_cqe_cache_refill finds a free slot
   from the application's head (READ_ONCE), and if there is none the CQE
   overflows (here, t1 stops); the CQE's fields are written with
   WRITE_ONCE, and io_commit_cqring publishes the tail with
   smp_store_release.

   -DACQ_HEAD uses liburing/old/include/liburing.h, the current file with
   liburing commit a4d619a2 ("drop acquire ordering on CQ head load in
   __io_uring_peek_cqe()", June 2026) undone, so that the application loads
   the head with acquire. -DNO_KERNEL_RELEASE makes the kernel's tail store
   a WRITE_ONCE. */
#ifdef ACQ_HEAD
#include "liburing/old/include/liburing.h"
#endif
#include "liburing/src/queue.c"

int bad;
int got;

long uring_test_syscall(long n, long a, long b, long c, long d, long e,
			long f) {
	(void) n; (void) a; (void) c; (void) d; (void) e; (void) f;
	return b;
}

/* The shared parts of the ring, as in the kernel's struct io_rings. */
struct {
	unsigned sq_head, sq_tail, cq_head, cq_tail;
	unsigned sq_flags, cq_flags, cq_overflow;
} rings;
unsigned sq_array[1];
struct io_uring_sqe sqes[1];
struct io_uring_cqe cqes[1] = { { .user_data = 99, .res = 99 } };

/* As io_uring_queue_init leaves it for a ring with one entry. */
struct io_uring ring = {
	.sq = {
		.khead = &rings.sq_head,
		.ktail = &rings.sq_tail,
		.kflags = &rings.sq_flags,
		.array = sq_array,
		.sqes = sqes,
		.ring_mask = 0,
		.ring_entries = 1,
	},
	.cq = {
		.khead = &rings.cq_head,
		.ktail = &rings.cq_tail,
		.kflags = &rings.cq_flags,
		.koverflow = &rings.cq_overflow,
		.cqes = cqes,
		.ring_mask = 0,
		.ring_entries = 1,
	},
	.ring_fd = 3,
	.enter_ring_fd = 3,
	.features = IORING_FEAT_EXT_ARG,
};

static void reap(void) {
	struct io_uring_cqe *cqe;

	if (io_uring_peek_cqe(&ring, &cqe) == 0) {
		if (cqe->res != got + 1)
			bad = 1;
		got++;
		io_uring_cqe_seen(&ring, cqe);
	}
}

void t0(void) {
	reap();
	reap();
}

/* The kernel's own state for the ring. */
unsigned cached_cq_tail;
unsigned cq_entries = 1;

#define READ_ONCE(x) (*(volatile __typeof__(x) *) &(x))
#define WRITE_ONCE(x, v) (*(volatile __typeof__(x) *) &(x) = (v))
/* arm64's 32-bit smp_store_release (asm/barrier.h). */
static inline void smp_store_release(unsigned *p, unsigned v) {
	__asm__ volatile("stlr %w1, %0" : "=Q" (*p) : "r" (v) : "memory");
}

/* io_cqe_cache_refill's count of free slots, from io_cqring_queued. */
static unsigned cq_free(void) {
	int diff = (int) (cached_cq_tail - READ_ONCE(rings.cq_head));
	unsigned queued = diff >= 0 ? ((unsigned) diff < cq_entries ? (unsigned) diff : cq_entries) : 0;
	return cq_entries - queued;
}

static int post(int res) {
	struct io_uring_cqe *cqe;

	if (!cq_free())
		return 0;
	cqe = &cqes[cached_cq_tail & (cq_entries - 1)];
	cached_cq_tail++;
	WRITE_ONCE(cqe->user_data, (__u64) res);
	WRITE_ONCE(cqe->res, res);
	WRITE_ONCE(cqe->flags, 0);
	/* io_commit_cqring */
#ifdef NO_KERNEL_RELEASE
	WRITE_ONCE(rings.cq_tail, cached_cq_tail);
#else
	smp_store_release(&rings.cq_tail, cached_cq_tail);
#endif
	return 1;
}

void t1(void) {
	if (post(1))
		post(2);
}
