/* cflags: -std=c++17 -ffreestanding -nostdlibinc -fno-exceptions -fno-rtti */
/* tokio's Notify, from tokio/src/sync/notify.rs and util/linked_list.rs
   (MIT License) as in tokio 1.54.1 (November 2026), with one Notified
   future and one notifier.

   Commit d4fb4bb9 (tokio 1.54.0, #8325) changed every access to
   Notify::state from SeqCst to Acquire (loads), Release (stores) or
   AcqRel/Acquire (compare-exchanges, and notify_waiters' fetch_add). Its
   argument: every lock-avoidance decision is made by a read-modify-write,
   which reads the latest value, every stale load is checked again by a
   later read-modify-write or under the waiters mutex, and what the
   orderings must carry is the happens-before from data written before
   notify_one or notify_waiters to the task that consumes the
   notification. -DOLD restores SeqCst everywhere. To show what the
   harness catches, -DRELAXED_TAKE makes poll's optimistic NOTIFIED to
   EMPTY compare-exchange (which takes a notify_one permit) relaxed, and
   -DRELAXED_INIT makes poll's first load of state (which sees that
   notify_waiters was called) relaxed.

   P0 creates the future and polls it once. If that returns Pending, it
   loads `woken` once (relaxed, so that only Notify carries data) and, if
   the waker has run, polls again. Once Ready, it reads `data`. P1 writes
   `data` and notifies.

   Default: P1 calls notify_one. The bug is a lost wakeup (P0 not ready
   and its waker never run) or a stale read (P0 ready but `data` 0).

   -DWAITERS: P1 calls notify_waiters instead, which only wakes futures
   created before it. P0 publishes `created` (release) after notified()
   and P1 notifies only if it sees it (acquire); `skipped` records that it
   didn't. The bug is as before, when P1 notified.

   -DDROP (with notify_one): P0 polls once and drops the future, which
   passes on a notify_one notification it received but didn't consume.
   The bug is the permit lost (P0 not ready and state not NOTIFIED) or
   duplicated (P0 ready and state NOTIFIED), or a stale read.

   Left out: notify_last (the same with the other end of the list),
   WakeList batching (one waiter fits), tracing, taskdump, panics. A Waker
   is a flag; wake() stores 1 to `woken`. The Mutex is a spinlock (an
   acquire compare-and-swap and a release store). */

typedef unsigned long usize;

#ifdef OLD
#define ACQUIRE __ATOMIC_SEQ_CST
#define RELEASE __ATOMIC_SEQ_CST
#define ACQREL __ATOMIC_SEQ_CST
#else
#define ACQUIRE __ATOMIC_ACQUIRE
#define RELEASE __ATOMIC_RELEASE
#define ACQREL __ATOMIC_ACQ_REL
#endif
#ifdef RELAXED_TAKE
#define TAKE_ORDER __ATOMIC_RELAXED
#define TAKE_FAILURE __ATOMIC_RELAXED
#else
#define TAKE_ORDER ACQREL
#define TAKE_FAILURE ACQUIRE
#endif
#ifdef RELAXED_INIT
#define INIT_ORDER __ATOMIC_RELAXED
#else
#define INIT_ORDER ACQUIRE
#endif

// Results, read by the litmus condition.
int ready, d, woken, skipped;
int data, created, dropmode;

struct Mutex { int locked; };
static void lock(Mutex* mu) {
  int expected = 0;
  while (!__atomic_compare_exchange_n(&mu->locked, &expected, 1, false,
                                      __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
    expected = 0;
  }
}
static void unlock(Mutex* mu) { __atomic_store_n(&mu->locked, 0, __ATOMIC_RELEASE); }

// ===== util/linked_list.rs =====

struct Waiter;
struct Pointers { Waiter* prev; Waiter* next; };

// Waiter: pointers, waker (a flag: Some or None), notification.
struct Waiter {
  Pointers pointers;
  int waker;
  usize notification;
};

struct LinkedList { Waiter* head; Waiter* tail; };

static void push_front(LinkedList* l, Waiter* ptr) {
  ptr->pointers.next = l->head;
  ptr->pointers.prev = nullptr;
  if (l->head) l->head->pointers.prev = ptr;
  l->head = ptr;
  if (!l->tail) l->tail = ptr;
}

static Waiter* pop_back(LinkedList* l) {
  Waiter* last = l->tail;
  if (!last) return nullptr;
  l->tail = last->pointers.prev;
  if (Waiter* prev = last->pointers.prev) prev->pointers.next = nullptr;
  else l->head = nullptr;
  last->pointers.prev = nullptr;
  last->pointers.next = nullptr;
  return last;
}

static bool is_empty(LinkedList* l) { return l->head == nullptr; }

static Waiter* remove(LinkedList* l, Waiter* node) {
  if (Waiter* prev = node->pointers.prev) {
    prev->pointers.next = node->pointers.next;
  } else {
    if (l->head != node) return nullptr;
    l->head = node->pointers.next;
  }
  if (Waiter* next = node->pointers.next) {
    next->pointers.prev = node->pointers.prev;
  } else {
    if (l->tail != node) return nullptr;
    l->tail = node->pointers.prev;
  }
  node->pointers.next = nullptr;
  node->pointers.prev = nullptr;
  return node;
}

// GuardedLinkedList: a circular list through the guard node.
static void into_guarded(LinkedList* l, Waiter* guard) {
  if (Waiter* head = l->head) {
    head->pointers.prev = guard;
    guard->pointers.next = head;
    Waiter* tail = l->tail;
    tail->pointers.next = guard;
    guard->pointers.prev = tail;
  } else {
    guard->pointers.prev = guard;
    guard->pointers.next = guard;
  }
}

static Waiter* guarded_pop_back(Waiter* guard) {
  Waiter* last = guard->pointers.prev;
  if (last == guard) return nullptr;
  Waiter* before_last = last->pointers.prev;
  guard->pointers.prev = before_last;
  before_last->pointers.next = guard;
  last->pointers.prev = nullptr;
  last->pointers.next = nullptr;
  return last;
}

// ===== sync/notify.rs =====

static const usize NOTIFICATION_NONE = 0, NOTIFICATION_ONE = 1, NOTIFICATION_ALL = 2;

static void store_release(Waiter* w, usize n) {
  __atomic_store_n(&w->notification, n, __ATOMIC_RELEASE);
}
static usize load_notification(Waiter* w, int order) {
  return __atomic_load_n(&w->notification, order);
}
static void clear(Waiter* w) {
  __atomic_store_n(&w->notification, NOTIFICATION_NONE, __ATOMIC_RELAXED);
}

static const usize NOTIFY_WAITERS_SHIFT = 2;
static const usize STATE_MASK = (1 << NOTIFY_WAITERS_SHIFT) - 1;
static const usize NOTIFY_WAITERS_CALLS_MASK = ~STATE_MASK;
static const usize EMPTY = 0, WAITING = 1, NOTIFIED = 2;

static usize set_state(usize data, usize state) {
  return (data & NOTIFY_WAITERS_CALLS_MASK) | (state & STATE_MASK);
}
static usize get_state(usize data) { return data & STATE_MASK; }
static usize get_num_notify_waiters_calls(usize data) {
  return (data & NOTIFY_WAITERS_CALLS_MASK) >> NOTIFY_WAITERS_SHIFT;
}
static usize inc_num_notify_waiters_calls(usize data) {
  return data + (1 << NOTIFY_WAITERS_SHIFT);
}

// Notify
usize notify_state;
static Mutex waiters_mu;
static LinkedList waiters;

static bool cas(usize* p, usize* curr, usize desired) {
  return __atomic_compare_exchange_n(p, curr, desired, false, ACQREL, ACQUIRE);
}

static void wake() { __atomic_store_n(&woken, 1, __ATOMIC_RELAXED); }

// Returns whether a waker was taken (to be woken after unlocking).
static bool notify_locked(LinkedList* waiters, usize* state, usize curr) {
  switch (get_state(curr)) {
    case EMPTY:
    case NOTIFIED: {
      usize actual = curr;
      if (cas(state, &actual, set_state(curr, NOTIFIED))) return false;
      __atomic_store_n(state, set_state(actual, NOTIFIED), RELEASE);
      return false;
    }
    default: {  // WAITING
      Waiter* waiter = pop_back(waiters);  // Fifo
      int waker = waiter->waker;
      waiter->waker = 0;
      store_release(waiter, NOTIFICATION_ONE);
      if (is_empty(waiters)) {
        __atomic_store_n(state, set_state(curr, EMPTY), RELEASE);
      }
      return waker;
    }
  }
}

static void notify_one() {
  usize curr = __atomic_load_n(&notify_state, ACQUIRE);
  while (get_state(curr) == EMPTY || get_state(curr) == NOTIFIED) {
    usize new_ = set_state(curr, NOTIFIED);
    if (cas(&notify_state, &curr, new_)) return;
  }
  lock(&waiters_mu);
  curr = __atomic_load_n(&notify_state, ACQUIRE);
  bool waker = notify_locked(&waiters, &notify_state, curr);
  unlock(&waiters_mu);
  if (waker) wake();
}

static void notify_waiters() {
  // lock_waiter_list
  lock(&waiters_mu);
  usize curr = __atomic_load_n(&notify_state, ACQUIRE);
  // inner_notify_waiters
  if (get_state(curr) == EMPTY || get_state(curr) == NOTIFIED) {
    __atomic_fetch_add(&notify_state, (usize)1 << NOTIFY_WAITERS_SHIFT, ACQREL);
    unlock(&waiters_mu);
    return;
  }
  usize new_state = set_state(inc_num_notify_waiters_calls(curr), EMPTY);
  __atomic_store_n(&notify_state, new_state, RELEASE);
  Waiter guard = {};
  into_guarded(&waiters, &guard);  // std::mem::take
  waiters.head = waiters.tail = nullptr;
  bool wakers = false;
  while (Waiter* waiter = guarded_pop_back(&guard)) {
    if (waiter->waker) {
      waiter->waker = 0;
      wakers = true;
    }
    store_release(waiter, NOTIFICATION_ALL);
  }
  // The list is empty, so NotifyWaitersList's drop takes no lock.
  unlock(&waiters_mu);
  if (wakers) wake();
}

// Notified
enum State { Init, Waiting, Done };
struct Notified {
  State state;
  usize notify_waiters_calls;
  Waiter waiter;
};

static Notified the_notified;

static void notified(Notified* n) {
  usize state = __atomic_load_n(&notify_state, ACQUIRE);
  n->state = Init;
  n->notify_waiters_calls = get_num_notify_waiters_calls(state);
  n->waiter = Waiter{};
}

// poll_notified with a context (a waker); returns whether Ready.
static bool poll_notified(Notified* n) {
  Waiter* waiter = &n->waiter;
  for (;;) {
    switch (n->state) {
      case Init: {
        usize curr = __atomic_load_n(&notify_state, INIT_ORDER);
        if (get_num_notify_waiters_calls(curr) != n->notify_waiters_calls) {
          n->state = Done;
          continue;
        }
        usize expected = set_state(curr, NOTIFIED);
        if (__atomic_compare_exchange_n(&notify_state, &expected,
                                        set_state(curr, EMPTY), false,
                                        TAKE_ORDER, TAKE_FAILURE)) {
          n->state = Done;
          continue;
        }
        lock(&waiters_mu);
        curr = __atomic_load_n(&notify_state, ACQUIRE);
        if (get_num_notify_waiters_calls(curr) != n->notify_waiters_calls) {
          n->state = Done;
          unlock(&waiters_mu);
          continue;
        }
        for (;;) {
          usize s = get_state(curr);
          if (s == EMPTY) {
            if (cas(&notify_state, &curr, set_state(curr, WAITING))) break;
            // assert_eq!(get_state(actual), NOTIFIED); curr = actual
          } else if (s == WAITING) {
            break;
          } else {  // NOTIFIED
            if (cas(&notify_state, &curr, set_state(curr, EMPTY))) {
              n->state = Done;
              goto outer_continue;
            }
            // assert_eq!(get_state(actual), EMPTY); curr = actual
          }
        }
        waiter->waker = 1;
        push_front(&waiters, waiter);
        n->state = Waiting;
        unlock(&waiters_mu);
        return false;
      outer_continue:
        unlock(&waiters_mu);
        continue;
      }
      case Waiting: {
        if (load_notification(waiter, __ATOMIC_ACQUIRE) != NOTIFICATION_NONE) {
          waiter->waker = 0;
          clear(waiter);
          n->state = Done;
          return true;
        }
        lock(&waiters_mu);
        if (load_notification(waiter, __ATOMIC_RELAXED) != NOTIFICATION_NONE) {
          waiter->waker = 0;
          clear(waiter);
          unlock(&waiters_mu);
          n->state = Done;
          return true;
        }
        usize curr = __atomic_load_n(&notify_state, ACQUIRE);
        if (get_num_notify_waiters_calls(curr) != n->notify_waiters_calls) {
          waiter->waker = 0;
          remove(&waiters, waiter);
          n->state = Done;
        } else {
          waiter->waker = 1;
          unlock(&waiters_mu);
          return false;
        }
        unlock(&waiters_mu);
        continue;
      }
      case Done:
        return true;
    }
  }
}

static void drop_notified(Notified* n) {
  Waiter* waiter = &n->waiter;
  if (n->state != Waiting) return;
  lock(&waiters_mu);
  usize notify_state_ = __atomic_load_n(&notify_state, ACQUIRE);
  usize notification = load_notification(waiter, __ATOMIC_RELAXED);
  remove(&waiters, waiter);
  if (is_empty(&waiters) && get_state(notify_state_) == WAITING) {
    notify_state_ = set_state(notify_state_, EMPTY);
    __atomic_store_n(&notify_state, notify_state_, RELEASE);
  }
  bool waker = false;
  if (notification == NOTIFICATION_ONE) {
    waker = notify_locked(&waiters, &notify_state, notify_state_);
  }
  unlock(&waiters_mu);
  if (waker) wake();
}

extern "C" void setup(void) {
#ifdef DROP
  dropmode = 1;
#endif
}

extern "C" void t0(void) {
  Notified* n = &the_notified;
  notified(n);
#ifdef WAITERS
  __atomic_store_n(&created, 1, __ATOMIC_RELEASE);
#endif
  bool r = poll_notified(n);
#ifndef DROP
  if (!r && __atomic_load_n(&woken, __ATOMIC_RELAXED)) r = poll_notified(n);
#endif
  if (r) {
    ready = 1;
    d = __atomic_load_n(&data, __ATOMIC_RELAXED);
  }
#ifdef DROP
  drop_notified(n);
#endif
}

extern "C" void t1(void) {
#ifdef WAITERS
  if (!__atomic_load_n(&created, __ATOMIC_ACQUIRE)) {
    skipped = 1;
    return;
  }
#endif
  __atomic_store_n(&data, 1, __ATOMIC_RELAXED);
#ifdef WAITERS
  notify_waiters();
#else
  notify_one();
#endif
}
