/* Stand-in for Include/internal/pycore_stackref.h (at commit 0ada998ebb)
   in a free-threaded build: only what typecache.c uses. */
#ifndef STUB_PYCORE_STACKREF_H
#define STUB_PYCORE_STACKREF_H

typedef union _PyStackRef {
    uintptr_t bits;
} _PyStackRef;

#define Py_TAG_REFCNT 1
static const _PyStackRef PyStackRef_NULL = { .bits = 0 };
#define PyStackRef_XCLOSE(ref) ((void)0)

/* Functions and descriptors assigned to types have deferred reference
   counts (gh-139103), so taking a reference to one reads nothing more. */
#define _PyObject_HasDeferredRefcount(op) 1
#define _Py_TryIncrefCompare(src, op) 1
#define PyStackRef_FromPyObjectSteal(op) ((_PyStackRef){ .bits = (uintptr_t)(op) })

/* Unchanged. */
static inline int
_Py_TryIncrefCompareStackRef(PyObject **src, PyObject *op, _PyStackRef *out)
{
    if (_PyObject_HasDeferredRefcount(op)) {
        *out = (_PyStackRef){ .bits = (uintptr_t)op | Py_TAG_REFCNT };
        return 1;
    }
    if (_Py_TryIncrefCompare(src, op)) {
        *out = PyStackRef_FromPyObjectSteal(op);
        return 1;
    }
    return 0;
}

/* Unchanged. */
static inline int
_Py_TryXGetStackRef(PyObject **src, _PyStackRef *out)
{
    PyObject *op = _PyObject_CAST(_Py_atomic_load_ptr_relaxed(src));
    if (op == NULL) {
        *out = PyStackRef_NULL;
        return 1;
    }
    return _Py_TryIncrefCompareStackRef(src, op, out);
}

#endif
