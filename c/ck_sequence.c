/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's sequence lock (ck/include/ck_sequence.h, unchanged).
   A writer updates a two-word record once, from (0, 0) to (1, 1); a
   reader copies it with CK_SEQUENCE_READ, retrying until the sequence
   number was the same even value before and after. The record is plain
   memory, as in ck's own tests.

   The writer separates its stores with ck_pr_fence_store() and the reader
   its loads with ck_pr_fence_load() (DMB ISHST and DMB ISHLD on aarch64).
   -DWEAK_WRITER or -DWEAK_READER turns that side's fences into compiler
   barriers only. */

#include <ck_pr.h>
#ifdef WEAK_WRITER
#define ck_pr_fence_store ck_pr_barrier
#endif
#ifdef WEAK_READER
#define ck_pr_fence_load ck_pr_barrier
#endif
#include <ck_sequence.h>

static ck_sequence_t seq = CK_SEQUENCE_INITIALIZER;
static unsigned long rec[2];
unsigned long copy[2];

void writer(void) {
  ck_sequence_write_begin(&seq);
  rec[0] = 1;
  rec[1] = 1;
  ck_sequence_write_end(&seq);
}

void reader(void) {
  unsigned int version;
  unsigned long a, b;
  CK_SEQUENCE_READ(&seq, &version) {
    a = rec[0];
    b = rec[1];
  }
  copy[0] = a;
  copy[1] = b;
}
