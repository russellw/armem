//! Partial-order reduction for the promise-first exploration in
//! `promising.rs`.
//!
//! Promise-first exploration enumerates memories: sequences of write
//! messages. Writes that nothing orders can be appended in any order, so
//! `n` unrelated writes give `n!` memories (and every prefix of each), even
//! though the order of writes to different locations is usually
//! unobservable. The reduction explores one order of each such pair.
//!
//! The argument is axiomatic. Promising-ARM is equivalent to Arm's
//! axiomatic model, in which an execution is consistent if its
//! *ordered-before* relation (`ob`) is acyclic, and the memory of an
//! execution can be any linearization of `ob` and coherence restricted to
//! writes. Fix a priority order on messages. Among those linearizations
//! there is a least one in lexicographic order. In it, whenever two
//! adjacent messages `x`, `y` appear with `y` of higher priority (lower in
//! the order), they are ordered (`x` ob-before `y`) or overlap; otherwise
//! swapping them would give a smaller linearization. So a memory may be
//! pruned when its last two messages are out of priority order and
//! *cannot* be ordered: no execution needs it as a prefix of its least
//! memory. (That any linearization will do, also with armem's mixed-size
//! extension, is the assumption this rests on; it is not machine-checked,
//! so `tests/por.rs` checks the reduction against the full exploration on
//! every litmus test and on random tests.)
//!
//! When can `x` (written by thread A) be ob-before an adjacent, disjoint
//! `y` (by thread B)? No write lies between them in memory, so no path
//! from `x` to `y` goes through another (non-stack) write: after leaving
//! `x` it passes only through reads, and it can only move from one thread
//! to another at a from-read edge into `y`. So one of these holds:
//!
//! - Another thread C reads (part of) `x`, and then either C = B (the read
//!   may be ordered before the write `y`), or C also reads the location of
//!   `y` (a from-read edge into `y`), in the same read or in a later read
//!   that C orders after it: by a barrier, a dependency, or because the
//!   read of `x` is an acquire.
//! - A orders `x` before later accesses. If B = A, they may include `y`
//!   (`y` is a release, or A has a barrier, ...); if B ≠ A, the path must
//!   leave A through a from-read edge, so A must order the write `x` before
//!   a later read (by a full barrier, `x` being a release or atomic write
//!   followed by an acquire, ...) and read the location of `y`.
//!
//! Two refinements use where accesses lie in program order. If B = A,
//! local order follows program order, so `x` must come before `y` with an
//! ordering point between them (`Reduction::orders_writes`, from the
//! epochs of `Epoch`). And a from-read edge into `y` needs a read of `y`'s
//! location that returns a write coherence-before `y`, which a thread
//! cannot make if it reaches that location only through a pointer that
//! A publishes after `y` (see `Publication`).
//!
//! What threads may read, and how they may order accesses, is
//! over-approximated before exploring. [`analyze`] runs each thread alone,
//! letting every load return any value that some write (or initial memory)
//! may put in each byte, until the sets of possible values stop growing.
//! It also keeps the whole values written with each footprint, so that an
//! aligned read of bytes written only with its own footprint (a pointer,
//! say) returns those values rather than every combination of their bytes.
//! Every real execution reads and writes only values found this way, so the
//! footprints it records (of all reads, of acquire reads, of release
//! writes and of atomic writes) cover every execution. A byte that may
//! hold too many values, or whose values keep growing round after round
//! (a counter), becomes unknown. Anything computed from an unknown
//! value is tainted: a branch on it goes both ways, and an access at an
//! address computed from it makes the analysis give up. Values loaded from
//! memory are tainted too, so the analysis also finds address dependencies
//! between reads, and stores of loaded values that the thread may read back
//! (such a read is ordered after the reads the store depends on). If the
//! analysis gives up (or gets too big), nothing is pruned.

use crate::core::{Core, MemOp, RV, Step, StepQueue, VR, View, low_bytes, rmw_result};
use crate::hash::FxHashMap;
use crate::isa::{Addr, Barrier, Code, Instr, LoadKind, RmwOp, Size};
use crate::litmus::Test;
use crate::promising::Msg;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Most values the analysis lets one read return before it treats the
/// value as unknown.
const MAX_READ_VALUES: usize = 256;
/// Most values the analysis tracks for one byte before it treats the byte
/// as holding any value.
const MAX_BYTE_VALUES: usize = 16;
/// Most states the analysis may visit (over all threads and rounds)
/// before it gives up.
const MAX_STATES: usize = 300_000;
/// Most rounds of the value fixpoint before the analysis gives up.
const MAX_ROUNDS: usize = 64;
/// Most values the analysis keeps for a byte that only one thread writes
/// (see `Own`) when it runs again after giving up, before it treats the
/// byte as holding any value. A counter that the thread increments on
/// every pass of a loop (as folly's flat combiner counts its passes) gives
/// each pass new states otherwise, and the runs then grow with the fuel
/// until the analysis gives up.
const WIDENED_OWN_VALUES: usize = 2;

/// How far `Havoc` runs go: the fuel, and the most values kept for a byte
/// in `Own` (None for no limit).
#[derive(Clone, Copy)]
struct Bound {
    fuel: u32,
    own_values: Option<usize>,
}
/// A byte whose set of values grows in this many rounds of a streak is
/// taken to hold any value (widening). A counter, such as a sequence
/// number that each round's writes increase, would otherwise add values
/// round after round until it reached `MAX_BYTE_VALUES`, while the runs
/// that read it grew with every value.
const WIDEN_AFTER: usize = 3;
/// A byte is widened only once it holds more than this many values (its
/// initial value aside). A pointer that comes to point to each of a few
/// nodes, one more in each round as the values spread from thread to
/// thread, is not a counter.
const WIDEN_MIN_VALUES: usize = 4;
/// A streak ends after this many rounds in a row without growth. A single
/// round is not enough: a counter that one thread increments and others
/// copy (an epoch, and each thread's record of the epoch it observed)
/// grows every other round, as the copies follow the counter.
const STREAK_GAP: usize = 2;

/// Memory footprints: (address, length) pairs.
pub type Footprints = BTreeSet<(u64, u64)>;

fn hits(f: &Footprints, m: &Msg) -> bool {
    f.iter().any(|&(a, n)| a < m.addr + m.size.bytes() && m.addr < a + n)
}

/// What the reduction needs to know about each thread: footprints of its
/// accesses to shared memory (its private stack excluded) and the kinds of
/// ordering it may use.
#[derive(Debug, Clone, Default)]
pub struct Reduction {
    reads: Vec<Footprints>,
    /// All writes.
    writes: Vec<Footprints>,
    /// Acquire reads (including those of atomics).
    acquire_reads: Vec<Footprints>,
    /// Release writes (including those of atomics).
    release_writes: Vec<Footprints>,
    /// Writes of atomics and store-exclusives.
    atomic_writes: Vec<Footprints>,
    /// Writes of acquire atomics that write back the value they read (an
    /// add of 0), and of store-exclusives that write back the value an
    /// acquire load-exclusive read, which copy a published value (see
    /// `Publication`) without publishing anything, and all other writes.
    copy_writes: Vec<Footprints>,
    noncopy_writes: Vec<Footprints>,
    /// The thread may order any read before any later read: it has a full
    /// barrier, DMB LD, or an ISB (after a control dependency), or its
    /// dependencies were too costly to find.
    orders_rr: Vec<bool>,
    /// For each instruction that may read another thread's writes, the
    /// footprints of its reads and of the reads the thread orders after
    /// them by dependencies (see `Dep`). (If these were too costly to find,
    /// `orders_rr` is set instead.)
    dep_rr: Vec<Vec<(Footprints, Footprints)>>,
    /// The thread has a full barrier or an acquire-release atomic, which
    /// order any earlier write before any later read.
    full: Vec<bool>,
    /// The thread has an acquire read (which orders an earlier release or
    /// atomic write before it).
    acquire: Vec<bool>,
    /// The thread has DMB ST.
    dmb_st: Vec<bool>,
    /// For each thread, the epochs (`Epoch::may`) of its writes to each
    /// footprint, or None if they were too costly to find.
    write_epochs: Vec<Option<BTreeMap<(u64, u64), BTreeSet<u8>>>>,
    /// For each thread, the writes it publishes (see `Publication`).
    publications: Vec<Vec<Publication>>,
    /// What memory may hold, for `future_values`.
    vals: Known,
    /// The threads that may write each byte.
    writers: HashMap<u64, BTreeSet<usize>>,
}

/// Writes of thread A that other threads reach only through a pointer
/// that A publishes after them. A reads-from edge from A's write `p` of
/// the value `v` to location `P`, which A writes after a barrier that
/// follows each write `y` in `protected` (or which is a release), orders
/// `y` before `p`. If a read `q` by thread C returns `v` from `P`, the
/// value no other write gives it, then a read `r` that C orders after `q`
/// (by a dependency, by `q` being an acquire, or by a `DMB LD` or full
/// barrier between them) cannot read `y`'s location from a write
/// coherence-before `y`: `r fr y ob p rf q ob r` would be a cycle. So if
/// every read by C of `y`'s footprint is ordered after such a `q`, C cannot
/// give `y` a from-read edge. (Only orderings that certainly hold count
/// here: unlike `dep_rr`, this must not over-approximate them.)
#[derive(Debug, Clone)]
struct Publication {
    /// The footprints of A's writes that every publication orders before
    /// itself, and of its other writes. (A message may be part of a write,
    /// as each half of a 16-byte store is.)
    protected: Footprints,
    unprotected: Footprints,
    /// For each thread C, the footprints of its reads that are not ordered
    /// after a read of a published value (None if not found).
    unordered: Vec<Option<Footprints>>,
}

/// The values a later write by another thread may give a read (see
/// `Reduction::future_values`).
pub enum Future {
    /// No other thread writes these bytes.
    None,
    Values(Vec<u128>),
    /// The analysis does not know them.
    Unknown,
}

/// What threads may do after the memory `.., x`, as found by exploring each
/// thread against it (`Mode::Observe` in `promising.rs`), for
/// `Reduction::prunable_after`.
#[derive(Debug, Default)]
pub struct Observed {
    /// Per thread: footprints of reads that are ordered after `x` (or read
    /// part of it) and read some byte from a message up to `x`.
    pub reads: Vec<Footprints>,
    /// Per thread: footprints of writes ordered after `x`.
    pub writes: Vec<Footprints>,
}

impl Reduction {
    /// Do the reads of thread `c` of `y`'s footprint all read `y` or a
    /// later write? (See `Publication`.)
    fn fresh(&self, c: usize, y: &Msg) -> bool {
        self.publications.get(y.tid).is_some_and(|ps| {
            ps.iter().any(|p| {
                hits(&p.protected, y) && !hits(&p.unprotected, y) && p.unordered[c].as_ref().is_some_and(|u| !hits(u, y))
            })
        })
    }

    /// May thread `a` order its write `x` before its later write `y` (in
    /// program order)? Only with an ordering point between them: a full or
    /// store barrier, an acquire read (after a release `x`), or `y` being a
    /// release. Any path from `x` to `y` that leaves the thread must
    /// return to it by a from-read edge into `y`, which `prunable` considers
    /// separately; within the thread, local order follows program order.
    fn orders_writes(&self, a: usize, x: &Msg, y: &Msg) -> bool {
        let old = self.write_read(a, x) || self.dmb_st[a] || hits(&self.release_writes[a], y);
        let Some(Some(ep)) = self.write_epochs.get(a) else { return old };
        let epochs = |m: &Msg| {
            ep.iter().filter(|(f, _)| f.0 < m.addr + m.size.bytes() && m.addr < f.0 + f.1).flat_map(|(_, e)| e.iter().copied()).collect::<BTreeSet<u8>>()
        };
        let (ex, ey) = (epochs(x), epochs(y));
        if ex.is_empty() || ey.is_empty() {
            return old;
        }
        old && ex.iter().any(|&i| ey.iter().any(|&j| i < j || i == SATURATED && j == SATURATED))
    }

    /// May thread `a` order its write `x` before a later read?
    fn write_read(&self, a: usize, x: &Msg) -> bool {
        self.full[a] || self.acquire[a] && (hits(&self.release_writes[a], x) || hits(&self.atomic_writes[a], x))
    }

    /// May the memory `.., x, y` be skipped? (`x` is its second-to-last
    /// message and `y` its last.)
    pub fn prunable(&self, x: &Msg, y: &Msg) -> bool {
        if priority(y) >= priority(x) || overlaps(x, y) {
            return false;
        }
        let (a, b) = (x.tid, y.tid);
        let via_reader = (0..self.reads.len()).any(|c| {
            c != a
                && hits(&self.reads[c], x)
                && (c == b
                    || hits(&self.reads[c], y)
                        && !self.fresh(c, y)
                        && (self.orders_rr[c]
                            || hits(&self.acquire_reads[c], x)
                            || self.dep_rr[c].iter().any(|(s, a)| hits(s, x) && hits(a, y))
                            || self.reads[c].iter().any(|&(p, n)| {
                                let f = BTreeSet::from([(p, n)]);
                                hits(&f, x) && hits(&f, y)
                            })))
        });
        let internal = if a == b {
            self.orders_writes(a, x, y)
        } else {
            self.write_read(a, x) && hits(&self.reads[a], y) && !self.fresh(a, y)
        };
        !via_reader && !internal
    }

    /// May the memory `mem, y` be skipped because `y` is one piece of a
    /// write split into single-copy-atomic pieces (an unaligned store, say)
    /// and a piece of the same write with higher priority is not yet in
    /// `mem`, and nothing may order that piece after `y`?
    ///
    /// `prunable` only compares the last two messages, so the pieces of one
    /// write, which nothing orders, could be appended in any subset before
    /// the rest: the 16 byte pieces of an unaligned 16-byte store gave
    /// 2^16 memories. Let thread A write `y` with a write whose footprint
    /// `F` (the only one by A overlapping `y`) contains `y` and starts below
    /// it, and let some byte of `F` below `y` have no message of A in
    /// `mem`. Suppose an execution's least memory (see the module comment)
    /// begins `mem, y`, with `y` from an instance `I` of such a write.
    /// - No earlier write by A overlaps `F` except one with footprint `F`,
    ///   whose piece at `y`'s bytes would be coherence-before `y`, and so in
    ///   `mem`: there is none, since `mem` has no message of A overlapping
    ///   `y`.
    /// - So the piece `x` of `I` holding that byte below `y` comes after
    ///   `y`, and has higher priority. Its ordered-before predecessors in
    ///   A are those of `y` (the same instruction, and so the same
    ///   dependencies, barriers and acquires), which precede `y`. It has no
    ///   coherence predecessor: no other thread writes `F`, and A has no
    ///   earlier write to it. Nor a from-read predecessor: each other
    ///   thread's reads of `F` read `x` or a later write (`fresh`).
    /// - So moving `x` to just before `y` gives a memory of the same
    ///   execution that is smaller, a contradiction.
    ///
    /// A pair of registers counts as one footprint (see `StorePair` in
    /// `Havoc`), which overlaps the two halves', so their pieces are never
    /// pruned this way: they have different data dependencies.
    pub fn piece_prunable(&self, y: &Msg, mem: &[Msg]) -> bool {
        let a = y.tid;
        let (ya, yn) = (y.addr, y.size.bytes());
        let overlap = |f: (u64, u64), g: (u64, u64)| f.0 < g.0 + g.1 && g.0 < f.0 + f.1;
        let mut ws = self.writes[a].iter().copied().filter(|&f| overlap(f, (ya, yn)));
        let (Some(f), None) = (ws.next(), ws.next()) else { return false };
        if !(f.0 < ya && ya + yn <= f.0 + f.1) {
            return false;
        }
        let Some(size) = Size::from_bytes(f.1) else { return false };
        let whole = Msg { addr: f.0, size, val: 0, tid: a };
        if self.writes[a].iter().any(|&g| g != f && overlap(g, f))
            || hits(&self.atomic_writes[a], &whole)
            || hits(&self.release_writes[a], &whole)
            || (f.0..f.0 + f.1).any(|b| self.writers.get(&b).is_some_and(|w| w.iter().any(|&t| t != a)))
            || (0..self.reads.len()).any(|c| c != a && hits(&self.reads[c], &whole) && !self.fresh(c, &whole))
        {
            return false;
        }
        let by_a = |lo: u64, hi: u64| mem.iter().any(|m| m.tid == a && m.addr < hi && lo < m.addr + m.size.bytes());
        !by_a(ya, ya + yn) && (f.0..ya).any(|b| !by_a(b, b + 1))
    }
}

impl Reduction {
    /// Values a read of `n` bytes at `a` by thread `tid` may return from a
    /// write that some other thread may make: an over-approximation, as
    /// with any read of the analysis.
    pub fn future_values(&self, tid: usize, a: u64, n: u64, init_byte: &dyn Fn(u64) -> u8) -> Future {
        if (a..a + n).all(|b| self.writers.get(&b).is_none_or(|w| w.iter().all(|&t| t == tid))) {
            return Future::None;
        }
        let init = |a: u64, n: u64| (a..a + n).rev().fold(0u128, |v, b| (v << 8) | init_byte(b) as u128);
        if let Some(ws) = self.vals.whole(a, n) {
            if ws.len() < MAX_READ_VALUES {
                return Future::Values(std::iter::once(init(a, n)).chain(ws).collect::<BTreeSet<_>>().into_iter().collect());
            }
        }
        if n == 16 && a % 8 == 0 {
            if let (Some(lo), Some(hi)) = (self.vals.whole(a, 8), self.vals.whole(a + 8, 8)) {
                let lo: BTreeSet<u128> = std::iter::once(init(a, 8)).chain(lo).collect();
                let hi: BTreeSet<u128> = std::iter::once(init(a + 8, 8)).chain(hi).collect();
                if lo.len() * hi.len() < MAX_READ_VALUES {
                    return Future::Values(lo.iter().flat_map(|&l| hi.iter().map(move |&h| (h << 64) | l)).collect());
                }
            }
        }
        let mut out = vec![0u128];
        for b in (a..a + n).rev() {
            let mut byte = match self.vals.bytes.get(&b) {
                None => BTreeSet::new(),
                Some(Some(v)) => v.clone(),
                Some(None) => return Future::Unknown,
            };
            byte.insert(init_byte(b));
            if out.len() * byte.len() > MAX_READ_VALUES {
                return Future::Unknown;
            }
            out = out.iter().flat_map(|v| byte.iter().map(move |&x| (v << 8) | x as u128)).collect();
        }
        Future::Values(out)
    }

    /// May a single-copy-atomic read of `n` bytes at `a` by thread `tid`
    /// take some bytes from a write by another thread and others from
    /// another write? Only if such a write covers part of the read: its
    /// footprint overlaps the read without containing it, or it is split
    /// into pieces (it is unaligned, or larger than 8 bytes, as is the read).
    pub fn may_mix(&self, tid: usize, a: u64, n: u64) -> bool {
        self.writes.iter().enumerate().any(|(t, f)| {
            t != tid
                && f.iter().any(|&(wa, wn)| {
                    wa < a + n && a < wa + wn && (!(wa <= a && a + n <= wa + wn) || wa % wn != 0 || wn > 8 && n > 8)
                })
        })
    }

    /// The footprints of thread `tid`'s reads that overlap `x`.
    pub fn reads_overlapping(&self, tid: usize, x: &Msg) -> Footprints {
        self.reads[tid].iter().filter(|&&(a, n)| a < x.addr + x.size.bytes() && x.addr < a + n).copied().collect()
    }

    /// Whether `prunable_after` might prune `.., x, y` where `prunable`
    /// does not (so whether `Observed` is worth finding).
    pub fn may_prune_after(&self, x: &Msg, y: &Msg) -> bool {
        priority(y) < priority(x) && !overlaps(x, y)
    }

    /// May the memory `.., x, y` be skipped, given what the threads may do
    /// after `.., x`? (A sharper `prunable`, for one memory.)
    ///
    /// As in `prunable`, `x` and `y` are adjacent, so any ob path from `x`
    /// to `y` passes through no other write: some thread reads `y`'s
    /// footprint from a write before `y` (a from-read edge into `y`) in a
    /// read ordered after `x`, or `y`'s writer orders `y` after `x`. In
    /// Promising-ARM an access ordered after `x` has a pre-view of at least
    /// `ts(x)`; a read of `y`'s footprint from a write before `y` has one
    /// below `ts(y)` (`y` would lie between otherwise), and so does the
    /// write `y`. So both have a pre-view of exactly `ts(x)`, and
    /// `Observed` records every such access in any execution whose memory
    /// extends `.., x` (with a read that takes some bytes from `x` itself
    /// counted as ordered after it).
    ///
    /// The swapped memory `.., y, x` must also be reachable: `y`'s writer
    /// must be able to promise `y` without `x`. It may read `x` on the way
    /// to `y`, but not in a read ordered before `y` (`y` would be ordered
    /// after `x`), so in `.., y, x` that read may return an older value
    /// instead and the thread take the same path to `y`: the value read
    /// reaches `y` only through dependencies, which would order the read
    /// before `y`. A store-exclusive may fail anyway, and so may a
    /// compare-and-swap on a later write (see `cas_fails_later` in
    /// `promising.rs`), which is what lets `y`'s writer certify `y` without
    /// the write that made it fail. (Tested by `tests/por.rs` and the
    /// random tests, not proved.)
    pub fn prunable_after(&self, x: &Msg, y: &Msg, obs: &Observed) -> bool {
        self.may_prune_after(x, y)
            && !obs.reads.iter().any(|r| hits(r, y))
            && !hits(&obs.writes[y.tid], y)
    }

    /// The memory private to each thread, as sorted, disjoint byte ranges:
    /// bytes that no other thread may read or write (however the thread
    /// accesses them: the memory model still orders its exclusive, atomic,
    /// acquire and release accesses there). Every access of the thread, and every
    /// location in `observed`, lies either wholly in a thread's private
    /// memory or wholly outside it.
    pub fn private(&self, observed: &[(u64, u64)]) -> Vec<Vec<(u64, u64)>> {
        let n = self.reads.len();
        let bytes = |f: &Footprints| -> BTreeSet<u64> { f.iter().flat_map(|&(a, len)| a..a + len).collect() };
        // A store-exclusive's footprint counts even if it never succeeds.
        let accessed: Vec<BTreeSet<u64>> =
            (0..n).map(|t| &(&bytes(&self.reads[t]) | &bytes(&self.writes[t])) | &bytes(&self.atomic_writes[t])).collect();
        let mut private: Vec<BTreeSet<u64>> = (0..n)
            .map(|t| {
                let mut p = accessed[t].clone();
                for (u, other) in accessed.iter().enumerate() {
                    if u != t {
                        p.retain(|b| !other.contains(b));
                    }
                }
                p
            })
            .collect();
        // An access or location partly private is made wholly shared, until
        // none is left.
        let fps: Vec<(u64, u64)> = (0..n)
            .flat_map(|t| self.reads[t].iter().chain(&self.writes[t]).chain(&self.atomic_writes[t]).copied())
            .chain(observed.iter().copied())
            .collect();
        loop {
            let mut changed = false;
            for p in private.iter_mut() {
                for &(a, len) in &fps {
                    let inside = (a..a + len).filter(|b| p.contains(b)).count() as u64;
                    if inside != 0 && inside != len {
                        (a..a + len).for_each(|b| {
                            p.remove(&b);
                        });
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        private
            .iter()
            .map(|p| {
                let mut ranges: Vec<(u64, u64)> = Vec::new();
                for &b in p {
                    match ranges.last_mut() {
                        Some(r) if r.1 == b => r.1 = b + 1,
                        _ => ranges.push((b, b + 1)),
                    }
                }
                ranges
            })
            .collect()
    }
}

fn priority(m: &Msg) -> (usize, u64, u64, u128) {
    (m.tid, m.addr, m.size.bytes(), m.val)
}

fn overlaps(x: &Msg, y: &Msg) -> bool {
    x.addr < y.addr + y.size.bytes() && y.addr < x.addr + x.size.bytes()
}

/// The barriers and atomics in `code`: (full barrier or acquire-release
/// atomic, DMB LD or ISB, DMB ST, any acquire read).
fn orderings(code: &Code) -> (bool, bool, bool, bool) {
    let (mut full, mut rr, mut st, mut acquire) = (false, false, false, false);
    for i in &code.instrs {
        match *i {
            Instr::Barrier(Barrier::Full) => full = true,
            Instr::Barrier(Barrier::Ld | Barrier::Isb) => rr = true,
            Instr::Barrier(Barrier::St) => st = true,
            Instr::Load { kind, .. } => acquire |= kind != LoadKind::Plain,
            Instr::LoadExclPair { acquire: a, .. } => acquire |= a,
            Instr::Rmw { acquire: a, release: r, .. } | Instr::Cas { acquire: a, release: r, .. } => {
                acquire |= a;
                full |= a && r;
            }
            _ => {}
        }
    }
    (full, rr, st, acquire)
}

/// One thread's run in the analysis: a core, its exclusive monitor, its
/// own latest writes to the bytes that only it writes, (when finding
/// dependencies, see `Dep`) whether it has passed an acquire read ordered
/// after the source, its epoch (when finding ordering points), and (when
/// finding the readers of publications) whether it has read a published
/// value.
type HState = (Core, Monitor, Own, bool, Epoch, bool);

/// A run's exclusive monitor: the location of the last exclusive load, and
/// the value it returned if it was an acquire and the analysis knows it
/// (see `copying` in `Havoc`).
type Monitor = Option<(u64, Size, Option<u64>)>;

/// Where a run is between the thread's ordering points (when finding
/// them, see `Events`; otherwise always the default). Each count stops at
/// `SATURATED`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Epoch {
    /// Points that may order an earlier write before a later one: full
    /// and store barriers, and acquire and release accesses.
    may: u8,
    /// Full and store barriers, which order every earlier write before
    /// every later one.
    barriers: u8,
    /// Release writes since the last such barrier: a release write orders
    /// every earlier write (in program order) before itself.
    releases: u8,
}

/// The count at which an `Epoch` count stops: two writes at it may have
/// any number of ordering points between them.
const SATURATED: u8 = 7;

fn bump(n: u8) -> u8 {
    (n + 1).min(SATURATED)
}

/// A write found when finding ordering points: its footprint, its value
/// (None: unknown), the epoch it was made in, and whether it is a release.
type Event = ((u64, u64), Option<u128>, Epoch, bool);

/// The writes of one thread with the epochs they were made in (see
/// `Epoch`), from a run of the analysis that follows ordering points
/// in program order.
#[derive(Default)]
struct Events {
    events: BTreeSet<Event>,
}

/// A thread's latest write to each byte that no other thread writes (None:
/// an unknown value), and (when finding dependencies) whether the write
/// depends on the source. Coherence makes such a byte read exactly as the
/// thread last wrote it: no other write to it exists, and a read cannot
/// return a write that comes later in program order.
type Own = BTreeMap<u64, (Option<u8>, bool)>;

/// A run that finds which reads the thread orders after the reads of one
/// instruction (the source), other than by barriers: reads whose address
/// depends on it, reads of writes whose address or data depend on it
/// (Arm's `(addr | data); rfi`), and every read after an acquire read so
/// ordered. Only the source's reads, and reads ordered after them, give
/// tainted values, so the taint follows these orders transitively.
struct Dep {
    /// The source instruction (`usize::MAX` for none).
    source: usize,
    /// Reads that are sources too: a read of exactly one of these
    /// footprints that returns its value (see `Publication`).
    published: BTreeSet<((u64, u64), u128)>,
    /// Footprints of the reads that are not ordered after a source.
    unordered: Footprints,
    /// Bytes that writes depending on the source may have written, as found
    /// by the previous run (a read of one may read such a write back).
    tainted: BTreeSet<u64>,
    /// The same, found by this run.
    new_tainted: BTreeSet<u64>,
    /// Footprints of the source's reads, and of the reads ordered after
    /// them.
    sources: Footprints,
    after: Footprints,
}

/// Taint (dependency view) of a value loaded from memory in the analysis.
const LOADED: View = 1;
/// Taint of a value the analysis does not know: anything computed from it
/// is only a stand-in for the possible values.
const UNKNOWN: View = 2;

fn taint(v: RV) -> View {
    v.view.max(v.pick)
}

/// Most values `masked_values` splits an unknown value into.
const MAX_MASKED_VALUES: u32 = 16;

/// If `ins` keeps only a few bits of an unknown value (an AND with an
/// immediate, or an unsigned bitfield extract), the register it sets, the
/// values it may set it to, and whether it sets the flags too. An index
/// into a small array is often computed this way (`e & 3`), and with the
/// value split, the address of the element stays known.
fn masked_values(core: &Core, ins: &Instr) -> Option<(crate::isa::RegOp, Vec<u64>, bool)> {
    use crate::isa::{AluOp, BfKind, Operand};
    let (rd, mask, shift, flags) = match *ins {
        Instr::Alu { op: AluOp::And, rd, rn, op2: Operand::Imm(m), flags } if taint(core.get(rn)) >= UNKNOWN => {
            (rd, if rd.wide { m } else { m & 0xffff_ffff }, 0, flags)
        }
        Instr::Bitfield { kind: BfKind::Unsigned, rd, rn, immr, imms } if taint(core.get(rn)) >= UNKNOWN => {
            let bits = if rd.wide { 64 } else { 32 };
            let (immr, imms) = (immr as u32, imms as u32);
            let (width, to) = if imms >= immr { (imms - immr + 1, 0) } else { (imms + 1, bits - immr) };
            if width > 32 {
                return None;
            }
            (rd, (1u64 << width) - 1, to, false)
        }
        _ => return None,
    };
    if mask.count_ones() > MAX_MASKED_VALUES.ilog2() || rd.idx == crate::isa::ZR && !flags {
        return None;
    }
    // Every subset of the mask's bits.
    let mut vals = Vec::new();
    let mut v = 0u64;
    loop {
        vals.push(v << shift);
        if v == mask {
            break;
        }
        v = (v.wrapping_sub(mask)) & mask;
    }
    Some((rd, vals, flags))
}

/// The values a byte may hold besides its initial value (None: any).
type ByteVals = Option<BTreeSet<u8>>;

/// The whole values the writes with one footprint may store (None: any).
type WordVals = Option<BTreeSet<u128>>;

/// What the analysis has found that memory may hold.
///
/// Byte by byte, a pointer that may be null or point to one of a few
/// objects lets its bytes combine into many addresses no write stored, and
/// reading through those finds yet more values. So the whole values each
/// write footprint may store are kept too. An aligned read whose bytes are
/// all written only with its own footprint reads them from one write (or
/// initial memory), as an aligned access is single-copy atomic, and so
/// returns only those whole values.
#[derive(Clone, Debug, Default)]
struct Known {
    bytes: HashMap<u64, ByteVals>,
    words: HashMap<(u64, u64), WordVals>,
    /// The footprints of the writes to each byte.
    cover: HashMap<u64, BTreeSet<(u64, u64)>>,
}

impl Known {
    /// The values an aligned read of `n` bytes at `a` may return besides
    /// the initial value, if they can be found from whole writes: when
    /// every write to its bytes has a footprint that contains the whole
    /// read (the same one, or a pair or 16-byte write around it). An
    /// aligned read is single-copy atomic, so at the point where it reads,
    /// one such write is the latest for all its bytes, and it returns a
    /// slice of that write's value.
    fn whole(&self, a: u64, n: u64) -> Option<BTreeSet<u128>> {
        if !matches!(n, 1 | 2 | 4 | 8) || a % n != 0 {
            return None;
        }
        let mut fps: BTreeSet<(u64, u64)> = BTreeSet::new();
        for b in a..a + n {
            if !matches!(self.bytes.get(&b), Some(Some(_))) {
                return None;
            }
            fps.extend(self.cover.get(&b)?);
        }
        let contains = |(wa, wn): (u64, u64)| wa <= a && a + n <= wa + wn;
        let mut out = BTreeSet::new();
        for &(wa, wn) in &fps {
            if !contains((wa, wn)) {
                return None;
            }
            // A 16-byte write's values are kept per half too (see
            // `Havoc::write_parts`), so the half within which the read
            // lies has them all, and stays known when the other half (of a
            // pair) is not.
            if wn == 16 && [wa, wa + 8].iter().any(|&h| contains((h, 8)) && fps.contains(&(h, 8))) {
                continue;
            }
            let mask = if n == 16 { u128::MAX } else { (1u128 << (8 * n)) - 1 };
            for v in self.words.get(&(wa, wn))?.as_ref()? {
                out.insert((v >> (8 * (a - wa))) & mask);
            }
        }
        Some(out)
    }
}

/// The address operand of a memory instruction.
fn mem_addr(i: &Instr) -> Option<&Addr> {
    match i {
        Instr::Load { addr, .. }
        | Instr::Store { addr, .. }
        | Instr::StoreExcl { addr, .. }
        | Instr::LoadExclPair { addr, .. }
        | Instr::StoreExclPair { addr, .. }
        | Instr::LoadPair { addr, .. }
        | Instr::StorePair { addr, .. }
        | Instr::Rmw { addr, .. }
        | Instr::Cas { addr, .. }
        | Instr::VLoad { addr, .. }
        | Instr::VStore { addr, .. } => Some(addr),
        _ => None,
    }
}

/// Runs of one thread in which every load may return any value the
/// analysis has found for its bytes.
struct Havoc<'a> {
    test: &'a Test,
    prog: &'a Code,
    tid: usize,
    fuel: u32,
    /// See `Bound`, and the values seen for each byte in `Own`.
    own_values: Option<usize>,
    own_seen: HashMap<u64, BTreeSet<u8>>,
    /// The values seen in each register at each instruction, for the same
    /// limit.
    reg_seen: HashMap<(usize, usize), BTreeSet<u64>>,
    vals: &'a Known,
    /// The threads that may write each byte (none if absent).
    writers: &'a HashMap<u64, BTreeSet<usize>>,
    /// The bytes the thread may read, as found so far: `Own` keeps the
    /// values only of these (others it never reads back, so their values
    /// would only split states).
    read_bytes: &'a HashSet<u64>,
    /// Bytes read whose values `Own` dropped, as they were not in
    /// `read_bytes`: if any, the run must be repeated with them added.
    missed: HashSet<u64>,
    /// States the analysis may still visit, over all threads and rounds.
    budget: &'a mut usize,
    reads: Footprints,
    acquire_reads: Footprints,
    release_writes: Footprints,
    atomic_writes: Footprints,
    writes: HashMap<u64, ByteVals>,
    word_writes: HashMap<(u64, u64), WordVals>,
    /// Footprints of writes whose address or data depends on a read.
    dep_writes: Footprints,
    /// Some read's address depends on an earlier read.
    addr_dep: bool,
    /// Footprints of all writes, and of exclusive reads.
    write_fps: Footprints,
    /// Footprints of the writes that copy the value they read (see
    /// `Reduction::copy_writes`), and of all other writes.
    copy_fps: Footprints,
    noncopy_fps: Footprints,
    /// Set while such a copy writes.
    copying: bool,
    /// Instructions that read memory, with the footprints of their reads.
    read_pcs: BTreeMap<usize, Footprints>,
    /// Set when finding dependencies.
    dep: Option<Dep>,
    /// The memory instruction being performed, whether its run has passed
    /// an acquire ordered after the source, and whether one of its reads is
    /// the source or is ordered after it.
    pc: usize,
    after: bool,
    ordered: bool,
    /// Set when finding ordering points (see `Epoch`).
    events: Option<Events>,
    /// The epoch of the memory instruction being performed, and whether it
    /// is a release.
    epoch: Epoch,
    release: bool,
    /// Live registers at each instruction (see `crate::live`), with none
    /// live at the end, as the analysis ignores final registers.
    live: Option<&'a crate::live::Liveness>,
}

impl<'a> Havoc<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        test: &'a Test,
        tid: usize,
        bound: Bound,
        vals: &'a Known,
        writers: &'a HashMap<u64, BTreeSet<usize>>,
        read_bytes: &'a HashSet<u64>,
        budget: &'a mut usize,
        dep: Option<Dep>,
        live: Option<&'a crate::live::Liveness>,
    ) -> Havoc<'a> {
        Havoc {
            test,
            prog: &test.threads[tid].code,
            tid,
            fuel: bound.fuel,
            own_values: bound.own_values,
            own_seen: HashMap::new(),
            reg_seen: HashMap::new(),
            vals,
            writers,
            read_bytes,
            missed: HashSet::new(),
            budget,
            reads: Footprints::new(),
            write_fps: Footprints::new(),
            copy_fps: Footprints::new(),
            noncopy_fps: Footprints::new(),
            copying: false,
            acquire_reads: Footprints::new(),
            release_writes: Footprints::new(),
            atomic_writes: Footprints::new(),
            writes: HashMap::new(),
            word_writes: HashMap::new(),
            dep_writes: Footprints::new(),
            addr_dep: false,
            read_pcs: BTreeMap::new(),
            dep,
            pc: 0,
            after: false,
            ordered: false,
            events: None,
            epoch: Epoch::default(),
            release: false,
            live,
        }
    }

    fn start(&self) -> Core {
        let th = &self.test.threads[self.tid];
        Core::new(self.test.init_regs[self.tid], self.tid, self.test.threads.len(), th.entry)
    }

    /// A read of `n` bytes at `addr`: every value it may return, with its
    /// taint; or one stand-in value tainted `UNKNOWN`, if some byte may hold
    /// any value or there are too many combinations.
    fn read(&mut self, own: &Own, addr: RV, n: u64, acquire: bool) -> Vec<(u128, View)> {
        let mut vals = self.read_values(own, addr, n, acquire);
        if let Some(d) = &self.dep
            && !d.published.is_empty()
        {
            for (v, t) in &mut vals {
                if *t < LOADED && d.published.contains(&((addr.val, n), *v)) {
                    *t = LOADED;
                }
            }
        }
        vals
    }

    /// `read`, without the taint of published values.
    fn read_values(&mut self, own: &Own, addr: RV, n: u64, acquire: bool) -> Vec<(u128, View)> {
        let a = addr.val;
        self.reads.insert((a, n));
        if acquire {
            self.acquire_reads.insert((a, n));
        }
        if !(a..a + n).any(|b| self.own_only(b)) {
            if let Some(ws) = self.vals.whole(a, n) {
                let init = self.stand_in(a, n);
                if ws.len() < MAX_READ_VALUES {
                    let t = self.dep_taint(own, addr, n);
                    return std::iter::once(init).chain(ws).collect::<BTreeSet<_>>().into_iter().map(|v| (v, t)).collect();
                }
            }
            // A 16-byte read (of a pair) is two single-copy-atomic halves
            // at most, so it returns any combination of their values.
            if n == 16
                && a % 8 == 0
                && let (Some(lo), Some(hi)) = (self.vals.whole(a, 8), self.vals.whole(a + 8, 8))
            {
                let lo: BTreeSet<u128> = std::iter::once(self.stand_in(a, 8)).chain(lo).collect();
                let hi: BTreeSet<u128> = std::iter::once(self.stand_in(a + 8, 8)).chain(hi).collect();
                if lo.len() * hi.len() < MAX_READ_VALUES {
                    let t = self.dep_taint(own, addr, n);
                    return lo.iter().flat_map(|&l| hi.iter().map(move |&h| (h << 64) | l)).map(|v| (v, t)).collect();
                }
            }
        }
        let mut out = vec![0u128];
        for b in (a..a + n).rev() {
            let init = self.test.init_byte(b);
            if self.own_only(b) {
                match own.get(&b) {
                    Some((None, _)) => {
                        if !self.read_bytes.contains(&b) {
                            self.missed.extend(a..a + n);
                        }
                        self.dep_taint(own, addr, n);
                        return vec![(self.stand_in(a, n), UNKNOWN)];
                    }
                    Some((Some(v), _)) => out.iter_mut().for_each(|x| *x = (*x << 8) | *v as u128),
                    None => out.iter_mut().for_each(|x| *x = (*x << 8) | init as u128),
                }
                continue;
            }
            let mut byte = match self.vals.bytes.get(&b) {
                None => BTreeSet::new(),
                Some(Some(v)) => v.clone(),
                Some(None) => {
                    self.dep_taint(own, addr, n);
                    return vec![(self.stand_in(a, n), UNKNOWN)];
                }
            };
            byte.insert(init);
            if out.len() * byte.len() > MAX_READ_VALUES {
                self.dep_taint(own, addr, n);
                return vec![(self.stand_in(a, n), UNKNOWN)];
            }
            out = out.iter().flat_map(|v| byte.iter().map(move |&x| (v << 8) | x as u128)).collect();
        }
        let t = self.dep_taint(own, addr, n);
        out.into_iter().map(|v| (v, t)).collect()
    }

    /// The taint of a value read from `n` bytes at `addr`: `LOADED`, or when
    /// finding dependencies, `LOADED` only if the read is the source or is
    /// ordered after it.
    fn dep_taint(&mut self, own: &Own, addr: RV, n: u64) -> View {
        let a = addr.val;
        self.read_pcs.entry(self.pc).or_default().insert((a, n));
        self.addr_dep |= taint(addr) != 0;
        let after = self.after;
        let Some(d) = &mut self.dep else { return LOADED };
        // A byte in `tainted` may hold a write that depends on the source,
        // or another one: that over-approximates the reads ordered after
        // the source, as `dep_rr` needs, but publications need the reads
        // that are certainly ordered after one (see `Publication`). A byte
        // only this thread writes holds its own latest write.
        let maybe = d.published.is_empty() && (a..a + n).any(|b| d.tainted.contains(&b));
        let ordered = after || taint(addr) >= LOADED || maybe || (a..a + n).any(|b| own.get(&b).is_some_and(|e| e.1));
        if ordered {
            d.after.insert((a, n));
        } else {
            d.unordered.insert((a, n));
        }
        if self.pc == d.source {
            d.sources.insert((a, n));
        }
        if ordered || self.pc == d.source {
            self.ordered = true;
            LOADED
        } else {
            0
        }
    }

    /// Whether no other thread may write byte `b`.
    fn own_only(&self, b: u64) -> bool {
        self.writers.get(&b).is_none_or(|w| w.iter().all(|&t| t == self.tid))
    }

    /// The initial value of `n` bytes at `a`.
    fn stand_in(&self, a: u64, n: u64) -> u128 {
        (a..a + n).rev().fold(0, |v, b| (v << 8) | self.test.init_byte(b) as u128)
    }

    /// A write of `n` bytes of `v` at `addr`; `data` is the taint of `v`.
    fn write(&mut self, own: &mut Own, addr: RV, n: u64, v: u128, data: View) {
        self.write_parts(own, addr, n, &[(v, data)]);
    }

    /// A single-copy-atomic write of `n` bytes at `addr` made of equal
    /// parts (the registers of a pair), each a value and its taint. The
    /// whole values of each half of a 16-byte write are kept as well as
    /// those of the whole write, so a register whose value is unknown (a
    /// counter) leaves the other (a pointer) known.
    fn write_parts(&mut self, own: &mut Own, addr: RV, n: u64, parts: &[(u128, View)]) {
        let a = addr.val;
        let h = n / parts.len() as u64;
        let data = parts.iter().map(|p| p.1).max().unwrap();
        let part = |i: u64| parts[(i / h) as usize];
        let byte = |i: u64| {
            let (v, t) = part(i);
            (t < UNKNOWN).then_some((v >> (8 * (i % h))) as u8)
        };
        self.write_fps.insert((a, n));
        if self.copying {
            self.copy_fps.insert((a, n));
        } else {
            self.noncopy_fps.insert((a, n));
        }
        if let Some(e) = &mut self.events {
            let whole = parts.iter().enumerate().fold(0u128, |w, (j, p)| w | p.0 << (8 * h * j as u64));
            e.events.insert(((a, n), (data < UNKNOWN).then_some(whole), self.epoch, self.release));
        }
        if taint(addr) != 0 || data != 0 {
            self.dep_writes.insert((a, n));
        }
        let depends = match &mut self.dep {
            Some(d) if data >= LOADED || taint(addr) >= LOADED => {
                d.new_tainted.extend(a..a + n);
                true
            }
            _ => false,
        };
        for i in 0..n {
            if self.own_only(a + i) {
                let mut e = if self.read_bytes.contains(&(a + i)) { (byte(i), depends) } else { (None, false) };
                if let (Some(limit), Some(v)) = (self.own_values, e.0) {
                    let seen = self.own_seen.entry(a + i).or_default();
                    if !seen.contains(&v) && seen.len() >= limit {
                        e.0 = None;
                    } else {
                        seen.insert(v);
                    }
                }
                own.insert(a + i, e);
            }
        }
        let whole = parts.iter().enumerate().fold(0u128, |w, (j, p)| w | p.0 << (8 * h * j as u64));
        let mut words = vec![((a, n), whole, data)];
        // A 16-byte write's values are kept per 8-byte half too (see
        // `Known::whole`), each with its own taint if it is a pair.
        if n == 16 {
            for j in 0..2u64 {
                let t = if parts.len() == 2 { parts[j as usize].1 } else { data };
                words.push(((a + 8 * j, 8), (whole >> (64 * j)) & u64::MAX as u128, t));
            }
        }
        for (fp, v, t) in words {
            let w = self.word_writes.entry(fp).or_insert_with(|| Some(BTreeSet::new()));
            match w {
                Some(set) if t < UNKNOWN => {
                    set.insert(v);
                }
                _ => *w = None,
            }
        }
        for i in 0..n {
            let e = self.writes.entry(a + i).or_insert_with(|| Some(BTreeSet::new()));
            match (e, byte(i)) {
                (Some(set), Some(b)) => {
                    set.insert(b);
                }
                (e, _) => *e = None,
            }
        }
    }

    /// Make unknown each register that has held more than `limit` values
    /// at this instruction (see `Bound`): a counter kept in a register
    /// (folly's flat combiner sums the requests of a session) otherwise
    /// gives each pass of its loop new states. An unknown value stands for
    /// any value: a branch on it takes both ways, and an access at an
    /// address computed from it gives up. The frame pointer and the link
    /// register (X29, X30) are left alone: a function called from several
    /// places holds several return addresses there, and a return must go
    /// to one of them. The value of an unknown register is only a stand-in,
    /// so it is set to 0 (as are unknown flags): otherwise a counter
    /// computed from one gives new states as before.
    fn widen(&mut self, core: &mut Core, limit: usize) {
        if taint(core.flags) >= UNKNOWN {
            core.flags.val = 0;
        }
        for (i, r) in core.regs.iter_mut().enumerate().take(29) {
            if taint(*r) >= UNKNOWN {
                r.val = 0;
                continue;
            }
            let seen = self.reg_seen.entry((core.pc, i)).or_default();
            if !seen.contains(&r.val) && seen.len() >= limit {
                *r = RV { val: 0, view: UNKNOWN, pick: UNKNOWN };
            } else {
                seen.insert(r.val);
            }
        }
    }

    /// Explore every run of the thread. None if the analysis gives up.
    fn run(&mut self, start: Core) -> Option<()> {
        let mut visited: FxHashMap<HState, u32> = FxHashMap::default();
        let mut queue = StepQueue::new();
        queue.push(start.steps, (start, None, Own::new(), false, Epoch::default(), false));
        while let Some(mut st) = queue.pop() {
            if let Some(limit) = self.own_values {
                self.widen(&mut st.0, limit);
            }
            let mut key = st.clone();
            key.0.steps = 0;
            if let Some(l) = self.live {
                l.canon(&mut key.0);
            }
            match visited.entry(key) {
                Entry::Occupied(e) if *e.get() <= st.0.steps => continue,
                Entry::Occupied(mut e) => *e.get_mut() = st.0.steps,
                Entry::Vacant(e) => {
                    e.insert(st.0.steps);
                }
            }
            *self.budget = self.budget.checked_sub(1)?;
            if st.0.steps >= self.fuel {
                continue;
            }
            for x in self.successors(st)? {
                queue.push(x.0.steps, x);
            }
        }
        Some(())
    }

    /// The states after the next memory operation, or the next branch on an
    /// unknown value. Other local steps have one successor, so they are
    /// taken at once rather than each being a state of the search.
    fn successors(&mut self, (mut core, mon, mut own, after, epoch, seen): HState) -> Option<Vec<HState>> {
        let prog = self.prog;
        let reg = |v: u64, t: View| RV { val: v, view: t, pick: t };
        let status = |v: u64| RV::imm(v);
        let op = loop {
            let pc = core.pc;
            if let Some(a) = prog.instrs.get(pc).and_then(mem_addr) {
                // An access at an unknown address could touch anything
                // (including another thread's stack); give up.
                if taint(core.addr(a)) >= UNKNOWN {
                    return None;
                }
            }
            // A return the liveness does not expect: give up, as states may
            // have been merged that this return tells apart.
            if self.live.is_some_and(|l| !l.ret_ok(prog, &core)) {
                return None;
            }
            let split = prog.instrs.get(pc).and_then(|ins| masked_values(&core, ins));
            match core.step(prog) {
                Step::Done | Step::Fault(_) => return Some(vec![]),
                Step::Local { .. } if split.is_some() => {
                    // A few bits of an unknown value: take each value they
                    // may have, as loaded from memory (so still a
                    // dependency on the read).
                    let (rd, vals, flags) = split.unwrap();
                    return Some(
                        vals.into_iter()
                            .map(|v| {
                                let mut c = core.clone();
                                c.set(rd, reg(v, LOADED));
                                if flags {
                                    let sign = if rd.wide { 1u64 << 63 } else { 1 << 31 };
                                    let nz = ((v & sign != 0) as u64) << 3 | ((v == 0) as u64) << 2;
                                    c.flags = reg(nz, LOADED);
                                }
                                (c, mon, own.clone(), after, epoch, seen)
                            })
                            .collect(),
                    );
                }
                Step::Local { ctrl: Some(t), .. } if t >= UNKNOWN => {
                    // A branch on an unknown value: take both ways.
                    let other = match prog.instrs[pc] {
                        Instr::BCond { target, .. } | Instr::Cbz { target, .. } | Instr::Tbz { target, .. } => {
                            if core.pc == target { pc + 1 } else { target }
                        }
                        _ => return None,
                    };
                    let mut alt = core.clone();
                    alt.pc = other;
                    return Some(vec![(core, mon, own.clone(), after, epoch, seen), (alt, mon, own, after, epoch, seen)]);
                }
                Step::Local { .. } if core.steps >= self.fuel => return Some(vec![(core, mon, own, after, epoch, seen)]),
                Step::Local { .. } => {}
                Step::Mem(op) => break op,
            }
        };
        let acquire = match op {
            MemOp::Load { kind, .. } => kind != LoadKind::Plain,
            MemOp::LoadExclPair { acquire, .. } | MemOp::Rmw { acquire, .. } => acquire,
            _ => false,
        };
        let release = match op {
            MemOp::Store { release, .. }
            | MemOp::StoreExcl { release, .. }
            | MemOp::StoreExclPair { release, .. }
            | MemOp::Rmw { release, .. } => release,
            _ => false,
        };
        // The epoch of this access's writes, and of the run after it. (An
        // acquire or release access counts as an ordering point both before
        // and after itself, which may only order more: the write of an
        // acquire-release atomic is ordered before later writes.)
        let (mut now, mut next) = (epoch, epoch);
        if self.events.is_some() {
            let barrier = matches!(op, MemOp::Barrier(Barrier::Full | Barrier::St));
            if barrier || acquire || release {
                now.may = bump(now.may);
            }
            if barrier {
                now.barriers = bump(now.barriers);
                now.releases = 0;
            }
            next = now;
            if acquire || release {
                next.may = bump(now.may);
            }
            if release {
                next.releases = bump(now.releases);
            }
        }
        (self.epoch, self.release) = (now, release);
        // A store-exclusive that fails writes nothing, so it is no release
        // write: the run after it keeps the count of releases (a retry loop
        // would otherwise saturate it).
        let failed = Epoch { releases: now.releases, ..next };
        let next = std::cell::Cell::new(next);
        let mut out = Vec::new();
        let mut push = |mut c: Core, m: Monitor, o: Own| {
            c.retire(prog);
            out.push((c, m, o, after, next.get(), seen));
        };
        (self.pc, self.after, self.ordered) = (core.pc, after, false);
        match op {
            MemOp::Load { addr, rt, size, signed, excl, kind } => {
                for (v, t) in self.read(&own, addr, size.bytes(), kind != LoadKind::Plain) {
                    let mut c = core.clone();
                    c.set(rt, reg(size.extend(v as u64, signed), t));
                    let copied = (kind != LoadKind::Plain && t < UNKNOWN).then_some(v as u64);
                    push(c, if excl { Some((addr.val, size, copied)) } else { mon }, own.clone());
                }
            }
            MemOp::LoadExclPair { addr, rt, rt2, size, acquire } => {
                // Read as two halves, each with its own values and taint
                // (a superset of the pair's single-copy-atomic values), so
                // that a counter in one half that the analysis does not
                // know leaves the other half, a pointer say, known.
                let total = size.double();
                let n = size.bytes();
                self.reads.insert((addr.val, total.bytes()));
                if acquire {
                    self.acquire_reads.insert((addr.val, total.bytes()));
                }
                let second = self.read(&own, RV { val: addr.val + n, ..addr }, n, acquire);
                for (lo, t1) in self.read(&own, addr, n, acquire) {
                    for &(hi, t2) in &second {
                        let mut c = core.clone();
                        c.set(rt, reg(lo as u64, t1));
                        c.set(rt2, reg(hi as u64, t2));
                        push(c, Some((addr.val, total, None)), own.clone());
                    }
                }
            }
            MemOp::LoadPair { addr, rt, rt2, size, signed } => {
                let n = size.bytes();
                // Two reads, or one under FEAT_LSE2.
                self.reads.insert((addr.val, 2 * n));
                let second = self.read(&own, RV { val: addr.val + n, ..addr }, n, false);
                for (v1, t1) in self.read(&own, addr, n, false) {
                    for &(v2, t2) in &second {
                        let mut c = core.clone();
                        c.set(rt, reg(size.extend(v1 as u64, signed), t1));
                        c.set(rt2, reg(size.extend(v2 as u64, signed), t2));
                        push(c, mon, own.clone());
                    }
                }
            }
            MemOp::VLoad { addr, vt, vt2, size } => {
                let n = size.bytes();
                if vt2.is_some() {
                    self.reads.insert((addr.val, 2 * n));
                }
                let mut cores = vec![core];
                for (i, v) in std::iter::once(vt).chain(vt2).enumerate() {
                    let vals = self.read(&own, RV { val: addr.val + i as u64 * n, ..addr }, n, false);
                    cores = cores
                        .iter()
                        .flat_map(|c| {
                            vals.iter().map(move |&(val, t)| {
                                let mut c = c.clone();
                                c.vset(v, VR { val, view: t, pick: t });
                                c
                            })
                        })
                        .collect();
                }
                for c in cores {
                    push(c, mon, own.clone());
                }
            }
            MemOp::Store { addr, data, size, release } => {
                if release {
                    self.release_writes.insert((addr.val, size.bytes()));
                }
                self.write(&mut own, addr, size.bytes(), size.mask(data.val) as u128, taint(data));
                push(core, mon, own);
            }
            MemOp::StorePair { addr, data, data2, size } => {
                let n = size.bytes();
                // Two writes, but one access for finding private memory
                // (see `Reduction::private`), as for `LoadPair`.
                self.write_fps.insert((addr.val, 2 * n));
                self.write(&mut own, addr, n, size.mask(data.val) as u128, taint(data));
                self.write(&mut own, RV { val: addr.val + n, ..addr }, n, size.mask(data2.val) as u128, taint(data2));
                push(core, mon, own);
            }
            MemOp::VStore { addr, data, data2, size } => {
                let n = size.bytes();
                if data2.is_some() {
                    self.write_fps.insert((addr.val, 2 * n));
                }
                for (i, d) in std::iter::once(data).chain(data2).enumerate() {
                    self.write(&mut own, RV { val: addr.val + i as u64 * n, ..addr }, n, low_bytes(d.val, n), d.view.max(d.pick));
                }
                push(core, mon, own);
            }
            MemOp::StoreExcl { addr, data, rs, size, release } => {
                self.atomic_writes.insert((addr.val, size.bytes()));
                if release {
                    self.release_writes.insert((addr.val, size.bytes()));
                }
                if let Some((a, sz, copied)) = mon
                    && (a, sz) == (addr.val, size)
                {
                    // A store of the value an acquire exclusive load read
                    // writes it back, as an atomic add of 0 does.
                    self.copying = taint(data) < UNKNOWN && copied == Some(size.mask(data.val));
                    let mut o = own.clone();
                    self.write(&mut o, addr, size.bytes(), size.mask(data.val) as u128, taint(data));
                    self.copying = false;
                    let mut c = core.clone();
                    c.set(rs, status(0));
                    push(c, None, o);
                }
                core.set(rs, status(1));
                next.set(failed);
                push(core, None, own);
            }
            MemOp::StoreExclPair { addr, data, data2, rs, size, release } => {
                let total = size.double();
                self.atomic_writes.insert((addr.val, total.bytes()));
                if release {
                    self.release_writes.insert((addr.val, total.bytes()));
                }
                if mon.is_some_and(|(a, sz, _)| (a, sz) == (addr.val, total)) {
                    let mut o = own.clone();
                    let (d1, d2) = (size.mask(data.val) as u128, size.mask(data2.val) as u128);
                    self.write_parts(&mut o, addr, total.bytes(), &[(d1, taint(data)), (d2, taint(data2))]);
                    let mut c = core.clone();
                    c.set(rs, status(0));
                    push(c, None, o);
                }
                core.set(rs, status(1));
                next.set(failed);
                push(core, None, own);
            }
            MemOp::Rmw { addr, op, operand, cmp, rt, size, acquire, release, .. } => {
                let n = size.bytes();
                self.atomic_writes.insert((addr.val, n));
                if release {
                    self.release_writes.insert((addr.val, n));
                }
                // An acquire atomic that writes back what it read.
                self.copying = acquire
                    && cmp.is_none()
                    && taint(operand) == 0
                    && size.mask(operand.val) == 0
                    && matches!(op, RmwOp::Add | RmwOp::Clr | RmwOp::Eor | RmwOp::Set);
                for (old, t) in self.read(&own, addr, n, acquire) {
                    let old = old as u64;
                    let mut o = own.clone();
                    let known = t < UNKNOWN && cmp.is_none_or(|c| taint(c) < UNKNOWN);
                    if known {
                        if let Some(new) = rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                            // The new value may depend on the old one.
                            self.write(&mut o, addr, n, new as u128, taint(operand).max(t.min(LOADED)));
                        }
                    } else if cmp.is_some() || op == RmwOp::Swp {
                        // A CAS may succeed (or fail); either way only the
                        // operand can be written.
                        self.write(&mut o, addr, n, size.mask(operand.val) as u128, taint(operand).max(t.min(LOADED)));
                    } else {
                        self.write(&mut o, addr, n, 0, UNKNOWN);
                    }
                    let mut c = core.clone();
                    c.set(rt, reg(old, t));
                    push(c, mon, o);
                }
                self.copying = false;
            }
            MemOp::Barrier(_) => push(core, mon, own),
            MemOp::Clrex => push(core, None, own),
        }
        // An acquire read ordered after the source orders every later read.
        if acquire && self.ordered {
            for s in &mut out {
                s.3 = true;
            }
        }
        // A read of a published value (see `Publication`) is ordered before
        // every later read if it is an acquire, or once the run passes a
        // barrier that orders reads.
        let published_read = match op {
            MemOp::Load { addr, rt, size, signed, .. } => Some((addr, rt, size, signed)),
            MemOp::Rmw { addr, rt, size, .. } => Some((addr, rt, size, false)),
            _ => None,
        };
        if let (Some(d), Some((addr, rt, size, signed))) = (&self.dep, published_read)
            && !d.published.is_empty()
            && rt.idx != crate::isa::ZR
        {
            for s in &mut out {
                let got = s.0.get(rt).val;
                let published = d.published.iter().any(|&(f, v)| f == (addr.val, size.bytes()) && size.extend(v as u64, signed) == got);
                if published {
                    s.5 = true;
                    s.3 |= acquire;
                }
            }
        }
        if seen && matches!(op, MemOp::Barrier(Barrier::Full | Barrier::Ld)) {
            for s in &mut out {
                s.3 = true;
            }
        }
        Some(out)
    }
}

/// For each instruction of thread `tid` that reads memory, the footprints
/// of its reads and of the reads the thread orders after them other than
/// by barriers (see `Dep`), or None if that is too costly to find.
fn dep_orders(
    test: &Test,
    tid: usize,
    bound: Bound,
    vals: &Known,
    writers: &HashMap<u64, BTreeSet<usize>>,
    read_bytes: &HashSet<u64>,
    pcs: &BTreeSet<usize>,
    live: Option<&crate::live::Liveness>,
) -> Option<Vec<(Footprints, Footprints)>> {
    let mut budget = MAX_STATES;
    let mut out = Vec::new();
    'pcs: for &source in pcs {
        let mut tainted = BTreeSet::new();
        for _ in 0..MAX_ROUNDS {
            let dep = Dep {
                source,
                published: BTreeSet::new(),
                unordered: Footprints::new(),
                tainted: tainted.clone(),
                new_tainted: BTreeSet::new(),
                sources: Footprints::new(),
                after: Footprints::new(),
            };
            let mut h = Havoc::new(test, tid, bound, vals, writers, read_bytes, &mut budget, Some(dep), live);
            h.run(h.start())?;
            if !h.missed.is_empty() {
                return None;
            }
            let d = h.dep.take().unwrap();
            if d.new_tainted.is_subset(&tainted) {
                out.push((d.sources, d.after));
                continue 'pcs;
            }
            tainted.extend(d.new_tainted);
        }
        return None;
    }
    Some(out)
}

/// The writes of thread `tid` with their epochs (see `Epoch`), or None if
/// that is too costly to find.
#[allow(clippy::too_many_arguments)]
fn write_events(
    test: &Test,
    tid: usize,
    bound: Bound,
    vals: &Known,
    writers: &HashMap<u64, BTreeSet<usize>>,
    read_bytes: &HashSet<u64>,
    live: Option<&crate::live::Liveness>,
) -> Option<BTreeSet<Event>> {
    let mut budget = MAX_STATES;
    let mut h = Havoc::new(test, tid, bound, vals, writers, read_bytes, &mut budget, None, live);
    h.events = Some(Events::default());
    h.run(h.start())?;
    h.missed.is_empty().then(|| h.events.take().unwrap().events)
}

/// Is the write `y` ordered before the write `p` (by the same thread) in
/// every run in which both are made? Epochs only grow along a run, so a
/// lower count means earlier in program order.
fn must_precede(y: &Event, p: &Event) -> bool {
    let (ey, (ep, release)) = (y.2, (p.2, p.3));
    ey.barriers < ep.barriers
        || release && ey.barriers == ep.barriers && ep.barriers < SATURATED && ep.releases < SATURATED && ey.releases <= ep.releases
}

/// The publications of writer `a` (see `Publication`), without the
/// readers' side: groups of (location, value) pairs, each with the
/// footprints it protects. `copies_only(t, p)` says whether thread `t`
/// writes the location `p` only with acquire atomics or exclusive pairs
/// that write back the value they read, and with `p`'s footprint (see
/// `Reduction::copy_writes`).
///
/// Such copies do not stop `p` from publishing. Where A's write `p` alone
/// gives `P` the value `v`, a copy writes `v` only after reading it from
/// `p` or from an earlier copy, which it reads either from another thread
/// (an `rfe` edge, ordered) or from its own thread's earlier copy, whose
/// read is an acquire and so orders every later access. Its write follows
/// its read (`rmw`, which a successful store-exclusive forms with its
/// load-exclusive). By induction every copy, and every read of `v` from
/// one (`rfe`, or after the acquire in the copying thread), is ordered
/// after `p`, as a read of `v` from `p` itself is.
fn publications(
    test: &Test,
    a: usize,
    events: &BTreeSet<Event>,
    writers: &HashMap<u64, BTreeSet<usize>>,
    copies_only: &dyn Fn(usize, (u64, u64)) -> bool,
) -> Vec<(BTreeSet<((u64, u64), u128)>, Footprints)> {
    let overlap = |f: (u64, u64), g: (u64, u64)| f.0 < g.0 + g.1 && g.0 < f.0 + f.1;
    let fps: BTreeSet<(u64, u64)> = events.iter().map(|e| e.0).collect();
    // For each footprint it may protect, the (location, value) pairs that do.
    let mut by: BTreeMap<(u64, u64), BTreeSet<((u64, u64), u128)>> = BTreeMap::new();
    for &p in &fps {
        let (pa, pn) = p;
        let only_a = (pa..pa + pn)
            .all(|b| writers.get(&b).is_some_and(|w| w.contains(&a) && w.iter().all(|&t| t == a || copies_only(t, p))));
        let same_fp = events.iter().filter(|e| overlap(e.0, p)).all(|e| e.0 == p && e.1.is_some());
        if !matches!(pn, 1 | 2 | 4 | 8) || pa % pn != 0 || !only_a || !same_fp {
            continue;
        }
        let init = (pa..pa + pn).rev().fold(0u128, |v, b| (v << 8) | test.init_byte(b) as u128);
        let values: BTreeSet<u128> = events.iter().filter(|e| e.0 == p).filter_map(|e| e.1).filter(|&v| v != init).collect();
        for v in values {
            let ps: Vec<&Event> = events.iter().filter(|e| e.0 == p && e.1 == Some(v)).collect();
            for &y in &fps {
                if y != p && events.iter().filter(|e| e.0 == y).all(|e| ps.iter().all(|q| must_precede(e, q))) {
                    by.entry(y).or_default().insert((p, v));
                }
            }
        }
    }
    let mut groups: BTreeMap<BTreeSet<((u64, u64), u128)>, Footprints> = BTreeMap::new();
    for (y, k) in by {
        groups.entry(k).or_default().insert(y);
    }
    groups.into_iter().take(MAX_PUBLICATIONS).collect()
}

/// Most publication groups found for each writer.
const MAX_PUBLICATIONS: usize = 8;

/// The footprints of the reads of thread `tid` that are not ordered (by
/// dependencies or an acquire, see `Dep`) after a read of one of the
/// `published` (location, value) pairs, or None if that is too costly to
/// find.
#[allow(clippy::too_many_arguments)]
fn unpublished_reads(
    test: &Test,
    tid: usize,
    bound: Bound,
    vals: &Known,
    writers: &HashMap<u64, BTreeSet<usize>>,
    read_bytes: &HashSet<u64>,
    published: &BTreeSet<((u64, u64), u128)>,
    live: Option<&crate::live::Liveness>,
) -> Option<Footprints> {
    // One run: the bytes that writes depending on a source may hold
    // (`Dep::tainted`) are not used here.
    let mut budget = MAX_STATES;
    let dep = Dep {
        source: usize::MAX,
        published: published.clone(),
        unordered: Footprints::new(),
        tainted: BTreeSet::new(),
        new_tainted: BTreeSet::new(),
        sources: Footprints::new(),
        after: Footprints::new(),
    };
    let mut h = Havoc::new(test, tid, bound, vals, writers, read_bytes, &mut budget, Some(dep), live);
    h.run(h.start())?;
    h.missed.is_empty().then(|| h.dep.take().unwrap().unordered)
}

/// Compute what the reduction needs for `test`, or None if the analysis
/// gives up (then nothing should be pruned). If it gives up, it runs again
/// with the values of bytes in `Own` widened (`WIDENED_OWN_VALUES`), which
/// is less precise but may stay within its budget.
pub fn analyze(test: &Test, fuel: u32, live: bool) -> Option<Reduction> {
    analyze_with(test, Bound { fuel, own_values: None }, live)
        .or_else(|| analyze_with(test, Bound { fuel, own_values: Some(WIDENED_OWN_VALUES) }, live))
}

fn analyze_with(test: &Test, bound: Bound, live: bool) -> Option<Reduction> {
    let n = test.threads.len();
    let lives: Vec<Option<crate::live::Liveness>> =
        test.threads.iter().map(|th| live.then(|| crate::live::Liveness::new(&th.code, 0))).collect();
    let mut vals = Known::default();
    let mut red = Reduction {
        reads: vec![Footprints::new(); n],
        writes: vec![Footprints::new(); n],
        acquire_reads: vec![Footprints::new(); n],
        release_writes: vec![Footprints::new(); n],
        atomic_writes: vec![Footprints::new(); n],
        copy_writes: vec![Footprints::new(); n],
        noncopy_writes: vec![Footprints::new(); n],
        ..Reduction::default()
    };
    let mut read_pcs: Vec<BTreeMap<usize, Footprints>> = vec![BTreeMap::new(); n];
    let mut dep_writes = vec![Footprints::new(); n];
    let mut addr_dep = vec![false; n];
    let mut read_bytes = vec![HashSet::new(); n];
    let mut budget = MAX_STATES;
    // For each byte in a streak of rounds in which its values grow: the
    // rounds it has grown in, and the rounds since it last grew.
    let mut streaks: HashMap<u64, (usize, usize)> = HashMap::new();
    let mut writers: HashMap<u64, BTreeSet<usize>> = HashMap::new();
    // The threads that may write each byte other than with copies (see
    // `Reduction::copy_writes`), for `Havoc::own_only`. A copy is a
    // single-copy-atomic read-modify-write that writes back the value it
    // read, so it sits in the coherence order right after the write it
    // read, with the same value: the values a byte takes, in coherence
    // order, are those of the other writes. If one thread T makes them all,
    // a read by T returns T's latest write, as with no copies: a later
    // write of T is coherence-after the read, and so is any copy of it.
    let mut own_writers: HashMap<u64, BTreeSet<usize>> = HashMap::new();
    for _ in 0..MAX_ROUNDS {
        let mut grown: BTreeSet<u64> = BTreeSet::new();
        let mut new_vals = vals.clone();
        let mut new_writers = writers.clone();
        let mut new_own_writers = own_writers.clone();
        let mut new_writer = false;
        let mut words_grown = false;
        for tid in 0..n {
            let mut h = loop {
                let mut h = Havoc::new(test, tid, bound, &vals, &own_writers, &read_bytes[tid], &mut budget, None, lives[tid].as_ref());
                let r = h.run(h.start());
                if h.missed.is_empty() {
                    r?;
                    break h;
                }
                let missed = std::mem::take(&mut h.missed);
                read_bytes[tid].extend(missed);
            };
            for (pc, mut f) in std::mem::take(&mut h.read_pcs) {
                read_pcs[tid].entry(pc).or_default().append(&mut f);
            }
            dep_writes[tid].append(&mut h.dep_writes);
            addr_dep[tid] |= h.addr_dep;
            red.reads[tid].append(&mut h.reads);
            red.writes[tid].append(&mut h.write_fps);
            red.acquire_reads[tid].append(&mut h.acquire_reads);
            red.release_writes[tid].append(&mut h.release_writes);
            red.atomic_writes[tid].append(&mut h.atomic_writes);
            for &(a, len) in &h.noncopy_fps {
                for b in a..a + len {
                    new_writer |= new_own_writers.entry(b).or_default().insert(tid);
                }
            }
            red.copy_writes[tid].append(&mut h.copy_fps);
            red.noncopy_writes[tid].append(&mut h.noncopy_fps);
            for ((a, len), vs) in h.word_writes {
                for b in a..a + len {
                    new_vals.cover.entry(b).or_default().insert((a, len));
                }
                let e = new_vals.words.entry((a, len)).or_insert_with(|| Some(BTreeSet::new()));
                let before = e.as_ref().map(BTreeSet::len);
                match (e.as_mut(), vs) {
                    (Some(set), Some(vs)) => {
                        set.extend(vs);
                        if set.len() >= MAX_READ_VALUES {
                            *e = None;
                        }
                    }
                    _ => *e = None,
                }
                words_grown |= e.as_ref().map(BTreeSet::len) != before;
            }
            for (b, vs) in h.writes {
                new_writer |= new_writers.entry(b).or_default().insert(tid);
                let e = new_vals.bytes.entry(b).or_insert_with(|| Some(BTreeSet::new()));
                let before = e.as_ref().map(BTreeSet::len);
                match (e.as_mut(), vs) {
                    (Some(set), Some(vs)) => {
                        set.extend(vs);
                        if set.len() > MAX_BYTE_VALUES {
                            *e = None;
                        }
                    }
                    _ => *e = None,
                }
                if e.as_ref().map(BTreeSet::len) != before {
                    grown.insert(b);
                }
            }
        }
        for (b, (_, idle)) in streaks.iter_mut() {
            if !grown.contains(b) {
                *idle += 1;
            }
        }
        streaks.retain(|_, &mut (_, idle)| idle < STREAK_GAP);
        for &b in &grown {
            let (n, idle) = streaks.entry(b).or_insert((0, 0));
            *n += 1;
            *idle = 0;
            if *n >= WIDEN_AFTER && new_vals.bytes.get(&b).is_some_and(|v| v.as_ref().is_some_and(|v| v.len() > WIDEN_MIN_VALUES)) {
                new_vals.bytes.insert(b, None);
            }
        }
        // A footprint with a byte that may hold any value may too, so a
        // counter's whole values stop growing once its bytes are widened.
        for (&(a, len), e) in &mut new_vals.words {
            if e.is_some() && (a..a + len).any(|b| matches!(new_vals.bytes.get(&b), Some(None))) {
                *e = None;
            }
        }
        vals = new_vals;
        writers = new_writers;
        own_writers = new_own_writers;
        if grown.is_empty() && !new_writer && !words_grown {
            for (tid, th) in test.threads.iter().enumerate() {
                let (full, rr, st, acquire) = orderings(&th.code);
                // Whether the thread may order reads by dependencies at all:
                // by an address dependency, or by reading back a write that
                // depends on a read.
                let read_back = dep_writes[tid]
                    .iter()
                    .any(|&(a, len)| red.reads[tid].iter().any(|&(p, m)| a < p + m && p < a + len));
                let deps = if addr_dep[tid] || read_back {
                    // Only reads of other threads' writes matter (as `x` in
                    // `prunable`).
                    let others = |f: &Footprints| {
                        f.iter().any(|&(a, len)| (a..a + len).any(|b| writers.get(&b).is_some_and(|w| w.iter().any(|&t| t != tid))))
                    };
                    let pcs: BTreeSet<usize> = read_pcs[tid].iter().filter(|(_, f)| others(f)).map(|(&pc, _)| pc).collect();
                    dep_orders(test, tid, bound, &vals, &own_writers, &read_bytes[tid], &pcs, lives[tid].as_ref())
                } else {
                    Some(Vec::new())
                };
                red.orders_rr.push(full || rr || deps.is_none());
                red.dep_rr.push(deps.unwrap_or_default());
                red.full.push(full);
                red.acquire.push(acquire);
                red.dmb_st.push(st);
            }
            // Ordering points in program order, and publications.
            for a in 0..n {
                let events = write_events(test, a, bound, &vals, &own_writers, &read_bytes[a], lives[a].as_ref());
                red.write_epochs.push(events.as_ref().map(|es| {
                    let mut m: BTreeMap<(u64, u64), BTreeSet<u8>> = BTreeMap::new();
                    for e in es {
                        m.entry(e.0).or_default().insert(e.2.may);
                    }
                    m
                }));
                let mut pubs = Vec::new();
                let es_fps: Footprints = events.iter().flatten().map(|e| e.0).collect();
                let copies_only = |t: usize, p: (u64, u64)| {
                    let overlap = |f: &(u64, u64)| f.0 < p.0 + p.1 && p.0 < f.0 + f.1;
                    !red.noncopy_writes[t].iter().any(overlap) && red.copy_writes[t].iter().filter(|f| overlap(f)).all(|&f| f == p)
                };
                for (published, protected) in events.map(|es| publications(test, a, &es, &writers, &copies_only)).unwrap_or_default() {
                    let unordered = (0..n)
                        .map(|c| {
                            let reads = c != a && protected.iter().any(|&f| red.reads[c].iter().any(|&(p, m)| p < f.0 + f.1 && f.0 < p + m));
                            reads.then(|| unpublished_reads(test, c, bound, &vals, &own_writers, &read_bytes[c], &published, lives[c].as_ref())).flatten()
                        })
                        .collect();
                    let unprotected = es_fps.difference(&protected).copied().collect();
                    pubs.push(Publication { protected, unprotected, unordered });
                }
                red.publications.push(pubs);
            }
            red.vals = vals;
            red.writers = writers;
            return Some(red);
        }
    }
    None
}
