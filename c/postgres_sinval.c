/* cflags: -ffreestanding */
/* PostgreSQL's shared invalidation queue (src/backend/storage/ipc/
   sinvaladt.c) as at commit df3978c234 (September 2026) and, with -DOLD,
   just before it: a backend that commits a catalog change appends
   invalidation messages to a circular buffer in shared memory and
   advances maxMsgNum, and every other backend reads the messages up to
   maxMsgNum when it next takes a lock (PostgreSQL License, see
   postgres/LICENSE).

   Commit df3978c234 ("Convert SISeg->maxMsgNum to an atomic variable")
   removes the spinlock msgnumLock, which the writer held to store
   maxMsgNum and the reader held to load it, and which existed only for
   its memory barriers. In its place the writer stores maxMsgNum with
   pg_atomic_write_membarrier_u32() and the reader loads it with
   pg_atomic_read_membarrier_u32(). The other accesses are made while
   holding SInvalWriteLock, so they can be unordered reads.

   Two orderings matter. The writer's copies of the messages into the
   buffer must be seen before its new maxMsgNum. And the reader stores
   its hasMessages = false before it loads maxMsgNum, while the writer
   stores maxMsgNum before it sets every backend's hasMessages = true:
   if the reader's store of false lands after the writer's true while
   its load returns the old maxMsgNum, the reader misses the message
   until another one arrives. That needs the reader's store ordered
   before its load, which a release or full barrier gives but an
   acquire alone does not (store buffering, so it would fail under TSO
   too).

   On aarch64 PostgreSQL builds its atomics on GCC builtins:
   pg_atomic_read_membarrier_u32() is __sync_fetch_and_add(ptr, 0), and
   pg_atomic_write_membarrier_u32() is __atomic_exchange_n(..., SEQ_CST).
   The spinlock's TAS is __sync_lock_test_and_set() (an acquire exchange,
   after a plain test while it spins) and S_UNLOCK is
   __sync_lock_release() (a release store).

   writer() inserts one relcache invalidation, then releases a lock that
   reader() then acquires (in PostgreSQL, the heavyweight lock on the
   relation, whose LockRelease and LockAcquire take the lock manager's
   partition LWLock). reader() reads its messages once while the writer
   runs, as a backend does whenever it takes a lock, and again once it
   holds the lock, when it must have the message. The condition is that
   it did not get exactly that one message, in all.

   -DACQUIRE_READ loads maxMsgNum with an acquire load instead, and
   -DRELEASE_WRITE stores it with a release store instead, which orders
   the messages before it but not the stores of hasMessages after it.

   Left out: SICleanupQueue (the queue never fills or reaches its cleanup
   threshold here, so it is never called), backend startup and exit,
   signals, and Asserts. Both LWLocks are spinlocks (an acquire exchange
   and a release store); each has one user here. */

typedef unsigned short uint16;
typedef unsigned int uint32;
typedef signed char int8;
typedef int int32;
typedef uint32 Oid;
typedef _Bool bool;
#define true 1
#define false 0
#define Min(a, b) ((a) < (b) ? (a) : (b))

int errors;

/* storage/sinval.h */
typedef struct
{
	int8		id;
	Oid			dbId;
	uint32		hashValue;
} SharedInvalCatcacheMsg;

#define SHAREDINVALRELCACHE_ID	(-2)

typedef struct
{
	int8		id;
	Oid			dbId;
	Oid			relId;
} SharedInvalRelcacheMsg;

typedef struct
{
	Oid			spcOid;
	Oid			dbOid;
	Oid			relNumber;
} RelFileLocator;

typedef struct
{
	int8		id;
	int8		backend_hi;
	uint16		backend_lo;
	RelFileLocator rlocator;
} SharedInvalSmgrMsg;

typedef union
{
	int8		id;
	SharedInvalCatcacheMsg cc;
	SharedInvalRelcacheMsg rc;
	SharedInvalSmgrMsg sm;
} SharedInvalidationMessage;

/* port/atomics: the aarch64 GCC builtins, as above */
typedef struct pg_atomic_uint32 { volatile uint32 value; } pg_atomic_uint32;

static inline uint32 pg_atomic_read_u32(volatile pg_atomic_uint32 *ptr)
{
	return ptr->value;
}

static inline uint32 pg_atomic_read_membarrier_u32(volatile pg_atomic_uint32 *ptr)
{
#ifdef ACQUIRE_READ
	return __atomic_load_n(&ptr->value, __ATOMIC_ACQUIRE);
#else
	return __sync_fetch_and_add(&ptr->value, 0);
#endif
}

static inline void pg_atomic_write_membarrier_u32(volatile pg_atomic_uint32 *ptr, uint32 val)
{
#ifdef RELEASE_WRITE
	__atomic_store_n(&ptr->value, val, __ATOMIC_RELEASE);
#else
	(void) __atomic_exchange_n(&ptr->value, val, __ATOMIC_SEQ_CST);
#endif
}

/* storage/s_lock.h for aarch64, and s_lock.c's spinning without the
   delays */
typedef int slock_t;
static inline int tas(volatile slock_t *lock) { return __sync_lock_test_and_set(lock, 1); }
#define TAS_SPIN(lock) (*(volatile slock_t *)(lock) ? 1 : tas(lock))
#define SpinLockAcquire(lock) do { if (tas(lock)) while (TAS_SPIN(lock)) ; } while (0)
#define SpinLockRelease(lock) __sync_lock_release(lock)

/* The LWLocks, as spinlocks */
int SInvalWriteLock, SInvalReadLock;
#define LW_EXCLUSIVE 0
#define LW_SHARED 1
static void LWLockAcquire(int *lock, int mode)
{
	while (__atomic_exchange_n(lock, 1, __ATOMIC_ACQUIRE) != 0)
		;
}
static void LWLockRelease(int *lock) { __atomic_store_n(lock, 0, __ATOMIC_RELEASE); }

/* ---- sinvaladt.c ---- */

#define MAXNUMMESSAGES 4096
#define MSGNUMWRAPAROUND (MAXNUMMESSAGES * 262144)
#define CLEANUP_MIN (MAXNUMMESSAGES / 2)
#define CLEANUP_QUANTUM (MAXNUMMESSAGES / 16)
#define SIG_THRESHOLD (MAXNUMMESSAGES / 2)
#define WRITE_QUANTUM 64

typedef struct ProcState
{
	int			procPid;
	uint32		nextMsgNum;
	bool		resetState;
	bool		signaled;
	bool		hasMessages;
	bool		sendOnly;
	uint32		nextLXID;
} ProcState;

#define NumProcStateSlots 2

typedef struct SISeg
{
	uint32		minMsgNum;
#ifdef OLD
	uint32		maxMsgNum;
#else
	pg_atomic_uint32 maxMsgNum;
#endif
	uint32		nextThreshold;

#ifdef OLD
	slock_t		msgnumLock;
#endif

	SharedInvalidationMessage buffer[MAXNUMMESSAGES];

	int			numProcs;
	int		   *pgprocnos;
	ProcState	procState[NumProcStateSlots];
} SISeg;

static SISeg seg;
static int pgprocnos[NumProcStateSlots];
static SISeg *shmInvalBuffer = &seg;

/* Never called here (see above). */
static void
SICleanupQueue(bool callerHasWriteLock, int minFree)
{
	errors++;
	seg.nextThreshold = 0x7fffffff;
}

void
SIInsertDataEntries(const SharedInvalidationMessage *data, int n)
{
	SISeg	   *segP = shmInvalBuffer;

	while (n > 0)
	{
		int			nthistime = Min(n, WRITE_QUANTUM);
		uint32		numMsgs;
		uint32		max;
		int			i;

		n -= nthistime;

		LWLockAcquire(&SInvalWriteLock, LW_EXCLUSIVE);

		for (;;)
		{
#ifdef OLD
			numMsgs = segP->maxMsgNum - segP->minMsgNum;
#else
			numMsgs = pg_atomic_read_u32(&segP->maxMsgNum) - segP->minMsgNum;
#endif
			if (numMsgs + nthistime > MAXNUMMESSAGES ||
				numMsgs >= segP->nextThreshold)
				SICleanupQueue(true, nthistime);
			else
				break;
		}

#ifdef OLD
		max = segP->maxMsgNum;
#else
		max = pg_atomic_read_u32(&segP->maxMsgNum);
#endif
		while (nthistime-- > 0)
		{
			segP->buffer[max % MAXNUMMESSAGES] = *data++;
			max++;
		}

#ifdef OLD
		/* Update current value of maxMsgNum using spinlock */
		SpinLockAcquire(&segP->msgnumLock);
		segP->maxMsgNum = max;
		SpinLockRelease(&segP->msgnumLock);
#else
		/* Update current value of maxMsgNum using barrier */
		pg_atomic_write_membarrier_u32(&segP->maxMsgNum, max);
#endif

		for (i = 0; i < segP->numProcs; i++)
		{
			ProcState  *stateP = &segP->procState[segP->pgprocnos[i]];

			stateP->hasMessages = true;
		}

		LWLockRelease(&SInvalWriteLock);
	}
}

#define READER_PROCNO 0
#define WRITER_PROCNO 1
#define MyProcNumber READER_PROCNO

int
SIGetDataEntries(SharedInvalidationMessage *data, int datasize)
{
	SISeg	   *segP;
	ProcState  *stateP;
	uint32		max;
	int			n;

	segP = shmInvalBuffer;
	stateP = &segP->procState[MyProcNumber];

	if (!stateP->hasMessages)
		return 0;

	LWLockAcquire(&SInvalReadLock, LW_SHARED);

	stateP->hasMessages = false;

#ifdef OLD
	/* Fetch current value of maxMsgNum using spinlock */
	SpinLockAcquire(&segP->msgnumLock);
	max = segP->maxMsgNum;
	SpinLockRelease(&segP->msgnumLock);
#else
	/* Fetch current value of maxMsgNum using barrier */
	max = pg_atomic_read_membarrier_u32(&segP->maxMsgNum);
#endif

	if (stateP->resetState)
	{
		stateP->nextMsgNum = max;
		stateP->resetState = false;
		stateP->signaled = false;
		LWLockRelease(&SInvalReadLock);
		return -1;
	}

	n = 0;
	while (n < datasize && stateP->nextMsgNum < max)
	{
		data[n++] = segP->buffer[stateP->nextMsgNum % MAXNUMMESSAGES];
		stateP->nextMsgNum++;
	}

	if (stateP->nextMsgNum >= max)
		stateP->signaled = false;
	else
		stateP->hasMessages = true;

	LWLockRelease(&SInvalReadLock);
	return n;
}

#undef MyProcNumber

static SharedInvalidationMessage relcache_msg(Oid relId)
{
	SharedInvalidationMessage msg;

	msg.rc.id = SHAREDINVALRELCACHE_ID;
	msg.rc.dbId = 5;
	msg.rc.relId = relId;
	return msg;
}

/* SharedInvalShmemInit and SharedInvalBackendInit for both backends,
   then an earlier invalidation that the reader has not read yet. */
void setup(void)
{
	SharedInvalidationMessage msg = relcache_msg(1000);

	seg.minMsgNum = 0;
	seg.nextThreshold = CLEANUP_MIN;
	seg.pgprocnos = pgprocnos;
	for (int i = 0; i < NumProcStateSlots; i++)
	{
		seg.procState[i].procPid = 100 + i;
		seg.procState[i].nextMsgNum = 0;
		seg.pgprocnos[seg.numProcs++] = i;
	}
	SIInsertDataEntries(&msg, 1);
}

/* The heavyweight lock the writer holds until it has sent its message. */
int relation_lock = 1;

int got, rel0, rel1, locked, bad;

#define MAXINVALMSGS 32

/* What the reader has received, kept in its registers and on its stack and
   stored once at the end, as stores to the locations the condition reads
   are shared writes, which multiply the orders of writes to explore. */
struct received { int got, rel0, rel1, bad; };

static void receive(SharedInvalidationMessage *messages, struct received *r)
{
	int			n = SIGetDataEntries(messages, MAXINVALMSGS);

	if (n < 0)
		r->bad++;
	for (int i = 0; i < n; i++, r->got++)
	{
		if (messages[i].id != SHAREDINVALRELCACHE_ID)
			r->bad++;
		else if (r->got == 0)
			r->rel0 = messages[i].rc.relId;
		else if (r->got == 1)
			r->rel1 = messages[i].rc.relId;
	}
}

void reader(void)
{
	SharedInvalidationMessage messages[MAXINVALMSGS];
	struct received r = { 0, 0, 0, 0 };

	receive(messages, &r);

	/* LockAcquire, tried once: the condition needs the lock taken. */
	if (__atomic_exchange_n(&relation_lock, 1, __ATOMIC_ACQUIRE) == 0)
	{
		locked = 1;

		/* AcceptInvalidationMessages */
		receive(messages, &r);
	}
	got = r.got;
	rel0 = r.rel0;
	rel1 = r.rel1;
	bad = r.bad;
}

void writer(void)
{
	SharedInvalidationMessage msg = relcache_msg(1234);

	SIInsertDataEntries(&msg, 1);

	/* LockRelease */
	__atomic_store_n(&relation_lock, 0, __ATOMIC_RELEASE);
}
