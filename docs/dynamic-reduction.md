# Design: a dynamic partial-order reduction

Status: proposed (November 2026). Not implemented.

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

These replace the `via_reader` and `internal` checks for pairs whose
static check fails. Neither condition mentions which thread read `x`,
so the case analysis of `reduce.rs` collapses into a view comparison.

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
  now. Nothing else is required: a run may stop anywhere. The thread's
  new writes are appended with timestamp `F`.

The exploration records, per thread, the footprints of reads from
messages at or before `ts(x)` with `vpre = ts(x)`, and of writes with
`vpre = ts(x)`. If the pre-pass gave up, or the exploration exceeds a
budget, `visit` falls back to the static check for that memory.

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
  have to be allowed. Check that this does not swamp the exploration.
- Mixed-size reads that take bytes from both `x` and older writes: make
  sure condition 1 is recorded for them.
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
