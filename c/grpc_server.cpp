/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti -Igrpc/stub */
/* A server's request for a call racing the server's shutdown, from gRPC's
   src/core/server/server.cc, server.h and src/core/util/mpscq.cc (Apache
   License 2.0, see grpc/LICENSE) as at commit 28d1790b9c (November 2026).

   grpc_server_request_call (and _registered_call) begins an operation on
   the notification queue, then Server::QueueRequestedCall checks
   ShutdownCalled() and, if shutdown has not been called, pushes the
   request onto a RealRequestMatcher's lock-free queue, where it waits for
   an incoming call. Server::ShutdownAndNotify fails every queued request
   (KillPendingWorkLocked, under mu_call_), and only then marks shutdown as
   called (ShutdownUnrefOnShutdownCall). If no request is in flight, that
   calls MaybeFinishShutdown, which fails the queued requests once more and,
   when no channels, connections or listeners are left, publishes the
   shutdown. Nothing fails a request pushed after that, so its operation
   never ends: the queue's shutdown never finishes, a thread waiting on it
   for GRPC_QUEUE_SHUTDOWN waits forever, and ~RealRequestMatcher's
   GRPC_CHECK_EQ(queue.Pop(), nullptr) fails if the server is destroyed.
   The callback API's matcher takes a shutdown ref (ShutdownRefOnRequest)
   around its work; this path takes none.

   This is an interleaving race, not one of memory order: QueueRequestedCall
   need only be delayed between its check and its push. In the Ruby
   extension the GVL serializes the two calls, so Ruby cannot hit it.

   P0 is QueueRequestedCall for one request; P1 is ShutdownAndNotify on a
   server with no channels, connections or listeners left, so that its
   MaybeFinishShutdown publishes at once. (With listeners, the last
   MaybeFinishShutdown comes later, from ListenerDestroyDone, which only
   moves the point after which a push is lost.) The condition is that the
   request is failed other than once, or that shutdown is not published.
   With -DQUEUED, setup() first queues another request, so P0's push is
   not the first and RequestCallWithPossiblePublish takes no lock.

   -DFIX_LOCK holds mu_call_ across QueueRequestedCall's check and push.
   -DFIX_RECHECK checks ShutdownCalled() again after the push and, if it
   is now set, fails what is queued itself, under mu_call_. That looks
   like store buffering (P0 pushes then loads the flag; P1 changes the
   flag then pops), which ARM allows unless both sides are ordered. But
   it holds even with -DRELAXED, which makes ShutdownCalled()'s load
   relaxed: for P1's pop to miss P0's node, the pop must reach its
   Push(&stub_), an acq_rel exchange of head_, the location P0's push
   exchanges too. Whichever of the two exchanges comes second reads from
   the first, so P0 sees the flag P1 changed before, or P1 sees P0's node.
   Without -DQUEUED, the push is the first, and the lock it takes orders
   the recheck as well.

   Left out: tracing, channelz, the pending-call queues (empty here), the
   completion queue itself (FailCall counts calls instead of ending the
   operation), StopListening and the channel broadcast. Mutexes are
   spinlocks (an acquire exchange and a release store). */

#include <atomic>

int failed0, failed1, published;

struct Mutex { int locked; };
static void Lock(Mutex* mu) {
  while (__atomic_exchange_n(&mu->locked, 1, __ATOMIC_ACQUIRE) != 0) {
  }
}
static void Unlock(Mutex* mu) {
  __atomic_store_n(&mu->locked, 0, __ATOMIC_RELEASE);
}

// src/core/util/mpscq.cc

struct Node {
  std::atomic<Node*> next;
};

struct MultiProducerSingleConsumerQueue {
  std::atomic<Node*> head_;
  Node* tail_;
  Node stub_;

  bool Push(Node* node) {
    node->next.store(nullptr, std::memory_order_relaxed);
    Node* prev = head_.exchange(node, std::memory_order_acq_rel);
    prev->next.store(node, std::memory_order_release);
    return prev == &stub_;
  }

  Node* PopAndCheckEnd(bool* empty) {
    Node* tail = tail_;
    Node* next = tail_->next.load(std::memory_order_acquire);
    if (tail == &stub_) {
      // indicates the list is actually (ephemerally) empty
      if (next == nullptr) {
        *empty = true;
        return nullptr;
      }
      tail_ = next;
      tail = next;
      next = tail->next.load(std::memory_order_acquire);
    }
    if (next != nullptr) {
      *empty = false;
      tail_ = next;
      return tail;
    }
    Node* head = head_.load(std::memory_order_acquire);
    if (tail != head) {
      *empty = false;
      // indicates a retry is in order: we're still adding
      return nullptr;
    }
    Push(&stub_);
    next = tail->next.load(std::memory_order_acquire);
    if (next != nullptr) {
      *empty = false;
      tail_ = next;
      return tail;
    }
    // indicates a retry is in order: we're still adding
    *empty = false;
    return nullptr;
  }
};

struct LockedMultiProducerSingleConsumerQueue {
  MultiProducerSingleConsumerQueue queue_;
  Mutex mu_;

  bool Push(Node* node) { return queue_.Push(node); }

  Node* Pop() {
    Lock(&mu_);
    bool empty = false;
    Node* node;
    do {
      node = queue_.PopAndCheckEnd(&empty);
    } while (node == nullptr && !empty);
    Unlock(&mu_);
    return node;
  }
};

// src/core/server/server.cc and server.h

struct RequestedCall {
  Node mpscq_node;
};

static RequestedCall the_rc, other_rc;
static LockedMultiProducerSingleConsumerQueue requests_per_cq_;
static Mutex mu_global_, mu_call_;
static std::atomic<int> shutdown_refs_;
static bool shutdown_published_;

// FailCall ends the request's operation on its queue with an error.
static void FailCall(RequestedCall* rc, int* failed) {
  if (rc == &the_rc) *failed += 1;
}

static bool ShutdownCalled() {
#ifdef RELAXED
  return (shutdown_refs_.load(std::memory_order_relaxed) & 1) == 0;
#endif
  return (shutdown_refs_.load(std::memory_order_acquire) & 1) == 0;
}

static bool ShutdownReady() {
  return shutdown_refs_.load(std::memory_order_acquire) == 0;
}

// RealRequestMatcher::KillRequests, for the one queue.
static void KillRequests(int* failed) {
  RequestedCall* rc;
  while ((rc = reinterpret_cast<RequestedCall*>(requests_per_cq_.Pop())) !=
         nullptr) {
    FailCall(rc, failed);
  }
}

// KillRequests then ZombifyPending, which has nothing to do here.
static void KillPendingWorkLocked(int* failed) { KillRequests(failed); }

// RealRequestMatcher::RequestCallWithPossiblePublish, with no pending
// calls, so the matching loop takes mu_call_, finds nothing and stops.
static void RequestCallWithPossiblePublish(RequestedCall* call) {
  if (requests_per_cq_.Push(&call->mpscq_node)) {
    Lock(&mu_call_);
    Unlock(&mu_call_);
  }
}

static void QueueRequestedCall(RequestedCall* rc) {
#ifdef FIX_LOCK
  Lock(&mu_call_);
  if (ShutdownCalled()) {
    Unlock(&mu_call_);
    FailCall(rc, &failed0);
    return;
  }
  bool first = requests_per_cq_.Push(&rc->mpscq_node);
  Unlock(&mu_call_);
  if (first) {  // the matching loop of RequestCallWithPossiblePublish
    Lock(&mu_call_);
    Unlock(&mu_call_);
  }
#else
  if (ShutdownCalled()) {
    FailCall(rc, &failed0);
    return;
  }
  RequestCallWithPossiblePublish(rc);
#ifdef FIX_RECHECK
  if (ShutdownCalled()) {
    Lock(&mu_call_);
    KillRequests(&failed0);
    Unlock(&mu_call_);
  }
#endif
#endif
}

// Called with mu_global_ held and mu_call_ not held.
static void MaybeFinishShutdown() {
  if (!ShutdownReady() || shutdown_published_) {
    return;
  }
  Lock(&mu_call_);
  KillPendingWorkLocked(&failed1);
  Unlock(&mu_call_);
  // No channels, connections or listeners are left.
  shutdown_published_ = true;
  published = 1;
}

static void ShutdownUnrefOnShutdownCall() {
  if (shutdown_refs_.fetch_sub(1, std::memory_order_acq_rel) == 1) {
    // There is no request in-flight.
    MaybeFinishShutdown();
  }
}

static void ShutdownAndNotify() {
  Lock(&mu_global_);
  if (shutdown_published_ || ShutdownCalled()) {
    Unlock(&mu_global_);
    return;
  }
  Lock(&mu_call_);
  KillPendingWorkLocked(&failed1);
  Unlock(&mu_call_);
  ShutdownUnrefOnShutdownCall();
  Unlock(&mu_global_);
}

extern "C" void setup(void) {
  MultiProducerSingleConsumerQueue* q = &requests_per_cq_.queue_;
  q->head_.store(&q->stub_, std::memory_order_relaxed);
  q->tail_ = &q->stub_;
  shutdown_refs_.store(1, std::memory_order_relaxed);
#ifdef QUEUED
  requests_per_cq_.Push(&other_rc.mpscq_node);
#endif
}

extern "C" void t0(void) { QueueRequestedCall(&the_rc); }
extern "C" void t1(void) { ShutdownAndNotify(); }
