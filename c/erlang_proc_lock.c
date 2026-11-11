/* cflags: -ffreestanding -Ierlang/stub */
/* Erlang's process locks (erlang/erl_process_lock.c and .h, unchanged;
   OLD selects the header from before commit 46ff98a5d5). Thread t0
   holds a process's main lock and unlocks it; t1 locks and unlocks it.
   If t1 finds the lock taken, it queues itself as a waiter (the stub
   makes it give up spinning at once), sets its flag word uaflgs and
   polls it (see erlang/stub/erl_threads.h). t0, seeing the waiter flag,
   hands the lock over under the pix lock: it dequeues t1, clears the
   waiter flag, releases the pix lock and clears uaflgs.

   bad is set if the main lock is still held after t1 unlocks it. With
   DATA, t0 writes data before unlocking and bad is also set if t1, holding
   the lock, reads it stale. */

#include "erlang/erl_process_lock.c"

erts_tse_t erts_test_tse;

static Process proc;
#ifdef DATA
static int data;
#endif
int bad;

void setup(void)
{
    proc.common.id = 0;
    erts_atomic32_init_nob(&proc.lock.flags, ERTS_PROC_LOCK_MAIN);
}

void t0(void)
{
#ifdef DATA
    data = 42;
#endif
    erts_proc_unlock(&proc, ERTS_PROC_LOCK_MAIN);
}

void t1(void)
{
    erts_proc_lock(&proc, ERTS_PROC_LOCK_MAIN);
#ifdef DATA
    if (data != 42)
        bad = 1;
#endif
    erts_proc_unlock(&proc, ERTS_PROC_LOCK_MAIN);
    if (erts_atomic32_read_nob(&proc.lock.flags) & ERTS_PROC_LOCK_MAIN)
        bad = 1;
}
