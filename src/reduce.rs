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
//! What threads may read, and how they may order accesses, is
//! over-approximated before exploring. [`analyze`] runs each thread alone,
//! letting every load return any value that some write (or initial memory)
//! may put in each byte, until the sets of possible values stop growing.
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

use crate::core::{Core, MemOp, RV, Step, StepQueue, VR, View, low_bytes, pair_value, rmw_result, split_pair};
use crate::isa::{Addr, Barrier, Code, Instr, LoadKind, RmwOp, Size};
use crate::litmus::Test;
use crate::promising::Msg;
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
/// A byte whose set of values grows in this many consecutive rounds is
/// taken to hold any value (widening). A counter, such as a sequence
/// number that each round's writes increase, would otherwise add values
/// round after round until it reached `MAX_BYTE_VALUES`, while the runs
/// that read it grew with every value.
const WIDEN_AFTER: usize = 3;

/// Memory footprints: (address, length) pairs.
type Footprints = BTreeSet<(u64, u64)>;

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
    /// Exclusive reads.
    exclusive_reads: Vec<Footprints>,
    /// Acquire reads (including those of atomics).
    acquire_reads: Vec<Footprints>,
    /// Release writes (including those of atomics).
    release_writes: Vec<Footprints>,
    /// Writes of atomics and store-exclusives.
    atomic_writes: Vec<Footprints>,
    /// The thread may order a read before a later read other than by
    /// making the first an acquire: a full barrier, DMB LD, an ISB after a
    /// control dependency, an address dependency, or reading back a write
    /// that depends on the first read.
    orders_rr: Vec<bool>,
    /// The thread has a full barrier or an acquire-release atomic, which
    /// order any earlier write before any later read.
    full: Vec<bool>,
    /// The thread has an acquire read (which orders an earlier release or
    /// atomic write before it).
    acquire: Vec<bool>,
    /// The thread has DMB ST.
    dmb_st: Vec<bool>,
}

impl Reduction {
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
                        && (self.orders_rr[c]
                            || hits(&self.acquire_reads[c], x)
                            || self.reads[c].iter().any(|&(p, n)| {
                                let f = BTreeSet::from([(p, n)]);
                                hits(&f, x) && hits(&f, y)
                            })))
        });
        let internal = if a == b {
            self.write_read(a, x) || self.dmb_st[a] || hits(&self.release_writes[a], y)
        } else {
            self.write_read(a, x) && hits(&self.reads[a], y)
        };
        !via_reader && !internal
    }
}

impl Reduction {
    /// The memory private to each thread, as sorted, disjoint byte ranges:
    /// bytes that no other thread may read or write, and that the thread
    /// accesses only with plain loads and stores. Every access of the
    /// thread, and every location in `observed`, lies either wholly in a
    /// thread's private memory or wholly outside it.
    pub fn private(&self, observed: &[(u64, u64)]) -> Vec<Vec<(u64, u64)>> {
        let n = self.reads.len();
        let bytes = |f: &Footprints| -> BTreeSet<u64> { f.iter().flat_map(|&(a, len)| a..a + len).collect() };
        let accessed: Vec<BTreeSet<u64>> = (0..n).map(|t| &bytes(&self.reads[t]) | &bytes(&self.writes[t])).collect();
        let mut private: Vec<BTreeSet<u64>> = (0..n)
            .map(|t| {
                let mut p = accessed[t].clone();
                for f in [&self.acquire_reads[t], &self.release_writes[t], &self.atomic_writes[t], &self.exclusive_reads[t]] {
                    p.retain(|b| !bytes(f).contains(b));
                }
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
            .flat_map(|t| self.reads[t].iter().chain(&self.writes[t]).copied())
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

/// One thread's run in the analysis: a core, its exclusive monitor, and
/// its own latest writes to the bytes that only it writes.
type HState = (Core, Option<(u64, Size)>, Own);

/// A thread's latest write to each byte that no other thread writes (None:
/// an unknown value). Coherence makes such a byte read exactly as the
/// thread last wrote it: no other write to it exists, and a read cannot
/// return a write that comes later in program order.
type Own = BTreeMap<u64, Option<u8>>;

/// Taint (dependency view) of a value loaded from memory in the analysis.
const LOADED: View = 1;
/// Taint of a value the analysis does not know: anything computed from it
/// is only a stand-in for the possible values.
const UNKNOWN: View = 2;

fn taint(v: RV) -> View {
    v.view.max(v.pick)
}

/// The values a byte may hold besides its initial value (None: any).
type ByteVals = Option<BTreeSet<u8>>;

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
    vals: &'a HashMap<u64, ByteVals>,
    /// The threads that may write each byte (none if absent).
    writers: &'a HashMap<u64, BTreeSet<usize>>,
    /// States the analysis may still visit, over all threads and rounds.
    budget: &'a mut usize,
    reads: Footprints,
    acquire_reads: Footprints,
    release_writes: Footprints,
    atomic_writes: Footprints,
    writes: HashMap<u64, ByteVals>,
    /// Footprints of writes whose address or data depends on a read.
    dep_writes: Footprints,
    /// Some read's address depends on an earlier read.
    addr_dep: bool,
    /// Footprints of all writes, and of exclusive reads.
    write_fps: Footprints,
    exclusive_reads: Footprints,
}

impl Havoc<'_> {
    /// A read of `n` bytes at `addr`: every value it may return, with its
    /// taint; or one stand-in value tainted `UNKNOWN`, if some byte may hold
    /// any value or there are too many combinations.
    fn read(&mut self, own: &Own, addr: RV, n: u64, acquire: bool) -> Vec<(u128, View)> {
        let a = addr.val;
        self.reads.insert((a, n));
        if acquire {
            self.acquire_reads.insert((a, n));
        }
        self.addr_dep |= addr.view != 0;
        let mut out = vec![0u128];
        for b in (a..a + n).rev() {
            let init = self.test.init_byte(b);
            if self.own_only(b) {
                match own.get(&b) {
                    Some(None) => return vec![(self.stand_in(a, n), UNKNOWN)],
                    Some(Some(v)) => out.iter_mut().for_each(|x| *x = (*x << 8) | *v as u128),
                    None => out.iter_mut().for_each(|x| *x = (*x << 8) | init as u128),
                }
                continue;
            }
            let mut byte = match self.vals.get(&b) {
                None => BTreeSet::new(),
                Some(Some(v)) => v.clone(),
                Some(None) => return vec![(self.stand_in(a, n), UNKNOWN)],
            };
            byte.insert(init);
            if out.len() * byte.len() > MAX_READ_VALUES {
                return vec![(self.stand_in(a, n), UNKNOWN)];
            }
            out = out.iter().flat_map(|v| byte.iter().map(move |&x| (v << 8) | x as u128)).collect();
        }
        out.into_iter().map(|v| (v, LOADED)).collect()
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
        let a = addr.val;
        self.write_fps.insert((a, n));
        for i in 0..n {
            if self.own_only(a + i) {
                own.insert(a + i, (data < UNKNOWN).then_some((v >> (8 * i)) as u8));
            }
        }
        if addr.view != 0 || data != 0 {
            self.dep_writes.insert((a, n));
        }
        for i in 0..n {
            let e = self.writes.entry(a + i).or_insert_with(|| Some(BTreeSet::new()));
            match e {
                Some(set) if data < UNKNOWN => {
                    set.insert((v >> (8 * i)) as u8);
                }
                _ => *e = None,
            }
        }
    }

    /// Explore every run of the thread. None if the analysis gives up.
    fn run(&mut self, start: Core) -> Option<()> {
        let mut visited: HashMap<HState, u32> = HashMap::new();
        let mut queue = StepQueue::new();
        queue.push(start.steps, (start, None, Own::new()));
        while let Some(st) = queue.pop() {
            let mut key = st.clone();
            key.0.steps = 0;
            match visited.get(&key) {
                Some(&n) if n <= st.0.steps => continue,
                _ => {
                    visited.insert(key, st.0.steps);
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
    fn successors(&mut self, (mut core, mon, mut own): HState) -> Option<Vec<HState>> {
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
            match core.step(prog) {
                Step::Done | Step::Fault(_) => return Some(vec![]),
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
                    return Some(vec![(core, mon, own.clone()), (alt, mon, own)]);
                }
                Step::Local { .. } if core.steps >= self.fuel => return Some(vec![(core, mon, own)]),
                Step::Local { .. } => {}
                Step::Mem(op) => break op,
            }
        };
        let mut out = Vec::new();
        let mut push = |mut c: Core, m: Option<(u64, Size)>, o: Own| {
            c.retire(prog);
            out.push((c, m, o));
        };
        match op {
            MemOp::Load { addr, rt, size, signed, excl, kind } => {
                if excl {
                    self.exclusive_reads.insert((addr.val, size.bytes()));
                }
                for (v, t) in self.read(&own, addr, size.bytes(), kind != LoadKind::Plain) {
                    let mut c = core.clone();
                    c.set(rt, reg(size.extend(v as u64, signed), t));
                    push(c, if excl { Some((addr.val, size)) } else { mon }, own.clone());
                }
            }
            MemOp::LoadExclPair { addr, rt, rt2, size, acquire } => {
                let total = size.double();
                self.exclusive_reads.insert((addr.val, total.bytes()));
                for (v, t) in self.read(&own, addr, total.bytes(), acquire) {
                    let (lo, hi) = split_pair(v, size);
                    let mut c = core.clone();
                    c.set(rt, reg(lo, t));
                    c.set(rt2, reg(hi, t));
                    push(c, Some((addr.val, total)), own.clone());
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
                self.write(&mut own, addr, n, size.mask(data.val) as u128, taint(data));
                self.write(&mut own, RV { val: addr.val + n, ..addr }, n, size.mask(data2.val) as u128, taint(data2));
                push(core, mon, own);
            }
            MemOp::VStore { addr, data, data2, size } => {
                let n = size.bytes();
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
                if mon == Some((addr.val, size)) {
                    let mut o = own.clone();
                    self.write(&mut o, addr, size.bytes(), size.mask(data.val) as u128, taint(data));
                    let mut c = core.clone();
                    c.set(rs, status(0));
                    push(c, None, o);
                }
                core.set(rs, status(1));
                push(core, None, own);
            }
            MemOp::StoreExclPair { addr, data, data2, rs, size, release } => {
                let total = size.double();
                self.atomic_writes.insert((addr.val, total.bytes()));
                if release {
                    self.release_writes.insert((addr.val, total.bytes()));
                }
                if mon == Some((addr.val, total)) {
                    let t = taint(data).max(taint(data2));
                    let mut o = own.clone();
                    self.write(&mut o, addr, total.bytes(), pair_value(data.val, data2.val, size), t);
                    let mut c = core.clone();
                    c.set(rs, status(0));
                    push(c, None, o);
                }
                core.set(rs, status(1));
                push(core, None, own);
            }
            MemOp::Rmw { addr, op, operand, cmp, rt, size, acquire, release, .. } => {
                let n = size.bytes();
                self.atomic_writes.insert((addr.val, n));
                if release {
                    self.release_writes.insert((addr.val, n));
                }
                for (old, t) in self.read(&own, addr, n, acquire) {
                    let old = old as u64;
                    let mut o = own.clone();
                    let known = t < UNKNOWN && cmp.is_none_or(|c| taint(c) < UNKNOWN);
                    if known {
                        if let Some(new) = rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                            // The new value may depend on the old one.
                            self.write(&mut o, addr, n, new as u128, taint(operand).max(LOADED));
                        }
                    } else if cmp.is_some() || op == RmwOp::Swp {
                        // A CAS may succeed (or fail); either way only the
                        // operand can be written.
                        self.write(&mut o, addr, n, size.mask(operand.val) as u128, taint(operand).max(LOADED));
                    } else {
                        self.write(&mut o, addr, n, 0, UNKNOWN);
                    }
                    let mut c = core.clone();
                    c.set(rt, reg(old, t));
                    push(c, mon, o);
                }
            }
            MemOp::Barrier(_) => push(core, mon, own),
            MemOp::Clrex => push(core, None, own),
        }
        Some(out)
    }
}

/// Compute what the reduction needs for `test`, or None if the analysis
/// gives up (then nothing should be pruned).
pub fn analyze(test: &Test, fuel: u32) -> Option<Reduction> {
    let n = test.threads.len();
    let mut vals: HashMap<u64, ByteVals> = HashMap::new();
    let mut red = Reduction {
        reads: vec![Footprints::new(); n],
        writes: vec![Footprints::new(); n],
        exclusive_reads: vec![Footprints::new(); n],
        acquire_reads: vec![Footprints::new(); n],
        release_writes: vec![Footprints::new(); n],
        atomic_writes: vec![Footprints::new(); n],
        ..Reduction::default()
    };
    let mut dep_writes = vec![Footprints::new(); n];
    let mut addr_dep = vec![false; n];
    let mut budget = MAX_STATES;
    // Consecutive rounds in which each byte's values have grown.
    let mut streaks: HashMap<u64, usize> = HashMap::new();
    let mut writers: HashMap<u64, BTreeSet<usize>> = HashMap::new();
    for _ in 0..MAX_ROUNDS {
        let mut grown: BTreeSet<u64> = BTreeSet::new();
        let mut new_vals = vals.clone();
        let mut new_writers = writers.clone();
        let mut new_writer = false;
        for (tid, th) in test.threads.iter().enumerate() {
            let mut h = Havoc {
                test,
                prog: &th.code,
                tid,
                fuel,
                vals: &vals,
                writers: &writers,
                budget: &mut budget,
                reads: Footprints::new(),
                write_fps: Footprints::new(),
                exclusive_reads: Footprints::new(),
                acquire_reads: Footprints::new(),
                release_writes: Footprints::new(),
                atomic_writes: Footprints::new(),
                writes: HashMap::new(),
                dep_writes: Footprints::new(),
                addr_dep: false,
            };
            h.run(Core::new(test.init_regs[tid], tid, n, th.entry))?;
            red.reads[tid].append(&mut h.reads);
            red.writes[tid].append(&mut h.write_fps);
            red.exclusive_reads[tid].append(&mut h.exclusive_reads);
            red.acquire_reads[tid].append(&mut h.acquire_reads);
            red.release_writes[tid].append(&mut h.release_writes);
            red.atomic_writes[tid].append(&mut h.atomic_writes);
            dep_writes[tid].append(&mut h.dep_writes);
            addr_dep[tid] |= h.addr_dep;
            for (b, vs) in h.writes {
                new_writer |= new_writers.entry(b).or_default().insert(tid);
                let e = new_vals.entry(b).or_insert_with(|| Some(BTreeSet::new()));
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
        streaks.retain(|b, _| grown.contains(b));
        for &b in &grown {
            let n = streaks.entry(b).or_insert(0);
            *n += 1;
            if *n >= WIDEN_AFTER {
                new_vals.insert(b, None);
            }
        }
        vals = new_vals;
        writers = new_writers;
        if grown.is_empty() && !new_writer {
            for (tid, th) in test.threads.iter().enumerate() {
                let (full, rr, st, acquire) = orderings(&th.code);
                // A read of a write that depends on an earlier read is ordered
                // after that read.
                let read_back = dep_writes[tid]
                    .iter()
                    .any(|&(a, len)| red.reads[tid].iter().any(|&(p, m)| a < p + m && p < a + len));
                red.orders_rr.push(full || rr || addr_dep[tid] || read_back);
                red.full.push(full);
                red.acquire.push(acquire);
                red.dmb_st.push(st);
            }
            return Some(red);
        }
    }
    None
}
