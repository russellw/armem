/* The arrival (gather) and departure (release) of LLVM libomp's
   distributed barrier (__kmp_dist_barrier_gather and
   __kmp_dist_barrier_release in openmp/runtime/src/kmp_barrier.cpp,
   Apache-2.0 WITH LLVM-exception), transcribed from llvm-project main
   6cd3ff3a0 (November 2026), which has 0fa6182e2 ("[OpenMP][libomp] Fix
   dist barrier arrival synchronization", #213845, September 2026). With
   -DPRE the stillNeed flags are as before it: a volatile flag, stored and
   loaded with plain accesses and no fences.

   Threads are in groups of threads_per_group; the first of each group is
   its leader, and thread 0 (the primary) also leads the first group. To
   arrive, a thread stores 0 to its stillNeed flag. A leader waits for the
   flags of its group, stores its own, and then waits for the flags of all
   the leaders; then each thread executes a full fence (KMP_MFENCE). To
   depart, the primary stores the next value to the go flag of each
   leader and of each thread in its group, and each leader in turn stores
   it to the go flags of its group; the go flags are std::atomic, stored
   seq_cst and waited for with acquire loads. Tasking, sleeping
   (blocktime), the checks that a thread is still in the team, and the
   flag for the next iteration are left out.

   Each thread writes its own data word, passes the barrier once, and
   reads the next thread's. The condition is a thread reading the old
   value. Two threads form one group by default; with -DGROUPS three
   threads form groups {0, 1} and {2} (libomp_dist_barrier3.c), so that t1's write gets to t2
   through t0, and t2's to t1 through t0's go stores. With -DREDUCE each
   leader also reads the data of the threads in its group right after it
   sees their flags, before the fence, as the reduction does.

   Before 0fa6182e2 the store of stillNeed was not ordered after a
   thread's earlier writes, so on ARM a leader could see a thread arrive
   before its data: a bug on ARM only. After it the store is a release.
   0fa6182e2 also adds an acquire fence after each wait for the flags;
   -DNO_ACQ removes them. On ARM they are not needed for reads after the
   barrier, which the full fence orders, but they are for the
   reduction's reads. */

typedef unsigned int u32;
typedef unsigned long u64;

#ifdef GROUPS
#define NPROC 3
#define THREADS_PER_GROUP 2
#else
#define NPROC 2
#define THREADS_PER_GROUP 2
#endif
/* threads_per_go is 1, so thread t waits on go[t]. */
#define GOS_PER_GROUP THREADS_PER_GROUP
#define NUM_GOS NPROC
#define MAX_ITERS 3

#ifdef PRE
static volatile u32 still_need[3] = {1, 1, 1};
#define STILL_LOAD(t) (still_need[t])
#define STILL_DONE(t) (still_need[t] = 0)
#define ACQUIRE_FENCE() ((void)0)
#else
static u32 still_need[3] = {1, 1, 1};
#define STILL_LOAD(t) __atomic_load_n(&still_need[t], __ATOMIC_RELAXED)
#define STILL_DONE(t) __atomic_store_n(&still_need[t], 0, __ATOMIC_RELEASE)
#ifdef NO_ACQ
#define ACQUIRE_FENCE() ((void)0)
#else
#define ACQUIRE_FENCE() __atomic_thread_fence(__ATOMIC_ACQUIRE)
#endif
#endif

static u64 go[NUM_GOS];
static const u64 next_go = 0 + MAX_ITERS; /* iter is 0 */

int data[NPROC];
int bad;

static void gather(int tid) {
  if (tid % THREADS_PER_GROUP == 0) {
    int group_end = tid + THREADS_PER_GROUP;
    if (group_end > NPROC)
      group_end = NPROC;
    u32 pending;
    do {
      pending = 0;
      for (int thr = tid + 1; thr < group_end; thr++)
        pending += STILL_LOAD(thr);
    } while (pending > 0);
    ACQUIRE_FENCE();
#ifdef REDUCE
    for (int thr = tid + 1; thr < group_end; thr++)
      if (data[thr] != 1)
        bad = 1;
#endif
    STILL_DONE(tid);
    do {
      pending = 0;
      for (int thr = 0; thr < NPROC; thr += THREADS_PER_GROUP)
        pending += STILL_LOAD(thr);
    } while (pending > 0);
    ACQUIRE_FENCE();
  } else {
    STILL_DONE(tid);
  }
  __sync_synchronize(); /* KMP_MFENCE() */
}

static void release(int tid) {
  if (tid != 0) {
    if (__atomic_load_n(&go[tid], __ATOMIC_SEQ_CST) != next_go)
      while (__atomic_load_n(&go[tid], __ATOMIC_ACQUIRE) != next_go)
        ;
    if (tid % THREADS_PER_GROUP == 0) {
      for (int i = tid + 1; i < tid + GOS_PER_GROUP && i < NUM_GOS; i++)
        __atomic_store_n(&go[i], next_go, __ATOMIC_SEQ_CST);
      __sync_synchronize();
    }
  } else {
    for (int i = 0; i < NUM_GOS; i += GOS_PER_GROUP)
      __atomic_store_n(&go[i], next_go, __ATOMIC_SEQ_CST);
    for (int i = 1; i < GOS_PER_GROUP && i < NUM_GOS; i++)
      __atomic_store_n(&go[i], next_go, __ATOMIC_SEQ_CST);
    __sync_synchronize();
  }
}

static void run(int tid) {
  data[tid] = 1;
  gather(tid);
  release(tid);
#ifndef REDUCE
  if (data[(tid + 1) % NPROC] != 1)
    bad = 1;
#endif
}

void t0(void) { run(0); }
void t1(void) { run(1); }
void t2(void) { run(2); }
