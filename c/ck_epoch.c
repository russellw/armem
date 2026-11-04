/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's epoch-based reclamation (ck/include/ck_epoch.h and
   ck/src/ck_epoch.c, unchanged). A reader reads an object through a
   shared pointer in an epoch section, twice. A writer replaces the
   pointer, waits for a grace period with ck_epoch_synchronize, then
   "frees" the old object by overwriting its value. The reader must never
   see a freed value.

   The reader's record is registered beforehand (it is in the epoch's
   list of records from the start, as ck_epoch_register would leave it),
   and the epoch starts at 1, as ck_epoch_init sets it.

   The bug below needs the second section: its record still shows the
   epoch the first section observed while the writer checks it, and the
   writer takes that as the reader having seen the current epoch.

   -DOLD_BEGIN removes the ck_pr_fence_load() that ck commit 1faa673
   ("add missing barrier for RMO architectures in begin", June 2026) put
   in ck_epoch_begin, between the load of the global epoch and the
   section's own loads. It applies only to ck_epoch.h, where begin is the
   only function this test calls that uses the fence. Then ARM may read
   the shared pointer before the global epoch: the second section can
   read the old pointer, then observe the epoch the writer advanced to
   after replacing it, so the writer stops waiting for it and frees the
   object it still reads. */

#include <ck_pr.h>
#include <ck_stack.h>
#ifdef OLD_BEGIN
#define ck_pr_fence_load ck_pr_barrier
#endif
#include <ck_epoch.h>
#undef ck_pr_fence_load
#include "ck/src/ck_epoch.c"

#define FREED 0xdead

struct object {
  unsigned int value;
};

static struct object first = { 1 }, second = { 2 };
static struct object *shared = &first;

static ck_epoch_t epoch;
static ck_epoch_record_t reader_record = { .global = &epoch, .state = CK_EPOCH_STATE_USED };
static ck_epoch_record_t writer_record = { .global = &epoch, .state = CK_EPOCH_STATE_USED };
static ck_epoch_t epoch = { .epoch = 1, .records = { .head = &reader_record.record_next } };

unsigned int got[2];

static unsigned int read_object(void) {
  ck_epoch_begin(&reader_record, NULL);
  struct object *o = ck_pr_load_ptr(&shared);
  unsigned int v = ck_pr_load_uint(&o->value);
  ck_epoch_end(&reader_record, NULL);
  return v;
}

void reader(void) {
  got[0] = read_object();
  got[1] = read_object();
}

void writer(void) {
  struct object *old = shared;
  ck_pr_store_ptr(&shared, &second);
  ck_epoch_synchronize(&writer_record);
  ck_pr_store_uint(&old->value, FREED);
}

/* ck_epoch.c's ck_epoch_register calls memset; this test does not. */
__attribute__((no_builtin)) void *memset(void *d, int c, size_t n) {
  unsigned char *dc = d;
  while (n--)
    *dc++ = (unsigned char)c;
  return d;
}
