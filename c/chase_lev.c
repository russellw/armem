/* The Chase-Lev work-stealing deque, in the C11 version of Lê, Pop, Cohen
   and Zappa Nardelli, "Correct and Efficient Work-Stealing for Weak Memory
   Models" (PPoPP 2013), with a fixed-size buffer (the test never fills
   it, so the resize is left out). The owner pushes and takes at the
   bottom; thieves steal from the top. One thread owns the deque: it
   pushes 1, then 2, then takes twice. Another steals once. Each value
   must be taken exactly once: by the owner (which takes 2, then 1), or 1
   by the thief (and the owner's second take then finds the deque empty).
   A steal that loses the race for the last item returns ABORT.

   Each macro below names one ordering it needs; defining it as
   __ATOMIC_RELAXED (e.g. -DPUSH_FENCE=__ATOMIC_RELAXED) removes it:
   * PUSH_FENCE: the release fence between a push's store of the item and
     its store of bottom. Without it the thief can see the new bottom
     before the item, and steal what was in the slot before.
   * TAKE_FENCE: the seq_cst fence between a take's store of bottom and
     its load of top. Without it the take's load of top can be satisfied
     before its store of bottom is visible to the thief (store
     buffering, allowed on x86 too), so the owner and the thief can both
     take the last item.
   * STEAL_FENCE: the seq_cst fence between a steal's loads of top and
     bottom, the thief's half of the same store-buffering pattern.
   * STEAL_TOP, STEAL_BOTTOM: the acquire loads of top and bottom in a
     steal.
   * PUSH_TOP: the acquire load of top in a push.
   * CAS: the compare-and-swaps of top, in a take and a steal. */

typedef __INT64_TYPE__ i64;

#ifndef PUSH_FENCE
#define PUSH_FENCE __ATOMIC_RELEASE
#endif
#ifndef TAKE_FENCE
#define TAKE_FENCE __ATOMIC_SEQ_CST
#endif
#ifndef STEAL_FENCE
#define STEAL_FENCE __ATOMIC_SEQ_CST
#endif
#ifndef STEAL_TOP
#define STEAL_TOP __ATOMIC_ACQUIRE
#endif
#ifndef STEAL_BOTTOM
#define STEAL_BOTTOM __ATOMIC_ACQUIRE
#endif
#ifndef PUSH_TOP
#define PUSH_TOP __ATOMIC_ACQUIRE
#endif
#ifndef CAS
#define CAS __ATOMIC_SEQ_CST
#endif

#define SIZE 4
#define EMPTY 99
#define ABORT 98

i64 top, bottom;
i64 buffer[SIZE];
i64 took, stole1, stole2; /* the owner's take, and the thief's steals */

static void push(i64 x) {
  i64 b = __atomic_load_n(&bottom, __ATOMIC_RELAXED);
  i64 t = __atomic_load_n(&top, PUSH_TOP);
  if (b - t > SIZE - 1)
    __builtin_trap(); /* full: the paper resizes here */
  __atomic_store_n(&buffer[b % SIZE], x, __ATOMIC_RELAXED);
  __atomic_thread_fence(PUSH_FENCE);
  __atomic_store_n(&bottom, b + 1, __ATOMIC_RELAXED);
}

static i64 take(void) {
  i64 b = __atomic_load_n(&bottom, __ATOMIC_RELAXED) - 1;
  __atomic_store_n(&bottom, b, __ATOMIC_RELAXED);
  __atomic_thread_fence(TAKE_FENCE);
  i64 t = __atomic_load_n(&top, __ATOMIC_RELAXED);
  i64 x;
  if (t <= b) {
    x = __atomic_load_n(&buffer[b % SIZE], __ATOMIC_RELAXED);
    if (t == b) {
      if (!__atomic_compare_exchange_n(&top, &t, t + 1, 0, CAS, __ATOMIC_RELAXED))
        x = EMPTY;
      __atomic_store_n(&bottom, b + 1, __ATOMIC_RELAXED);
    }
  } else {
    x = EMPTY;
    __atomic_store_n(&bottom, b + 1, __ATOMIC_RELAXED);
  }
  return x;
}

static i64 steal(void) {
  i64 t = __atomic_load_n(&top, STEAL_TOP);
  __atomic_thread_fence(STEAL_FENCE);
  i64 b = __atomic_load_n(&bottom, STEAL_BOTTOM);
  i64 x = EMPTY;
  if (t < b) {
    x = __atomic_load_n(&buffer[t % SIZE], __ATOMIC_RELAXED);
    if (!__atomic_compare_exchange_n(&top, &t, t + 1, 0, CAS, __ATOMIC_RELAXED))
      return ABORT;
  }
  return x;
}

void owner(void) {
  push(1);
  push(2);
  took = take();
}

void thief(void) {
  stole1 = steal();
  stole2 = steal();
}
