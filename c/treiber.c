/* A Treiber stack: a lock-free stack kept as a linked list, whose top is
   changed only by compare-and-swap. One thread pushes 1, then 2; another
   pops once, returning a value or EMPTY. The nodes are static,
   since each is pushed once and never reused (so there is no ABA
   problem). A node's value is plain memory. Its link is a relaxed atomic,
   as in most C11 implementations, since a pop may read it while a push
   writes it; this also keeps clang from moving the push's store of the
   link out of its retry loop, after the compare-and-swap that publishes
   the node.

   Each macro below names one ordering it needs; defining it as
   __ATOMIC_RELAXED (e.g. -DPUSH=__ATOMIC_RELAXED) removes it:
   * PUSH: the popper can see a node on the stack before the node's value
     and link are in it. It then reads a value never pushed, or follows a
     stale link and loses the node below. A bug on ARM only.
   * POP: harmless on ARM hardware: the popper reads the node's value and
     link through the pointer it read from top, and that address
     dependency keeps those reads after the read of top. It is still a
     data race in C, so a compiler may break it. */

typedef __UINT64_TYPE__ u64;

#ifndef PUSH
#define PUSH __ATOMIC_RELEASE /* push: compare-and-swap of top */
#endif
#ifndef POP
#define POP __ATOMIC_ACQUIRE /* pop: read of top */
#endif

#define EMPTY 99

struct node {
  u64 val;
  struct node *next;
};

struct node *top;
struct node n1, n2;
u64 got; /* the value popped */

static void push(struct node *n, u64 val) {
  struct node *old = __atomic_load_n(&top, __ATOMIC_RELAXED);
  n->val = val;
  do
    __atomic_store_n(&n->next, old, __ATOMIC_RELAXED);
  while (!__atomic_compare_exchange_n(&top, &old, n, 0, PUSH, __ATOMIC_RELAXED));
}

static u64 pop(void) {
  struct node *old = __atomic_load_n(&top, POP);
  do
    if (!old)
      return EMPTY;
  while (!__atomic_compare_exchange_n(&top, &old, __atomic_load_n(&old->next, __ATOMIC_RELAXED), 0, POP, POP));
  return old->val;
}

void pusher(void) {
  push(&n1, 1);
  push(&n2, 2);
}

void popper(void) { got = pop(); }
