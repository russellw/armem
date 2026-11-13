/* cflags: -ffreestanding */
/* PostgreSQL's asynchronous IO with io_method=worker, as at commit
   e9a3615a5 (June 2025) and, with -DOLD, just before it: an IO handle's
   journey from the backend that issues the IO to the IO worker that
   performs it and back. The functions are copied from
   src/backend/storage/aio/aio.c, aio_callback.c and method_worker.c
   (PostgreSQL License, see postgres/LICENSE).

   Commit e9a3615a5 ("aio: Add missing memory barrier when waiting for IO
   handle") says that on some armv8 hardware (observed on Apple hardware)
   the IO worker's update of PgAioHandle->state could become visible
   before its update of ->distilled_result, "leading to rather confusing
   assertion failures", most often in WaitReadBuffers(). It reproduced
   within seconds with io_max_concurrency=1 and io_combine_limit=1, so
   that a backend always has to wait for a free handle. The fix adds
   pg_read_barrier() in pgaio_io_wait_for_free() between the two checks
   of a handle's state for PGAIO_HS_COMPLETED_SHARED and the
   pgaio_io_reclaim() that follows, which runs the local completion
   callbacks on ->distilled_result. The other paths that reclaim a handle
   already read the state in pgaio_io_was_recycled(), which has the
   barrier. The rest of the commit adds assertions and comments.

   The worker publishes the result with pgaio_io_update_state(), which
   puts pg_write_barrier() (a release fence, DMB ISH) before the store of
   the state. pg_read_barrier() is an acquire fence (DMB ISHLD).

   backend() is a backend with io_max_concurrency=1. It acquires the
   handle, stages a read and submits it to the worker's queue, and then,
   to start a second IO, acquires a handle again, which waits for the
   first IO and reclaims its handle. worker() waits until the queue is not
   empty (as the worker sleeps on its latch until the backend sets it),
   takes the IO from the queue under the queue's lock, performs it
   (reading 8192 bytes) and completes it. The condition is that the
   result returned to the backend (in the PgAioReturn that ReadBuffers
   passes, which WaitReadBuffers then checks) is not OK with 8192 bytes.

   Left out: debug messages, resource owners, interrupts (HOLD_INTERRUPTS
   and critical sections only count, in a counter per process, so here
   per thread), injection points, Asserts (as in a production build),
   condition variables (the waits spin), latches, batch mode, synchronous
   execution, the reopening of files, and the IO's callbacks, of which
   there are none. The handle has only the fields used; the fields before
   ->distilled_result have the same bitfields and types. MyProcNumber is
   a macro, set for the functions each thread runs.

   The LWLock is a spinlock (an acquire exchange and a release store).
   Each turn of its exchange that finds the lock held writes back the 1
   it read, so the tests bound that with --stutter 1. */

typedef unsigned char uint8;
typedef unsigned short uint16;
typedef unsigned int uint32;
typedef unsigned long uint64;
typedef int int32;
typedef _Bool bool;
#define true 1
#define false 0
#define NULL ((void *)0)
#define UINT32_MAX 0xffffffffu

#define Assert(x) ((void)0)
#define pg_read_barrier() __atomic_thread_fence(__ATOMIC_ACQUIRE)
#define pg_write_barrier() __atomic_thread_fence(__ATOMIC_RELEASE)

/* Per process in PostgreSQL, so per thread here. */
int InterruptHoldoffCount[2], CritSectionCount[2];
#define HOLD_INTERRUPTS() (InterruptHoldoffCount[MyProcNumber]++)
#define RESUME_INTERRUPTS() (InterruptHoldoffCount[MyProcNumber]--)
#define START_CRIT_SECTION() (CritSectionCount[MyProcNumber]++)
#define END_CRIT_SECTION() (CritSectionCount[MyProcNumber]--)
#define pgaio_debug_io(...) ((void)0)
#define pgaio_debug(...) ((void)0)
#define INJECTION_POINT(...) ((void)0)
int errors;
#define elog(...) (errors++)
#define ereport(...) (errors++)
#define IsUnderPostmaster true
#define ConditionVariablePrepareToSleep(cv) ((void)0)
#define ConditionVariableSleep(cv, ev) ((void)0)
#define ConditionVariableCancelSleep() ((void)0)
#define ConditionVariableBroadcast(cv) ((void)0)

/* lib/ilist.h: a doubly linked list with a count. */
typedef struct dlist_node { struct dlist_node *prev, *next; } dlist_node;
typedef struct dclist_head { dlist_node head; uint32 count; } dclist_head;
static void dclist_init(dclist_head *h)
{ h->head.prev = h->head.next = &h->head; h->count = 0; }
static bool dclist_is_empty(dclist_head *h) { return h->count == 0; }
static uint32 dclist_count(dclist_head *h) { return h->count; }
static void dclist_push_head(dclist_head *h, dlist_node *n)
{ n->next = h->head.next; n->prev = &h->head; n->next->prev = n; h->head.next = n; h->count++; }
static void dclist_push_tail(dclist_head *h, dlist_node *n)
{ n->prev = h->head.prev; n->next = &h->head; n->prev->next = n; h->head.prev = n; h->count++; }
static void dclist_delete_from(dclist_head *h, dlist_node *n)
{ n->prev->next = n->next; n->next->prev = n->prev; h->count--; }
static dlist_node *dclist_pop_head_node(dclist_head *h)
{ dlist_node *n = h->head.next; dclist_delete_from(h, n); return n; }
#define dclist_container(type, member, ptr) ((type *)((char *)(ptr) - __builtin_offsetof(type, member)))
#define dclist_head_element(type, member, h) dclist_container(type, member, (h)->head.next)

/* storage/aio_types.h and aio_internal.h */
typedef enum PgAioResultStatus
{
	PGAIO_RS_UNKNOWN,			/* not yet completed / uninitialized */
	PGAIO_RS_OK,
	PGAIO_RS_PARTIAL,			/* did not fully succeed, no warning/error */
	PGAIO_RS_WARNING,			/* [partially] succeeded, with a warning */
	PGAIO_RS_ERROR,				/* failed entirely */
} PgAioResultStatus;

typedef struct PgAioResult
{
	uint32		id:6;
	uint32		status:3;
	uint32		error_data:23;
	int32		result;
} PgAioResult;

typedef struct PgAioReturn
{
	PgAioResult result;
	int			target_data;
} PgAioReturn;

typedef enum PgAioHandleState
{
	PGAIO_HS_IDLE = 0,
	PGAIO_HS_HANDED_OUT,
	PGAIO_HS_DEFINED,
	PGAIO_HS_STAGED,
	PGAIO_HS_SUBMITTED,
	PGAIO_HS_COMPLETED_IO,
	PGAIO_HS_COMPLETED_SHARED,
	PGAIO_HS_COMPLETED_LOCAL,
} PgAioHandleState;

typedef enum PgAioOp { PGAIO_OP_INVALID = 0, PGAIO_OP_READV, PGAIO_OP_WRITEV } PgAioOp;
typedef enum PgAioTargetID { PGAIO_TID_INVALID = 0, PGAIO_TID_SMGR } PgAioTargetID;
enum { PGAIO_HCB_INVALID = 0 };
#define PGAIO_HANDLE_MAX_CALLBACKS 4
#define PGAIO_SUBMIT_BATCH_SIZE 4

typedef struct PgAioHandle
{
	PgAioHandleState state:8;
	PgAioTargetID target:8;
	PgAioOp		op:8;
	uint8		flags;
	uint8		num_callbacks;
	uint8		callbacks[PGAIO_HANDLE_MAX_CALLBACKS];
	uint8		callbacks_data[PGAIO_HANDLE_MAX_CALLBACKS];
	uint8		handle_data_len;
	int32		owner_procno;
	int32		result;
	dlist_node	node;
	void	   *resowner;
	uint64		generation;
	int			cv;
	PgAioResult distilled_result;
	PgAioReturn *report_return;
	int			target_data;
} PgAioHandle;

typedef struct PgAioBackend
{
	uint32		io_handle_off;
	dclist_head idle_ios;
	PgAioHandle *handed_out_io;
	bool		in_batchmode;
	uint16		num_staged_ios;
	PgAioHandle *staged_ios[PGAIO_SUBMIT_BATCH_SIZE];
	dclist_head in_flight_ios;
} PgAioBackend;

typedef struct PgAioCtl { PgAioHandle *io_handles; } PgAioCtl;

#define BACKEND_PROCNO 0
#define WORKER_PROCNO 1
int io_max_concurrency = 1;
PgAioHandle handles[1];
PgAioCtl ctl = { handles };
PgAioCtl *pgaio_ctl = &ctl;
PgAioBackend backend0;
PgAioBackend *pgaio_my_backend = &backend0;

static uint32 pgaio_io_get_id(PgAioHandle *ioh) { return ioh - pgaio_ctl->io_handles; }

/* The worker's submission queue (method_worker.c) and its LWLock. */
typedef struct AioWorkerSubmissionQueue
{
	uint32		size;
	uint32		head;
	uint32		tail;
	uint32		ios[4];
} AioWorkerSubmissionQueue;
AioWorkerSubmissionQueue queue0 = { 4 };
AioWorkerSubmissionQueue *io_worker_submission_queue = &queue0;

int AioWorkerSubmissionQueueLock;
#define LW_EXCLUSIVE 0
static void LWLockAcquire(int *lock, int mode)
{
	while (__atomic_exchange_n(lock, 1, __ATOMIC_ACQUIRE) != 0)
		;
}
static void LWLockRelease(int *lock) { __atomic_store_n(lock, 0, __ATOMIC_RELEASE); }

/* ---- aio.c ---- */

static inline void pgaio_io_update_state(PgAioHandle *ioh, PgAioHandleState new_state);
static void pgaio_io_reclaim(PgAioHandle *ioh);
static void pgaio_io_wait_for_free(void);
static void pgaio_io_wait(PgAioHandle *ioh, uint64 ref_generation);
static PgAioResult pgaio_io_call_complete_local(PgAioHandle *ioh);
static void pgaio_io_call_complete_shared(PgAioHandle *ioh);
static void pgaio_submit_staged(void);

static inline void
pgaio_io_update_state(PgAioHandle *ioh, PgAioHandleState new_state)
{
	/*
	 * Ensure the changes signified by the new state are visible before the
	 * new state becomes visible.
	 */
	pg_write_barrier();

	ioh->state = new_state;
}

/* The backend's side. */
#define MyProcNumber BACKEND_PROCNO

static PgAioHandle *
pgaio_io_acquire_nb(void *resowner, PgAioReturn *ret)
{
	PgAioHandle *ioh = NULL;

	if (pgaio_my_backend->num_staged_ios >= PGAIO_SUBMIT_BATCH_SIZE)
	{
		Assert(pgaio_my_backend->num_staged_ios == PGAIO_SUBMIT_BATCH_SIZE);
		pgaio_submit_staged();
	}

	if (pgaio_my_backend->handed_out_io)
		elog(ERROR, "API violation: Only one IO can be handed out");

	HOLD_INTERRUPTS();

	if (!dclist_is_empty(&pgaio_my_backend->idle_ios))
	{
		dlist_node *ion = dclist_pop_head_node(&pgaio_my_backend->idle_ios);

		ioh = dclist_container(PgAioHandle, node, ion);

		Assert(ioh->state == PGAIO_HS_IDLE);
		Assert(ioh->owner_procno == MyProcNumber);

		pgaio_io_update_state(ioh, PGAIO_HS_HANDED_OUT);
		pgaio_my_backend->handed_out_io = ioh;

		if (ret)
		{
			ioh->report_return = ret;
			ret->result.status = PGAIO_RS_UNKNOWN;
		}
	}

	RESUME_INTERRUPTS();

	return ioh;
}

static PgAioHandle *
pgaio_io_acquire(void *resowner, PgAioReturn *ret)
{
	PgAioHandle *h;

	while (true)
	{
		h = pgaio_io_acquire_nb(resowner, ret);

		if (h != NULL)
			return h;

		/*
		 * Evidently all handles by this backend are in use. Just wait for
		 * some to complete.
		 */
		pgaio_io_wait_for_free();
	}
}

static void
pgaio_io_prepare_submit(PgAioHandle *ioh)
{
	pgaio_io_update_state(ioh, PGAIO_HS_SUBMITTED);

	dclist_push_tail(&pgaio_my_backend->in_flight_ios, &ioh->node);
}

/* pgaio_io_stage, without stage callbacks or synchronous execution. */
static void
pgaio_io_stage(PgAioHandle *ioh, PgAioOp op)
{
	HOLD_INTERRUPTS();

	ioh->op = op;
	ioh->result = 0;

	pgaio_io_update_state(ioh, PGAIO_HS_DEFINED);

	/* allow a new IO to be staged */
	pgaio_my_backend->handed_out_io = NULL;

	pgaio_io_update_state(ioh, PGAIO_HS_STAGED);

	pgaio_my_backend->staged_ios[pgaio_my_backend->num_staged_ios++] = ioh;

	if (!pgaio_my_backend->in_batchmode)
		pgaio_submit_staged();

	RESUME_INTERRUPTS();
}

/* method_worker.c */
static bool
pgaio_worker_submission_queue_insert(PgAioHandle *ioh)
{
	AioWorkerSubmissionQueue *queue;
	uint32		new_head;

	queue = io_worker_submission_queue;
	new_head = (queue->head + 1) & (queue->size - 1);
	if (new_head == queue->tail)
		return false;			/* full */

	queue->ios[queue->head] = pgaio_io_get_id(ioh);
	queue->head = new_head;

	return true;
}

/* pgaio_worker_submit and pgaio_worker_submit_internal, without the
   fallback to synchronous IO (the queue is never full) and the latch. */
static int
pgaio_worker_submit(uint16 num_staged_ios, PgAioHandle **staged_ios)
{
	for (int i = 0; i < num_staged_ios; i++)
	{
		PgAioHandle *ioh = staged_ios[i];

		pgaio_io_prepare_submit(ioh);
	}

	LWLockAcquire(&AioWorkerSubmissionQueueLock, LW_EXCLUSIVE);
	for (int i = 0; i < num_staged_ios; ++i)
	{
		if (!pgaio_worker_submission_queue_insert(staged_ios[i]))
			errors++;
	}
	LWLockRelease(&AioWorkerSubmissionQueueLock);

	return num_staged_ios;
}

static void
pgaio_submit_staged(void)
{
	if (pgaio_my_backend->num_staged_ios == 0)
		return;

	START_CRIT_SECTION();

	pgaio_worker_submit(pgaio_my_backend->num_staged_ios,
						pgaio_my_backend->staged_ios);

	END_CRIT_SECTION();

	pgaio_my_backend->num_staged_ios = 0;
}

static bool
pgaio_io_was_recycled(PgAioHandle *ioh, uint64 ref_generation, PgAioHandleState *state)
{
	*state = ioh->state;
	pg_read_barrier();

	return ioh->generation != ref_generation;
}

/* pgaio_io_wait. With io_method=worker there is no wait_one. */
static void
pgaio_io_wait(PgAioHandle *ioh, uint64 ref_generation)
{
	PgAioHandleState state;
	bool		am_owner;

	am_owner = ioh->owner_procno == MyProcNumber;

	if (pgaio_io_was_recycled(ioh, ref_generation, &state))
		return;

	if (am_owner)
	{
		if (state != PGAIO_HS_SUBMITTED
			&& state != PGAIO_HS_COMPLETED_IO
			&& state != PGAIO_HS_COMPLETED_SHARED
			&& state != PGAIO_HS_COMPLETED_LOCAL)
		{
			elog(PANIC, "waiting for own IO %d in wrong state: %s",
				 pgaio_io_get_id(ioh), pgaio_io_get_state_name(ioh));
		}
	}

	while (true)
	{
		if (pgaio_io_was_recycled(ioh, ref_generation, &state))
			return;

		switch (state)
		{
			case PGAIO_HS_IDLE:
			case PGAIO_HS_HANDED_OUT:
				elog(ERROR, "IO in wrong state: %d", state);
				break;

			case PGAIO_HS_SUBMITTED:
			case PGAIO_HS_DEFINED:
			case PGAIO_HS_STAGED:
			case PGAIO_HS_COMPLETED_IO:
				/* ensure we're going to get woken up */
				ConditionVariablePrepareToSleep(&ioh->cv);

				while (!pgaio_io_was_recycled(ioh, ref_generation, &state))
				{
					if (state == PGAIO_HS_COMPLETED_SHARED ||
						state == PGAIO_HS_COMPLETED_LOCAL)
						break;
					ConditionVariableSleep(&ioh->cv, WAIT_EVENT_AIO_IO_COMPLETION);
				}

				ConditionVariableCancelSleep();
				break;

			case PGAIO_HS_COMPLETED_SHARED:
			case PGAIO_HS_COMPLETED_LOCAL:

				/*
				 * Note that no interrupts are processed between
				 * pgaio_io_was_recycled() and this check - that's important
				 * as otherwise an interrupt could have already reclaimed the
				 * handle.
				 */
				if (am_owner)
					pgaio_io_reclaim(ioh);
				return;
		}
	}
}

static void
pgaio_io_reclaim(PgAioHandle *ioh)
{
	/* This is only ok if it's our IO */
	Assert(ioh->owner_procno == MyProcNumber);
	Assert(ioh->state != PGAIO_HS_IDLE);

	/* see comment in function header */
	HOLD_INTERRUPTS();

	/*
	 * It's a bit ugly, but right now the easiest place to put the execution
	 * of local completion callbacks is this function, as we need to execute
	 * local callbacks just before reclaiming at multiple callsites.
	 */
	if (ioh->state == PGAIO_HS_COMPLETED_SHARED)
	{
		PgAioResult local_result;

		local_result = pgaio_io_call_complete_local(ioh);
		pgaio_io_update_state(ioh, PGAIO_HS_COMPLETED_LOCAL);

		if (ioh->report_return)
		{
			ioh->report_return->result = local_result;
			ioh->report_return->target_data = ioh->target_data;
		}
	}

	/* if the IO has been defined, it's on the in-flight list, remove */
	if (ioh->state != PGAIO_HS_HANDED_OUT)
		dclist_delete_from(&pgaio_my_backend->in_flight_ios, &ioh->node);

	/*
	 * Update generation & state first, before resetting the IO's fields,
	 * otherwise a concurrent "viewer" could think the fields are valid, even
	 * though they are being reset.  Increment the generation first, so that
	 * we can assert elsewhere that we never wait for an IDLE IO.  While it's
	 * a bit weird for the state to go backwards for a generation, it's OK
	 * here, as there cannot be references to the "reborn" IO yet.  Can't
	 * update both at once, so something has to give.
	 */
	ioh->generation++;
	pgaio_io_update_state(ioh, PGAIO_HS_IDLE);

	/* ensure the state update is visible before we reset fields */
	pg_write_barrier();

	ioh->op = PGAIO_OP_INVALID;
	ioh->target = PGAIO_TID_INVALID;
	ioh->flags = 0;
	ioh->num_callbacks = 0;
	ioh->handle_data_len = 0;
	ioh->report_return = NULL;
	ioh->result = 0;
	ioh->distilled_result.status = PGAIO_RS_UNKNOWN;

	/*
	 * We push the IO to the head of the idle IO list, that seems more cache
	 * efficient in cases where only a few IOs are used.
	 */
	dclist_push_head(&pgaio_my_backend->idle_ios, &ioh->node);

	RESUME_INTERRUPTS();
}

static void
pgaio_io_wait_for_free(void)
{
	int			reclaimed = 0;

	/*
	 * First check if any of our IOs actually have completed - when using
	 * worker, that'll often be the case. We could do so as part of the loop
	 * below, but that'd potentially lead us to wait for some IO submitted
	 * before.
	 */
	for (int i = 0; i < io_max_concurrency; i++)
	{
		PgAioHandle *ioh = &pgaio_ctl->io_handles[pgaio_my_backend->io_handle_off + i];

		if (ioh->state == PGAIO_HS_COMPLETED_SHARED)
		{
			/*
			 * Note that no interrupts are processed between the state check
			 * and the call to reclaim - that's important as otherwise an
			 * interrupt could have already reclaimed the handle.
			 *
			 * Need to ensure that there's no reordering, in the more common
			 * paths, where we wait for IO, that's done by
			 * pgaio_io_was_recycled().
			 */
#ifndef OLD
			pg_read_barrier();
#endif
			pgaio_io_reclaim(ioh);
			reclaimed++;
		}
	}

	if (reclaimed > 0)
		return;

	/*
	 * If we have any unsubmitted IOs, submit them now. We'll start waiting in
	 * a second, so it's better they're in flight. This also addresses the
	 * edge-case that all IOs are unsubmitted.
	 */
	if (pgaio_my_backend->num_staged_ios > 0)
		pgaio_submit_staged();

	/* possibly some IOs finished during submission */
	if (!dclist_is_empty(&pgaio_my_backend->idle_ios))
		return;

	if (dclist_count(&pgaio_my_backend->in_flight_ios) == 0)
		ereport(ERROR, "no free IOs despite no in-flight IOs");

	/*
	 * Wait for the oldest in-flight IO to complete.
	 *
	 * XXX: Reusing the general IO wait is suboptimal, we don't need to wait
	 * for that specific IO to complete, we just need *any* IO to complete.
	 */
	{
		PgAioHandle *ioh = dclist_head_element(PgAioHandle, node,
											   &pgaio_my_backend->in_flight_ios);
		uint64		generation = ioh->generation;

		switch (ioh->state)
		{
				/* should not be in in-flight list */
			case PGAIO_HS_IDLE:
			case PGAIO_HS_DEFINED:
			case PGAIO_HS_HANDED_OUT:
			case PGAIO_HS_STAGED:
			case PGAIO_HS_COMPLETED_LOCAL:
				elog(ERROR, "shouldn't get here with io:%d in state %d",
					 pgaio_io_get_id(ioh), ioh->state);
				break;

			case PGAIO_HS_COMPLETED_IO:
			case PGAIO_HS_SUBMITTED:
				pgaio_io_wait(ioh, generation);
				break;

			case PGAIO_HS_COMPLETED_SHARED:

				/*
				 * It's possible that another backend just finished this IO.
				 *
				 * Note that no interrupts are processed between the state
				 * check and the call to reclaim - that's important as
				 * otherwise an interrupt could have already reclaimed the
				 * handle.
				 *
				 * Need to ensure that there's no reordering, in the more
				 * common paths, where we wait for IO, that's done by
				 * pgaio_io_was_recycled().
				 */
#ifndef OLD
				pg_read_barrier();
#endif
				pgaio_io_reclaim(ioh);
				break;
		}

		if (dclist_count(&pgaio_my_backend->idle_ios) == 0)
			elog(PANIC, "no idle IO after waiting for IO to terminate");
		return;
	}
}

/* aio_callback.c, with no callbacks registered. */
static PgAioResult
pgaio_io_call_complete_local(PgAioHandle *ioh)
{
	PgAioResult result;

	START_CRIT_SECTION();

	/* start with distilled result from shared callback */
	result = ioh->distilled_result;

	END_CRIT_SECTION();

	return result;
}

#undef MyProcNumber

/* The worker's side. */
#define MyProcNumber WORKER_PROCNO

static void
pgaio_io_call_complete_shared(PgAioHandle *ioh)
{
	PgAioResult result;

	START_CRIT_SECTION();

	result.status = PGAIO_RS_OK;	/* low level IO is always considered OK */
	result.result = ioh->result;
	result.id = PGAIO_HCB_INVALID;
	result.error_data = 0;

	ioh->distilled_result = result;

	END_CRIT_SECTION();
}

static void
pgaio_io_process_completion(PgAioHandle *ioh, int result)
{
	Assert(ioh->state == PGAIO_HS_SUBMITTED);

	Assert(CritSectionCount > 0);

	ioh->result = result;

	pgaio_io_update_state(ioh, PGAIO_HS_COMPLETED_IO);

	INJECTION_POINT("aio-process-completion-before-shared", ioh);

	pgaio_io_call_complete_shared(ioh);

	pgaio_io_update_state(ioh, PGAIO_HS_COMPLETED_SHARED);

	/* condition variable broadcast ensures state is visible before wakeup */
	ConditionVariableBroadcast(&ioh->cv);

	/* contains call to pgaio_io_call_complete_local() */
	if (ioh->owner_procno == MyProcNumber)
		pgaio_io_reclaim(ioh);
}

static uint32
pgaio_worker_submission_queue_consume(void)
{
	AioWorkerSubmissionQueue *queue;
	uint32		result;

	queue = io_worker_submission_queue;
	if (queue->tail == queue->head)
		return UINT32_MAX;		/* empty */

	result = queue->ios[queue->tail];
	queue->tail = (queue->tail + 1) & (queue->size - 1);

	return result;
}

/* pgaio_io_perform_synchronously, for a read of one block. */
static void
pgaio_io_perform_synchronously(PgAioHandle *ioh)
{
	int			result = ioh->op == PGAIO_OP_READV ? 8192 : -1;

	START_CRIT_SECTION();
	pgaio_io_process_completion(ioh, result);
	END_CRIT_SECTION();
}

#undef MyProcNumber

void setup(void)
{
	dclist_init(&pgaio_my_backend->idle_ios);
	dclist_init(&pgaio_my_backend->in_flight_ios);
	handles[0].owner_procno = BACKEND_PROCNO;
	handles[0].generation = 1;
	dclist_push_head(&pgaio_my_backend->idle_ios, &handles[0].node);
}

PgAioReturn ret;
int status, bytes;

void backend(void)
{
	PgAioHandle *ioh = pgaio_io_acquire(NULL, &ret);

	ioh->target = PGAIO_TID_SMGR;
	pgaio_io_stage(ioh, PGAIO_OP_READV);

	/* The next IO waits for this one. */
	pgaio_io_acquire(NULL, NULL);

	status = ret.result.status;
	bytes = ret.result.result;
}

/* The loop of IoWorkerMain, once. */
void worker(void)
{
	uint32		io_index;

	/* WaitLatch until the backend's SetLatch */
	while (__atomic_load_n(&io_worker_submission_queue->head, __ATOMIC_RELAXED) == 0)
		;

	LWLockAcquire(&AioWorkerSubmissionQueueLock, LW_EXCLUSIVE);
	io_index = pgaio_worker_submission_queue_consume();
	LWLockRelease(&AioWorkerSubmissionQueueLock);

	if (io_index != UINT32_MAX)
		pgaio_io_perform_synchronously(&pgaio_ctl->io_handles[io_index]);
}
