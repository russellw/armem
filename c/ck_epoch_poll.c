/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* Concurrency Kit's non-blocking reclamation with ck_epoch_poll
   (ck/include/ck_epoch.h and ck/src/ck_epoch.c, unchanged). A reader
   reads an object through a shared pointer in an epoch section, twice.
   A writer replaces the pointer, defers freeing the old object with
   ck_epoch_call, then calls ck_epoch_poll twice, which frees it once a
   grace period has passed (the deferred function overwrites its value).
   The reader must never see a freed value.

   The records are registered beforehand (they are in the epoch's list of
   records from the start, as ck_epoch_register would leave them), and the
   epoch starts at 1, as ck_epoch_init sets it.

   The writer's deferred objects are on lists in its own record, which ck
   pops with an atomic exchange. No other thread accesses them, so they
   are private memory, atomics included. */

#include <ck_pr.h>
#include <ck_stack.h>
#include <ck_epoch.h>
#include "ck/src/ck_epoch.c"

#define FREED 0xdead
#define SECTIONS 2
#define POLLS 2

struct object {
  unsigned int value;
  ck_epoch_entry_t entry;
};

static struct object first = { 1 }, second = { 2 };
static struct object *shared = &first;

static ck_epoch_t epoch;
static ck_epoch_record_t writer_record = { .global = &epoch, .state = CK_EPOCH_STATE_USED };
static ck_epoch_record_t reader_record = {
  .global = &epoch,
  .state = CK_EPOCH_STATE_USED,
  .record_next = { .next = &writer_record.record_next },
};
static ck_epoch_t epoch = { .epoch = 1, .records = { .head = &reader_record.record_next } };

unsigned int got[SECTIONS];

static unsigned int read_object(void) {
  ck_epoch_begin(&reader_record, NULL);
  struct object *o = ck_pr_load_ptr(&shared);
  unsigned int v = ck_pr_load_uint(&o->value);
  ck_epoch_end(&reader_record, NULL);
  return v;
}

void reader(void) {
  for (int i = 0; i < SECTIONS; i++)
    got[i] = read_object();
}

static void destroy(ck_epoch_entry_t *e) {
  struct object *o = (struct object *)((char *)e - __builtin_offsetof(struct object, entry));
  ck_pr_store_uint(&o->value, FREED);
}

void writer(void) {
  struct object *old = shared;
  ck_pr_store_ptr(&shared, &second);
  ck_epoch_call(&writer_record, &old->entry, destroy);
  for (int i = 0; i < POLLS; i++)
    ck_epoch_poll(&writer_record);
}

/* ck_epoch.c's ck_epoch_register calls memset; this test does not. */
__attribute__((no_builtin)) void *memset(void *d, int c, size_t n) {
  unsigned char *dc = d;
  while (n--)
    *dc++ = (unsigned char)c;
  return d;
}
