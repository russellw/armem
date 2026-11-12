/* Stand-in for include/haproxy/api.h: only what list.h uses (in 2.2 and
   3.0), as an aarch64 build with clang compiles it. _HA_ATOMIC_XCHG and the
   barriers are copied from include/haproxy/atomic.h for aarch64, where they
   are the same at 787dc2095 and in 3.0; __ha_cpu_relax, which only 3.0's
   list.h uses, is 3.0's for aarch64. The program includes the version's
   list-t.h itself. */
#ifndef _HAPROXY_API_H
#define _HAPROXY_API_H

typedef __SIZE_TYPE__ size_t;
#define NULL ((void *)0)

/* Debugging checks, compiled out by default. */
#define BUG_ON_HOT(cond) do { } while (0)

#define _HA_ATOMIC_XCHG(val, new)     __atomic_exchange_n(val, new, __ATOMIC_RELAXED)

static __inline void
__ha_barrier_load(void)
{
	__asm __volatile("dmb ishld" ::: "memory");
}

static __inline void
__ha_barrier_store(void)
{
	__asm __volatile("dmb ishst" ::: "memory");
}

static __inline void
__ha_barrier_full(void)
{
	__asm __volatile("dmb ish" ::: "memory");
}

#define __ha_cpu_relax() ({ asm volatile("isb" ::: "memory"); 1; })

#endif
