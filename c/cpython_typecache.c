/* cflags: -ffreestanding -fno-vectorize -fno-slp-vectorize -Icpython/stub_main -Icpython/main/Include -Icpython/main/Include/internal */
/* CPython's per-type method cache (cpython/main/Python/typecache.c and
   pycore_typecache.h, unchanged from commit 0ada998ebb on main, November
   2026), with CPython's atomics from the same commit and stand-ins for
   the rest of CPython in cpython/stub_main, in a free-threaded build.

   Commit daf09e17bf (July 2026, gh-145685) replaced the global type cache
   and its seqlock (see cpython_seqlock.c) with a cache per type: a hash
   table of (name, value) entries for one version of the type, reached by
   an acquire load of the type's cache pointer. A lookup
   (_PyTypeCache_Lookup) loads the pointer, probes for the name with
   acquire loads, reads the value (relaxed), and returns it only if the
   cache's version is still the type's (a relaxed load). Writers hold the
   type lock. Insertion stores an entry's value and then its name
   (release), or first replaces a full cache with a bigger copy (release).
   Modifying a type (type_setattro) sets its version to 0 (relaxed), swaps
   in the shared empty cache (release), and then changes the type's dict.

   setup() gives the type version 1 and caches its attribute n as v1,
   through _PyTypeCache_Insert. P0 then sets the attribute to v2, as
   type_setattro does, and looks it up as a cache miss does in
   _PyType_LookupStackRefAndVersion: under the type lock, it assigns
   version 2, finds v2 in the dict and caches it, which allocates a new
   cache. P1 looks n up twice, lock-free. The condition is that a hit
   returns a value that is not the type's value at the version returned,
   or that the second lookup hits an older version than the first.

   The writer's steps are copied from typeobject.c, without the parts that
   do not touch the cache: watchers, subclasses, slots and reference
   counts. The dict is one pointer, written with a release store as
   _PyDict_SetItem_LockHeld publishes a value. Only P0 writes, so the type
   lock is left out.

   Two switches weaken the cache's orderings, to check that the harness
   can find a bug: -DRELAXED_STORES makes its release stores (of an
   entry's name and of the type's cache pointer) relaxed, and
   -DRELAXED_LOADS makes its acquire loads (of the cache pointer and of
   an entry's name) relaxed.

   -fno-vectorize and -fno-slp-vectorize stop clang from turning
   cache_insert's updates of used and available into a vector ADD, which
   armem does not support. */

#include "Python.h"
#include "pycore_pyatomic_ft_wrappers.h"
#ifdef RELAXED_STORES
#undef FT_ATOMIC_STORE_PTR_RELEASE
#define FT_ATOMIC_STORE_PTR_RELEASE FT_ATOMIC_STORE_PTR_RELAXED
#endif
#ifdef RELAXED_LOADS
#undef FT_ATOMIC_LOAD_PTR_ACQUIRE
#define FT_ATOMIC_LOAD_PTR_ACQUIRE FT_ATOMIC_LOAD_PTR_RELAXED
#endif
#include "cpython/main/Python/typecache.c"

static _Alignas(16) char heap[1024];
static size_t heap_used;

void *
PyMem_Calloc(size_t nelem, size_t elsize)
{
    void *p = heap + heap_used;
    heap_used += (nelem * elsize + 15) & ~(size_t)15;
    return p;
}

PyTypeObject type;
PyObject n = { 3 };   /* an interned name, with its hash */
PyObject v1, v2;      /* the attribute's values at versions 1 and 2 */
PyObject *dict_n;     /* the type's dict, holding only n */
int bad;

void
setup(void)
{
    _PyTypeCache_InitType(&type);
    type.tp_version_tag = 1;
    dict_n = &v1;
    _PyTypeCache_Insert(&type, &n, &v1);
}

/* type_setattro on n: type_update_dict, with the parts of
   _PyType_Modified_Unlocked and set_version_unlocked that the cache
   sees. */
static void
set_attr(PyObject *value)
{
    FT_ATOMIC_STORE_UINT_RELAXED(type.tp_version_tag, 0); /* 0 is not a valid version tag */
    _PyTypeCache_Invalidate(&type);
    __atomic_store_n(&dict_n, value, __ATOMIC_RELEASE);
}

/* The locked part of _PyType_LookupStackRefAndVersion, with
   assign_version_tag giving the next version, 2, and find_name_in_mro
   reading the dict. */
static void
lookup_locked(void)
{
    unsigned int version_tag = type.tp_version_tag;
    if (version_tag == 0) {
        FT_ATOMIC_STORE_UINT_RELAXED(type.tp_version_tag, 2);
        version_tag = type.tp_version_tag;
    }
    PyObject *value = dict_n;
    if (version_tag != 0 && version_tag == type.tp_version_tag) {
        _PyTypeCache_Insert(&type, &n, value);
    }
}

void
writer(void)
{
    set_attr(&v2);
    lookup_locked();
}

/* A hit must return the value at the version it returns; returns that
   version, or 0 for a miss. */
static unsigned int
check(void)
{
    struct _PyTypeCacheLookupResult r = _PyTypeCache_Lookup(&type, &n);
    if (!r.cache_hit) {
        return 0;
    }
    PyObject *value = (PyObject *)(r.value.bits & ~(uintptr_t)Py_TAG_REFCNT);
    if (!(r.version_tag == 1 && value == &v1) && !(r.version_tag == 2 && value == &v2)) {
        bad = 1;
    }
    return r.version_tag;
}

void
reader(void)
{
    unsigned int first = check();
    unsigned int second = check();
    if (second != 0 && second < first) {
        bad = 1;
    }
}
