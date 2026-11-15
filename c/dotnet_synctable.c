/* cflags: -ffreestanding */
/* .NET CoreCLR's sync block table (src/coreclr/vm/syncblk.h and
   syncblk.cpp, object.inl, MIT, see dotnet/LICENSE): a thread asking
   for an object's hash code without a lock while another thread gives the
   object a sync block, as in main (November 2026) and, with -DOLD, just
   before a95fccd35 (October 2026, "Order sync-table lookups and SyncBlock
   publication", dotnet/runtime#135179, fixing #134252).

   An object's header word holds a thin lock, a hash code, or an index into
   g_pSyncTable, the process-wide array of SyncTableEntries. Each entry
   points to the object's SyncBlock. ObjHeader::GetSyncBlock() runs under
   the sync block cache lock. It takes a free SyncBlock and a new slot from
   NewSyncBlockSlot(). When no slot is free, NewSyncBlockSlot() calls
   Grow(), which copies the table into one twice the size and publishes it
   with InterlockedExchangeT(&g_pSyncTable, ...), keeping the old table
   until the next GC. Then, holding the header's spin lock,
   GetSyncBlock() moves the header's hash code into the SyncBlock, stores
   the SyncBlock in the table entry, and puts the index into the header
   with an InterlockedCompareExchange (SetIndex()).

   Object::TryGetHashCode() takes no lock. It reads the header, and if that
   holds a sync block index it calls PassiveGetSyncBlock(), which reads the
   header again and then g_pSyncTable[index].m_SyncBlock. Before a95fccd35
   both header reads were plain (Volatile<DWORD>::LoadWithoutBarrier()),
   and the load of g_pSyncTable does not depend on the header, so on ARM
   it can be satisfied first. The lookup then pairs the new index with the
   old table, which has no entry there. a95fccd35 reads the header with
   acquire (GetBitsAcquire(), an LDAR) in PassiveGetSyncBlock() and
   publishes the entry with VolatileStore(), a release store.

   With clang, the PAL's Interlocked functions are the __sync and __atomic
   builtins with no added barrier (PAL_InterlockedOperationBarrier() is
   empty under clang): LDAXR/STLXR loops, or with -march=armv8.1-a, as
   when the runtime finds LSE atomics, CASAL, SWPAL and LDCLRAL.

   The object starts with hash code 5 in its header and the table full:
   two entries, of which entry 1 belongs to another object. P1 runs
   GetSyncBlock() for the object, as Monitor.Enter() does for a hashed
   object, so it grows the table to four entries and takes index 2. The
   old table is followed by heap memory that happens to hold a pointer to
   a stale SyncBlock with hash code 7 (or 0 with -DZERO_HEAP, which the
   lookup returns as "no hash code"). P0 runs TryGetHashCode(), as
   RuntimeHelpers.TryGetHashCode() and ConditionalWeakTable do. The
   object's hash code never changes, so the condition is P0 returning
   anything but 5.

   -DNO_GROW leaves a free slot in the table instead (the free list holds
   index 1, the table has three entries), so the table isn't replaced.
   -DPLAIN_ENTRY keeps main's reader but stores the entry plainly, as
   before a95fccd35.

   The results: before a95fccd35 (-DOLD) the lookup returns the wrong
   hash code on ARM only, with or without LSE. With -DNO_GROW it holds,
   since the entry's address depends on the index read from the header.
   main holds in both, and so does -DPLAIN_ENTRY: the header's
   compare-and-swap (STLXR or CASAL) already orders the entry store before
   the index, so the acquire in the reader is the half of the fix that
   matters. Each verdict holds at -O0 to -O2 and -Os.

   Left out: the cache lock (one thread takes it here), the free list of
   SyncBlocks (P1 takes a fresh one), the card table bit, the thin lock
   transfer (the header holds a hash code), contracts and debug checks.
   Only TryGetHashCode() reads the table here; GetSyncBlock()'s own fast
   path goes through the same PassiveGetSyncBlock(). */

typedef unsigned DWORD;
typedef int LONG;
typedef unsigned long size_t;

#define BIT_SBLK_SPIN_LOCK 0x10000000
#define BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX 0x08000000
#define BIT_SBLK_IS_HASHCODE 0x04000000
#define HASHCODE_BITS 26
#define MASK_HASHCODE ((1 << HASHCODE_BITS) - 1)
#define SYNCBLOCKINDEX_BITS 26
#define MASK_SYNCBLOCKINDEX ((1 << SYNCBLOCKINDEX_BITS) - 1)

#define HASH 5

/* For -O0 and the table copies. Like a libc memcpy and memset, these
   copy or clear an aligned word with one store. */
__attribute__((no_builtin)) void *memcpy(void *d, const void *s, size_t n) {
	unsigned long *dw = d;
	const unsigned long *sw = s;
	for (; n; n -= 8)
		*dw++ = *sw++;
	return d;
}

__attribute__((no_builtin)) void *memset(void *d, int c, size_t n) {
	unsigned long *dw = d;
	(void) c;
	for (; n; n -= 8)
		*dw++ = 0;
	return d;
}

/* The PAL's Interlocked functions under clang. */
static inline LONG InterlockedCompareExchange(volatile LONG *d, LONG x, LONG c) {
	return __sync_val_compare_and_swap(d, c, x);
}

static inline LONG InterlockedAnd(volatile LONG *d, LONG v) {
	return __sync_fetch_and_and(d, v);
}

#define InterlockedExchangeT(t, v) __atomic_exchange_n(t, v, __ATOMIC_ACQ_REL)

/* VolatileLoad() and VolatileStore() on arm64 with GCC or clang. */
#define VolatileLoad(p) __atomic_load_n(p, __ATOMIC_ACQUIRE)
#define VolatileStore(p, v) __atomic_store_n(p, v, __ATOMIC_RELEASE)
#define LoadWithoutBarrier(p) (*(volatile DWORD *) (p))

struct SyncBlock {
	DWORD m_dwSyncIndex;
	DWORD m_dwHashCode;
};

struct Object;

struct SyncTableEntry {
	struct SyncBlock *m_SyncBlock;
	struct Object *m_Object;
};

/* The object: its header word, then the object itself. */
struct ObjHeader {
	DWORD m_SyncBlockValue;
	DWORD m_alignpad;
	struct Object *obj;
} hdr;

struct SyncTableEntry *g_pSyncTable;

/* The sync block cache's state. */
DWORD m_FreeSyncTableIndex;
DWORD m_SyncTableSize;
size_t m_FreeSyncTableList;
struct SyncTableEntry *m_OldSyncTables;

/* The tables as allocated by new SyncTableEntry[n]: the first two
   entries of old_heap are the first table, and the rest is whatever the
   heap holds after it. */
struct SyncTableEntry old_heap[4];
struct SyncTableEntry new_table[4];
struct SyncBlock other_sb, new_sb, stale_sb;
struct Object *other_obj = (struct Object *) &other_sb;

/* Results, read by the litmus condition. */
int result, bad;

static DWORD GetBits(struct ObjHeader *h) {
	return LoadWithoutBarrier(&h->m_SyncBlockValue);
}

static DWORD GetBitsAcquire(struct ObjHeader *h) {
	return VolatileLoad(&h->m_SyncBlockValue);
}

static DWORD GetHeaderSyncBlockIndex(struct ObjHeader *h) {
	DWORD value = LoadWithoutBarrier(&h->m_SyncBlockValue);
	if ((value & (BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX | BIT_SBLK_IS_HASHCODE)) != BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX)
		return 0;
	return value & MASK_SYNCBLOCKINDEX;
}

static struct SyncBlock *PassiveGetSyncBlock(struct ObjHeader *h) {
#ifdef OLD
	return g_pSyncTable[(int) GetHeaderSyncBlockIndex(h)].m_SyncBlock;
#else
	DWORD value = GetBitsAcquire(h);
	if ((value & (BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX | BIT_SBLK_IS_HASHCODE)) != BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX)
		return 0;
	DWORD index = value & MASK_SYNCBLOCKINDEX;
	return g_pSyncTable[(int) index].m_SyncBlock;
#endif
}

static void SetIndex(struct ObjHeader *h, DWORD indx) {
	LONG newValue;
	LONG oldValue;
	while (1) {
		oldValue = LoadWithoutBarrier(&h->m_SyncBlockValue);
		newValue = (indx |
			(oldValue & ~(BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX | BIT_SBLK_IS_HASHCODE | MASK_SYNCBLOCKINDEX)));
		if (InterlockedCompareExchange((LONG *) &h->m_SyncBlockValue, newValue, oldValue) == oldValue)
			return;
	}
}

/* The header's spin lock; nobody else holds it here. */
static void EnterSpinLock(struct ObjHeader *h) {
	while (1) {
		LONG curValue = LoadWithoutBarrier(&h->m_SyncBlockValue);
		if (!(curValue & BIT_SBLK_SPIN_LOCK)) {
			LONG newValue = curValue | BIT_SBLK_SPIN_LOCK;
			LONG result = InterlockedCompareExchange((LONG *) &h->m_SyncBlockValue, newValue, curValue);
			if (result == curValue)
				break;
		}
	}
}

static void ReleaseSpinLock(struct ObjHeader *h) {
	InterlockedAnd((LONG *) &h->m_SyncBlockValue, ~BIT_SBLK_SPIN_LOCK);
}

static DWORD SetHashCode(struct SyncBlock *sb, DWORD hashCode) {
	DWORD result = InterlockedCompareExchange((LONG *) &sb->m_dwHashCode, hashCode, 0);
	if (result == 0)
		return hashCode;
	return result;
}

static void Grow(void) {
	DWORD newSyncTableSize = m_SyncTableSize * 2;
	struct SyncTableEntry *newSyncTable = new_table;

	/* Chain the old table, to be freed at the next GC. */
	g_pSyncTable[0].m_Object = (struct Object *) m_OldSyncTables;
	m_OldSyncTables = g_pSyncTable;

	memset(newSyncTable, 0, newSyncTableSize * sizeof(struct SyncTableEntry));
	memcpy(newSyncTable, g_pSyncTable, m_SyncTableSize * sizeof(struct SyncTableEntry));

	InterlockedExchangeT(&g_pSyncTable, newSyncTable);

	m_FreeSyncTableIndex++;
	m_SyncTableSize = newSyncTableSize;
}

static DWORD NewSyncBlockSlot(struct Object *obj) {
	DWORD indexNewEntry;
	if (m_FreeSyncTableList) {
		indexNewEntry = (DWORD) (m_FreeSyncTableList >> 1);
		m_FreeSyncTableList = (size_t) g_pSyncTable[indexNewEntry].m_Object & ~1;
	} else if ((indexNewEntry = m_FreeSyncTableIndex) >= m_SyncTableSize) {
		Grow();
	} else {
		m_FreeSyncTableIndex++;
	}
	g_pSyncTable[indexNewEntry].m_SyncBlock = 0;
	g_pSyncTable[indexNewEntry].m_Object = obj;
	return indexNewEntry;
}

/* ObjHeader::GetSyncBlock(), under the cache lock. */
static struct SyncBlock *GetSyncBlock(struct ObjHeader *h) {
	struct SyncBlock *syncBlock = PassiveGetSyncBlock(h);
	DWORD indx = 0;
	if (syncBlock)
		return syncBlock;

	/* SyncBlockCache::LockHolder */
	syncBlock = PassiveGetSyncBlock(h);
	if (syncBlock)
		return syncBlock;

	syncBlock = &new_sb; /* GetNextFreeSyncBlock() */
	if ((indx = GetHeaderSyncBlockIndex(h)) == 0)
		indx = NewSyncBlockSlot(h->obj);

	/* new (syncBlock) SyncBlock(indx) */
	syncBlock->m_dwSyncIndex = indx;
	syncBlock->m_dwHashCode = 0;

	EnterSpinLock(h);
	DWORD bits = GetBits(h);
	if ((bits & BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX) != 0 && (bits & BIT_SBLK_IS_HASHCODE) != 0)
		SetHashCode(syncBlock, bits & MASK_HASHCODE);

#if defined(OLD) || defined(PLAIN_ENTRY)
	g_pSyncTable[indx].m_SyncBlock = syncBlock;
#else
	VolatileStore(&g_pSyncTable[indx].m_SyncBlock, syncBlock);
#endif

	if (GetHeaderSyncBlockIndex(h) == 0)
		SetIndex(h, BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX | indx);

	ReleaseSpinLock(h);
	return syncBlock;
}

/* Object::TryGetHashCode() */
static LONG TryGetHashCode(struct ObjHeader *h) {
	DWORD bits = GetBits(h);
	if (bits & BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX) {
		if (bits & BIT_SBLK_IS_HASHCODE) {
			return bits & MASK_HASHCODE;
		} else {
			struct SyncBlock *psb = PassiveGetSyncBlock(h);
			if (psb != 0)
				return psb->m_dwHashCode;
		}
	}
	return 0;
}

void setup(void) {
	hdr.obj = (struct Object *) &hdr.obj;
	hdr.m_SyncBlockValue = BIT_SBLK_IS_HASH_OR_SYNCBLKINDEX | BIT_SBLK_IS_HASHCODE | HASH;
	g_pSyncTable = old_heap;
#ifdef NO_GROW
	/* Entry 1 is free; entry 2 belongs to another object. */
	m_SyncTableSize = 3;
	m_FreeSyncTableIndex = 3;
	old_heap[1].m_Object = (struct Object *) 1; /* end of the free list */
	m_FreeSyncTableList = 1 << 1;
	other_sb.m_dwSyncIndex = 2;
	old_heap[2].m_SyncBlock = &other_sb;
	old_heap[2].m_Object = other_obj;
#else
	m_SyncTableSize = 2;
	m_FreeSyncTableIndex = 2;
	other_sb.m_dwSyncIndex = 1;
	old_heap[1].m_SyncBlock = &other_sb;
	old_heap[1].m_Object = other_obj;
#ifndef ZERO_HEAP
	stale_sb.m_dwHashCode = 7;
	old_heap[2].m_SyncBlock = &stale_sb;
#endif
#endif
}

void reader(void) {
	result = TryGetHashCode(&hdr);
	if (result != HASH)
		bad = 1;
}

void writer(void) {
	GetSyncBlock(&hdr);
}
