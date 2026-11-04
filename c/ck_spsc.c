/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's ring buffer (ck/include/ck_ring.h, unchanged) with one
   producer and one consumer. The ring has 2 slots, which holds 1 entry:
   ck_ring keeps a slot free to tell a full ring from an empty one. The
   producer sends pointers to the items 1, 2, 3; the consumer receives
   them and reads each item. The third item goes in the first item's slot.

   -DOLD_FENCE restores the barrier ck_ring used before commit c7945fe
   (June 2026): a store fence (DMB ISHST) between the consumer's read of a
   slot and its update of c_head, instead of a load-store fence (DMB SY).
   ARM may perform the read after the update, but here that is harmless:
   the slot is written again only after the update of the following
   dequeue, which runs DMB ISHLD first and so orders the earlier read
   before it. Compare ck_spmc.c. */

#include <ck_pr.h>
#ifdef OLD_FENCE
#define ck_pr_fence_load_store ck_pr_fence_store
#endif
#include <ck_ring.h>

#define ITEMS 3

static ck_ring_buffer_t buf[2];
ck_ring_t ring = { .size = 2, .mask = 1 };
static unsigned long item[ITEMS] = { 1, 2, 3 };
unsigned long got[ITEMS];

void producer(void) {
  for (int i = 0; i < ITEMS; i++)
    while (!ck_ring_enqueue_spsc(&ring, buf, &item[i])) {}
}

void consumer(void) {
  for (int i = 0; i < ITEMS; i++) {
    unsigned long *p;
    while (!ck_ring_dequeue_spsc(&ring, buf, &p)) {}
    got[i] = *p;
  }
}

/* For -O0, where the copies of entries call it. Like a libc memcpy, it
   copies an aligned word with one load and one store. */
__attribute__((no_builtin)) void *memcpy(void *d, const void *s, size_t n) {
  if (((unsigned long)d | (unsigned long)s | n) % 8 == 0) {
    unsigned long *dw = d;
    const unsigned long *sw = s;
    for (; n; n -= 8)
      *dw++ = *sw++;
  } else {
    char *dc = d;
    const char *sc = s;
    while (n--)
      *dc++ = *sc++;
  }
  return d;
}
