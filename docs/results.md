# Results

armem's runs on concurrent libraries: for each, the code and harnesses
under `c/`, the variants tested, what armem finds on ARM and under TSO,
and how long the searches take. See the [README](../README.md) for how
to compile and run the programs (in [C programs](../README.md#c-programs))
and what the verdicts mean.

## Concurrency Kit

`c/ck/include` holds the headers of [Concurrency
Kit](https://github.com/concurrencykit/ck) that its ring buffer
(`ck_ring.h`), spinlocks (`ck_spinlock.h`), sequence lock
(`ck_sequence.h`), epoch-based reclamation (`ck_epoch.h`) and FIFOs
(`ck_fifo.h`) need, and
`c/ck/src/ck_epoch.c`, unchanged from ck commit `8f0847c` (BSD license,
in `c/ck/LICENSE`). Its aarch64 port implements barriers and atomics in
inline assembly. `c/ck/stub` holds the `ck_md.h` that ck's configure
script generates for aarch64 Linux, and the declarations from
`<string.h>` that ck uses (there is no AArch64 libc here, so a program
that calls `memcpy` defines it). A program's first line can give clang flags
(`/* cflags: ... */`), and the ck programs use it to compile with
`-ffreestanding` and these include paths:

```
clang --target=aarch64-linux-gnu -O2 -fno-pic -ffreestanding -Ic/ck/include -Ic/ck/stub -c c/ck_spmc.c
armem -f 1000 --elf ck_spmc.o c/ck_spmc.litmus
```

* `ck_spsc.c`: one producer sends 1, 2, 3 through a ring of 2 slots
  (ck_ring keeps one free, so it holds one entry) to one consumer. The
  condition is the consumer receiving anything else.
* `ck_spmc.c`: the same ring, with two consumers taking one entry each
  with `ck_ring_dequeue_spmc`, which reads the slot and then claims it by
  a compare-and-swap of the consumer counter. The condition is the
  consumers receiving anything but 1 and 2.

In June 2026, ck commit `c7945fe` changed the fence between a consumer's
read of a slot and its update of the consumer counter from a store fence
(`DMB ISHST`) to a load-store fence (`DMB SY`). The commit message calls
the old fence "overly weak in theory ... No impact on a real modern CPU".
`-DOLD_FENCE` restores it (it turns `ck_pr_fence_load_store` back into
`ck_pr_fence_store`; the code is the same as before the commit, up to
register allocation):

| Program | Variant | ARM | TSO |
|---|---|---|---|
| ck_spsc | current | no | no |
| ck_spsc | old fence | no | no |
| ck_spmc | current | no | no |
| ck_spmc | old fence | yes, but no at `-O0` | no |

The architecture allows the bug. A store fence does not order a read
before a later write, so with `ck_spmc` at `-O1` and above, a consumer can
claim slot 0 and read it only after the other consumer has claimed slot 1
and the producer has put item 3 in slot 0. The first consumer then
receives item 3, which also stays in the ring. With one consumer
(`ck_spsc`), the old fence is enough: the producer reuses a slot only
after the dequeue that follows it, which runs `DMB ISHLD` before its own
update and so orders the earlier read first. At `-O0`, the copy of the
entry is a call to `memcpy`, which stores it to the consumer's stack. That
store is data dependent on the read, and the store fence orders it before
the update. This is the case the commit's comment warns about: the
compiler may keep the copy in a register instead, and does at `-O1`.

ck can also use the LSE atomics (`-march=armv8.1-a
-DCK_MD_LSE_ENABLE`, `CAS` instead of `LDXR`/`STXR`), with the same
verdicts. Each test takes at most 6 s (`ck_spmc` at `-O0`), and under
1 s at `-O1` and above. The harness's `memcpy` copies an aligned word
with one load, as a libc's does. Copying byte by byte made each read of a
slot eight reads that could each see a different write, and the `-O0`
tests took minutes.

`ck_spinlock.c` runs each of ck's spinlocks, chosen with `-DLOCK=N`: 1
compare-and-swap, 2 exchange, 3 ticket, 4 MCS, 5 CLH, 6 Anderson (array)
and 7 decrement. Two threads each take the lock once and increment a
plain counter, and the condition is the counter ending at anything but
2. Each lock acquires with `ck_pr_fence_lock()` and releases with
`ck_pr_fence_unlock()` (both `DMB SY` on aarch64). `-DWEAK_LOCK` or
`-DWEAK_UNLOCK` turns one of them into a compiler barrier only. Every
lock is correct as written, and either change makes it lose an increment
on ARM only, at `-O0` and `-O2`. The MCS lock's queue nodes are static
rather than on the stack, as in ck's own tests, since a thread here
cannot access another's stack. MCS is the slowest: up to 8 s at `-O2`,
and 5 s at `-O0` as written (its weakened variants take over 20 s there
and are left out of `expected.txt`). The others take under 3 s.

`ck_sequence.c` runs ck's sequence lock: a writer updates a two-word
record once, and a reader copies it with `CK_SEQUENCE_READ`. The
condition is a torn copy. It is correct as written. `-DWEAK_WRITER`
(no store fences in the writer) and `-DWEAK_READER` (no load fences in
the reader) both tear the copy on ARM only. Each test takes 0.1 s.

`ck_epoch.c` runs ck's epoch-based reclamation. A reader reads an object
through a shared pointer in an epoch section, twice. A writer replaces
the pointer, waits for a grace period with `ck_epoch_synchronize`, then
frees the old object (it overwrites the value). The condition is the
reader seeing a freed value. The reader's record is in the epoch's list
from the start, as `ck_epoch_register` would leave it.

In June 2026, ck commit `1faa673` ("add missing barrier for RMO
architectures in begin") added a `ck_pr_fence_load()` to
`ck_epoch_begin`, between the load of the global epoch and the
section's own loads. `-DOLD_BEGIN` removes it, which gives the same
reader code as before the commit, and the condition becomes reachable on
ARM only, at `-O0` and `-O2`. ARM may read the shared pointer before the
global epoch. The reader's second section can read the old pointer, then
observe the epoch the writer advanced to after replacing it. The writer
checked the record while it still showed the epoch from the first
section, which was current then, so it advanced the epoch again,
stopped waiting, and freed the object that the reader still reads. With
one section, the bug does not appear: the record's starting epoch, 0,
is never current, so the writer waits for the section to end. The tests
take up to 4 s (at `-O0`).

`ck_epoch_poll.c` reclaims through `ck_epoch_poll` instead, which never
waits. The writer replaces the pointer, defers freeing the old object
with `ck_epoch_call`, and polls twice; the reader again reads the object
in two sections. It is correct, and the tests take 3.4 s at `-O2` and
7.3 s at `-O0` (which needs `fuel=2000`). The writer's deferred lists
are private memory even though ck pops them with an atomic exchange (see
[How it works](../README.md#how-it-works)); before atomics could be private, the
tests took over 2 minutes.

ck commit `d1b348c` (June 2026) changed which deferred objects a poll
may free ("This held in production only due to x86 store / atomic
ordering guarantees but falls apart on RMO"). With the poll from before
that commit, this test is also correct, as are versions with three
sections and three polls. That fits ck's own comments: the old poll
fails only when `ck_epoch_call` files an object under an epoch that
another thread has already advanced, so it needs a second thread that
polls.

`ck_epoch_poll3.c` adds that thread: a helper with its own record and
nothing deferred, which polls once, while the writer polls once. The
writer removes the object with an atomic exchange, since with a plain
store the removal can be reordered after the reader's check, which is
a bug under TSO as well. As written the test is correct, and with
`-DOLD_POLL`, which polls with a copy of the poll from before `d1b348c`
(`c/ck_epoch_poll_old.c`), the reader sees the freed object on ARM
only, at `-O2` and `-O0`. This is the bug that commit fixed. The tests
take 5 to 7 s on ARM and 3 to 4 s under TSO at `-O2`, and up to 17 s
at `-O0`, which is left out of the test suite. With two writer polls
the verdicts are the same, in 24 to 30 s. Before the pre-pass could
handle three threads (see [How it works](../README.md#how-it-works)), and before
the reference models had private memory, neither exhaustive search nor
3,000 sampled paths finished within 200 s.

`ck_fifo_spsc.c` runs ck's single-producer, single-consumer FIFO
(`ck_fifo.h`), a linked list whose head is always a stub node. The
producer sends 1, 2, 3 with only two nodes, so it must recycle each node
(`ck_fifo_spsc_recycle`) once the consumer has moved past it. The
condition is the consumer receiving anything but 1, 2, 3. Enqueue fills
a node, runs a store fence and links the node; dequeue reads a node's
value, runs a store fence and moves the head past it. The FIFO is correct
as written. `-DWEAK_ENQ` or `-DWEAK_DEQ` drops one of the fences, and
either makes it lose or corrupt a value on ARM only, at `-O0` to `-O2`:
without the dequeue's fence, the consumer can move the head before its
read of the value, and the producer then refills the node first. The
values are sent as the pointers themselves. Had the consumer read an
item through each pointer, the read's address dependency would order the
read of the node before the next update of the head, and `-DWEAK_DEQ`
would be harmless. The tests take under 0.3 s, except those with
`-DWEAK_ENQ`: the producer's writes are then unordered, and the consumer
sees them in many orders, so they take 1.6 s at `-O2` and 4.9 s at `-O0`
(2 minutes before the dynamic reduction).

`ck_fifo_mpmc.c` runs ck's multi-producer, multi-consumer FIFO, a
Michael-Scott queue whose head, tail and links are (pointer, generation)
pairs, each updated by a 16-byte compare-and-swap (`LDXP`/`STXP` on
aarch64). As in the test of node re-use that ck added in commit `e78c421`
(June 2026), the queue starts with the entries 1 and 2, and each of two
threads dequeues once and at once enqueues the value again, in the node
the dequeue gave back. The queue is never empty, so the condition is a
dequeue that reports it empty, or the threads receiving anything but 1
and 2.

As written (ck `8f0847c`, the latest commit as of November 2026), the
condition is reachable on ARM only: both threads can receive 2, and
item 1 is lost. A dequeue reads the value of the node after the head
("Save value before commit") and then claims it by moving the head with
`ck_pr_cas_ptr_2`, which on aarch64 orders nothing. So ARM may perform
the read after the compare-and-swap. Meanwhile the other thread dequeues
the next entry, which moves the head past that node and gives it back
as garbage, and it re-enqueues its own value, 2, in it. The late read
returns 2. In Arm's model this is load buffering: the second thread's
write of the value is ordered after its read of the head (a control
dependency), but nothing orders the first thread's read of the value
before its write of the head. x86 cannot do this, since its
compare-and-swap is a full barrier. `-DFENCE_DEQ` adds
`ck_pr_fence_load_store()` (`DMB SY`) between the read and the
compare-and-swap, and then the exhaustive search shows the condition
unreachable (4,894 memories in 35 s at `-O2`, and the same at `-O1` and
`-Os`, with the dynamic reduction of
[dynamic-reduction.md](dynamic-reduction.md); without it, the
search did not finish in 20 minutes). As written, the exhaustive search
finds the bug in 6,752 memories and 58 s. `ck_fifo_mpmc_trydequeue`
reads the value the same way. The bug needs a node to be re-used while another
thread may still read it, which ck's own test does on purpose. It is
reported as
[concurrencykit/ck#297](https://github.com/concurrencykit/ck/issues/297).

ck changed this FIFO twice in 2026, and `ck_fifo_mpmc_variants.c` has
the code from before each change. `-DOLD_DEQ` uses the dequeue from
before `e78c421`, which could report the queue empty when its snapshot
of the head had been re-used. With `-DFENCE_DEQ -DOLD_DEQ`, a thread
receives nothing even under SC: that bug is not about the memory
model. `-DOLD_ENQ` uses the enqueue from before `ed6bace` (September
2026), which started a node's generation at 0 and had no load fence
before re-checking the tail's generation; with `-DFENCE_DEQ -DOLD_ENQ`,
the exhaustive search shows the condition unreachable too (4,934
memories in 76 s at `-O2`). The sampled tests take
5 to 7 s at `-O1` to `-O2` (and the same verdict at `-Os`), and TSO
about 2 s; at `-O0`, 20 sampled paths take over 10 minutes.

## DPDK

`c/dpdk/include` holds the ring headers of [DPDK](https://www.dpdk.org)
(`lib/ring/rte_ring*.h`, unchanged from DPDK commit `a13c105336`, 26.11-rc2;
BSD license, in `c/dpdk/LICENSE`). `c/dpdk/stub` holds just enough of
DPDK's EAL headers for them: `rte_stdatomic.h` unchanged, and small
stand-ins for the rest (each says what it covers), configured as DPDK's
build configures aarch64, with the C11 ring (`RTE_USE_C11_MEM_MODEL`).

`dpdk_ring.c` runs the multi-producer, multi-consumer ring with 4 slots,
holding the item 1; the empty slots hold 99. Thread `t0` acts first as a
producer and then as a consumer: it enqueues 2, then dequeues two items
at once. Thread `t1` dequeues one. The condition is any other outcome
than `t0` receiving 1 and 2, or `t1` receiving 1 (or nothing).

DPDK commit `a4ad0eba9d` (November 2025, by Arm) changed how a head move
orders its read of the head and of the other side's tail. It says the
old code fails "when a CPU first acts as a producer and later as a
consumer", but only "when load-acquire is implemented with RCpc
semantics (e.g. some AArch64 CPUs which support the LDAPR ... instructions)".
`-DOLD_HEAD` restores the old code (in `c/dpdk_ring_variants.h`), and
`-march=armv8.3-a` makes clang compile acquire loads to `LDAPR` instead
of `LDAR`. Before that, DPDK commit `9bc2cbb007` (November 2017) added a
fence between those two reads in the generic ring, after a crash on an
ARM server. `-DRTE_GENERIC_RING` selects the generic ring, and
`-DOLD_GENERIC` removes the fence:

| Ring | Variant | `LDAR` (armv8-a) | `LDAPR` (armv8.3-a) |
|---|---|---|---|
| C11 | current | no | no |
| C11 | before `a4ad0eba9d` | no | **ARM only** |
| generic | current | no | no |
| generic | before `9bc2cbb007` | **ARM only** | **ARM only** |

That holds at `-O0` to `-O2` and `-Os`. With either old version, `t1`
receives 99: it sees `t0`'s move of the consumer head but an older
producer tail, so its count of entries is negative and wraps around, and
it dequeues an empty slot. With the C11 ring, `t0` reads its own release
store of the producer tail with `LDAPR`, which may happen before that
store becomes visible to other threads (`LDAR` may not). So its move of
the consumer head, which depends on that read, can be seen first. The
tests take under 0.5 s, except the old C11 head move with `LDAPR`
(4.4 s at `-O1` to `-O2`, 55 s at `-O0`, which is left out of the test
suite).

The ring's other two multi-thread modes, head/tail sync (HTS, `-DHTS`)
and relaxed tail sync (RTS, `-DRTS`), had the same kind of change in DPDK
commits `66d5f96278` and `36b69b5f95`, in the same series as
`a4ad0eba9d`. Those commits describe it as establishing "a safe partial
order", not as fixing a failure. `-DOLD_HTS` and `-DOLD_RTS` restore the
old code (`c/dpdk/old`, unchanged from the commits' parents). In
`dpdk_ring.c` neither old mode goes wrong. `dpdk_ring3.c` moves the
enqueue of 2 to a third thread, `t2`, so that `t0` can take an item it
did not write:

| Ring | Variant | `LDAR` (armv8-a) | `LDAPR` (armv8.3-a) |
|---|---|---|---|
| HTS | current | no | no |
| HTS | before `66d5f96278` | **ARM only** | **ARM only** |
| RTS | current | no | no |
| RTS | before `36b69b5f95` | **ARM only** | **ARM only** |

That holds at `-O1`, `-O2` and `-Os`, and for RTS also with a maximum
distance between head and tail (`-DHTD_MAX=3`) above its default, which
is 0 here. At `-O0` the search runs out of memory. With either old mode,
`t0` dequeues both items but receives 99 for the second. The old code
reads the producer tail with a plain load and only then does the
acquire compare-and-swap of the consumer head. An acquire orders the
loads after it, not the ones before, so `t0`'s loads of the slots may be
satisfied before its load of the tail: it reads the tail that `t2`'s
release store set and slots older than that store. The old code's own
comment says the acquire compare-and-swap "serves as a hoist barrier to
prevent ... OOO reads of cons tail value", which it does not. The new
code loads the tail with acquire. In `dpdk_ring3.c` the old C11 head move
and the current rings are right, and the generic ring without the fence
of `9bc2cbb007` is wrong. The tests take under 4 s.

## liburcu

`c/urcu/include` holds the headers of
[liburcu](https://liburcu.org) (`include/urcu`, unchanged from liburcu
commit `645fdc9`, July 2026; mostly LGPL-2.1-or-later, see
`c/urcu/LICENSE.md`). `c/urcu/stub` holds the generated `urcu/config.h` as
configure sets it for aarch64 Linux, a `urcu/futex.h` that records each
`FUTEX_WAKE`, a `urcu/tls-compat.h` that makes thread-local variables
ordinary globals (each test has one reader), and small stand-ins for the
C library headers.

`urcu_qsbr.c` and `urcu_mb.c` test the QSBR and mb flavors' wakeup of
a waiting grace period. The reader, `t0`, calls the real header code: it
reports a quiescent state (`-DOFFLINE`: goes offline) or leaves its
read-side critical section. The writer, `t1`, runs one pass of
`wait_for_readers` (copied from `src/urcu-qsbr.c` and `src/urcu.c`), the
pass that arms the futex after the busy-wait attempts run out. It then
checks the reader's counter, and would sleep in `wait_gp` if the reader
has not reported and the futex is still -1. The condition is that it
would sleep and the reader never wakes it, which hangs
`synchronize_rcu`.

liburcu commits `9e6448d` (QSBR) and `96ef712` (mb) (January 2026)
changed the reader's store of its counter from `CMM_SEQ_CST` to
`CMM_SEQ_CST_FENCE`, because "Storing to rcu_reader.ctr with a
CMM_SEQ_CST store does not ensure that the following relaxed load of
the futex waiting state is ordered after that store". `-DOLD_QSBR` and
`-DOLD_MB` undo just those commits (`c/urcu/old`, through
`c/urcu_variants.h`):

| Flavor | Variant | `LDAR` (armv8-a) | `LDAPR` (armv8.3-a) | TSO |
|---|---|---|---|---|
| QSBR (quiescent state, offline) | current | no | no | no |
| QSBR (quiescent state, offline) | before `9e6448d` | **yes** | **yes** | yes |
| mb | current | no | no | no |
| mb | before `96ef712` | **yes** | **yes** | yes |

That holds at `-O0` to `-O2` and `-Os`, and each test takes about 0.1 s.
With C11 atomics, which liburcu uses whenever the compiler supports C11
(since commit `89280d0`, in 0.15.0), the old store compiles to an `STLR`
with nothing after it. The reader's next load, of its waiting flag or
the futex, is a plain `LDR`, which may be satisfied before the `STLR`
becomes visible. The writer makes the mirror-image store and load, with a
full barrier between them. That is store buffering: each side misses the
other's store. The fix adds a `DMB ISH` after the `STLR`. TSO allows
store buffering too, and armem's TSO reference runs the `STLR` as an
ordinary store, so it reports the bug under TSO as well. On real x86 the
bug cannot happen, because compilers make a seq_cst store an `XCHG`,
which is a full barrier. So it hits ARM and other weak architectures
only. Compiled without C11 atomics (`-std=gnu99`), liburcu puts a full
barrier on each side of the store, and the old code is right.

## QEMU

`c/qemu` holds QEMU's `util/event.c` and `include/qemu/atomic.h` and
`compiler.h`, unchanged from QEMU commit `f71afb4` (November 2026;
GPL-2.0-or-later, see `c/qemu/COPYING` and `c/qemu/LICENSE`).
`c/qemu/stub` holds a `qemu/thread.h` with just the `QemuEvent` type
and functions, a `qemu/osdep.h` with the little that `event.c` uses,
and a `qemu/futex.h` (Linux's, with `HAVE_FUTEX`) whose `FUTEX_WAKE`
is recorded. Its `FUTEX_WAIT` makes the full barrier the kernel makes,
then checks the futex word: if the thread would sleep, it records that
and ends the thread, by branching to the address its thread function
returns to.

`qemu_event.c` tests the call_rcu thread of `util/rcu.c` waiting for
work. The event is set, as `call_rcu1` leaves it, and no work is
pending. The thread, `t0`, runs the loop at the top of
`call_rcu_thread`: it resets the event, reads `rcu_call_count`, and if
that is 0 waits on the event. `t1` runs `call_rcu1` without the queue:
it increments `rcu_call_count` and sets the event. The condition is
that `t0` sleeps and no one wakes it, which leaves the work, and every
later `call_rcu`, waiting.

QEMU commit `9586a132` ("qemu-thread-posix: cleanup, fix, document
QemuEvent", March 2023, in QEMU 8.0) says that "QemuEvent is currently
broken on ARM due to missing memory barriers after qatomic_*()".
`-DOLD_EVENT` undoes it (`c/qemu/old/util/event.c`, the current file
with just that commit's changes undone):

| Variant | LDAXR/STLXR (armv8-a) | LSE (armv8.1-a) | TSO |
|---|---|---|---|
| current | no | no | no |
| before `9586a132` | **yes** | no | no |

That holds at `-O0` to `-O2` and `-Os`, and each test takes under 2 s.
The old `qemu_event_reset` changes `EV_SET` to `EV_FREE` with a seq_cst
`qatomic_or` and nothing after it. Without LSE, that is an
LDAXR/STLXR loop, and the caller's relaxed load of `rcu_call_count` may
be satisfied before the `STLXR`'s write is visible. `qemu_event_set`
makes the mirror-image store and load, `rcu_call_count` then the
event, with a full barrier between them. That is store buffering: `t1`
still sees `EV_SET`, so it does not wake anyone, and `t0` sees no work,
so it waits, turns the event from `EV_FREE` to `EV_BUSY` and sleeps.
On x86 the OR is a locked instruction, a full barrier. With LSE it is an
`LDSETAL`, whose write Arm's model orders before later loads (herd7
agrees), so the old code is right there. Of the commit's changes, only
the one to `qemu_event_reset` matters here: undoing it alone brings the
bug back, and the old `qemu_event_reset` with just
`smp_mb__after_rmw()` added at its end is right. The commit also adds
`smp_mb__after_rmw()` after the exchange in `qemu_event_set`, to pair
with the kernel's barrier in `FUTEX_WAIT`, and the stub futex does not
model the kernel's side of that.

## libuv

`c/libuv` holds libuv's `src/uv-common.c` and `src/unix/async.c`, with
the headers they include from libuv, unchanged from libuv 1.53.0
(September 2026; MIT, see `c/libuv/LICENSE`). On master (November 2026)
the two functions tested here are the same. `c/libuv/stub` holds stand-ins
for the C library headers they include, compiled with `-nostdlibinc` so
that none of the host's headers are used. Its `read` finds no data, and
its `write` (the eventfd write that wakes the loop) is recorded.
`externs.h` defines each function the two files call but do not define
(the rest of libuv and the C library) as a single `BRK`, which armem
reports as a fault if it runs.

libuv's documentation says that `uv_async_send` and the callback it
causes are sequentially consistent: everything the sending thread did
before the call is visible to the callback, even when libuv coalesces
the call with an earlier one that the loop has not yet handled.
`libuv_async.c` tests the coalesced case. The handle is pending, as an
earlier `uv_async_send` leaves it. `t0` runs the loop's
`uv__async_io`, which clears the pending flag with a seq_cst
`fetch_and` and runs the callback, which reads `data`. `t1` stores 1 to
`data` and calls `uv_async_send`. The condition is that `t1`'s call is
coalesced (no notification is written) and the callback reads 0. That
callback is the one that answers `t1`'s call, so `t1`'s store is lost.

libuv commit `cdc632c6` ("async: make seq_cst the default", June 2026,
in 1.53.0) made the pending flag's CAS seq_cst and removed a relaxed
load of the flag before it, which returned at once if the flag was set.
`-DOLD_SEND` undoes it (`c/libuv/old/src/uv-common.c`). `-DDATA` gives
`t1`'s store an ordering, and `-DSEE` the callback's load (both relaxed
by default):

| Variant | Store | Callback's load | LDAXR/STLXR (armv8-a) | LSE (armv8.1-a) | TSO |
|---|---|---|---|---|---|
| current | relaxed | relaxed | **yes** | **yes** | yes (no with LSE) |
| current | release or seq_cst | relaxed | **yes** | no | no |
| current | relaxed | acquire | **yes** | **yes** | yes (no with LSE) |
| current | release | acquire | no | no | no |
| before `cdc632c6` | any | any | yes | yes | yes |

The first row holds at `-O0` to `-O2` and `-Os`, and the others at `-O0`
and `-O2`. Each test takes under a second, and at `-O0` each thread
needs 3,000 instructions. So the current code still loses a coalesced
call on ARM, unless the caller stores with release and the callback
loads with acquire, or, with LSE, the caller stores with release. A coalesced `uv_async_send` is a CAS that fails, because the flag is
set. A failed CAS makes no write, so without LSE it is an `LDAXR` (then
`CLREX`), and with LSE a `CASAL` that is only an acquire load. Neither
orders `t1`'s earlier store, unless that store is a release (`STLR`
before an acquire load is ordered), and so with a relaxed store it is
store buffering against the loop. On the loop's side, without LSE the
seq_cst `fetch_and` is an `LDAXR`/`STLXR` loop, and the callback's
relaxed load may be satisfied before the `STLXR`'s write is visible, as
in [QEMU](#qemu). With LSE it is an `LDCLRAL`, which orders later loads.
herd7 agrees on both patterns: an `STLR` then a failing `LDAXR` against
an `LDAXR`/`STLXR` then `LDR` is allowed (and forbidden with an `LDAR`),
and an `STR` then a failing `CASAL` against an `LDCLRAL` then `LDR` is
allowed (and forbidden with an `STLR`). Nor does the code keep its
promise under C11: a failed seq_cst CAS is a seq_cst load, which does not order
a relaxed store before it, and a seq_cst RMW does not order a relaxed
load after it. A program that protects its data with a mutex is not
affected, as an unlock is a release and a lock an acquire. Under TSO,
armem runs the same ARM code, where the failing `LDAXR` is just a load.
x86's `LOCK CMPXCHG` is a full barrier even when it fails.

A seq_cst fence at the start of `uv_async_send` and another after the
`fetch_and` in `uv__async_io` makes every row no, with and without LSE,
at `-O0` and `-O2`. This version is not in `c/`; it was checked with
copies of the two files. This was reported as
[libuv/libuv#5416](https://github.com/libuv/libuv/issues/5416).

`uv_async_send` also counts itself busy while it writes to the eventfd.
Its compare-and-swap adds 2 to a counter above the pending flag, and it
subtracts 2 after the write. `uv__async_spin`, which `uv_close` and loop
teardown run, waits for the counter to reach 0, after which nothing may
use the handle or the fd. In 1.53.0 the subtraction and the spin's load
are relaxed. libuv commit `067c56e3` (September 2026, #5307, after
1.53.0) makes them a release and an acquire, for ThreadSanitizer's
report of `close` in `uv__async_stop` racing `write` in `uv__async_send`
(#5297). `libuv_close.c` tests teardown. `t0` runs `uv__async_stop`,
which spins on the handle, closes the fd and sets it to -1, and then
reuses the handle's memory. `t1` calls `uv_async_send`. The stub `write`
records the fd it was given (in `uv_test_fd`), and the test defines
`uv__close` and `uv__io_stop` itself (`-DUV_TEST_TEARDOWN` leaves them
out of `externs.h`). The condition is a write to any fd other than 3.
`-DFIXED` uses `c/libuv/fixed/src/uv-common.c`, which is 1.53.0's file
with `067c56e3` applied:

| Variant | LDAXR/STLXR (armv8-a) | LSE (armv8.1-a) | TSO |
|---|---|---|---|
| 1.53.0 | **yes** (no at `-O0`) | **yes** | no |
| `067c56e3` | no | no | no |

That holds at `-O0` to `-O2` and `-Os`. In
1.53.0, `t1` can load fd -1, the value set after the close. Its loads of
`handle->loop` and the fd are satisfied after its subtraction is visible
to `t0` (load buffering), and nothing in the relaxed code forbids that.
At `-O0` without LSE clang compiles the relaxed subtraction as an
`LDAXR`/`STLXR` loop anyway. In executions where `t0` spins while `t1`
is mid-send, `t0` runs all 997 turns of the spin's inner loop and calls
`sched_yield`, which here just returns. The next pass of the outer loop
repeats a state armem has already seen, so it is pruned. That takes 8,000
instructions of fuel, or 20,000 at `-O0` (about 20 s per run). On
Linux this is probably not reachable: the real `write` gets from the fd
to the eventfd's spinlock through address dependencies, and taking the
lock is an acquire that orders the subtraction after the loads. The
stub's `write` is a plain store, so only libuv's own orderings count, as
in C11. With them, 1.53.0 relies on the kernel, as the commit message
says, and `067c56e3` removes that reliance.

## liburing

`c/liburing/src` holds liburing's `src/queue.c` and the headers it
includes from liburing, unchanged from liburing commit `c63f4aa6`
(November 2026, 2.16; MIT or LGPL-2.1, see `c/liburing/LICENSE` and
`c/liburing/COPYING`), with `liburing/compat.h` and
`liburing/io_uring_version.h` as its `configure` generates them.
`c/liburing/stub` holds stand-ins for the C library and Linux headers,
compiled with `-nostdlibinc`. It also holds a stand-in for liburing's
`src/arch/aarch64/syscall.h`, whose system calls are `SVC` instructions:
here each one calls `uring_test_syscall`, which the test defines, and
liburing's `src/arch/syscall-defs.h` (unchanged) builds its wrappers
from those calls. The kernel's side of each test is a few lines of
`io_uring/` from Linux commit `d4872371` (November 2026), with arm64's
atomics and barriers written as the kernel's inline assembly. The only
exception is the `PRFM` hint before the LL/SC loop, which is left out.

With `IORING_SETUP_SQPOLL`, a kernel thread polls the submission queue,
and when it finds no work it sleeps. `io_sq_thread` first sets
`IORING_SQ_NEED_WAKEUP` in the ring's flags, then (after
`smp_mb__after_atomic`) checks the SQ tail once more. `io_uring_submit`
stores the tail with release, then reads the flags, and if the flag is
set it calls `io_uring_enter` with `IORING_ENTER_SQ_WAKEUP`.
`liburing_sqpoll.c` runs `io_uring_get_sqe`, `io_uring_prep_nop` and
`io_uring_submit` in `t0`, and the kernel thread's check in `t1`. The
condition is that the thread would sleep and `t0` does not wake it,
which leaves the new entry unsubmitted.

liburing commit `744f4156` ("Fix memory ordering in
sq_ring_needs_enter", March 2022, liburing issue 541) added a full fence
between the tail store and the load of the flags. `-DOLD_SUBMIT` undoes
it (`c/liburing/old/src/queue.c`), and `-DNO_KERNEL_MB` removes the
kernel's `smp_mb__after_atomic`:

| Variant | LDXR/STXR (armv8-a) | LSE (armv8.1-a) | TSO |
|---|---|---|---|
| current | no | no | no |
| before `744f4156` | yes | yes | yes |
| kernel without `smp_mb__after_atomic` | **yes** | **yes** | no |

That holds at `-O0` to `-O2` and `-Os`, and with `LDAPR`
(`-march=armv8.3-a`), and each test takes under 0.1 s. The old code is
store buffering: `t0`'s `STLR` of the tail and its later `LDR` of the
flags are not ordered, on ARM or on x86 (where a release store is a
plain `MOV`), so the bug was not ARM-only. The kernel's barrier is
needed only on ARM. Its `atomic_or` is relaxed on arm64 (an
`LDXR`/`STXR` loop, or `STSET`), and nothing orders it before the
`LDAR` of the tail. On x86 it is a locked `OR`, a full barrier. herd7
agrees on all three patterns, with the LSE atomic.

`liburing_cq.c` tests the completion queue, with a single entry so that
the kernel reuses its slot. `t1` posts two completions (res 1, then 2) as
`io_cqe_cache_refill` and `io_commit_cqring` do: it reads the head the
application last released (`READ_ONCE`), and only if the slot is free
writes the CQE with `WRITE_ONCE` and releases the tail. If the slot is
not free, the completion would overflow, and here `t1` stops. `t0` calls
`io_uring_peek_cqe` twice, and for each CQE it gets, checks its res and
calls `io_uring_cqe_seen`. The condition is that it reads a CQE other
than the one posted for that slot.

liburing commit `81cb9479` (July 2025) made `__io_uring_peek_cqe` load
the CQ head with acquire, on the grounds that the CQE's address depends
on the head, and C does not order address dependencies. Commit `a4d619a2`
(June 2026) reverted it: only the application writes the head, and the
acquire load of the tail already orders the CQE's contents.
`-DACQ_HEAD` restores the acquire (`c/liburing/old/include/liburing.h`).
Both versions are right at `-O0` to `-O2` and `-Os`, with and without
LSE and `LDAPR`, so the revert is safe on ARM. Each takes under 0.1 s.
The kernel's side of slot reuse is ordered by a control dependency, from
its load of the head to its stores to the CQE, which ARM respects for
stores. With `-DNO_KERNEL_RELEASE`, a `WRITE_ONCE` of the tail rather
than `smp_store_release`, `t0` may read the slot before the kernel's
writes to it. That is ARM-only (0.7 s, 1.5 s at `-O0`).

## readerwriterqueue

`c/readerwriterqueue` holds `readerwriterqueue.h` and `atomicops.h` from
moodycamel's ReaderWriterQueue, unchanged from commit `6867b564`
(September 2026; Simplified BSD, see `c/readerwriterqueue/LICENSE.md`).
It is C++, and `c/readerwriterqueue/stub` holds freestanding stand-ins
for the standard headers it includes (`<atomic>` is built on clang's
`__atomic` builtins), compiled with `-nostdlibinc`. `c/rwq.h` gives the
queue a bump allocator over zeroed memory, and its setup function (see
[Starting from an ELF file's memory](../README.md#starting-from-an-elf-files-memory))
constructs a `ReaderWriterQueue<int>` with one block, room for one element, and
enqueues 1. In both tests `t0` enqueues 2, which allocates a second
block, initializes it, links it after the first (`tailBlock->next`) and
makes it the tail block (`tailBlock`).

Pull request #172 (merged September 2026) has two commits. The first,
`0a5aee4`, fixes issue #171: the release fence sat between the two
stores, so `size_approx`, which walks the `next` chain, could reach the
new block before its fields. It moved the fence before both stores. That
left nothing between them, and the second commit, `ba8c662`, adds a
second fence there, replacing the comment that said none was needed.
`-DOLD` undoes both commits (`c/readerwriterqueue/old`), and `-DMID`
only the second (`c/readerwriterqueue/mid`):

| Variant | `rwq_size` | `rwq_dequeue` | TSO |
|---|---|---|---|
| current | no | no | no |
| after `0a5aee4` only | no | **yes** | no |
| before `0a5aee4` | **fault** | no | no |

`rwq_size.cpp` calls `size_approx` in `t1`. With the old code, on ARM, it
can read the new block's `next` before `t0` writes it, as the zero the
allocator's memory holds, and dereferences it: armem reports the null
pointer dereference. The current and intermediate versions are right.
Each run takes under 0.5 s at `-O0` to `-O2` and `-Os`, except the old
code's, which takes 5 to 12 s (330 s at `-Os`).

`rwq_dequeue.cpp` calls `try_dequeue` twice in `t1`, and the condition is
that it gets a value other than the next one enqueued. With only the
first commit, `t1` can see the new `tailBlock` while the first block's
`next` still points to itself; it then "advances" to the first block
again, which it has emptied, and dequeues a stale slot. The bug was
ARM-only, and was never in a release, since the PR was merged with both
commits. Each run takes 6 to 11 s at `-O2` and `-O1`, 1.5 to 3 minutes
at `-O0` (with 3,000 instructions of fuel), and over 3 minutes at `-Os`.
clang compiles `atomic_thread_fence(memory_order_release)` to `DMB ISH`,
a full barrier, so before the reduction used program order (see
[How it works](../README.md#how-it-works)) it took the new block's initializing
stores, which the consumer reaches only through the pointers stored
after the fence, to be observable in every order, and the search did not
finish in 15 minutes.

## Boost.Lockfree

`c/boost/include` holds Boost.Lockfree's `spsc_queue.hpp` and
`spsc_value.hpp`, and the headers of Boost.Lockfree's own that they
include, unchanged from Boost.Lockfree commit `d0172a2` (October 2026;
Boost Software License, see `c/boost/LICENSE`). `c/boost/stub` holds
freestanding stand-ins for the standard headers and the rest of Boost
that they include (`<atomic>` on clang's `__atomic` builtins, and a
Boost.Parameter that finds an option by its tag), and `c/boost_lockfree.h`
says what each test shares. Each test constructs its object in its setup
function.

`spsc_value` is a triple buffer. The writer writes into a buffer of its
own and exchanges that buffer's index with the shared one, tagged as new.
The reader, if the tag is set, exchanges its own index for it and reads
the buffer it gets, and the buffer it gave up becomes the writer's at the
writer's next exchange. In `boost_spsc_value.cpp`, `t0` writes 1 to 4 and
`t1` reads twice. The condition is a second read that returns less than
the first.

Commit `fa6cbdf` (August 2026, "spsc_value: strengthen memory barriers")
made both exchanges `acq_rel`, from `release` in the writer and `acquire`
in the reader. `-DOLD` uses the file from before it
(`c/boost/old/boost/lockfree/spsc_value.hpp`), and then the condition is
reachable on ARM only. The reader's exchange is `LDAXRB`/`STXRB` (or
`SWPAB`), and nothing orders the first read's load of its buffer before
the store of the second read's exchange, which gives that buffer back.
The writer gets it at its third exchange, and its fourth write stores 4
in it, which the first read then returns; the second returned 2. x86's
exchange is a full barrier. In C++ the writer's half of the fix is needed
too, since without it nothing makes the reader's load happen before the
writer's store, but on ARM that store has an address dependency on the
writer's exchange. With only the reader's exchange made `acquire` again
(a copy of the header, not in `c/`), the condition is reachable on ARM;
with only the writer's made `release`, the exhaustive search shows it
unreachable (751,807 memories in 11 minutes at `-O2`).

The current code is right at `-O1`, `-O2` and `-Os`, and with LSE. That
takes 5.7 s at `-O2`, 14 s at `-O1` and `-Os`, and 65 s with LSE. The old
code's bug takes 281 s to find exhaustively at `-O2`, and 2 to 4 s with
100 sampled paths. At `-O0` the search does not finish in 30 minutes, but
there the old code behaves as the current code does. `tagged_index` is a
one-byte struct, so `<atomic>` (here, as in libstdc++) exchanges it with
the generic `__atomic_exchange`, and at `-O0` that is a call to
`atomic::exchange` with the memory order as an argument, which clang
compiles to a switch on the order whose cases are all the same
compare-and-swap loop of `LDAXRB` and `STLXRB`.

`spsc_queue` is a ring buffer. The producer stores an element, then
releases the write index; the consumer acquires the write index, reads
the element, then releases the read index; and each acquires the other's
index to see how much room there is. In `boost_spsc_queue.cpp`, `t0`
tries three times to push the next of 1, 2, 3, and `t1` tries to pop
three times. The queue holds one element, so it has two slots, its
indices wrap with a mask, and the third push re-uses the first slot.
`-DCAP=2` gives it three slots, and the indices wrap by the subtraction
loop that `d0172a2` keeps from being turned into a division; `-DCAP=3`,
four. `boost_spsc_range.cpp` pushes and pops ranges, which wrap around
the end of the buffer, and the producer's second push re-uses a slot.
Both are right at `-O0` (with 3,000 instructions of fuel) to `-O2` and
`-Os`, and with `LDAPR`, each in under 0.5 s.

To check that the tests would notice, each acquire and release in
`spsc_queue.hpp`'s single-element and range functions was made relaxed in
a copy of it, one at a time. The producer's release of the write index,
the consumer's acquire of it and the consumer's release of the read index
are each needed, in both kinds of function: without one, the condition is
reachable on ARM only. The producer's acquire of the read index is not
needed on ARM, since its stores to the slot are control-dependent on that
load (as with `spsc -DROOM`). The consumer checks the values only after
its last pop. A first version branched on each value as it popped it, and
that branch ordered its read of the slot before its later store of the
read index, so the test could not tell when the release was missing.

## atomic_queue

`c/atomic_queue/include` holds `atomic_queue.h` and `defs.h` from
atomic_queue, unchanged from its commit `b7f1bef` (October 2026; MIT
License, see `c/atomic_queue/LICENSE`), and `c/atomic_queue/old_2020` and
`c/atomic_queue/old_2023` hold them from before two of its fixes.
`c/atomic_queue/stub` holds freestanding stand-ins for the standard
headers they include, and `c/atomic_queue.h` picks the version and says
what each test shares. The old headers write `Base::template f(...)`
without template arguments, which clang 19 and later reject by default,
so that one diagnostic is turned off around them.

`AtomicQueue` holds atomic elements, with a reserved value (`NIL`) for an
empty slot. In the single-producer, single-consumer queue, a push waits
for its slot to hold `NIL` and releases the element into it, and a pop
waits (acquire) for an element and releases `NIL` into the slot. In the
multi-producer, multi-consumer queue, a push is an `acq_rel`
compare-and-swap from `NIL` and a pop an `acq_rel` exchange with `NIL`.
In `atomic_queue1.cpp` the queue has one slot (`-DMULTI` for the
multi-producer one). `t0` stores 42 in `data` and pushes 1; `t1` pops and
loads `data`, and the condition is that it gets the element but not 42.
With `-DPTR` the element is `&data` and `t1` loads through it.

Commit `2bc627d` (April 2023, "Fix missing acquire memory order on pop for
atomic elements") made the single-consumer pop's load of the element
acquire. Before it (`-DOLD_2023`), the condition is reachable on ARM only,
at `-O0` to `-O2` and `-Os`: `t1`'s load of `data` depends on the element
only by control, which does not order a later load. With `-DPTR` it is
unreachable, since that load has an address dependency on the element.
In C++ both are data races.

`AtomicQueue2` holds elements of any type, with a state for each slot. A
push takes the state from `EMPTY` to `STORING` with a compare-and-swap,
stores the element and releases `STORED`; a pop takes it from `STORED` to
`LOADING`, loads the element and releases `EMPTY`. (In the
single-producer, single-consumer queue, `-DSINGLE`, each waits for the
state with an acquire load instead.) In `atomic_queue2.cpp` the queue
has one slot. `t0` pushes 1 and 2, and `t1` tries to pop twice; the
condition is a pop that returns a value other than the one pushed.

Commit `95bbd40` (November 2020, "Bugfix. Use acquire semantics when
loading states of non-atomic elements.") made both compare-and-swaps
acquire. Before it (`-DOLD_2020`), the pop's is `LDXRB`/`STXRB` (or
`CASB`), so its load of the element can come before it and read the slot
before the push stores into it. That is reachable on ARM only, at `-O1`,
`-O2` and `-Os`, and with LSE. x86's `cmpxchg` is a full barrier. Only
the pop's half of the fix matters on ARM. With only the pop's
compare-and-swap made acquire (a copy of the old header, not in `c/`),
the condition is unreachable at `-O1`, `-O2`, `-Os` and with LSE, since
the push's store into the slot is control-dependent on its
compare-and-swap's load. At `-O0` clang performs every compare-and-swap
as `LDAXRB`/`STLXRB` whatever order it is given (as with
[Boost.Lockfree](#boostlockfree)'s exchange), so there the old code
behaves as the current code does.

The current code is right in all of these, and with `LDAPR`. Each test
takes under 2.5 s. To check that the tests would notice, each ordering in
the current header's push and pop functions was weakened in a copy of it,
one at a time. `-DREUSE` checks the release in `AtomicQueue`'s pop: `t0`
tries to push three times, then stores 0 in `data`, while `t1` pops,
loads `data` and pops again. The third push waits for the second pop to
empty the slot, so that release orders `t1`'s load of `data` before
`t0`'s store of 0. Without the release, `t1` can load 0 (on ARM only).

| ordering weakened | needed on ARM? |
|---|---|
| `AtomicQueue` push's release (both queues) | yes |
| `AtomicQueue` single-consumer pop's acquire | yes, unless the consumer's loads have an address dependency on the element (`-DPTR`) |
| `AtomicQueue` multi-consumer pop's exchange, to release only | the same |
| `AtomicQueue` pop's release (`NIL` store; exchange to acquire only) | yes (`-DREUSE`) |
| `AtomicQueue` push's acquire (load of `NIL`; compare-and-swap to release only) | no: the store into the slot is control-dependent on it |
| `AtomicQueue2` pop's acquire of `STORED` (both queues) | yes, and on x86 too (see below) |
| `AtomicQueue2` pop's release of `EMPTY` | yes |
| `AtomicQueue2` push's release of `STORED` | yes |
| `AtomicQueue2` push's acquire of `EMPTY` | no: the store into the slot is control-dependent on it |

Without the acquire of `STORED`, nothing between the two pops tells clang
that the element may have changed. When the second pop's
compare-and-swap succeeds at its first try, the code clang emits reuses
the first pop's load of the element instead of loading it again, so the
condition is reachable under TSO too. The 2020 code, whose relaxed
compare-and-swap is a strong one in a differently shaped loop, does not
get that optimization at the levels tested.

## Erlang

`c/erlang` holds `erl_process_lock.c` and `erl_process_lock.h`, Erlang/OTP's
process locks, unchanged from its commit `045d30b` (November 2026; Apache
License 2.0, see `c/erlang/LICENSE`), and `c/erlang/old` holds the header
from before commit `46ff98a5d5`. `c/erlang/stub` holds stand-ins for the
erts headers they include. A process is just its id and lock there, and
the process table, which the lock functions need only for lookups, is
empty. The ethread atomics, mutex and events are written out as a 64-bit
ARM build with gcc 8.3 or later compiles them, so that, for example, a
compare-and-swap with release order is `DMB SY` and a relaxed
compare-and-swap. The stand-ins don't block: a waiter polls its flag, a
spurious wakeup the lock code allows for.

A process has five locks, bits of one word, each with a waiter bit and a
queue of waiting threads. The queues and waiter bits are protected by a
mutex, the pix lock. A thread that can't take a lock queues itself, sets
the waiter bit, and polls the word `uaflgs` in its event until another
thread clears it. A thread unlocking a lock with waiters hands it over
instead: under the pix lock it dequeues the next waiter, clears the waiter
bit if that was the last one, releases the pix lock and clears the
waiter's `uaflgs`. An unlock first clears every lock it releases that has
no waiter with one compare-and-swap, based on a relaxed load of the word.

In `erlang_proc_lock.c`, `t0` holds the main lock and unlocks it, and
`t1` locks and unlocks it. The condition is that the main lock is still
held after `t1` unlocks it. With `-DDATA`, `t0` also stores 42 in `data`
before unlocking, and `t1` loads it while holding the lock; the condition
then also holds if `t1` doesn't load 42.

Commit `46ff98a5d5` (February 2026, "erts: Fix proc unlock memory order")
makes the unlock's compare-and-swap unconditional. Before it (`-DOLD`),
an unlock skipped the compare-and-swap when every lock it released had a
waiter, so nothing checked the relaxed load. The commit says that some
schedulers got stuck in a lock's wait queue on ARM. armem finds how: `t1`
can see `t0` clear `uaflgs` and then load the lock word from before `t0`
cleared the waiter bit, since nothing orders `t1`'s two loads. It sees
itself as the lock's waiter, skips the compare-and-swap and hands the lock
to a waiter that isn't there. Here, that dereferences the empty queue's
null pointer: a fault, on ARM only, at `-O0`, `-O1`, `-O2` and `-Os` and
with LSE. With the fix, the stale load makes the compare-and-swap fail, and the unlock
tries again with the lock word it read.

The current code doesn't fault, and the lock is always free at the end.
But with `-DDATA` the condition is reachable on ARM only: `t1` can load
`data` before it loads `uaflgs` as clear, and so before `t0` stored 42.
That needs the handover to happen between `t1` queuing itself and its
first poll of `uaflgs`, so that `t1` never waits on its event. Making that poll an acquire load (in a copy of
`erl_process_lock.c`, not in `c/`) makes the condition unreachable with
`-DDATA`, and with the old header too, since `t1` then sees the waiter bit
cleared. This was reported as
[erlang/otp#12178](https://github.com/erlang/otp/issues/12178). The tests
take 2 to 55 s each. A shared write in each poll would
multiply the memories, so `erts_tse_set` keeps only the barriers of its
exchange. No thread reads the event word, since the modeled waits return
at once.

## folly

`c/folly/include` holds folly's `FlatCombining.h` and
`SaturatingSemaphore.h`, unchanged from folly commit `4a2f75a` (November
2026; Apache License 2.0, see `c/folly/LICENSE`). `c/folly/old` holds
`FlatCombining.h` from before commit `6d16b1d`. `c/folly/stub` holds
freestanding stand-ins for the standard headers and the rest of folly
that they include, and `-Dthrow=` lets the throws the test never reaches
compile without exceptions. `std::thread` discards its function, and the
test runs the dedicated combiner's loop as a thread of its own. A futex
wait that would sleep, on a folly semaphore or on glibc's mutex, ends
the thread, as in [QEMU](#qemu).

A flat combiner's dedicated thread sleeps on the semaphore `pending_`
until a requester posts it. It then resets `pending_` (a relaxed store),
loads `shutdown_`, takes the lock `m_`, scans the records for valid
requests and applies them, and waits again. A requester marks its record
valid and posts `pending_`; a new record is then linked into the list
and `pending_` posted again. A post that finds `pending_` already set
writes nothing: it fences, reloads `pending_`, and returns if that is
still set.

In `folly_fc.cpp`, setup posts `pending_`, as an earlier requester
would. `t0` runs the combiner, and `t1` makes one asynchronous request
on a new record. The condition is a lost wakeup: the combiner sleeps, no
post wakes it, and the request is never applied. With `-DSHUTDOWN`, `t1`
instead shuts the combiner down, which stores `shutdown_` and posts
`pending_`; the condition is then that the combiner sleeps for good, so
that its destructor's `join()` would hang.

Commit `6d16b1d` (October 2026, "fix lost FlatCombining notifications")
added a `seq_cst` fence after the combiner resets `pending_`. Its message
says that without it, `FCPriQueue.pushPop` hung in 17 of 200 runs on
aarch64, and that shutdown could hang on x86 too. With the file from
before it (`-DOLD`), armem finds both. A requester's second post finds
`pending_` still set from the first, since the combiner's reset is not
yet visible, and only fences and reloads it. The combiner's reset is a
plain `STR`, and the `LDAR` of `shutdown_` and the acquire
compare-and-swap of `m_` after it don't order it before the `LDAR` of the
list head, which can still be empty. On x86, the lock's compare-and-swap
is a full barrier, so the lost request is ARM-only. Nothing like it
comes between the reset and the load of `shutdown_`, so a lost shutdown
can happen under TSO too. With the fence, neither condition is
reachable.

The request test holds at `-O1`, `-O2` and `-Os` with LSE, in 30 to 40 s
for the current code and 60 to 70 s for the old. With `LDXR`/`STXR`
loops the ARM search runs out of fuel: the loop that links a new record
into the list writes the record's next pointer on every pass, and since a
store-exclusive may always fail, each pass adds a write and no state
repeats. With at most one spurious failure per run (`spurious=1`, see
[Limitations](../README.md#limitations-current)) it gives the same verdicts at
`-O1`, `-O2` and `-Os`, in about 60 s for the current code and 200 to
230 s for the old; `c/expected.txt` checks `-O2`. The
shutdown test holds at `-O0` to `-O2` and `-Os`, and with LSE at `-O2`,
in 2 to 8 s.

## HAProxy MT_LIST

HAProxy passes tasklets between threads on lists built with its MT_LIST
macros. A thread that wakes a tasklet owned by another appends it to the
owner's `shared_tasklet_list`. The owner takes the whole list with
`MT_LIST_BEHEAD`, splices it onto a private list, and runs each tasklet
after unlinking it with `LIST_DEL_INIT`. MT_LIST locks a link by
exchanging `MT_LIST_BUSY` (1) into it, with relaxed exchanges, and
unlocks it with a plain store. Its only ordering is
`__ha_barrier_store`, `DMB ISHST` on aarch64, between its stores.
Commit `787dc2095` (July 2020, "add missing store barrier on
MT_LIST_BEHEAD()") added four of them to `MT_LIST_BEHEAD`, after the
scheduler's list was corrupted about once per million connections on 8
Cortex-A53 cores.

`c/haproxy` holds `list.h` and `list-t.h` in three versions (LGPL 2.1,
see `c/haproxy/LICENSE`): `c/haproxy/include` from `787dc2095` (2.2),
`c/haproxy/old` with `list.h` from before it, and `c/haproxy/3.0` from
the 3.0 branch at `6038814` (October 2026), whose MT_LIST is the same as
2.8's. `c/haproxy/stub` holds stand-ins for the headers that `list.h`
includes: the exchange and the barriers, copied from HAProxy's
`atomic.h`.

In `haproxy_mt_list.c`, `waker` wakes tasklet `t1`, then `t2`, as 2.2's
`tasklet_wakeup_on` does (`-DV30`: as 3.0's, with `MT_LIST_APPEND`).
`owner` takes and runs the list once while it does, then waits for it
and drains the list again. The condition is that some tasklet that was
queued is not run exactly once, a list is left malformed, or the owner
meets a pointer that is not a tasklet or a list head (which HAProxy
would follow).

The condition is reachable on ARM in all three versions, and never
under TSO. The owner's exchange of the head's `prev` pointer can see the
waker's last unlock, which publishes `t2`, while the owner's later read
of `t1`'s `next` pointer, made by `LIST_DEL_INIT`, returns
`MT_LIST_BUSY` or the head. The waker writes that pointer before the
unlock, with a `DMB ISHST` between, but nothing orders the owner's read
after its exchange. The read's address comes from the owner's other
exchange, of the head's `next` pointer, which the waker doesn't write,
and `DMB ISHST` orders only stores. With `MT_LIST_BUSY`, `LIST_DEL_INIT`
writes to address 9, as in the crashes with `t->list.n = 0x1` at
`LIST_DEL_INIT` in HAProxy issue
[#958](https://github.com/haproxy/haproxy/issues/958). Those crashes had
other causes, which were fixed, but in that issue an Arm engineer noted
that MT_LIST's store barriers have nothing pairing them on the load side,
and in 2.8 and 3.0 they still don't. With a `__ha_barrier_load()` at the
end of `MT_LIST_BEHEAD`'s success path, or with acquire exchanges,
sampled runs no longer reach it. Mainline replaced MT_LIST in 3.1 with
`include/import/mt_list.h`, whose release fences compile to a full
`DMB ISH` on aarch64, which orders the loads as well. This was reported
as [haproxy/haproxy#3572](https://github.com/haproxy/haproxy/issues/3572).

The harness does not reach the bug `787dc2095` fixed: its store barriers
make no difference here. That probably needs a third thread, or
`MT_LIST_DEL` run concurrently with `MT_LIST_BEHEAD`.

Each MT_LIST lock spins by exchanging `MT_LIST_BUSY` over itself, which
writes on every pass, so `c/expected.txt` uses `stutter=1` (see
[Limitations](../README.md#limitations-current)), and with `LDXR`/`STXR` loops
`spurious=1` too. Even then the exhaustive ARM search of two threads
does not finish in 30 minutes, so the ARM side is sampled (20 paths, in
3 s with LSE, 8 s without, and 24 s at `-O0`). Sampling shows the
condition reachable at `-O0` to `-O2` and `-Os`, but cannot show that
any version is correct.


## CPython

CPython's free-threaded build cached type attribute lookups in a global
table guarded by a seqlock, `_PySeqLock` in `Python/lock.c`. A lookup
(`_PyType_LookupRef`) reads an entry's version, the type's version and the
entry's name and value between `_PySeqLock_BeginRead` and
`_PySeqLock_EndRead`, and uses the value only if they match and the
sequence is unchanged. An update (`update_cache_gil_disabled`) makes the
sequence odd with a compare-and-swap, stores the entry's fields (relaxed),
and stores the next even value. Commit `1d3cf79a5` (July 2024,
gh-121368) fixed its memory ordering: lookups could return incorrect
results on macOS arm64. It changed two functions:
`_PySeqLock_EndRead` re-read the sequence with an acquire load, and now
uses an acquire fence and a relaxed load; and `_PySeqLock_LockWrite` gained
a release fence after its compare-and-swap. This is the pattern of
`c/seqlock.c`, with the reader's fence (`R_FENCE`) or the writer's
(`W_FENCE`) missing.

`c/cpython` holds CPython's `pyatomic.h` headers unchanged from
`1d3cf79a5` (PSF License, see `c/cpython/LICENSE`), and the seqlock
section of `Python/lock.c` unchanged from `1d3cf79a5` (`c/cpython/new`)
and from just before it (`c/cpython/old`). The rest of `lock.c` (mutexes
and the parking lot) would need much of CPython to compile, so only that
section is taken. `c/cpython/stub` stands in for the rest of CPython
that they use.

In `cpython_seqlock.c`, the writer and reader copy CPython's, without
reference counting. `writer` replaces the entry for (version 1, `n1`, `a`)
with (version 2, `n2`, `b`), as a lookup of another type and name that
hash to the same slot would. `reader` looks up `n1` for a type whose
version is 1. The condition is that it gets `b`, the new entry's value
under the old entry's name and version. `-DOLD` takes both functions
from before the fix, and `-DOLD_READ` and `-DOLD_WRITE` one each:

| Version | `LDXR`/`STXR` | LSE |
|---|---|---|
| `1d3cf79a5` | no | no |
| `OLD` | ARM only | ARM only |
| `OLD_READ` | ARM only | ARM only |
| `OLD_WRITE` | ARM only | no |

The same holds at `-O0`, `-O1`, `-O2` and `-Os`, each in about 0.1 s.

With the old `_PySeqLock_EndRead`, the reader's load of the value can be
satisfied after its acquire load of the sequence, which orders only the
accesses after it: the reader takes the new value with the old sequence.
That happens on any ARM build, and is the half of the fix the commit
message describes. With the old `_PySeqLock_LockWrite`, the writer's
stores to the entry can be seen before its compare-and-swap makes the
sequence odd, but only where the compare-and-swap is an `LDAXR`/`STLXR`
loop: a store-release orders only what comes before it. LSE's `CASAL`
orders the later stores, as herd7 with Arm's model agrees on the
two-instruction test, so a build for Apple's arm64 chips, which all have
LSE, doesn't need that half.

CPython has since replaced the global table with a cache per type
(commit `daf09e17bf`, July 2026), which synchronizes by a release store
and acquire load of each entry's name and a check of the type's version,
and removed `_PySeqLock` (`d2f4649592`, August 2026).
`cpython_typecache.c` tests that cache as it is on main at `0ada998ebb`
(November 2026). `c/cpython/main` holds `Python/typecache.c`,
`pycore_typecache.h` and the atomics headers unchanged from that commit,
and `c/cpython/stub_main` stands in for the rest of CPython, as a
free-threaded build sees it.

Each type points to a hash table of (name, value) entries for one
version of the type. A lookup (`_PyTypeCache_Lookup`) loads the pointer
and probes for the name with acquire loads, reads the value, and returns
it only if the table's version is still the type's. Writers hold the
type lock: an insertion stores the value and then the name (release),
first replacing a full table with a bigger copy (release), and modifying
a type sets its version to 0 and swaps in the shared empty table before
changing the dict. `setup` caches attribute `n` as `v1` at version 1.
`writer` sets `n` to `v2`, as `type_setattro` does, then looks it up as a
miss does: it assigns version 2 and caches `v2` in a new table. `reader`
looks `n` up twice, without the lock. The condition is that a hit returns
a value that is not the attribute's value at the version it reports, or
that the second lookup hits an older version than the first.

It is never met, at `-O0`, `-O1`, `-O2` and `-Os`, with and without LSE,
each in about 0.2 s: every table is published by a release store and
read by an acquire load, a table's version never changes, and the
reader's check of the type's version comes after its acquire loads. To
show that the test can see a bug, `-DRELAXED_STORES` makes the cache's
release stores relaxed and `-DRELAXED_LOADS` its acquire loads; each
makes the condition reachable on ARM only. Clang vectorizes the
insertion's two counter updates at `-O1` and above into a vector `ADD`,
which armem does not support, so the program is compiled with
`-fno-vectorize -fno-slp-vectorize`.

## gRPC

gRPC's C core delivers completions through a completion queue. Its count
`pending_events` starts at 1, gains 1 for each operation begun and loses
1 for each ended, and `grpc_completion_queue_shutdown` drops the initial
1. Whichever of them brings it to 0 finishes the shutdown, which
`GRPC_CHECK`s that `shutdown_called` is set and shuts the pollset down,
waking threads blocked in `grpc_completion_queue_next` or `_pluck`.
Commit `f01a138e8` (April 2026, PR
[#41510](https://github.com/grpc/grpc/pull/41510)) says that on ARM64 a
server's shutdown could hang for 20 minutes or more because
`shutdown_called` "lacks memory barriers, causing blocked threads to never
wake up". It made the flag a `std::atomic<bool>`, stored with release and
loaded with acquire when the shutdown is finished.

`grpc_cq.cpp` copies the shutdown and end-of-operation functions from
`src/core/lib/surface/completion_queue.cc` (Apache License 2.0, see
`c/grpc/LICENSE`) at that commit and, with `-DOLD`, from just before it.
Tracing, debug-only tag checks, the queue's reference counts and the
EventEngine callbacks are left out. `gpr_mu` is a spinlock, and the
pollset's kick does nothing while its shutdown counts calls. `setup`
begins one operation, then `t0` shuts the queue down while `t1` ends the
operation. The condition is that the shutdown's check fails, or that it
is finished other than once. The queue is the `next` kind, or with
`-DPLUCK` or `-DCALLBACK` the `pluck` or `callback` kind.

Before the fix, the flag is written under the queue's lock, before the
shutdown's acq_rel `fetch_sub` of `pending_events`. It is read under the
lock, or in the finishing of the shutdown after a `fetch_sub` (or acquire
load) that reads the count the shutdown left. No blocked thread reads it.
The tests agree: neither the old code nor the new can meet the condition,
for all three kinds, with and without LSE, at `-O0`, `-O1`, `-O2` and
`-Os`, each in under 6 s. To show that the test can see a bug, `-DRELAXED`
makes the `fetch_sub`s relaxed. Only the callback queue, whose last
operation finishes the shutdown without the lock, depends on them:

| Kind | old | `f01a138e8` | old, `RELAXED` | `f01a138e8`, `RELAXED` |
|---|---|---|---|---|
| `next` | no | no | no | no |
| `pluck` | no | no | no | no |
| `callback` | no | no | ARM only | ARM only |

The new code's release store and acquire load of the flag don't help with
`RELAXED`, since neither is ordered with the `fetch_sub`s. At `-O0`
without LSE, clang builds every `fetch_sub` as an `LDAXR`/`STLXR` loop
whatever its order, so `RELAXED` changes nothing there. So `f01a138e8`
fixes no ARM-only bug, and the hang must have another cause. The Ruby
test that showed it is `active_call_spec.rb`, which PR
[#40770](https://github.com/grpc/grpc/pull/40770) proposed skipping on ARM
(it was closed unmerged).

To find that cause, `scripts/grpc_ruby_hang.sh` runs gRPC's Ruby specs
over and over against a given `grpc` gem, and dumps every thread's stack
with gdb when a run hangs. The `gRPC Ruby hang` workflow runs it by hand
on GitHub's arm64 runners, as gRPC's arm64 CI runs them: dbg and opt
builds from source, each spec file in its own process, 16 at once, with
`GRPC_VERBOSITY=DEBUG`. On Neoverse-N2 with 1.80.0, neither hung: 80,000
runs of `active_call_spec.rb` alone (opt, 4 at once on each of 4
runners), and 69,120 runs of 27 of CI's 29 spec files (dbg and opt, 16 at
once), 2,560 of them `active_call_spec.rb`. The only failures were 2 runs
of `channel_connection_spec.rb` that lost the port they restart on to
another spec.

Each turn of the spinlock's exchange that finds the lock held writes back
the 1 it read, so the tests bound that with `--stutter 1` (see
[Usage](../README.md#usage)). Without the bound, the `next` queue at `-O2` without
LSE takes about 150 s to give the same verdict, old or new, and with LSE
it doesn't finish in 10 minutes.

Reading the server's side of shutdown turned up a different bug, one
that doesn't depend on memory order. `grpc_server_request_call` begins an
operation on the notification queue, and `Server::QueueRequestedCall`
then checks `ShutdownCalled()` and, if shutdown hasn't been called, pushes
the request onto a lock-free queue to wait for a call.
`Server::ShutdownAndNotify` fails the queued requests first and marks
shutdown as called after, and `MaybeFinishShutdown` fails them once more
before publishing the shutdown. A request whose check comes before the
mark and whose push comes after that last kill is never failed. Its
operation never ends, so the queue's shutdown never finishes and a
thread waiting for it waits forever. If the server is destroyed instead,
`~RealRequestMatcher`'s `GRPC_CHECK_EQ(queue.Pop(), nullptr)` fails. Ruby
can't hit it, since the GVL serializes its calls to both functions, but a
C++ server that requests calls on one thread while another shuts it down
can.

`grpc_server.cpp` copies `QueueRequestedCall`, `ShutdownAndNotify`,
`MaybeFinishShutdown`, the matcher's queueing and killing, and the queue
(`src/core/util/mpscq.cc`) from commit `28d1790b9c` (November 2026), for
a server with no channels or listeners left, so that shutdown publishes
at once. `t0` requests a call and `t1` shuts the server down. The
condition is that the request is failed other than once, or that
shutdown isn't published. With `-DQUEUED`, another request is queued
first, so the push takes no lock. Two fixes are tried. `-DFIX_LOCK` holds
`mu_call_` across the check and the push. `-DFIX_RECHECK` checks the flag
again after the push and, if it is now set, fails the queued requests
itself:

| Variant | ARM | TSO |
|---|---|---|
| as in gRPC | yes | yes |
| `-DFIX_LOCK` | no | no |
| `-DFIX_RECHECK` | no | no |
| `-DFIX_RECHECK -DRELAXED` | no | no |

Each verdict is the same with and without `-DQUEUED` and LSE, at `-O2`
and, without `-DQUEUED`, at `-O0` (`-DQUEUED` at `-O0` needs more fuel than 1000 and
takes minutes). The recheck looks like store buffering, which ARM allows
unless both sides are ordered. But it holds even with `-DRELAXED`, which
makes `ShutdownCalled()`'s load relaxed. For the shutdown's pop to miss
the request, the pop must reach its `Push(&stub_)`, an acq_rel exchange of
the same `head_` that the request's push exchanges. Whichever exchange
comes second reads from the first, so either the request sees the flag
or the pop sees the request.

`c/grpc/repro/repro.cc` shows the bug in gRPC itself. Each iteration
starts a server with no ports and races `grpc_server_request_call`
against `grpc_server_shutdown_and_notify` on two threads; the request's
tag should come back with `success=0`. `build.sh` builds it against gRPC
core from the `grpc` Ruby source gem, optionally with `delay-hook.patch`
(which sleeps between the check and the push when
`GRPC_REPRO_QUEUE_DELAY_US` is set) and `fix-recheck.patch` (the recheck
fix). On x86_64 with 1.85.0, whose request and shutdown code is the same
as `28d1790b9c`'s:

| Build | Result |
|---|---|
| unmodified, 4 pairs at once | 4 requests lost within 80,000 races (14 s) |
| recheck fix, 4 and 8 pairs | none lost in 1,000,000 races each |
| delay hook, 100 ms | lost on the first race |
| delay hook and recheck fix, 100 ms | none lost in 20 races |

Each lost request's tag never comes back, though the shutdown's does,
and `grpc_completion_queue_shutdown` then never finishes. Reported as
gRPC issue [#43934](https://github.com/grpc/grpc/issues/43934).

## PostgreSQL AIO

PostgreSQL 18's asynchronous IO, with `io_method=worker`, hands each IO
to an IO worker process through a handle in shared memory. The backend
stages the IO and puts the handle's index on the workers' submission
queue under an LWLock. A worker takes it, performs the IO and completes
it: it stores the raw result, runs the shared completion callbacks into
`->distilled_result`, and sets the handle's state to
`PGAIO_HS_COMPLETED_SHARED` with `pgaio_io_update_state()`, which puts
`pg_write_barrier()` (a release fence) before the store. The backend
that owns the handle sees that state and reclaims it
(`pgaio_io_reclaim()`), running the local callbacks on
`->distilled_result` and passing the result back to the caller,
`WaitReadBuffers()` among them. Commit `e9a3615a5` (June 2025, "aio: Add
missing memory barrier when waiting for IO handle") says that on Apple
hardware the new state could be seen before the result, which caused
rare assertion failures, most often in `WaitReadBuffers()`. Its fix
adds `pg_read_barrier()` in `pgaio_io_wait_for_free()`, which a
backend that has no free handle calls, between its two checks of the
state and the reclaims that follow. The usual path, waiting for a given
IO, already reads the state in `pgaio_io_was_recycled()`, which has the
barrier.

`postgres_aio.c` copies the functions the handle passes through from
`src/backend/storage/aio/aio.c`, `aio_callback.c` and `method_worker.c`
at that commit and, with `-DOLD`, from just before it (PostgreSQL
License, see `c/postgres/LICENSE`). Debug messages, resource owners,
interrupts, Asserts (as in a production build), condition variables
(the waits spin), latches, and synchronous execution are left out, and
the IO has no callbacks. The LWLock is a spinlock. `backend`, with
`io_max_concurrency=1`, acquires a handle, stages a read and submits it,
then acquires a handle for its next IO, which waits for the first and
reclaims it. `worker` takes the IO from the queue, reads 8,192 bytes and
completes it. The condition is that the result passed back to the
backend is not `PGAIO_RS_OK` with 8,192 bytes.

The old code meets the condition on ARM only, and the fixed code never
does, at `-O1`, `-O2` and `-Os`, with and without LSE. The full search
takes 20 to 25 s for the fixed code and 30 to 120 s for the old, and
one sampled path of the old code finds it in about 12 s. At `-O0` the
ARM search does not finish in 15 minutes, but one sampled path finds the
old code's bug (in 13 s with LSE, and 27 s without, which needs 2,000
instructions of fuel), and neither version meets the condition under
TSO. `c/expected.txt` samples the old code. The backend gets the
status and the byte count that its own reclaim of the handle's previous
use left (`PGAIO_RS_UNKNOWN` and 0), in any mix, since its loads of
`->distilled_result` can be satisfied before its load of the state. The
handoff on master (`77bba5454f`, November 2026) is the same, with the
barriers, except that the handle's fields are no longer enum bitfields.

Each turn of the spinlock's exchange that finds the lock held writes
back the 1 it read, so the tests bound that with `--stutter 1` (see
[Usage](../README.md#usage)).

## PostgreSQL invalidation queue

A PostgreSQL backend that commits a catalog change appends invalidation
messages to a circular buffer in shared memory, under `SInvalWriteLock`,
and advances `maxMsgNum` (`src/backend/storage/ipc/sinvaladt.c`). Every
other backend, whenever it takes a lock, checks its `hasMessages` flag
and reads the messages up to `maxMsgNum` under `SInvalReadLock`. Commit
`df3978c234` (September 2026, "Convert SISeg->maxMsgNum to an atomic
variable") removes the spinlock `msgnumLock`, which the writer held to
store `maxMsgNum` and the reader to load it, and which was there only
for its barriers. Instead the writer stores it with
`pg_atomic_write_membarrier_u32()` and the reader loads it with
`pg_atomic_read_membarrier_u32()`. Two orderings matter. The messages
must be seen before the new `maxMsgNum`. And the reader stores its
`hasMessages = false` before it loads `maxMsgNum`, while the writer
stores `maxMsgNum` before it sets each backend's `hasMessages = true`.
If the reader's `false` lands after the writer's `true` while its load
returns the old `maxMsgNum`, it misses the message until another one
comes.

`postgres_sinval.c` copies `SIInsertDataEntries()` and
`SIGetDataEntries()` at that commit and, with `-DOLD`, from just before
it (PostgreSQL License, see `c/postgres/LICENSE`), with PostgreSQL's
arm64 atomics. The read is `__sync_fetch_and_add(ptr, 0)`, the write
`__atomic_exchange_n()` with `__ATOMIC_SEQ_CST`, and the spinlocks
`__sync_lock_test_and_set()` and `__sync_lock_release()`, with a plain
test while spinning. `setup` queues one message. `writer` queues
another, then releases a lock that `reader` then tries to take (in
PostgreSQL, the heavyweight lock on the relation). `reader` reads its
messages once while the writer runs, as on taking any lock, and again
if it gets the lock, by when it must have the writer's message. The
condition is that a reader that got the lock did not get exactly the two
messages. `SICleanupQueue()`, signals and Asserts are left out.

Neither the old code nor the new meets the condition, on ARM or under
TSO, at `-O0` to `-O2` and `-Os`, with and without LSE, in at most 6 s
each and without bounding `--stutter`: the commit is correct here. Both
full barriers are needed. With `-DACQUIRE_READ`, an acquire load of
`maxMsgNum`, the condition is met on ARM and under TSO (store
buffering). With `-DRELEASE_WRITE`, a release store, it is met on ARM
only: a release orders the messages before `maxMsgNum`, but not the
stores of `hasMessages` after it, which TSO keeps in order.

Before the pre-pass and private-memory changes this test prompted, the
new code's search did not finish. Each lock has one user here, so it is
private, and its acquire exchanges and release stores now make no write
messages (see [Private memory](../README.md#how-it-works)). The reader's add of 0
copies `maxMsgNum` without publishing anything. Without LSE, clang
compiles that add as an `LDAXR` and an `STLXR` of the same register.
The writer reads back its own `maxMsgNum`, and each 16-byte message is
copied with an unaligned `STUR`, 16 pieces nothing orders (see the
static reduction). At `-O2` the new code now takes 64 memories with LSE
and 183 without, and the old code 789 (from 3,950). A plain store of
`maxMsgNum`, which publishes nothing, still does not finish in 10
minutes: the reader's reads of the message are then not published, so
the reduction keeps every order of its pieces.

## tokio Notify

tokio's `Notify` (`tokio/src/sync/notify.rs`) keeps its state in one word:
two bits for `EMPTY`, `WAITING` or `NOTIFIED` (a stored `notify_one`
permit), and above them a count of `notify_waiters` calls. Waiting futures
are queued on an intrusive list under a mutex. Commit `d4fb4bb9` (tokio
1.54.0, #8325, August 2026) weakened all 19 accesses to the state from
`SeqCst` to `Acquire`, `Release` or `AcqRel`. Its argument: each decision
that avoids the lock is made by a read-modify-write, which reads the
latest value; a stale load is checked again by a later read-modify-write
or under the mutex; and what the orderings must carry is the
happens-before from data written before `notify_one` or `notify_waiters`
to the task that consumes the notification. Being a weakening, any
mistake in it would be a new bug.

`tokio_notify.cpp` is a C++ port of `Notify`, `Notified`'s poll and drop,
and the intrusive list (`util/linked_list.rs`, including the guarded list
`notify_waiters` uses) from tokio 1.54.1 (MIT License), with one future and
one notifier. `-DOLD` restores `SeqCst` everywhere. A waker is a flag,
which `wake()` sets with a relaxed store, so that only `Notify` carries
data, and the mutex is a spinlock. `t0` creates the future and polls it.
If the poll returns `Pending`, `t0` checks the flag once, and if the waker
has run it polls again. Once the poll returns `Ready`, `t0` reads `data`.
`t1` writes `data` and calls `notify_one`. The condition is a lost wakeup
(`t0` not ready and its waker never run) or a stale read (`t0` ready but
`data` 0). With `-DWAITERS`, `t1` calls `notify_waiters` instead, but only
if it sees that the future has been created, since `notify_waiters` only
wakes futures created before it. With `-DDROP`, `t0` polls once and then
drops the future, which must pass on a `notify_one` it received but did
not consume. The condition is then that the permit is lost or duplicated,
or a stale read. Two weakenings show what the harness catches.
`-DRELAXED_TAKE` makes the poll's optimistic `NOTIFIED` to `EMPTY`
compare-exchange, which takes a permit, relaxed. `-DRELAXED_INIT` makes
the poll's first load of the state, which sees that `notify_waiters` was
called, relaxed.

| Scenario | Variant | ARM | TSO |
|---|---|---|---|
| `notify_one` | 1.54.1 | no | no |
| `notify_one` | `-DOLD` | no | no |
| `notify_one` | `-DRELAXED_TAKE` | yes | no |
| `notify_waiters` | 1.54.1 | no | no |
| `notify_waiters` | `-DOLD` | no | no |
| `notify_waiters` | `-DRELAXED_INIT` | yes | no |
| drop | 1.54.1 | no (sampled) | no |
| drop | `-DOLD` | no (sampled) | no |
| drop | `-DRELAXED_TAKE` | yes | — |

So `d4fb4bb9` is correct here. The `notify_one` verdicts are exhaustive
at `-O0` to `-O2` and `-Os`, with and without LSE, in 2 to 15 s each.
At `-O0` without LSE, `-DRELAXED_TAKE` is not caught, because clang then
compiles every compare-and-swap as an `LDAXR`/`STLXR` loop whatever its
order. `notify_waiters` is exhaustive without LSE (14 s at `-O0`, 80 to
100 s optimized). With LSE its ARM search doesn't finish in 20 minutes,
old or new, at any level. At `-O2`, 200 single random paths each found
nothing for either version and caught `-DRELAXED_INIT` on all 200. The
drop scenario's ARM search doesn't finish in an hour at `-O2`, old or
new. Of 300 single random paths each (10 s limit), 283 finished without
meeting the condition for each version, and with `-DRELAXED_TAKE` 218 of
300 met it. Its TSO and SC searches finish at once. A run of several
`--sample` paths can stall on one expensive memory, so these runs used
`--sample 1` with seeds 1 to 300.

## MySQL NDB job buffers

NDB, MySQL Cluster's data node, runs its blocks on threads that pass
signals through job buffer queues (`storage/ndb/src/kernel/vm/mt.cpp`).
A queue is a ring of 32 KB job buffers with one consumer, the thread
that owns it, and producers that hold the queue's write lock. A producer
copies a signal into the current buffer and publishes it with
`publish_position()`: `wmb()`, then the buffer's new `m_len`. When the
buffer is nearly full it puts a fresh one (`m_len` 0) in the next slot
and, after another `wmb()`, advances `m_write_index`. The consumer takes
no lock. `read_jba_state()` (for the prio A queue; `read_all_jbb_state()`
for prio B) loads `m_write_index` and then the current buffer's `m_len`,
and after an `rmb()` `execute_signals()` runs the signals up to it. When
it reaches the end of a buffer that isn't the last, it moves to the next
buffer, loads that buffer's `m_len`, and goes on. On aarch64 `rmb()` and
`wmb()` are both `DMB ISH`.

Commit `145987997` (April 2022, Bug#33650674 "Missing memory barriers -
Apple M1") added an `rmb()` where `run_job_buffers()` reads the prio A
state again inside its loop, which had none. It also removed an earlier
workaround (bug#3589359) that ran an `rmb()` before every signal on
aarch64, as "too intrusive". `mysql_ndb_jb.c` holds excerpts of the
producer and consumer as in trunk (November 2026; GPL-2.0, see
`c/mysql/LICENSE`) and, with `-DFIX` and `-DOLD`, at `145987997` and just
before it. `-DOLD -DNO_WORKAROUND` also leaves out the per-signal
`rmb()`. `t1` sends two signals; by default each fills a buffer, so the
second goes into the next one, and `-DSAME` puts both in one buffer. `t0`
runs the prio A part of `run_job_buffers()` once, and records the
signals it executes. The buffers start zeroed, standing for a recycled
buffer's old contents, and the condition is a signal executed with other
contents than were sent. `-DNEXT_RMB` adds an `rmb()` after the next
buffer's `m_len` load.

| Version | Next buffer | Same buffer |
|---|---|---|
| before bug#3589359 (`-DOLD -DNO_WORKAROUND`) | yes (and faults) | yes |
| per-signal `rmb()` (`-DOLD`) | no | no |
| `145987997` (`-DFIX`) | yes | no |
| trunk | yes | no |
| `145987997` or trunk with `-DNEXT_RMB` | no | no |

Each verdict holds at `-O0` to `-O2` and `-Os`, with and without LSE,
with one exception (below), and none is reachable under TSO. So armem
finds the bug `145987997` fixed, and finds that removing the workaround
reopened another path, still open in trunk. Moving to the next buffer
loads its `m_len` and then its signals, and nothing orders those two
loads. A buffer the producer is still filling can then show a new
`m_len` with old contents, and the consumer executes a stale signal. The
default scenario's fault before bug#3589359 is a load through a stale
null `m_buffers[]` slot. Trunk's one exception is `-O0` without LSE,
where clang compiles the release exchange in `wakeup_all()`'s inlined
mutex as an `LDAXR`/`STLXR` loop, and that orders the loads. NDB's
`wakeup_all()` calls glibc's `pthread_mutex_lock()` and
`pthread_mutex_unlock()`, which glibc compiles like the `-O1` and up
code here at any NDB `-O` level. `execute_signals()` drains the prio B
queues the same way, so the same path applies to them. This was
reported as [MySQL bug #121785](https://bugs.mysql.com/121785), with
the `-DNEXT_RMB` `rmb()` as the suggested fix.

## .NET sync block table

In .NET's CoreCLR, an object's header word holds a thin lock, a hash
code, or an index into `g_pSyncTable`, the process-wide array of
`SyncTableEntry`s, each pointing to the object's `SyncBlock`
(`src/coreclr/vm/syncblk.h`, `syncblk.cpp`). `ObjHeader::GetSyncBlock()`
runs under the sync block cache lock. When no slot is free,
`NewSyncBlockSlot()` calls `Grow()`, which copies the table into one
twice the size and publishes it with an exchange, keeping the old table
until the next GC. Then, holding the header's spin lock, `GetSyncBlock()`
moves the header's hash code into the new `SyncBlock`, stores the
`SyncBlock` in the entry, and puts the index into the header with a
compare-and-swap. `Object::TryGetHashCode()` takes no lock: it reads the
header, and for an index, `PassiveGetSyncBlock()` reads the header again
and then `g_pSyncTable[index].m_SyncBlock`.

Commit `a95fccd35` (October 2026, "Order sync-table lookups and
SyncBlock publication", dotnet/runtime#135179, fixing #134252) makes
`PassiveGetSyncBlock()` read the header with acquire and stores the
entry with a release store. Before it, both loads of the header were
plain, and the load of `g_pSyncTable` doesn't depend on the header.
`dotnet_synctable.c` holds excerpts of both sides as in main (November
2026; MIT, see `c/dotnet/LICENSE`) and, with `-DOLD`, just before
`a95fccd35`, with the PAL's `Interlocked` functions as clang compiles
them (no added barrier). The object starts with hash code 5 in its
header, and the table is full. `t1` runs `GetSyncBlock()` for the
object, so it grows the table from two entries to four and takes index
2. The old table is followed by heap memory holding a stale `SyncBlock`
pointer (hash code 7, or null with `-DZERO_HEAP`). `t0` runs
`TryGetHashCode()`, and the condition is any result but 5. `-DNO_GROW`
takes a free slot instead, so the table isn't replaced. `-DPLAIN_ENTRY`
keeps main's reader but stores the entry plainly.

| Version | Table grows | Free slot (`-DNO_GROW`) |
|---|---|---|
| before `a95fccd35` (`-DOLD`) | yes | no |
| main | no | no |
| main's reader, plain entry store (`-DPLAIN_ENTRY`) | no | |

Each verdict holds at `-O0` to `-O2` and `-Os`, with and without LSE,
and none is reachable under TSO. Each run takes under a second. So
armem finds the bug `a95fccd35` fixed. The old lookup can load
`g_pSyncTable` before the header, then pair the new index with the old
table, reading past its end. With a free slot, the old code holds,
because the entry's address depends on the index read from the header.
Of the fix's two halves, only the reader's acquire matters: the
header's compare-and-swap (`STLXR` or `CASAL`) already orders the entry
store before the index. NativeAOT got the same fix in `d148189ce`, in
C# (`ObjectHeader.NativeAot.cs`), so it isn't run here. In current
CoreCLR, the lock-free readers that go from a header index to the table
go through `PassiveGetSyncBlock()`. The others are the GC's own scans,
the heap walk for ETW's bulk type events, and COM wrappers, which keep
their own copy of the index. Nothing to report.

## xenium kfifo queue

xenium's `kirsch_kfifo_queue` (`kirsch_kfifo_queue.hpp`) is a linked
list of segments of k slots, with `head_` and `tail_` pointing to
segments. `push()` scans the tail segment for an empty slot with relaxed
loads and fills one with a release compare-and-swap, or, if the segment
is full, links a new one and advances `tail_`. `do_pop()` takes `head_`
(acquire), scans its segment for a full slot with relaxed loads, and
loads `tail_` (acquire). If it found nothing, the queue is empty when
`head_` and `tail_` are the same segment and a relaxed reread of `tail_`
hasn't changed; otherwise `advance_head()` moves `head_` to the next
segment, which it loads (acquire) without checking for null, and
`do_pop()` retries.

Commit `048e278` (October 2026, "Fix potential race in kirsch kfifo
queue", on the unmerged branch `fix/kirsch-kfifo-queue`) adds an acquire
fence after a scan that finds nothing and makes the compare-and-swaps
that empty a slot release (acq_rel in `do_pop()`), because the relaxed
slot loads can be satisfied after the loads of `tail_`, so a pop can
miss an item and report the queue empty. `xenium_kfifo.cpp` runs the
header from main (`1c449ae`, July 2025; MIT, see `c/xenium/LICENSE`)
and, with `-DFIX`, from `048e278`, with segments of one slot and a
reclaimer that never frees. A (1) is pushed in setup, filling segment S.
`t1` pushes B (2), which links S2, moves `tail_` to it and puts B
there, and then pops A. `t0` pops once. The queue is never empty, and
the condition is `t0` finding it empty. To keep the search small, `t0`
runs `do_pop()` copied into the harness, stopping when it finds an item,
and `push()`'s first pass (which calls `advance_tail()`) is copied too,
with S2 allocated in setup; `push()` and `try_pop()` themselves run the
rest. `-DFENCE_ONLY` is main with just the fence, and `-DTAIL_ACQUIRE`
is main with the reread of `tail_` acquire.

| Version | Found empty | Null `head_` |
|---|---|---|
| main | yes | yes (fault) |
| `048e278` (`-DFIX`) | no | no |
| main with the fence (`-DFENCE_ONLY`) | no | no |
| main with an acquire reread (`-DTAIL_ACQUIRE`) | yes | no |

main and `048e278` give the same verdicts at `-O0` (with 3000 fuel) to
`-O2` and `-Os`, and the other two at `-O1` and `-O2`. Nothing is
reachable under TSO. Each run takes under a minute. So armem finds the
race `048e278` fixes, and a worse outcome of it on main: if the relaxed
reread of `tail_` sees S2 while the acquire load before it saw S, the
empty check fails, and `advance_head()`'s first load of `S->next` can
still see null while its second load sees S2. It then moves `head_` to
null, and the next pop faults loading the segment's `k`. Reported as
[xenium#48](https://github.com/mpoeter/xenium/issues/48). With the fix,
seeing the slot empty brings `t1`'s link of S2, so both loads see it.
The fence alone holds on ARM only because `t1`'s pop (`LDAXR`) can't
come before its `STLXR`s in `push()`; C++ doesn't promise that, so the
release compare-and-swaps are needed too. Making the reread acquire
stops the crash but not the false empty. With LSE some executions
exceed even 3000 fuel, so those builds aren't in `expected.txt`; main
still faults there.

An earlier version of the harness, running `push()` and `try_pop()`
unchanged on both threads, was too big to explore. Sampling it found
the same fault on main (one path in nine that finished), and none in
nine with the fix. A second scenario, in which `t0` pops A itself and
then pops again, can't reach the fault even on main: `t0`'s first pop
finds `head_` and `tail_` the same and calls `advance_tail()`, which
reads `S->next` itself.

## xenium hash map

xenium's `vyukov_hash_map` (`vyukov_hash_map.hpp`, `impl/vyukov_hash_map.hpp`,
`impl/vyukov_hash_map_traits.hpp`) keeps up to three items per bucket in
arrays `key[]` and `value[]`, and more in a list of extension items
taken from shared free lists. A bucket's `state` word holds a lock bit,
the item count, a delete marker and a version. `emplace` and `erase`
lock the bucket; `erase` bumps the version, and marks the slot it
overwrites while it moves an item into it (the last array item, or the
first extension item). `try_get_value()` takes no lock: it loads `state`
(acquire), compares each key, loads the value (acquire), and rereads
`state` (relaxed), retrying if the version changed and skipping a marked
slot.

PR #43 (squashed into main as `1c449ae`, July 2025, "Fix memory order
issues in vyukov_hash_map") made the key loads in `try_get_value()`
acquire and the stores of keys release, after the values (`89393e0`
for erase's moves, `27e22f6` for emplace's stores into array slots and
extension items, `fe837f7` for erase through an iterator).
`xenium_hash_map.cpp` runs main, and with `-DPRE` `e427a4e` (main before
#43), and with `-DNO_27E22F6` main with `27e22f6` reverted. The old
copies are in `c/xenium/e427a4e` and `c/xenium/no-27e22f6`. Keys and
values are ints, the hash is the identity, every key goes to bucket 0,
and the reclaimer never frees. In each scenario `t1` changes the bucket
while `t0` looks a key up once, and the condition is `t0` getting a value
never stored with its key (or, in the default scenario, missing a key
that is in the map throughout):

* default: J and K fill slots 0 and 1; `t1` erases J, which moves K into
  slot 0; `t0` looks up K.
* `-DREUSE`: `t1` erases K (the last slot) and emplaces L, which goes
  into K's old slot; `t0` looks up L.
* `-DEXT`: 128 buckets (the fewest with an extension bucket), three
  items in the array and K in an extension item; `t1` erases K and
  emplaces L, which takes K's extension item back from the free list;
  `t0` looks up L.

| Version | Move | Array slot reuse | Extension item reuse |
|---|---|---|---|
| main | no | no | no |
| `e427a4e` (`-DPRE`) | yes (gets J's value for K) | yes (gets K's value for L) | yes (gets K's value for L) |
| main without `27e22f6` | no | no | yes (gets K's value for L) |

Every verdict is the same at `-O0` (with 3000 fuel), `-O1`, `-O2` and `-Os`,
with and without LSE, and nothing is reachable under TSO. Each run takes
under 2 s, except `-DEXT` at `-O0` (about 75 s, 100 s with LSE). So
main holds, and the bugs #43 fixed are ARM-only: before it, `t0` can
read a moved or new key late, after its relaxed reread of `state`, and
pair it with the slot's old value. Part of `27e22f6` isn't needed on
ARM: once the key load is acquire, `emplace`'s relaxed stores into an
array slot come after its acquire compare-and-swap on `state`, which
reads from `erase`'s release of the new version, so a `t0` that sees
them also sees the version and retries. The C++ model gives no such
edge, so the fix is still right in C++. Its acquire load of an extension
item's key is needed on ARM: `t0` reaches a reused item through a
pointer read before the erase, so nothing orders its key load before
the reread of `state`.

## libomp distributed barrier

LLVM libomp's distributed barrier (`__kmp_dist_barrier_gather` and
`__kmp_dist_barrier_release` in `openmp/runtime/src/kmp_barrier.cpp`,
used with `KMP_*_BARRIER_PATTERN=dist`) puts threads in groups, each led
by its first thread. To arrive, a thread stores 0 to its `stillNeed`
flag. A leader waits for its group's flags, stores its own, and waits
for every leader's; each thread then executes a full fence. To depart,
the primary thread stores the next value to the go flags of the leaders
and of its own group, and each leader passes it on to its group; the go
flags are seq_cst stores, waited for with acquire loads.

`0fa6182e2` (September 2026, "Fix dist barrier arrival
synchronization", #213845) made `stillNeed`, a `volatile` flag before,
atomic: the arrival store is a release, and an acquire fence follows
each wait for the flags. Its reproducer fails on Grace with a thread
reading another's data from before the barrier. `libomp_dist_barrier.c`
transcribes main (`6cd3ff3a0`), with `-DPRE` the flag as before the fix,
and with `-DNO_ACQ` the fix without its acquire fences. Each thread
writes its own word, passes the barrier once and reads the next
thread's; the condition is a thread reading the old value. Two threads
form one group; `libomp_dist_barrier3.c` has three, in groups {0, 1} and
{2}, so that data also passes through a leader and the primary. With
`-DREDUCE` each leader instead reads its group's words right after it
sees their flags, before the full fence, as the reduction does.

| Version | After the barrier | In the reduction |
|---|---|---|
| main | no | no |
| before `0fa6182e2` (`-DPRE`) | yes | yes |
| main without the acquire fences (`-DNO_ACQ`) | no | yes |

The verdicts are the same with two and three threads and at `-O0` and
`-O2`, and nothing is reachable under TSO. So the fix is complete for
these paths, and the bug was ARM-only: the plain store of `stillNeed`
was not ordered after the thread's own writes. Reads after the barrier
don't need the acquire fences on ARM, because the full fence at the end
of the arrival orders them after the flag loads. The reduction runs
before that fence and does need them.
