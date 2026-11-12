/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -Igrpc/stub */
/* The shutdown of gRPC's completion queues (src/core/lib/surface/
   completion_queue.cc, Apache License 2.0, see grpc/LICENSE), as at
   commit f01a138e8 (April 2026) and, with -DOLD, just before it.

   Commit f01a138e8 ("Fix completion queue shutdown race on weak memory
   models (ARM)", gRPC PR 41510) says that on ARM64 a server's shutdown
   could hang for 20 minutes or more because the flag shutdown_called
   "lacks memory barriers, causing blocked threads to never wake up". It
   made the flag a std::atomic<bool>, stored with release and loaded with
   acquire in cq_finish_shutdown_* (and relaxed in cq_shutdown_*).

   Before the fix, the flag is written once, under the queue's lock, by
   grpc_completion_queue_shutdown (cq_shutdown_*), before that drops the
   queue's initial count from pending_events with an acq_rel fetch_sub.
   It is read only by cq_shutdown_* (under the lock) and by
   cq_finish_shutdown_*, which runs when pending_events reaches 0: either
   in cq_shutdown_* or in the cq_end_op_* that completes the last
   operation, after an acq_rel fetch_sub (or an acquire load) that reads
   the count the shutdown left. A thread blocked in cq_next or cq_pluck
   waits on pending_events and the pollset, never on the flag.

   setup() begins one operation (pending_events 1 to 2, as
   cq_begin_op_for_* does). P0 shuts the queue down while P1 ends the
   operation, so either may finish the shutdown. The condition is that
   cq_finish_shutdown_*'s GRPC_CHECK fails (it sees shutdown_called false,
   or for the next queue pending_events not 0), or that the shutdown is
   finished other than once. The kind of queue is GRPC_CQ_NEXT, or
   GRPC_CQ_PLUCK with -DPLUCK, or GRPC_CQ_CALLBACK with -DCALLBACK; only
   the callback queue's cq_end_op_for_callback finishes the shutdown
   without the lock, so only there is the fetch_sub all that orders the
   flag.

   -DRELAXED makes the acq_rel fetch_subs of pending_events relaxed, to
   check that the harness finds the bug that would then exist on the
   callback queue. The new code's release store and acquire load of the
   flag do not prevent it, since neither is ordered with the fetch_subs.
   At -O0 without LSE, clang builds every fetch_sub as an LDAXR/STLXR
   loop whatever its order, so -DRELAXED changes nothing there.

   Each run of the lock's exchange that finds the lock held writes back
   the 1 it read, so the tests bound that with --stutter 1; without the
   bound, the search with LSE does not finish in 10 minutes.

   The functions are copied from completion_queue.cc with tracing,
   debug-only tag checks, reference counts on the queue, and the
   EventEngine callbacks left out. gpr_mu is a spinlock (an acquire
   exchange and a release store) standing in for the mutex. The pollset's
   kick does nothing and its shutdown counts the calls in shutdowns; the
   next queue's event queue counts pushes. */

#include <atomic>

#ifdef RELAXED
#define memory_order_acq_rel memory_order_relaxed
#endif

typedef long intptr_t;
typedef unsigned long uintptr_t;

int check_failed;
int shutdowns;

#define GRPC_CHECK(c) do { if (!(c)) check_failed = 1; } while (0)
#define GRPC_CHECK_EQ(a, b) GRPC_CHECK((a) == (b))
#define GRPC_CQ_INTERNAL_REF(cq, reason) do { } while (0)
#define GRPC_CQ_INTERNAL_UNREF(cq, reason) do { } while (0)

struct gpr_mu { int locked; };
static void gpr_mu_lock(gpr_mu* mu) {
  while (__atomic_exchange_n(&mu->locked, 1, __ATOMIC_ACQUIRE) != 0) {
  }
}
static void gpr_mu_unlock(gpr_mu* mu) {
  __atomic_store_n(&mu->locked, 0, __ATOMIC_RELEASE);
}

struct grpc_error_handle {
  bool ok() const { return true; }
};
struct grpc_pollset;
struct grpc_pollset_worker;
struct grpc_closure;
struct cq_poller_vtable {
  grpc_error_handle (*kick)(grpc_pollset* pollset,
                            grpc_pollset_worker* pollset_worker);
  void (*shutdown)(grpc_pollset* pollset, grpc_closure* closure);
};
static grpc_error_handle stub_kick(grpc_pollset*, grpc_pollset_worker*) {
  return grpc_error_handle();
}
static void stub_shutdown(grpc_pollset*, grpc_closure*) {
  __atomic_fetch_add(&shutdowns, 1, __ATOMIC_RELAXED);
}
static const cq_poller_vtable poller = {stub_kick, stub_shutdown};

struct grpc_completion_queue {
  gpr_mu* mu;
  const cq_poller_vtable* poller_vtable;
  grpc_closure* pollset_shutdown_done;
};
#define POLLSET_FROM_CQ(cq) ((grpc_pollset*)nullptr)

struct grpc_cq_completion {
  void* tag;
  void (*done)(void* done_arg, grpc_cq_completion* storage);
  void* done_arg;
  uintptr_t next;
};

#ifdef OLD
#define SHUTDOWN_CALLED bool shutdown_called = false
#else
#define SHUTDOWN_CALLED std::atomic<bool> shutdown_called{false}
#endif

#if defined(PLUCK)
struct plucker {
  grpc_pollset_worker** worker;
  void* tag;
};
/* Every member has an initializer, so that cq_data is initialized
   statically, not by a constructor that armem would not run. */
struct cq_pluck_data {
  grpc_cq_completion completed_head{};
  grpc_cq_completion* completed_tail = nullptr;
  std::atomic<intptr_t> pending_events{1};
  std::atomic<intptr_t> things_queued_ever{0};
  std::atomic<bool> shutdown{false};
  SHUTDOWN_CALLED;
  int num_pluckers = 0;
  plucker pluckers[1] = {};
};
static cq_pluck_data cq_data;
#elif defined(CALLBACK)
struct cq_callback_data {
  std::atomic<intptr_t> pending_events{1};
  SHUTDOWN_CALLED;
};
static cq_callback_data cq_data;
#else
/* CqEventQueue, counting pushes. Push returns whether the queue was
   empty; only P1 pushes. */
struct CqEventQueue {
  int items = 0;
  bool Push(grpc_cq_completion*) { return items++ == 0; }
};
struct cq_next_data {
  CqEventQueue queue;
  std::atomic<intptr_t> things_queued_ever{0};
  std::atomic<intptr_t> pending_events{1};
  SHUTDOWN_CALLED;
};
static cq_next_data cq_data;
static grpc_completion_queue* g_cached_cq;
static grpc_cq_completion* g_cached_event;
#endif
#define DATA_FROM_CQ(cq) ((void*)&cq_data)

static gpr_mu mu;
static grpc_completion_queue the_cq = {&mu, &poller, nullptr};
static grpc_cq_completion storage;
static void done(void*, grpc_cq_completion*) {}

#if defined(PLUCK)

static void cq_finish_shutdown_pluck(grpc_completion_queue* cq) {
  cq_pluck_data* cqd = static_cast<cq_pluck_data*> DATA_FROM_CQ(cq);

#ifdef OLD
  GRPC_CHECK(cqd->shutdown_called);
#else
  GRPC_CHECK(cqd->shutdown_called.load(std::memory_order_acquire));
#endif
  GRPC_CHECK(!cqd->shutdown.load(std::memory_order_relaxed));
  cqd->shutdown.store(true, std::memory_order_relaxed);

  cq->poller_vtable->shutdown(POLLSET_FROM_CQ(cq), cq->pollset_shutdown_done);
}

static void cq_shutdown_pluck(grpc_completion_queue* cq) {
  cq_pluck_data* cqd = static_cast<cq_pluck_data*> DATA_FROM_CQ(cq);

  GRPC_CQ_INTERNAL_REF(cq, "shutting_down (pluck cq)");
  gpr_mu_lock(cq->mu);
#ifdef OLD
  if (cqd->shutdown_called) {
#else
  if (cqd->shutdown_called.load(std::memory_order_relaxed)) {
#endif
    gpr_mu_unlock(cq->mu);
    GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down (pluck cq)");
    return;
  }
#ifdef OLD
  cqd->shutdown_called = true;
#else
  cqd->shutdown_called.store(true, std::memory_order_release);
#endif
  if (cqd->pending_events.fetch_sub(1, std::memory_order_acq_rel) == 1) {
    cq_finish_shutdown_pluck(cq);
  }
  gpr_mu_unlock(cq->mu);
  GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down (pluck cq)");
}

static void cq_end_op_for_pluck(
    grpc_completion_queue* cq, void* tag, grpc_error_handle error,
    void (*done)(void* done_arg, grpc_cq_completion* storage), void* done_arg,
    grpc_cq_completion* storage, bool /*internal*/) {
  cq_pluck_data* cqd = static_cast<cq_pluck_data*> DATA_FROM_CQ(cq);
  int is_success = (error.ok());

  storage->tag = tag;
  storage->done = done;
  storage->done_arg = done_arg;
  storage->next = reinterpret_cast<uintptr_t>(&cqd->completed_head) |
                  static_cast<uintptr_t>(is_success);

  gpr_mu_lock(cq->mu);

  // Add to the list of completions
  cqd->things_queued_ever.fetch_add(1, std::memory_order_relaxed);
  cqd->completed_tail->next =
      reinterpret_cast<uintptr_t>(storage) | (1u & cqd->completed_tail->next);
  cqd->completed_tail = storage;

  if (cqd->pending_events.fetch_sub(1, std::memory_order_acq_rel) == 1) {
    cq_finish_shutdown_pluck(cq);
    gpr_mu_unlock(cq->mu);
  } else {
    grpc_pollset_worker* pluck_worker = nullptr;
    for (int i = 0; i < cqd->num_pluckers; i++) {
      if (cqd->pluckers[i].tag == tag) {
        pluck_worker = *cqd->pluckers[i].worker;
        break;
      }
    }

    grpc_error_handle kick_error =
        cq->poller_vtable->kick(POLLSET_FROM_CQ(cq), pluck_worker);
    gpr_mu_unlock(cq->mu);
    (void)kick_error;
  }
}

extern "C" void setup(void) {
  cq_data.completed_head.next = reinterpret_cast<uintptr_t>(&cq_data.completed_head);
  cq_data.completed_tail = &cq_data.completed_head;
  cq_data.pending_events.fetch_add(1, std::memory_order_relaxed);
}
extern "C" void t0(void) { cq_shutdown_pluck(&the_cq); }
extern "C" void t1(void) {
  cq_end_op_for_pluck(&the_cq, &storage, grpc_error_handle(), done, nullptr, &storage, false);
}

#elif defined(CALLBACK)

static void cq_finish_shutdown_callback(grpc_completion_queue* cq) {
  cq_callback_data* cqd = static_cast<cq_callback_data*> DATA_FROM_CQ(cq);

#ifdef OLD
  GRPC_CHECK(cqd->shutdown_called);
#else
  GRPC_CHECK(cqd->shutdown_called.load(std::memory_order_acquire));
#endif

  cq->poller_vtable->shutdown(POLLSET_FROM_CQ(cq), cq->pollset_shutdown_done);
}

static void cq_shutdown_callback(grpc_completion_queue* cq) {
  cq_callback_data* cqd = static_cast<cq_callback_data*> DATA_FROM_CQ(cq);

  GRPC_CQ_INTERNAL_REF(cq, "shutting_down (callback cq)");
  gpr_mu_lock(cq->mu);
#ifdef OLD
  if (cqd->shutdown_called) {
#else
  if (cqd->shutdown_called.load(std::memory_order_relaxed)) {
#endif
    gpr_mu_unlock(cq->mu);
    GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down (callback cq)");
    return;
  }
#ifdef OLD
  cqd->shutdown_called = true;
#else
  cqd->shutdown_called.store(true, std::memory_order_release);
#endif
  if (cqd->pending_events.fetch_sub(1, std::memory_order_acq_rel) == 1) {
    gpr_mu_unlock(cq->mu);
    cq_finish_shutdown_callback(cq);
  } else {
    gpr_mu_unlock(cq->mu);
  }
  GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down (callback cq)");
}

static void cq_end_op_for_callback(
    grpc_completion_queue* cq, void* tag, grpc_error_handle error,
    void (*done)(void* done_arg, grpc_cq_completion* storage), void* done_arg,
    grpc_cq_completion* storage, bool internal) {
  (void)internal;
  (void)tag;
  (void)error;
  cq_callback_data* cqd = static_cast<cq_callback_data*> DATA_FROM_CQ(cq);

  // The callback-based CQ isn't really a queue at all and thus has no need
  // for reserved storage. Invoke the done callback right away to release it.
  done(done_arg, storage);

  if (cqd->pending_events.fetch_sub(1, std::memory_order_acq_rel) == 1) {
    cq_finish_shutdown_callback(cq);
  }
}

extern "C" void setup(void) {
  cq_data.pending_events.fetch_add(1, std::memory_order_relaxed);
}
extern "C" void t0(void) { cq_shutdown_callback(&the_cq); }
extern "C" void t1(void) {
  cq_end_op_for_callback(&the_cq, &storage, grpc_error_handle(), done, nullptr, &storage, false);
}

#else

static void cq_finish_shutdown_next(grpc_completion_queue* cq) {
  cq_next_data* cqd = static_cast<cq_next_data*> DATA_FROM_CQ(cq);

#ifdef OLD
  GRPC_CHECK(cqd->shutdown_called);
#else
  GRPC_CHECK(cqd->shutdown_called.load(std::memory_order_acquire));
#endif
  GRPC_CHECK_EQ(cqd->pending_events.load(std::memory_order_relaxed), 0);

  cq->poller_vtable->shutdown(POLLSET_FROM_CQ(cq), cq->pollset_shutdown_done);
}

static void cq_shutdown_next(grpc_completion_queue* cq) {
  cq_next_data* cqd = static_cast<cq_next_data*> DATA_FROM_CQ(cq);

  GRPC_CQ_INTERNAL_REF(cq, "shutting_down");
  gpr_mu_lock(cq->mu);
#ifdef OLD
  if (cqd->shutdown_called) {
#else
  if (cqd->shutdown_called.load(std::memory_order_relaxed)) {
#endif
    gpr_mu_unlock(cq->mu);
    GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down");
    return;
  }
#ifdef OLD
  cqd->shutdown_called = true;
#else
  cqd->shutdown_called.store(true, std::memory_order_release);
#endif
  // Doing acq/release fetch_sub here to match with
  // cq_begin_op_for_next and cq_end_op_for_next functions which read/write
  // on this counter without necessarily holding a lock on cq
  if (cqd->pending_events.fetch_sub(1, std::memory_order_acq_rel) == 1) {
    cq_finish_shutdown_next(cq);
  }
  gpr_mu_unlock(cq->mu);
  GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down");
}

static void cq_end_op_for_next(
    grpc_completion_queue* cq, void* tag, grpc_error_handle error,
    void (*done)(void* done_arg, grpc_cq_completion* storage), void* done_arg,
    grpc_cq_completion* storage, bool /*internal*/) {
  cq_next_data* cqd = static_cast<cq_next_data*> DATA_FROM_CQ(cq);
  int is_success = (error.ok());

  storage->tag = tag;
  storage->done = done;
  storage->done_arg = done_arg;
  storage->next = static_cast<uintptr_t>(is_success);

  if (g_cached_cq == cq && g_cached_event == nullptr) {
    g_cached_event = storage;
  } else {
    // Add the completion to the queue
    bool is_first = cqd->queue.Push(storage);
    cqd->things_queued_ever.fetch_add(1, std::memory_order_relaxed);
    // Since we do not hold the cq lock here, it is important to do an 'acquire'
    // load here (instead of a 'no_barrier' load) to match with the release
    // store
    // (done via pending_events.fetch_sub(1, ACQ_REL)) in cq_shutdown_next
    //
    if (cqd->pending_events.load(std::memory_order_acquire) != 1) {
      // Only kick if this is the first item queued
      if (is_first) {
        gpr_mu_lock(cq->mu);
        grpc_error_handle kick_error =
            cq->poller_vtable->kick(POLLSET_FROM_CQ(cq), nullptr);
        gpr_mu_unlock(cq->mu);
        (void)kick_error;
      }
      if (cqd->pending_events.fetch_sub(1, std::memory_order_acq_rel) == 1) {
        GRPC_CQ_INTERNAL_REF(cq, "shutting_down");
        gpr_mu_lock(cq->mu);
        cq_finish_shutdown_next(cq);
        gpr_mu_unlock(cq->mu);
        GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down");
      }
    } else {
      GRPC_CQ_INTERNAL_REF(cq, "shutting_down");
      cqd->pending_events.store(0, std::memory_order_release);
      gpr_mu_lock(cq->mu);
      cq_finish_shutdown_next(cq);
      gpr_mu_unlock(cq->mu);
      GRPC_CQ_INTERNAL_UNREF(cq, "shutting_down");
    }
  }
}

extern "C" void setup(void) {
  cq_data.pending_events.fetch_add(1, std::memory_order_relaxed);
}
extern "C" void t0(void) { cq_shutdown_next(&the_cq); }
extern "C" void t1(void) {
  cq_end_op_for_next(&the_cq, &storage, grpc_error_handle(), done, nullptr, &storage, false);
}

#endif
