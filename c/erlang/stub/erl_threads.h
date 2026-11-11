/*
 * Stand-in for erts' erl_threads.h, giving the process lock code what it
 * uses from ethread as a 64-bit ARM build with gcc 8.3 or later compiles
 * it (gcc/ethread.h then trusts the __atomic builtins' barriers):
 *
 * - The _nob, _acqb and plain _relb operations are the __atomic builtins
 *   with relaxed, acquire and release order (gcc/ethr_atomic.h).
 * - There is no native compare-and-swap with release order, so
 *   ethr_atomics.h makes cmpxchg_relb a barrier with StoreLoad, which
 *   gcc/ethr_membar.h makes DMB SY, then a relaxed compare-and-swap.
 * - The mutex is ethread's own (ethr_mutex.h): locking is cmpxchg_acqb of
 *   the write flag, unlocking is cmpxchg_relb. Its waiting is modeled by
 *   retrying, since nothing here queues on it.
 * - A thread's event (ethr_event.h, futex version): resetting it is a
 *   relaxed store and a full barrier; setting it is an exchange with full
 *   barriers, here just the barrier, since nothing reads the event.
 *   erts_tse_wait() returns at once, a spurious wakeup the callers must
 *   allow for, so a waiter polls its flag.
 * - Every thread shares one event, enough for tests in which only one
 *   thread waits for process locks.
 */
#ifndef ERL_THREADS_H__
#define ERL_THREADS_H__

#include <stddef.h>
#include <stdint.h>

typedef int32_t erts_aint32_t;
typedef intptr_t erts_aint_t;
typedef uintptr_t Uint;
typedef intptr_t Sint;
typedef int64_t Sint64;
typedef int16_t Sint16;
typedef uintptr_t Eterm;

#define ERTS_INLINE inline
#define ERTS_GLB_INLINE static inline
#define ERTS_GLB_INLINE_INCL_FUNC_DEF 1
#define ETHR_HAVE_32BIT_NATIVE_ATOMIC_OPS 1
#define SIZEOF_INT 4
#define ASSERT(e) ((void) 0)
#define ERTS_LC_ASSERT(e) ((void) 0)
#define ETHR_COMPILER_BARRIER __asm__ __volatile__("" : : : "memory")
#define ETHR_FULL_FENCE__ __asm__ __volatile__("dmb sy" : : : "memory")
#define ETHR_MEMORY_BARRIER ETHR_FULL_FENCE__
#define ERTS_SPIN_BODY ((void) 0)
#define ERTS_LIKELY(x) __builtin_expect(!!(x), 1)
#define EBUSY 16
#define ETHR_LoadLoad (1 << 0)
#define ETHR_LoadStore (1 << 1)
#define ETHR_StoreLoad (1 << 2)
#define ETHR_StoreStore (1 << 3)
#define ETHR_MEMBAR(b) ETHR_FULL_FENCE__
#define ERTS_ALC_CACHE_LINE_ALIGN_SIZE(s) (((s) + 63) & ~(size_t) 63)

typedef struct { volatile erts_aint32_t value; } erts_atomic32_t;

static inline erts_aint32_t erts_atomic32_read_nob(erts_atomic32_t *a)
{ return __atomic_load_n(&a->value, __ATOMIC_RELAXED); }
static inline erts_aint32_t erts_atomic32_read_acqb(erts_atomic32_t *a)
{ return __atomic_load_n(&a->value, __ATOMIC_ACQUIRE); }
static inline void erts_atomic32_set_nob(erts_atomic32_t *a, erts_aint32_t v)
{ __atomic_store_n(&a->value, v, __ATOMIC_RELAXED); }
static inline void erts_atomic32_init_nob(erts_atomic32_t *a, erts_aint32_t v)
{ a->value = v; }
static inline erts_aint32_t
erts_atomic32_read_band_nob(erts_atomic32_t *a, erts_aint32_t m)
{ return __atomic_fetch_and(&a->value, m, __ATOMIC_RELAXED); }
static inline erts_aint32_t
erts_atomic32_read_bor_acqb(erts_atomic32_t *a, erts_aint32_t m)
{ return __atomic_fetch_or(&a->value, m, __ATOMIC_ACQUIRE); }
static inline erts_aint32_t
erts_atomic32_cmpxchg_nob(erts_atomic32_t *a, erts_aint32_t n, erts_aint32_t e)
{
    __atomic_compare_exchange_n(&a->value, &e, n, 0, __ATOMIC_RELAXED,
                                __ATOMIC_RELAXED);
    return e;
}
static inline erts_aint32_t
erts_atomic32_cmpxchg_acqb(erts_atomic32_t *a, erts_aint32_t n, erts_aint32_t e)
{
    __atomic_compare_exchange_n(&a->value, &e, n, 0, __ATOMIC_ACQUIRE,
                                __ATOMIC_ACQUIRE);
    return e;
}
static inline erts_aint32_t
erts_atomic32_cmpxchg_relb(erts_atomic32_t *a, erts_aint32_t n, erts_aint32_t e)
{
    ETHR_FULL_FENCE__;
    return erts_atomic32_cmpxchg_nob(a, n, e);
}
/* ethread's own mutex, uncontended paths; contention retries. */
typedef struct { erts_atomic32_t flgs; } erts_mtx_t;
#define ETHR_RWMTX_W_FLG__ ((erts_aint32_t) 1 << 31)
static inline void erts_mtx_lock(erts_mtx_t *m)
{
    while (erts_atomic32_cmpxchg_acqb(&m->flgs, ETHR_RWMTX_W_FLG__, 0) != 0)
        ;
}
static inline void erts_mtx_unlock(erts_mtx_t *m)
{
    (void) erts_atomic32_cmpxchg_relb(&m->flgs, 0, ETHR_RWMTX_W_FLG__);
}
static inline void erts_mtx_init(erts_mtx_t *m, const char *name, Eterm extra,
                                 int flags)
{
    (void) name; (void) extra; (void) flags;
    erts_atomic32_init_nob(&m->flgs, 0);
}
static inline int erts_lc_mtx_is_locked(erts_mtx_t *m) { (void) m; return 1; }
#define ERTS_LOCK_FLAGS_PROPERTY_STATIC 0
#define ERTS_LOCK_FLAGS_CATEGORY_PROCESS 0
#define make_small(i) ((Eterm) (i))

/* A thread specific event (ethr_ts_event). */
typedef struct erts_tse_t_ erts_tse_t;
struct erts_tse_t_ {
    erts_tse_t *next;
    erts_tse_t *prev;
    int uflgs;
    erts_atomic32_t uaflgs;
    erts_atomic32_t futex;
};
extern erts_tse_t erts_test_tse;
static inline erts_tse_t *erts_tse_fetch(void) { return &erts_test_tse; }
static inline void erts_tse_return(erts_tse_t *ep) { (void) ep; }
static inline void erts_tse_reset(erts_tse_t *ep)
{
    erts_atomic32_set_nob(&ep->futex, 0);
    ETHR_MEMORY_BARRIER;
}
static inline void erts_tse_set(erts_tse_t *ep)
{
    /* The exchange with full barriers, less its write, which nothing
       reads here (and which, shared, would multiply the memories). */
    (void) ep;
    ETHR_FULL_FENCE__;
}
static inline int erts_tse_wait(erts_tse_t *ep) { (void) ep; return 0; }

static inline int erts_thr_get_main_status(void) { return 1; }
static inline void erts_thr_yield(void) { }
static inline void erts_thr_install_exit_handler(void (*f)(void)) { (void) f; }

#endif
