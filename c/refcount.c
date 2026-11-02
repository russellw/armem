/* Reference counting: two threads share an object that holds one reference
   for each. Each thread uses the object (reads its value and marks it
   used), then drops its reference. The thread that drops the last one
   destroys the object: its destructor checks that both uses happened,
   then frees the object, which here means overwriting its value.

   This is the usual pattern (as in Boost's shared_ptr and Linux's
   refcount_dec_and_test): a release decrement, and an acquire fence before
   destruction. Each macro below names one ordering it needs; defining it
   as __ATOMIC_RELAXED (e.g. -DDEC=__ATOMIC_RELAXED) removes it:
   * DEC: a thread's use of the object can happen after its decrement, so
     after the other thread has destroyed the object: it reads the freed
     value, or the destructor misses its use.
   * ACQ: the destructor can read the object before the other thread's
     use, though it reads it after the other thread's decrement: only a
     control dependency (the branch on the count) orders the two, and ARM
     can perform reads early. (The destructor's write of the freed value
     cannot become visible early, since ARM does not let a store become
     visible before a load it depends on.)
   Both are bugs on ARM only. */

typedef __UINT64_TYPE__ u64;

#ifndef DEC
#define DEC __ATOMIC_RELEASE /* drop a reference */
#endif
#ifndef ACQ
#define ACQ __ATOMIC_ACQUIRE /* fence before destruction */
#endif

struct obj {
  u64 val;     /* 42 while the object is alive, 0 once it is freed */
  u64 used[2]; /* set by each thread's use */
};

struct obj o = {42, {0, 0}};
unsigned refs = 2;
u64 out0, out1;   /* the value each thread read */
u64 seen0, seen1; /* uses the destructor saw, if the thread ran it */

static void destroy(struct obj *p, u64 *seen) {
  *seen = p->used[0] + p->used[1];
  p->val = 0;
}

static void put(struct obj *p, u64 *seen) {
  if (__atomic_fetch_sub(&refs, 1, DEC) == 1) {
    __atomic_thread_fence(ACQ);
    destroy(p, seen);
  }
}

void t0(void) {
  out0 = o.val;
  o.used[0] = 1;
  put(&o, &seen0);
}

void t1(void) {
  out1 = o.val;
  o.used[1] = 1;
  put(&o, &seen1);
}
