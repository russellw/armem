/* A single-producer, single-consumer ring buffer of N slots. The producer
   waits for a free slot, writes an item into it, then publishes it by
   advancing head. The consumer waits for an item, reads it, then frees the
   slot by advancing tail. The buffer itself is plain (non-atomic) memory,
   as in most real implementations.

   Each macro below names one ordering it needs; defining it as
   __ATOMIC_RELAXED (e.g. -DPUB=__ATOMIC_RELAXED) removes it:
   * PUB: the consumer can read the slot before the item is in it.
   * FREE: the consumer's read of a slot can see the next item, which the
     producer wrote after seeing the slot freed.
   Both are bugs on ARM only. Two others behave differently:
   * SEE: with a relaxed load of head the C compiler is free to reuse its
     earlier read of buf[0] (which clang -O2 does when N is 1), so the
     compiled code is wrong on every machine. At -O0, -O1 and -Os clang
     reads it again, and the bug shows on ARM only.
   * ROOM: harmless on ARM hardware: the producer's store to the slot is
     control dependent on its load of tail, and ARM does not let a store
     become visible before a load it depends on. It is still a data race
     in C, so a compiler may break it. */

typedef __UINT64_TYPE__ u64;

#ifndef PUB
#define PUB __ATOMIC_RELEASE /* producer: store head */
#endif
#ifndef SEE
#define SEE __ATOMIC_ACQUIRE /* consumer: load head */
#endif
#ifndef FREE
#define FREE __ATOMIC_RELEASE /* consumer: store tail */
#endif
#ifndef ROOM
#define ROOM __ATOMIC_ACQUIRE /* producer: load tail */
#endif
#ifndef N
#define N 1 /* slots */
#endif
#ifndef ITEMS
#define ITEMS 2 /* items sent: 1, 2, ... */
#endif

u64 buf[N];
unsigned head, tail;
u64 got[ITEMS];

void producer(void) {
  for (unsigned i = 0; i < ITEMS; i++) {
    unsigned h = __atomic_load_n(&head, __ATOMIC_RELAXED);
    while (h - __atomic_load_n(&tail, ROOM) == N) {}
    buf[h % N] = i + 1;
    __atomic_store_n(&head, h + 1, PUB);
  }
}

void consumer(void) {
  for (unsigned i = 0; i < ITEMS; i++) {
    unsigned t = __atomic_load_n(&tail, __ATOMIC_RELAXED);
    while (__atomic_load_n(&head, SEE) == t) {}
    got[i] = buf[t % N];
    __atomic_store_n(&tail, t + 1, FREE);
  }
}
