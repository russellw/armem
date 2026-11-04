/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's ring buffer (ck/include/ck_ring.h, unchanged) with one
   producer and two consumers, which take one entry each with
   ck_ring_dequeue_spmc: each reads the next slot, then claims it by a
   compare-and-swap of c_head. The ring has 2 slots and holds 1 entry. The
   producer sends pointers to the items 1, 2, 3, so the third item goes in
   the first item's slot once both consumers have taken theirs.

   -DOLD_FENCE restores the barrier ck_ring used before commit c7945fe
   (June 2026): a store fence (DMB ISHST) between a consumer's read of the
   slot and its compare-and-swap, instead of a load-store fence (DMB SY).
   Then ARM may perform the read after the compare-and-swap. The other
   consumer's dequeue can follow it, and the producer reuse the slot,
   before the read: the first consumer receives item 3, which stays in the
   ring as well. The commit's message calls the old fence "overly weak in
   theory ... No impact on a real modern CPU"; the architecture allows the
   bug. In ck_spsc.c the old fence is enough. */

#include <ck_pr.h>
#ifdef OLD_FENCE
#define ck_pr_fence_load_store ck_pr_fence_store
#endif
#include <ck_ring.h>

#define ITEMS 3

static ck_ring_buffer_t buf[2];
ck_ring_t ring = { .size = 2, .mask = 1 };
static unsigned long item[ITEMS] = { 1, 2, 3 };
unsigned long got_a, got_b;

void producer(void) {
  for (int i = 0; i < ITEMS; i++)
    while (!ck_ring_enqueue_spmc(&ring, buf, &item[i])) {}
}

static unsigned long take(void) {
  unsigned long *p;
  while (!ck_ring_dequeue_spmc(&ring, buf, &p)) {}
  return *p;
}

void consumer_a(void) { got_a = take(); }
void consumer_b(void) { got_b = take(); }

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
