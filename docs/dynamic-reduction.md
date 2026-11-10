# Design: a dynamic partial-order reduction

Status: implemented, and the default since November 2026
(`--no-dynamic-por` turns it off). With the states past `x` cut off and
unknown values poisoned (see [The cut](#the-cut)), `ck_fifo_mpmc`, the
test it was designed for, finishes: with `-DFENCE_DEQ`, in 4,894
memories and 35 s, which shows the fix holds. Across the C programs it
takes 91 s, against 320 s for the static reduction alone. The sections
from [Counters](#counters) to [Cheaper observing](#cheaper-observing)
describe the earlier versions, which gave up on `ck_fifo_mpmc`.

## The problem

The partial-order reduction (`src/reduce.rs`) prunes a memory `..., x, y`
whose last two messages are out of priority order, when `x` cannot be
ordered before `y` (ob) in any execution. Whether it can is decided from
a pre-pass that gives each thread one set of footprints for the whole
program: where it may read, where it may read after a barrier or a
dependency, where it may write with a release, and so on.

That fails on `ck_fifo_mpmc`, and will fail on any program that re-uses
nodes. Both threads read and write every field of every node, since
either may hold any node, and ck's load fences order any read before
any later read. So for every pair of writes, the pre-pass says the other
thread may read one and then the other, and nothing is pruned. The
exhaustive search with two threads that each dequeue and enqueue once
does not finish: after 361,000 memories in 20 minutes it is still
finding new ones. Yet the first 60,001 memories hold only 182 sets of
writes, each with a single order of the writes to each location: almost
every memory is another interleaving of the writes to different
locations, one set coming in 4,974 orders.

Those orders are real ARM behaviors, but in a given execution few of
them are observed. Which ones are depends on which messages each thread
reads, which the static footprints cannot tell.

## The idea

Decide prunability per memory, from an exploration of each thread
against that memory, instead of per program.

`Explorer::visit(mem)` calls `prunable(x, y)` with `x = mem.last()` and
each candidate `y` (by thread `B`), so one analysis of `mem = Q·x` serves
all its children. The children's executions are those whose memory
extends `Q·x·y`. In them, `y` has timestamp `ts(x) + 1` and every other
new message comes after it.

### When x is ob-before y, in terms of views

As in `reduce.rs`: `x` and `y` are adjacent and disjoint, so a path from
`x` to `y` in ob goes through no other write. Either some thread `C`
reads `x` and then, ordered after that, reads the location of `y` from
a write before `y` (a from-read edge into `y`), or `B` orders `y` after
something that is itself ordered after `x` (its own read of `x`, or, if
`B = A`, a barrier or release after `x`).

Promising-ARM's views encode ob: an access ordered after `x` has a
pre-view (`vpre`) of at least `ts(x)`. That is the core of the model's
equivalence with the axiomatic one; this design assumes it in this
form, and the tests check the result. The window is narrow:

- A read of `y`'s location from a write `u` before `y` must have `vpre <
  ts(y)`, since `y` lies in `(u, vpre]` otherwise. Its `vpre` is
  therefore exactly `ts(x)`.
- The write `y` itself needs `vpre < ts(y)`, so if it is ordered after
  `x`, its `vpre` is exactly `ts(x)`.

So `Q·x·y` may be pruned (`y` before `x` in priority, the two disjoint)
unless one of these holds:

1. some thread may read (part of) `y`'s footprint from a message at or
   before `ts(x)` with `vpre = ts(x)` (with a mixed-size read that takes
   some bytes from `x`, the same read counts);
2. `B` may write `y`'s footprint with `vpre = ts(x)`.

The argument shows that the least linearization of each execution is
not pruned, but promise-first exploration must also *reach* it: `B`
must be able to promise `y` with `x` absent, certifying it in a solo
run. `B` may read `x` on the way to `y`, but not in a read ordered
before `y` (condition 2 would hold), so after `Q` alone that read may
return an older value and `B` take the same path to `y`. The value
read reaches `y` only through dependencies, and those would order the
read before `y`. What else a read's value decides can go either way
regardless: a store-exclusive may fail, and so may a compare-and-swap
(certification lets one fail on a later write, as `cas_fails_later` in
`promising.rs` explains).

An earlier version had a third condition, `B` may read `x`, after the
random tests in `tests/por.rs` found a counterexample without it. In
that test a thread's compare-and-swap fails only when another thread
has written its location; on success, its release write orders the
thread's next write (an acquire atomic) after it. So that write could
be promised only after the other thread's write, though nothing in the
execution orders it after that write. That was a gap in the model,
not in the reduction: Promising-ARM lets a compare-and-swap fail on a
write that is not yet in memory, and armem did not. With that fixed,
the third condition is gone, and the same tests pass without it. The
static check still has the same condition (`c == b` in `prunable`).

These replace the `via_reader` and `internal` checks for pairs whose
static check fails. Conditions 1 and 2 do not mention which thread read
`x`, so the case analysis of `reduce.rs` collapses into a view
comparison.

### An exploration with an abstract future

To find these reads and writes, run each thread alone against `Q·x`
(a new `Mode::Observe` of `Solo`), with two changes from `Collect`:

- **Future messages.** A read may return, besides the messages of
  `Q·x` and the thread's own new writes, a *future* message: any value
  that the pre-pass found some write may store in those bytes. A future
  message has the symbolic timestamp `F`, above `ts(x) + 1`. Views take
  values in `0..=ts(x)` or `F`. A read with `vpre = F` may read the
  latest message to its location in `Q·x` or a future one. That allows
  more than any real memory does (which also forbids skipping future
  writes before `vpre`), so it is an over-approximation. The state
  space stays finite: views are bounded, and values come from the
  pre-pass's finite sets.
- **Promises.** The thread's messages in `Q·x` are promises that every
  execution in the subtree fulfills at their timestamps, so states with
  a promise that can no longer be fulfilled are dropped (`doomed`), as
  now. Nothing else is required: a run may stop anywhere.
- **Own new writes.** These also come after `y`, so they get timestamp `F`
  too, and are kept only in the forwarding bank (which now holds each
  write's value). A later read of those bytes returns that write or a
  future one. Appending them as messages instead made the states grow
  with every turn of a retry loop (the Treiber stack's push did not
  finish), since a future value can make a compare-and-swap fail any
  number of times.

The exploration records, per thread, the footprints of reads from
messages at or before `ts(x)` with `vpre = ts(x)`, the footprints of
writes with `vpre = ts(x)`. A future read stands for any read that
takes some bytes from later writes. If such a read overlaps `x` and
could also take bytes from `x` (it is not single-copy atomic, or some
other thread's write covers only part of it), it is recorded. If the pre-pass gave
up, a future read's values are unknown, or the exploration exceeds
200,000 states, `visit` falls back to the static check for that memory.

Dropping doomed runs is where the precision comes from. Deep in the
search each thread has several promises in `Q`, which pin down which
node it dequeued and how far it has got. The static pre-pass knows
none of this.

### Why it stays sound

The least-linearization argument in `reduce.rs` needs only this: when a
memory is pruned, no execution whose memory extends it has `x`
ob-before `y`. Every such execution's run of thread `C` reads messages
of `Q·x`, its own writes and later messages, and fulfills its promises
in `Q·x`, so it is a run of the observing exploration, with each later
message read as a future one. Its views are at most those of the
abstract run, with `F` standing for any timestamp above `ts(y)`.
Conditions 1 and 2 then cover every ob path from `x` to `y`. The least
memory of every execution has no pruned prefix, so it is still
explored.

## Cost

One more solo exploration per thread per memory, about the size of a
`Collect` run, and larger where future reads branch. That roughly
doubles the cost of a memory, so it pays off only when it prunes more
than half of them. It is run only for memories where the static check
fails for some candidate.

Later, `Observe` might share states with `Collect`: both run from the
same start against the same memory, and differ only in the future
reads.

## Results

Step 0 (below) found the ceiling: pruning by conditions 1 and 2 with the
`Collect` runs' accesses, which is unsound, took `ck_fifo_mpmc`'s
exhaustive search to 3,811 memories in 11.5 s (2,821 in 7.2 s with
`-DFENCE_DEQ`, where it shows the fix holds). The relaxed Treiber stack
went from 7,824 memories to 677, and `ck_fifo_spsc -DWEAK_ENQ` from
63,314 to 1,297.

The sound version, with future reads, gives the same final states as
the full search on every litmus test, on 45,000 small and 800 large
random tests, and on the C programs. On the tests where the observing
explorations stay affordable, it reaches the ceiling:

| test | static reduction | `--dynamic-por` |
|---|---|---|
| Treiber stack, push relaxed | 7,824 memories, 1.6 s | 677, 0.6 s |
| `ck_fifo_spsc -DWEAK_ENQ` | 63,314, 38 s | 1,297, 1.7 s |
| `dcl -DPUB=relaxed` | 449 | 149 |
| `ck_epoch -DOLD_BEGIN` | 437, 0.35 s | 203, 0.44 s |
| `ck_epoch_poll3 -DOLD_POLL` | 913 | 479 |
| `ck_spmc`, `seqlock` | | no change |

(With the third condition, since dropped, Treiber took 1,745 memories,
`ck_fifo_spsc` 5,624, `dcl` 236, and `ck_epoch` did not shrink.)

### Counters

On `ck_fifo_mpmc` every observing exploration gives up, after two
states. Each dequeue increments the head's generation, and the pre-pass
widens that to any value, as it does any counter: it lets a load return
any value some write may store, so a thread can read the other thread's
incremented generation and increment it again, round after round.
Without values, the future reads cannot be explored.

Ways forward:

- **A fixpoint per memory.** Find the values later writes may store
  from the observing explorations themselves: run them with no future
  values, then with the values the other threads wrote, and so on.
  Promising-ARM's writes depend only on reads of earlier messages, so
  the `k`th later write has a value found by round `k`, and the number
  of rounds needed is bounded by the number of later writes. A
  counter still grows by one each round, but only up to that bound,
  which is small deep in the search. The bound needs the most writes
  any run can still make, and a run with a write in a loop has none.
- **Symbolic values.** Let an unknown value be a fresh symbol and allow
  every equality with values the thread holds. Generations are only
  compared for equality, but that would have to be checked (a sequence
  number compared by size would break it).
- **Runs that can complete.** Count only accesses of runs that can
  still fulfill all their promises, as the certification filter does
  with its candidates. Deep in the search each thread's promises fix
  which dequeue it makes, so its runs that would increment the
  generation again fall away. This also sharpens the reduction
  everywhere else.

### Rounds, and what they showed

The first way forward is implemented (`Explorer::observe`). It doesn't
need the bound on later writes: once a round adds no value, the values
are complete. A read of an exclusive pair is atomic only per 8-byte
half, so it takes each half from the memory or from a later write.

On `ck_fifo_mpmc`, the rounds converge in two on full-length memories:
the generations get one or two values each. But on shallower memories,
where the threads have made few promises, a thread's observing
exploration exceeds its 200,000 states even in the first round, so the
search still falls back to the static reduction there.

Without the third condition, step 0's ceiling of 3,811 memories
applies. With it, the ceiling (pruning by the observing explorations
without future reads, which is unsound) was 70,264 (both threads read every location, so each
may read `x`), which is what led to finding the compare-and-swap gap.
But with the condition gone, the exhaustive search still does not
finish in 30 minutes, because of the shallow memories.

### Where the shallow memories' states go

To see what fills the 200,000 states, the first 40 memories of
`ck_fifo_mpmc -O2` were run with a print at each give-up. Every one
was thread 0's exploration, on memories of 1 to 10 messages, which ends
with 100,000 to 140,000 distinct states. On these memories thread 0
has no promise outstanding: the messages are thread 1's, or thread 0
fulfills its own early. And 97 to 99% of the states are past `x`:
`v_rnew`, `v_wnew` and `v_cap` are all above `ts(x)`, and nearly all
have read a later write (`v_rold = F`). Each read of a later write
branches on every value the pre-pass (or an earlier round) allows, so
the exploration is thread 0 running its whole program against
arbitrary values from thread 1.

That rules out the two fixes proposed before:

- **Runs that can complete.** A thread with no promise outstanding can
  complete from every state, so counting only such runs drops nothing.
  It would bite deep in the search, where the explorations already fit.
- **Merging views below `ts(x)`.** Almost no view is below `ts(x)`.

A state past `x` can no longer make an access with `vpre = ts(x)`, so
it adds nothing to conditions 1 and 2, except by reading `x` itself
(a read with `vpre = F` may still read the latest message, and `x` is
the latest to its bytes until the thread's coherence passes it). Cutting
such states off would remove nearly all of them, but they also make the
later writes whose values feed the next round (`Solo::written`), and
on this test those are the generation counters, written by both
threads, whose values the pre-pass cannot bound. Cutting them off would
lose values and make the reduction unsound. The cut would be sound
where every byte the thread may still write has values the pre-pass
knows, but here that fails for exactly the bytes that matter.

So `ck_fifo_mpmc` needs a coarser abstraction of the counter values
(the symbolic values above, in which a generation is a fresh symbol
compared only for equality), or a cut-off that keeps the counter's
values some other way. That is where this work stopped.

### Across the C programs

Before making `--dynamic-por` the default (step 4 of the plan below),
each of the 198 exhaustive lines of `c/expected.txt` was run in the
ARM model with it and without it (clang 21, November 2026). Every line
reached the same final states, faults and fuel cut-offs. But the
dynamic reduction cut the memories explored by only 19% (from 142,933
to 115,327), and the total time went from 125 s to 321 s.

It wins where pruning a memory removes a subtree that nothing else
reaches: the relaxed Treiber stack (7,824 memories in 3.0 s to 677 in
1.1 s), `chase_lev -DPUSH_FENCE` (1,184 memories to 145; at `-O2`,
1.1 s to 0.5 s) and `dcl` with a relaxed unlock (815 to about 195
memories, twice as fast). It loses badly on ck's MCS spinlock (`-O0`:
2.2 s to 45 s), its decrement lock (`-O0 -DWEAK_LOCK`: 0.6 s to 39 s)
and `ck_epoch_poll3` (4.6 s to 15 s). A count of the states in each
solo exploration shows why. In the MCS lock at `-O2`, the `Collect`
runs of a memory take about 55 states, and observing it takes a
median of 9.9 times as many, and at the 90th percentile 151 times.
Most observes do prune (990 of 1,146), but the memories they prune are
mostly reached by other paths anyway: 3,932 pruned candidates save
690 memories. And the 156 observes that
fail run all 8 rounds, their counters growing in each, and they take
72% of the observing states. In the winners, the 90th percentile of
that ratio is 20 or less.

Limiting each observe to K times the `Collect` states of the same
memory is sound, since a memory whose observe fails falls back to the
static reduction. It cuts the tail but not the median: with K = 30 (and
at least 1,000 states), the suite takes 193 s, with 19 lines markedly
slower and 3 faster. A smaller K makes the observes of the Treiber
stack and `chase_lev` fail, and their gains go too. So the dynamic
reduction stays an option: what would make it pay is cheaper observing,
not a budget. That means cutting off the states past `x` (see the
section before this one), which is sound only where the thread can no
longer read `x`'s bytes and no other thread needs the values of its
later writes. In the spinlocks, the threads keep reading the lock word,
so the cut may not help them.

### Cheaper observing

The measurements below are of the 248 exhaustive lines of
`c/expected.txt` (clang 21, November 2026), run one at a time: runs in
parallel had made some lines look several times slower or faster than
they are. As it was, `--dynamic-por` took 176.7 s and 116,928 memories,
against 86.0 s and 145,337 without it.

**The cut is not needed, and would not be enough.** Cutting off the
states past `x` would lose the values of their later writes, wherever
another thread reads those bytes with values the pre-pass does not
know. That turned out to be every thread of the slow tests: in ck's
spinlocks the plain counter that the critical section increments is
widened, like any counter. And in the decrement lock, only 7,004 of
921,613 observing states were past `x`. The rest were the thread
spinning on its lock word, whose values grew round after round.

**Erasing views past `x`.** A state past `x` has no promise left (it
would be doomed), and every access it makes has the pre-view `F`, so
what it reads and writes, and what it records, no longer depend on its
views. `Solo::past` therefore sets them all to one value, and states
that differed only in views are explored once. The run goes on, so the
later writes still give their values to the next round. This is sound
whatever the test, but gains only about 10%.

**Starting from the parent's values.** The rounds' argument holds
whatever values the first round starts from, so `observe` starts from
the values found for the parent memory: every execution whose memory
extends `Q·x` extends `Q`, and its writes after `Q·x` come after `Q`.
In the MCS lock at `-O0`, 850 of the 990 observations that converge now do so
in one round instead of two.

**Giving up sooner.** The observations that do not converge are where
the states go: before these changes, 1,329 of them across the suite ran
all 8 rounds, and took 11.5 million of the 20.9 million observing
states. So an observation now gives up after 3 rounds; and if a memory's
observation gave up, its children's are not tried (a counter that grows
in one memory's rounds mostly grows in its children's too); and each
observation may take at most 10 times as many states as the `Collect`
runs of its memory (at least 1,000). Each of these only makes the
search fall back to the static reduction, so all are sound.

Together: 90.3 s and 127,173 memories, against 86.0 s and 145,337
without the dynamic reduction, with the same final states on every
line. No line is more than 0.5 s slower; the largest losses are ck's
epoch tests and `ck_epoch_poll3` (about 0.5 s each). The wins are as
before: the relaxed Treiber stack (1.6 s to 0.5 s), `chase_lev
-DPUSH_FENCE` at `-O2` (1,184 memories to 145) and `dcl`. At `-O0`,
that `chase_lev` test now prunes less (692 memories, not 145), as its
observations exceed the budget. Outside the suite, `ck_fifo_spsc
-DWEAK_ENQ` still goes from 63,314 memories in 37.6 s to 1,297 in
1.8 s. `ck_fifo_mpmc` still does not finish. Its first 3,000 memories
take 31 s, against 17 s with the static reduction (and more than 600 s
before these changes), since its shallow memories' observations give up,
and with them every deeper one.

So the dynamic reduction no longer costs much where it does not help,
but across the suite it does not pay either, and it stays an option.

### The cut

[Where the shallow memories' states go](#where-the-shallow-memories-states-go)
found that 97 to 99% of an observing exploration's states on
`ck_fifo_mpmc` are past `x`, and that they matter only for the values of
their later writes, which feed the rounds. An unsound experiment showed
what cutting them off is worth: with the states past `x` dropped, and reads
of later writes with unknown values left out, `ck_fifo_mpmc` reached the
ceiling of step 0 (3,811 memories in 27 s, and 2,821 in 17 s with
`-DFENCE_DEQ`).

`Explorer::observe_cut` makes that sound. Each thread is run once, not in
rounds, and:

- **Unknown values are poisoned.** A read of a later write whose values
  the analysis does not know (a widened counter) returns a poisoned value
  (`TState::poison`, one bit per register). For a read in pieces, such as
  the 16-byte read of a (pointer, generation) pair, only the registers
  that get bytes of unknown pieces are poisoned; the other pieces take the
  analysis's values as before.
- **States past `x` are cut off.** They can no longer make an access with
  pre-view `ts(x)` (see `Solo::past`), so what they would record is only
  their reads that take bytes from `x`. On the first cut, every read
  footprint the analysis found for the thread that overlaps `x` is
  recorded instead. Their writes need no values, as no round follows.
- **Poison is tracked until then.** Before a run gets past `x`, an
  instruction that reads a poisoned register is executed abstractly: an
  integer operation poisons what it writes; a conditional branch, select
  or compare on poisoned flags is run with each of the 16 flag values, and
  `CBZ` or `TBZ` on a poisoned register with zero and with all ones,
  which between them take every way it can go; a store of a poisoned
  value to shared memory makes a poisoned write. A later read of that write
  is poisoned, and the write may fulfill any promise with its footprint
  (whatever the promise's value). Anything else (a poisoned address,
  an atomic, a SIMD&FP register, a store to private memory) voids the
  cut, and the memory falls back to the rounds.

This allows at least every run the rounds would (each poisoned value
stands for any value), so it is sound in the same way. In `ck_fifo_mpmc`
a read of a later write is mostly followed by a load fence, which puts
the run past `x` at once; elsewhere, the poisoned generation is compared
by a compare-and-swap, or stored into a node by an enqueue. No cut on
that test falls back to the rounds (4 of its 6,435 give up at the
budget), and with `-DFENCE_DEQ -DOLD_ENQ` (the enqueue from before ck
`ed6bace`) the search shows the condition unreachable too, in 4,934
memories and 76 s.

Each cut gets the budget of the observation it replaces, but at least
20,000 states. On the shallow memories of `ck_fifo_mpmc` it takes up to
about 7,000 states, more than ten times their `Collect` runs; with a
floor of 10,000, 50 memories of that test gave up and it took twice as
long, while with no limit (200,000) ck's `LOCK=7` spinlock tests took 3
to 5 s more, their cuts giving up only at the limit.

Results (clang 21, November 2026), with the static reduction alone and
with the dynamic one:

| test | static reduction | dynamic reduction |
|---|---|---|
| `ck_fifo_mpmc -O2` | does not finish in 20 minutes | 6,752 memories, 58 s |
| `ck_fifo_mpmc -O2 -DFENCE_DEQ` | does not finish in 20 minutes | 4,894, 35 s |
| the 268 exhaustive lines of `c/expected.txt` | 422,677, 320 s | 119,710, 91 s |

Across the C programs, run one at a time, every line reached the same
final states. The largest gains are in moodycamel's ReaderWriterQueue
(`rwq_dequeue -O0 -DOLD`, from 56,820 memories in 99 s to 1,737 in 6.5 s,
and `rwq_size -DOLD` from 31,050 in 12 s to 288 in 0.2 s); the largest
losses are `ck_epoch_poll3 -O2` (2.2 s to 4.6 s, and 3.2 s to 5.8 s with
`-DOLD_POLL`). 22 lines are faster by more than 0.2 s and 5 slower.
`tests/por.rs` has a test of a generation counter that takes every
abstract path above (`generation_counter`), and the random tests (20,000
small and the 1,200 large) give the same final states as the full search.

## Plan

0. **Measure the ceiling first.** As a throwaway experiment, use the
   existing `Collect` runs (no future reads, so unsound) to record
   conditions 1 and 2, prune with them, and count `ck_fifo_mpmc`'s
   memories. If even that prunes little, stop here.
1. Add `Mode::Observe` to `Solo`: future reads from the pre-pass's value
   sets, views with `F`, the recording, and a state budget. Expose the
   pre-pass's per-byte values to it.
2. Add `Reduction::prunable_dyn(x, y, &Observed)`, used by `visit` when
   the static `prunable` returns false. Gate it behind an option
   (`--dynamic-por`) until it is validated.
3. Validate with `tests/por.rs`: every litmus test and the random tests,
   against the full exploration and the static reduction. Add the C
   programs at small sizes, and `ck_fifo_mpmc` with a memory budget,
   comparing the final states reached.
4. Measure `ck_fifo_mpmc` (exhaustive, and with `-DFENCE_DEQ`, whose fix
   the sampled tests only suggest), the relaxed Treiber stack and the
   `ck_fifo_spsc -DWEAK_ENQ` test. If it is a win, make it the default
   and document it in the README's "How it works". (Done, with the cut:
   see [The cut](#the-cut).)

## Open questions

- Store-exclusives and atomics: whether one succeeds depends on writes
  between its read and its write, so with future messages both outcomes
  have to be allowed. (Done: a future source never fails the atomicity
  check. This has not swamped any test so far.)
- Mixed-size reads that take bytes from both `x` and older writes: make
  sure condition 1 is recorded for them. (Done: `MP-bytes` caught the
  first version.)
- The `vpre ≥ ts(x)` characterization of ob covers the rules added to
  Arm's model since the Promising paper (pick dependencies, NoRet
  atomics, acquire+release atomics). armem follows those rules through
  views, but this needs checking case by case, and `tests/por.rs` should
  have litmus tests for each.
- Spin loops on future values: the pre-pass widens counters to
  unknown, and the observing exploration should do the same, or give up.
  (Done for the cut, which poisons them; the rounds give up after 3.)
- Private memory cannot hold a poisoned value, so a cut whose thread
  stores one there (a local spilled at `-O0`, say) falls back to the
  rounds. Poisoned bytes in `Stack` would cover it.

## Alternatives considered

- **A more precise static pre-pass**, such as tracking which node each
  thread may hold. Node re-use defeats it on exactly this test, since
  any thread may hold any node.
- **Deduplicating executions after the fact**, keeping only executions
  whose memory is their least linearization. This saves nothing: the
  memories are still explored.
- **DPOR over reads-from** (as in GenMC). It is a good fit for the
  axiomatic model, but promise certification needs total timestamp
  orders, so it would mean a different explorer, not an extension of
  this one.
