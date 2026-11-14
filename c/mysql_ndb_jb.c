/* cflags: -ffreestanding */
/* MySQL NDB's job buffers (storage/ndb/src/kernel/vm/mt.cpp, GPL-2.0
   with the Universal FOSS Exception, see mysql/LICENSE): a data node
   thread's prio A queue, filled by another thread and drained by its
   owner, as in trunk (November 2026) and, with -DFIX and -DOLD, at commit
   145987997 (April 2022, Bug#33650674 "Missing memory barriers - Apple
   M1") and just before it.

   A queue is a ring of job buffers. A producer, holding the queue's
   write lock, copies a signal into the current buffer and publishes it
   with publish_position(): wmb(), then the buffer's new m_len. When the
   buffer is nearly full, check_next_index_position() puts a fresh buffer
   (m_len 0) in the next slot and, after a wmb(), advances
   m_write_index. The consumer takes no lock. read_jba_state() loads
   m_write_index and then the current buffer's m_len into its
   thread-local read state, and after an rmb() execute_signals() runs the
   signals up to that m_len. When it reaches the end of a buffer that
   isn't the last one, it moves to the next buffer, loads that buffer's
   m_len, and goes on executing.

   On aarch64 mb(), rmb() and wmb() are all a seq_cst fence (DMB ISH),
   and read_barrier_depends() is empty. NDB_HAVE_XCNG is not defined, so
   wakeup_all() is a pthread mutex: an acquire compare-and-swap and a
   release exchange.

   Before 145987997, an earlier workaround (bug#3589359) ran an rmb()
   before every signal on aarch64. 145987997 removed it as too intrusive
   and added an rmb() where run_job_buffers() reads the prio A state
   again inside its loop, which had none. -DOLD is the code before
   145987997, and -DOLD -DNO_WORKAROUND also leaves out that per-signal
   rmb(), as before bug#3589359. Trunk keeps 145987997's barriers, makes
   read_jba_state()'s rmb() conditional on a new m_write_index, and
   calls wakeup_all() after moving to a new buffer.

   P1 sends two signals: (1, 11) and then (2, 12). By default the
   buffers hold one signal each, so the second signal goes into the next
   buffer. -DSAME makes them big enough for both. P0 runs the prio A part
   of run_job_buffers() once, as when the prio B queues are empty (in
   -DOLD and -DFIX it then reads the prio A state once before the loop
   and again in it), and records each signal it executes. The buffers
   start zeroed, standing for a recycled buffer's old contents. The
   condition is a signal executed with contents other than the ones
   sent, in order.

   The results: before bug#3589359 (-DOLD -DNO_WORKAROUND) both
   scenarios fail on ARM only, and the default one can also fault on a
   stale null m_buffers[] slot. The per-signal rmb() (-DOLD) closes both.
   145987997 (-DFIX) and trunk close -DSAME but not the default: moving
   to the next buffer loads its m_len and then its signals with nothing
   ordering the two loads, so a buffer the producer is still filling can
   show a new m_len with old contents. -DNEXT_RMB adds an rmb() after
   that m_len load (once per buffer), which closes it.

   At -O0 without LSE, clang compiles the release exchange in
   wakeup_all() as an LDAXR/STLXR loop, and the compare-and-swap's STLXR
   before that LDAXR orders the two loads, so trunk holds there. NDB's
   wakeup_all() calls glibc's pthread_mutex_lock() and unlock(), which
   are LDAXR/STXR and LDXR/STLXR (or CASA and SWPL) at any NDB -O level,
   as at -O1 and up here.

   Left out: the write lock (one producer), the prio B queues (which
   execute_signals() drains the same way), signal sections, prefetches,
   watchdog counters, congestion and wakeups of sleeping threads.
   release_buffer() only touches thread-local state, so it is empty
   here. */

typedef unsigned Uint32;

#define mb() __atomic_thread_fence(__ATOMIC_SEQ_CST)
#define rmb() mb()
#define wmb() mb()
#define read_barrier_depends() do {} while (0)

#ifdef SAME
#define JB_SIZE 8 /* thr_job_buffer::SIZE */
#else
#define JB_SIZE 4
#endif
#define QUEUE_SIZE 4 /* thr_job_queue::SIZE */
/* MAX_SIGNAL_SIZE * MAX_SIGNALS_BEFORE_FLUSH_OTHER, scaled down: the
   room a buffer must have left for the producer to keep using it. */
#define MAX_SIZE 3
#define SIGLEN 2

struct thr_job_buffer {
	Uint32 m_len;
	Uint32 m_prioa;
	Uint32 m_data[JB_SIZE];
};

struct thr_job_queue {
	unsigned m_read_index;
	unsigned m_write_index;
	unsigned m_cached_read_index;
	struct thr_job_buffer *m_current_write_buffer;
	unsigned m_current_write_buffer_len;
	unsigned m_size;
	struct thr_job_buffer *m_buffers[QUEUE_SIZE];
};

struct thr_jb_read_state {
	Uint32 m_read_index;
	Uint32 m_read_pos;
	struct thr_job_buffer *m_read_buffer;
	Uint32 m_read_end;
	Uint32 m_write_index;
};

static int is_empty(const struct thr_jb_read_state *r) {
	return (r->m_read_index == r->m_write_index) && (r->m_read_pos >= r->m_read_end);
}

struct thr_job_buffer buffers[3];
struct thr_job_queue jba;
struct thr_jb_read_state jba_read_state;
int job_buffer_full_called;

/* The mutex in wakeup_all(), with nobody waiting. */
int congestion_mutex;
int need_wakeup;

/* Results, read by the litmus condition. */
int count, bad;

/* Producer side, the same in all three versions. */

static inline void publish_position(struct thr_job_buffer *write_buffer, Uint32 write_pos) {
	wmb();
	write_buffer->m_len = write_pos;
}

static void check_next_index_position(struct thr_job_queue *q, Uint32 *write_pos,
		struct thr_job_buffer *write_buffer, struct thr_job_buffer *new_buffer, int prioa) {
	(void) write_buffer;
	const unsigned queue_size = q->m_size;
	Uint32 write_index = q->m_write_index;
	write_index = (write_index + 1) & (queue_size - 1);
	if (write_index == q->m_cached_read_index) {
		Uint32 read_index = q->m_read_index;
		if (write_index == read_index)
			job_buffer_full_called = 1;
		else
			q->m_cached_read_index = read_index;
	}
	new_buffer->m_len = 0;
	new_buffer->m_prioa = prioa;
	q->m_buffers[write_index] = new_buffer;
	wmb();
	q->m_write_index = write_index;
	q->m_current_write_buffer = new_buffer;
	q->m_current_write_buffer_len = 0;
	*write_pos = 0;
}

static inline int publish_signal(struct thr_job_queue *q, Uint32 write_pos,
		struct thr_job_buffer *write_buffer, struct thr_job_buffer *new_buffer, int prioa) {
	publish_position(write_buffer, write_pos);
	if (write_pos + MAX_SIZE > JB_SIZE) {
		check_next_index_position(q, &write_pos, write_buffer, new_buffer, prioa);
		return 1;
	}
	return 0;
}

/* copy_signal(), for a signal of a number and one word of data. */
static inline Uint32 copy_signal(Uint32 *dst, Uint32 n) {
	dst[0] = n;
	dst[1] = 10 + n;
	return SIGLEN;
}

static int insert_signal(struct thr_job_queue *q, Uint32 n, struct thr_job_buffer *new_buffer) {
	struct thr_job_buffer *write_buffer = q->m_current_write_buffer;
	Uint32 write_pos = q->m_current_write_buffer_len;
	const Uint32 siglen = copy_signal(write_buffer->m_data + write_pos, n);
	write_pos += siglen;
	write_pos = (write_pos + 1) & ~((Uint32) 1);
	q->m_current_write_buffer_len = write_pos;
	return publish_signal(q, write_pos, write_buffer, new_buffer, 1);
}

/* Consumer side. */

static inline int read_jba_state(void) {
	struct thr_jb_read_state *r = &jba_read_state;
#if defined(OLD)
	const Uint32 read_index = r->m_read_index;
	r->m_write_index = jba.m_write_index;
	read_barrier_depends();
	r->m_read_end = jba.m_buffers[read_index]->m_len;
#elif defined(FIX)
	r->m_write_index = jba.m_write_index;
	read_barrier_depends();
	r->m_read_end = r->m_read_buffer->m_len;
#else
	const Uint32 new_write_index = jba.m_write_index;
	if (r->m_write_index != new_write_index) {
		r->m_write_index = new_write_index;
		rmb();
	}
	r->m_read_end = r->m_read_buffer->m_len;
#endif
	return is_empty(r);
}

static void release_buffer(struct thr_job_buffer *jb) {
	(void) jb;
}

#if !defined(OLD) && !defined(FIX)
static inline int wakeup_all(void) {
	int expected = 0;
	while (!__atomic_compare_exchange_n(&congestion_mutex, &expected, 1, 0,
			__ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
		expected = 0;
	if (need_wakeup)
		need_wakeup = 0;
	__atomic_exchange_n(&congestion_mutex, 0, __ATOMIC_RELEASE);
	return 0;
}
#endif

/* Executing a signal records it. */
static void execute(const Uint32 *s) {
	count++;
	if (s[0] != (Uint32) count || s[1] != 10 + (Uint32) count)
		bad = 1;
}

static Uint32 execute_signals(struct thr_job_queue *q, struct thr_jb_read_state *r, Uint32 max_signals) {
	Uint32 num_signals;
	Uint32 read_index = r->m_read_index;
	Uint32 write_index = r->m_write_index;
	Uint32 read_pos = r->m_read_pos;
	Uint32 read_end = r->m_read_end;

	if (read_index == write_index && read_pos >= read_end)
		return 0;

	struct thr_job_buffer *read_buffer = r->m_read_buffer;

	for (num_signals = 0; num_signals < max_signals; num_signals++) {
		while (read_pos >= read_end) {
			if (read_index == write_index) {
				return num_signals;
			} else {
				/* Move to next buffer. */
				const unsigned queue_size = q->m_size;
				read_index = (read_index + 1) & (queue_size - 1);
				release_buffer(read_buffer);
				read_buffer = q->m_buffers[read_index];
				read_pos = 0;
				read_end = read_buffer->m_len;
#ifdef NEXT_RMB
				rmb();
#endif
				r->m_read_index = q->m_read_index = read_index;
				r->m_read_buffer = read_buffer;
				r->m_read_pos = read_pos;
				r->m_read_end = read_end;
#if !defined(OLD) && !defined(FIX)
				wakeup_all();
#endif
			}
		}
#if defined(OLD) && !defined(NO_WORKAROUND)
		rmb();
#endif
		const Uint32 *s = read_buffer->m_data + read_pos;
		read_pos += SIGLEN;
		read_pos = (read_pos + 1) & ~((Uint32) 1);
		r->m_read_pos = read_pos;
		execute(s);
	}
	return num_signals;
}

#define MAX_PRIOA (QUEUE_SIZE * JB_SIZE)

void setup(void) {
	jba.m_size = QUEUE_SIZE;
	jba.m_buffers[0] = &buffers[0];
	jba.m_current_write_buffer = &buffers[0];
	jba_read_state.m_read_buffer = &buffers[0];
}

void consumer(void) {
#if defined(OLD) || defined(FIX)
	/* run_job_buffers(), where read_all_jbb_state() found no prio B
	   signals, so the && goes on to read the prio A state. */
	if (read_jba_state())
		return;
	rmb();
	while (!read_jba_state()) {
#ifdef FIX
		rmb();
#endif
		execute_signals(&jba, &jba_read_state, MAX_PRIOA);
		break; /* no prio A signals sent by the signals executed */
	}
#else
	/* run_job_buffers() when read_all_jbb_state() finds no prio B
	   signals. */
	while (!read_jba_state()) {
		rmb();
		execute_signals(&jba, &jba_read_state, MAX_PRIOA);
		break;
	}
#endif
}

void producer(void) {
	int next = 1;
	if (insert_signal(&jba, 1, &buffers[next]))
		next++;
	insert_signal(&jba, 2, &buffers[next]);
}
