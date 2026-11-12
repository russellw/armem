/* Freestanding stand-in for CPython's Python.h and the internal headers
   that Python/typecache.c includes (as at commit 0ada998ebb), in a
   free-threaded build: only what typecache.c and pycore_typecache.h use,
   as an aarch64 Linux build with clang compiles them. The atomics are
   CPython's own (pyatomic.h and pycore_pyatomic_ft_wrappers.h, in
   cpython/main/Include). */
#ifndef STUB_PYTHON_H
#define STUB_PYTHON_H

#define Py_BUILD_CORE 1
#define Py_GIL_DISABLED 1

typedef __INT8_TYPE__ int8_t;
typedef __INT16_TYPE__ int16_t;
typedef __INT32_TYPE__ int32_t;
typedef __INT64_TYPE__ int64_t;
typedef __UINT8_TYPE__ uint8_t;
typedef __UINT16_TYPE__ uint16_t;
typedef __UINT32_TYPE__ uint32_t;
typedef __UINT64_TYPE__ uint64_t;
typedef __INTPTR_TYPE__ intptr_t;
typedef __UINTPTR_TYPE__ uintptr_t;
typedef __SIZE_TYPE__ size_t;
typedef __INTPTR_TYPE__ Py_ssize_t;
typedef Py_ssize_t Py_hash_t;
#define NULL ((void *)0)
#define offsetof(t, m) __builtin_offsetof(t, m)

/* From pyconfig.h and pyport.h. */
#define SIZEOF_LONG 8
#define _Py_USE_GCC_BUILTIN_ATOMICS 1
#define PyAPI_FUNC(RTYPE) RTYPE

/* A release build, without statistics. */
#define assert(e) ((void)0)
#define OBJECT_STAT_INC(name) ((void)0)

#include "pyatomic.h"

/* An object: here only an interned string, with its cached hash, or a
   value that the cache refers to. */
typedef struct _object {
    Py_hash_t hash;
} PyObject;
#define _PyObject_CAST(op) ((PyObject *)(op))
#define PyUnstable_Unicode_GET_CACHED_HASH(op) (((PyObject *)(op))->hash)

/* The fields of PyTypeObject that typecache.c uses. */
#define _Py_TPFLAGS_STATIC_BUILTIN (1 << 1)
typedef struct _typeobject {
    unsigned long tp_flags;
    unsigned int tp_version_tag;
    void *_tp_cache;
} PyTypeObject;

/* Static builtin types keep their cache per interpreter; the test's type
   is a heap type. */
typedef struct { void *_tp_cache; } managed_static_type_state;
typedef struct _is PyInterpreterState;
static inline PyInterpreterState *_PyInterpreterState_GET(void) { return NULL; }
static inline managed_static_type_state *
_PyStaticType_GetState(PyInterpreterState *interp, PyTypeObject *type) { return NULL; }

/* PyMem_Calloc as a bump allocator over zeroed memory, and
   _PyMem_FreeDelayed, which frees after every thread has passed a
   quiescent state, as never freeing. */
void *PyMem_Calloc(size_t nelem, size_t elsize);
static inline void _PyMem_FreeDelayed(void *ptr, size_t size) {}

#endif
