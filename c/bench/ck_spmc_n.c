/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* ck_spmc.c scaled up, for benchmarking sampling: the producer sends
   2 * TAKES items through the 2-slot ring, and each consumer takes TAKES
   of them. Every item is taken once, so with -DOLD_FENCE the bug shows
   as a duplicate (see ck_spmc.c). */

#include <ck_pr.h>
#ifdef OLD_FENCE
#define ck_pr_fence_load_store ck_pr_fence_store
#endif
#include <ck_ring.h>

#ifndef TAKES
#define TAKES 2
#endif
#define ITEMS (2 * TAKES)

static ck_ring_buffer_t buf[2];
ck_ring_t ring = { .size = 2, .mask = 1 };
static unsigned long item[8] = { 1, 2, 3, 4, 5, 6, 7, 8 };
unsigned long got_a[TAKES], got_b[TAKES];

void producer(void) {
  for (int i = 0; i < ITEMS; i++)
    while (!ck_ring_enqueue_spmc(&ring, buf, &item[i])) {}
}

static unsigned long take(void) {
  unsigned long *p;
  while (!ck_ring_dequeue_spmc(&ring, buf, &p)) {}
  return *p;
}

void consumer_a(void) {
  for (int i = 0; i < TAKES; i++)
    got_a[i] = take();
}
void consumer_b(void) {
  for (int i = 0; i < TAKES; i++)
    got_b[i] = take();
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
