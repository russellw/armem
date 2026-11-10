/* cflags: -ffreestanding -Ick/include -Ick/stub */
/* ck_epoch_poll.c with a third thread, a helper that polls too (with its
   own record and nothing deferred), as in a program where every thread
   polls. The writer removes the object with an atomic exchange, which
   orders the removal before what follows under TSO as well; a plain store
   could be reordered after the reader's check, which is a bug everywhere.

   OLD_POLL polls with ck_epoch_poll_deferred from before ck commit
   d1b348c (in ck_epoch_poll_old.c). The helper can advance the epoch
   between the writer's reading it and filing the object under it, and
   the old poll then frees the object one epoch too early. */

#include <ck_pr.h>
#include <ck_stack.h>
#include <ck_epoch.h>
#include "ck/src/ck_epoch.c"

#ifdef OLD_POLL
#include "ck_epoch_poll_old.c"
#define POLL old_ck_epoch_poll
#else
#define POLL ck_epoch_poll
#endif

#define FREED 0xdead
#ifndef SECTIONS
#define SECTIONS 2
#endif
#ifndef POLLS
#define POLLS 1
#endif
#ifndef HELPER_POLLS
#define HELPER_POLLS 1
#endif

struct object {
  unsigned int value;
  ck_epoch_entry_t entry;
};

static struct object first = { 1 }, second = { 2 };
static struct object *shared = &first;

static ck_epoch_t epoch;
static ck_epoch_record_t helper_record = { .global = &epoch, .state = CK_EPOCH_STATE_USED };
static ck_epoch_record_t writer_record = {
  .global = &epoch,
  .state = CK_EPOCH_STATE_USED,
  .record_next = { .next = &helper_record.record_next },
};
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
  struct object *old = ck_pr_fas_ptr(&shared, &second);
  ck_epoch_call(&writer_record, &old->entry, destroy);
  for (int i = 0; i < POLLS; i++)
    POLL(&writer_record);
}

void helper(void) {
  for (int i = 0; i < HELPER_POLLS; i++)
    POLL(&helper_record);
}

/* ck_epoch.c's ck_epoch_register calls memset; this test does not. */
__attribute__((no_builtin)) void *memset(void *d, int c, size_t n) {
  unsigned char *dc = d;
  while (n--)
    *dc++ = (unsigned char)c;
  return d;
}
