/* Double-checked initialization: two threads each ask for a shared object,
   which the first to arrive constructs. The fast path checks, without the
   lock, whether the object is already published. Otherwise a thread takes a
   spinlock, checks again, and constructs and publishes the object only if
   no one else has. Each thread then reads the object's value.

   By default the object is published through a pointer (inst). With
   -DFLAG it is a fixed object published through a flag (ready), as in
   code that initializes a static on first use.

   Each macro below names one ordering it needs; defining it as
   __ATOMIC_RELAXED (e.g. -DPUB=__ATOMIC_RELAXED) removes it:
   * PUB: a thread on the fast path can see the object published before
     its value is in it.
   * LOCK, UNLOCK: a thread that takes the lock after another has
     published the object can miss the publication and construct the
     object a second time.
   All three are bugs on ARM only. The fourth depends on the version:
   * SEE: with the pointer, harmless on ARM hardware: the read of the
     value has an address dependency on the read of inst, and ARM keeps
     dependent reads in order. It is still a data race in C, so a
     compiler may break it. With the flag, only a control dependency
     orders the read of the value, and ARM can read it early: a bug on
     ARM only. */

typedef __UINT64_TYPE__ u64;

#ifndef PUB
#define PUB __ATOMIC_RELEASE /* constructor: publish the object */
#endif
#ifndef SEE
#define SEE __ATOMIC_ACQUIRE /* fast path: check for the object */
#endif
#ifndef LOCK
#define LOCK __ATOMIC_ACQUIRE /* take the lock */
#endif
#ifndef UNLOCK
#define UNLOCK __ATOMIC_RELEASE /* release the lock */
#endif

struct obj {
  u64 val;
};

struct obj storage;
int made0, made1; /* whether each thread constructed the object */
int lock;
u64 out0, out1; /* the value each thread read */

#ifdef FLAG
int ready;
#else
struct obj *inst;
#endif

static void construct(struct obj *o, int *made) {
  o->val = 42;
  *made = 1;
}

/* A compare-and-swap spinlock: a failed attempt writes nothing. */
static void take(void) {
  int unlocked = 0;
  while (!__atomic_compare_exchange_n(&lock, &unlocked, 1, 0, LOCK, __ATOMIC_RELAXED))
    unlocked = 0;
}

static struct obj *get(int *made) {
#ifdef FLAG
  if (!__atomic_load_n(&ready, SEE)) {
    take();
    if (!__atomic_load_n(&ready, __ATOMIC_RELAXED)) {
      construct(&storage, made);
      __atomic_store_n(&ready, 1, PUB);
    }
    __atomic_store_n(&lock, 0, UNLOCK);
  }
  return &storage;
#else
  struct obj *p = __atomic_load_n(&inst, SEE);
  if (!p) {
    take();
    p = __atomic_load_n(&inst, __ATOMIC_RELAXED);
    if (!p) {
      p = &storage;
      construct(p, made);
      __atomic_store_n(&inst, p, PUB);
    }
    __atomic_store_n(&lock, 0, UNLOCK);
  }
  return p;
#endif
}

void t0(void) { out0 = get(&made0)->val; }

void t1(void) { out1 = get(&made1)->val; }
