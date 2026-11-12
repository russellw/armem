/* cflags: -ffreestanding -Icpython/stub -Icpython/Include */
/* CPython's seqlock (the _PySeqLock functions of Python/lock.c, unchanged,
   in cpython/new and cpython/old) and its use by the free-threaded type
   cache in Objects/typeobject.c, as at commit 1d3cf79a5 (July 2024). The
   pyatomic headers in cpython/Include are unchanged from 1d3cf79a5, and
   cpython/stub stands in for the rest of CPython that they use.

   The type cache maps (type version, name) to a borrowed reference to the
   attribute found. A lookup (_PyType_LookupRef) reads an entry's version,
   the type's version and the entry's name and value between
   _PySeqLock_BeginRead and _PySeqLock_EndRead, and uses the value only if
   the versions and the name match and the sequence is unchanged. An
   update (update_cache_gil_disabled) locks the sequence by a
   compare-and-swap to an odd value, stores the version, value and name
   (relaxed), and stores the next even value. The writer and reader below
   copy those functions without reference counting: the reader's
   _Py_TryIncref only reads and writes the value's own counts, and the
   writer's Py_DECREF and Py_NewRef only the old and new objects'.

   P0 replaces the entry for (version 1, n1, a) with (version 2, n2, b), as
   another type and name that hash to the same slot would. P1 looks up n1
   for a type whose version is 1. It must get a or miss; the condition is
   that it gets b, the new entry's value under the old entry's name and
   version (the "incorrect results" of gh-121368).

   -DOLD uses cpython/old/seqlock.c, from before commit 1d3cf79a5
   ("gh-121368: Fix seq lock memory ordering in _PyType_Lookup"), whose
   message says the bug can be triggered on macOS arm64. The fix changed
   two functions, which -DOLD_READ and -DOLD_WRITE take from the old file
   one at a time:
   * OLD_READ: _PySeqLock_EndRead re-reads the sequence with an acquire
     load, which orders the loads after it but not the critical section's
     loads before it, so the reader can read the new value and the old
     sequence. The fix makes it an acquire fence and a relaxed load.
   * OLD_WRITE: _PySeqLock_LockWrite has no fence after its
     compare-and-swap of the sequence, so the writer's stores to the entry
     may be seen before the odd sequence. That is so with an LDAXR/STLXR
     loop, whose store-release orders only what comes before it, but not
     with LSE's CASAL (as in Arm's model), so it is ARM-only and needs a
     build without LSE. The fix adds a release fence. */

#include "cpython_stub.h"
#include "pyatomic.h"

#ifdef OLD
#define OLD_READ
#define OLD_WRITE
#endif

/* Both versions, the old one's functions renamed. */
#define _PySeqLock_LockWrite old_LockWrite
#define _PySeqLock_AbandonWrite old_AbandonWrite
#define _PySeqLock_UnlockWrite old_UnlockWrite
#define _PySeqLock_BeginRead old_BeginRead
#define _PySeqLock_EndRead old_EndRead
#define _PySeqLock_AfterFork old_AfterFork
#include "cpython/old/seqlock.c"
#undef _PySeqLock_LockWrite
#undef _PySeqLock_AbandonWrite
#undef _PySeqLock_UnlockWrite
#undef _PySeqLock_BeginRead
#undef _PySeqLock_EndRead
#undef _PySeqLock_AfterFork
#undef SEQLOCK_IS_UPDATING
#include "cpython/new/seqlock.c"

#ifdef OLD_WRITE
#define LockWrite old_LockWrite
#else
#define LockWrite _PySeqLock_LockWrite
#endif
#ifdef OLD_READ
#define EndRead old_EndRead
#else
#define EndRead _PySeqLock_EndRead
#endif

typedef struct { int unused; } PyObject;

/* From Include/internal/pycore_typeobject.h, in a free-threaded build. */
struct type_cache_entry {
    unsigned int version;  // initialized from type->tp_version_tag
   _PySeqLock sequence;
    PyObject *name;        // reference to exactly a str or None
    PyObject *value;       // borrowed reference or NULL
};

PyObject n1, n2, a, b;
#define MISS ((PyObject *)99)

struct type_cache_entry entry = { 1, { 0 }, &n1, &a };
unsigned int tp_version_tag = 1; /* the version of P1's type */
PyObject *got;

/* update_cache, without counting references. */
static void
update_cache(struct type_cache_entry *entry, PyObject *name, unsigned int version_tag, PyObject *value)
{
    _Py_atomic_store_uint32_relaxed(&entry->version, version_tag);
    _Py_atomic_store_ptr_relaxed(&entry->value, value); /* borrowed */
    _Py_atomic_store_ptr_relaxed(&entry->name, name);
}

/* update_cache_gil_disabled, without counting references. */
static void
update_cache_gil_disabled(struct type_cache_entry *entry, PyObject *name,
                          unsigned int version_tag, PyObject *value)
{
    LockWrite(&entry->sequence);

    // update the entry
    if (entry->name == name &&
        entry->value == value &&
        entry->version == version_tag) {
        // We raced with another update, bail and restore previous sequence.
        _PySeqLock_AbandonWrite(&entry->sequence);
        return;
    }

    update_cache(entry, name, version_tag, value);

    // Then update sequence to the next valid value
    _PySeqLock_UnlockWrite(&entry->sequence);
}

/* The lock-free part of _PyType_LookupRef, without counting references:
   the value found, or MISS where it would fall back to the locked lookup. */
static PyObject *
lookup(PyObject *name)
{
    // synchronize-with other writing threads by doing an acquire load on the sequence
    while (1) {
        uint32_t sequence = _PySeqLock_BeginRead(&entry.sequence);
        uint32_t entry_version = _Py_atomic_load_uint32_relaxed(&entry.version);
        uint32_t type_version = _Py_atomic_load_uint32_acquire(&tp_version_tag);
        if (entry_version == type_version &&
            _Py_atomic_load_ptr_relaxed(&entry.name) == name) {
            PyObject *value = _Py_atomic_load_ptr_relaxed(&entry.value);
            // If the sequence is still valid then we're done
            if (EndRead(&entry.sequence, sequence)) {
                return value;
            }
        }
        else {
            // cache miss
            return MISS;
        }
    }
}

void writer(void) { update_cache_gil_disabled(&entry, &n2, 2, &b); }

void reader(void) { got = lookup(&n1); }
