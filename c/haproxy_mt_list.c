/* cflags: -ffreestanding -Ihaproxy/stub -Ihaproxy/include */
/* HAProxy's MT_LIST (haproxy/include/haproxy/list.h and list-t.h,
   unchanged from 787dc2095, with stand-ins for the headers list.h
   includes in haproxy/stub), run the way HAProxy's scheduler uses it to
   pass tasklets between threads.

   A tasklet woken from another thread goes on its owner's
   shared_tasklet_list with MT_LIST_ADDQ (tasklet_wakeup_on in task.h).
   The owner takes the whole list with MT_LIST_BEHEAD, splices it onto
   its private TL_URGENT list, and runs each tasklet after unlinking it
   with LIST_DEL_INIT (process_runnable_tasks and run_tasks_from_lists in
   task.c, and __tasklet_remove_from_tasklet_list in task.h). MT_LIST
   locks a link by exchanging MT_LIST_BUSY into it, with relaxed
   exchanges, and unlocks it with a plain store; its only ordering is
   DMB ISHST (__ha_barrier_store) between stores.

   P1 wakes tasklet t1, then t2. P0 owns the list: it takes and runs
   whatever is queued, once, while P1 runs; then it waits for P1 to finish
   and drains the list again. Each wakeup that returned 1 must then have
   run its tasklet once, the shared list must be empty and each tasklet
   unlinked; otherwise bad is 1. A pointer that is not t1, t2 or a list
   head also sets bad, rather than being followed, as HAProxy would.

   It is set on ARM, before and after 787dc2095: MT_LIST_BEHEAD's
   exchange of the head's prev pointer may see P1's last unlock, which
   publishes t2, while P0's later read of t1's next pointer is satisfied
   earlier and returns MT_LIST_BUSY or the head. Nothing orders that read
   after the exchange: the read's address comes from the other exchange
   (of the head's next pointer, which P1 does not write), and DMB ISHST
   orders only stores. HAProxy's LIST_DEL_INIT then writes through the
   pointer.

   -DOLD uses haproxy/old/haproxy/list.h, list.h before 787dc2095
   ("BUG/MEDIUM: lists: add missing store barrier on MT_LIST_BEHEAD()"),
   which added the store barriers that MT_LIST_DEL had and MT_LIST_BEHEAD
   lacked: on 8 Cortex-A53 cores, the scheduler's TL_URGENT list was
   corrupted about once per million connections.

   -DV30 uses haproxy/3.0/haproxy/list.h and list-t.h, from the 3.0
   branch at 6038814 (2.8 has the same MT_LIST). There a tasklet is woken
   with MT_LIST_APPEND, which does not check whether it is already in a
   list (a flag in the tasklet's state does that), and the scheduler is
   the same. Mainline replaced MT_LIST in 3.1 with include/import/mt_list.h,
   whose barriers are release fences: a full DMB ISH on aarch64, which also
   orders the loads. */

#include <haproxy/api.h>
#ifdef V30
#include "haproxy/3.0/haproxy/list-t.h"
#include "haproxy/3.0/haproxy/list.h"
#define MT_LIST_ADDQ MT_LIST_APPEND
#else
#include <haproxy/list-t.h>
#ifdef OLD
#include "haproxy/old/haproxy/list.h"
#else
#include <haproxy/list.h>
#endif
#endif

typedef __UINT64_TYPE__ u64;

struct tasklet {
	struct list list; /* first, as in struct tasklet, so it doubles as an mt_list */
	u64 runs;
};

struct mt_list shared = { &shared, &shared };
struct list urgent = { &urgent, &urgent };
struct tasklet t1 = { { &t1.list, &t1.list } };
struct tasklet t2 = { { &t2.list, &t2.list } };
u64 woke1, woke2; /* successful wakeups of t1 and t2 */
int done;         /* the waker has finished */
int bad;

static int known(const void *p) { return p == &t1 || p == &t2; }

/* Wakes a tasklet onto the shared list; 1 if it queued it. */
static int wakeup(struct tasklet *tl) {
	return MT_LIST_ADDQ(&shared, (struct mt_list *)&tl->list) == 1;
}

/* Takes the shared list and runs every tasklet on it. */
static void run_queue(void) {
	struct mt_list *tmp = MT_LIST_BEHEAD(&shared);
	if (tmp) {
		if (!known(tmp) || !known(tmp->prev)) {
			bad = 1;
			return;
		}
		LIST_SPLICE_END_DETACHED(&urgent, (struct list *)tmp);
	}
	for (int i = 0; !LIST_ISEMPTY(&urgent); i++) {
		struct list *l = urgent.n;
		if (i == 4 || !known(l) || !(known(l->n) || l->n == &urgent) || !(known(l->p) || l->p == &urgent)) {
			bad = 1;
			return;
		}
		struct tasklet *t = LIST_ELEM(l, struct tasklet *, list);
		LIST_DEL_INIT(&t->list);
		t->runs++;
	}
}

static int unlinked(struct tasklet *t) { return t->list.n == &t->list && t->list.p == &t->list; }

void owner(void) {
	run_queue();
	if (bad)
		return;
	while (!__atomic_load_n(&done, __ATOMIC_ACQUIRE))
		;
	run_queue();
	if (bad)
		return;
	if (shared.next != &shared || shared.prev != &shared || !unlinked(&t1) || !unlinked(&t2) ||
	    t1.runs != woke1 || t2.runs != woke2)
		bad = 1;
}

void waker(void) {
	if (wakeup(&t1))
		__atomic_fetch_add(&woke1, 1, __ATOMIC_RELAXED);
	if (wakeup(&t2))
		__atomic_fetch_add(&woke2, 1, __ATOMIC_RELAXED);
	__atomic_store_n(&done, 1, __ATOMIC_RELEASE);
}
