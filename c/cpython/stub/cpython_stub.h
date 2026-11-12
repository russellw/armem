/* Freestanding stand-ins for what CPython's pyatomic headers and the
   seqlock section of Python/lock.c use, as an aarch64 Linux build with
   clang compiles them. */

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
typedef __INTPTR_TYPE__ Py_ssize_t;

/* From pyconfig.h and pyport.h. */
#define SIZEOF_LONG 8
#define _Py_USE_GCC_BUILTIN_ATOMICS 1
#define PyAPI_FUNC(RTYPE) RTYPE

/* A release build. */
#define assert(e) ((void)0)

/* sched_yield, a system call that orders no memory accesses. */
static void _Py_yield(void) {}

/* From Include/internal/pycore_lock.h at 1d3cf79a5 (also before it). */
typedef struct {
    uint32_t sequence;
} _PySeqLock;
