# Design: a dynamic partial-order reduction

Status: implemented as `--dynamic-por` (off by default), November 2026.
See [Results](#results): it helps several tests, but on `ck_fifo_mpmc`,
the test it was designed for, it gives up, because of a counter (see
[Counters](#counters)).

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
   and document it in the README's "How it works".

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
