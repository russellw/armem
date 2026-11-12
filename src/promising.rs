//! The ARMv8 memory model, in the operational "Promising-ARM" style
//! (Pulte, Pichon-Pharabod, Kang, Lee, Hur: "Promising-ARM/RISC-V: a
//! simpler and faster operational concurrency model", PLDI 2019).
//!
//! Memory is a list of write messages; a message's timestamp is its
//! 1-based position in the list (0 denotes the initial value of all of
//! memory). Each thread keeps *views* (timestamps) that bound which
//! messages it may still read and where its writes may be placed.
//! Out-of-order writes are modeled by *promises*: a thread may append a
//! write to memory early, provided it can later fulfill it (certification).
//!
//! Exploration uses the promise-first strategy: in phase 1 threads make
//! promises (each one certified by a solo run of the promising thread);
//! in phase 2, with memory fixed, every thread runs independently and must
//! fulfill exactly its promises. Every behavior of the model is reachable
//! this way, and in phase 2 the threads cannot affect each other, so the
//! final states are simply the product of each thread's final states.
//!
//! Mixed-size accesses: memory is byte-addressed. A message has a
//! footprint (address and size), and coherence and forwarding are tracked
//! per byte. An aligned read is single-copy atomic: relative to other
//! threads it takes effect at one point of the memory order, and each byte
//! reads the latest write by another thread covering it at that point (or a
//! later write of the thread's own), so no aligned write is ever seen torn.
//! An unaligned read behaves as independent byte reads. (This mixed-size
//! extension is ours; it is not part of the published Promising-ARM model,
//! but it agrees with Arm's axiomatic model in herd7.)
//!
//! Dependencies follow Arm's model: registers carry a plain view (address,
//! data and ordinary control dependencies) and a *pick* view that also
//! includes dependencies through conditional selects and CAS comparisons.
//! Pick dependencies order later writes, but not later reads.

use crate::bytemap::ByteMap;
use crate::core::{Core, MemOp, RV, Step, StepQueue, VR, View, low_bytes, pair_value, rmw_result, split_pair};
use crate::hash::{FxHashMap, FxHashSet};
use crate::isa::{Barrier, Code, Instr, LoadKind, RmwOp, Size};
use crate::litmus::{Obs, Test};
use crate::Options;
use crate::outcome::{Outcomes, Projection};
use std::collections::hash_map::Entry;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Msg {
    pub addr: u64,
    pub size: Size,
    /// Little-endian contents (up to 16 bytes).
    pub val: u128,
    pub tid: usize,
}

impl Msg {
    fn covers(&self, b: u64) -> bool {
        b >= self.addr && b < self.addr + self.size.bytes()
    }
    fn byte(&self, b: u64) -> u8 {
        (self.val >> (8 * (b - self.addr))) as u8
    }
}


#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Fwd {
    ts: View,
    /// Address and data dependencies of the write.
    view: View,
    /// The same, including pick dependencies.
    pick: View,
    /// For the write of an exclusive pair or atomic: the view of its read.
    /// An acquire read of this write is ordered after that read (Arm's
    /// `[M]; rmw; lrs; [A|Q]`), though not after the write itself.
    xview: View,
    /// The write's address and value (determined by `ts`, except for the
    /// new writes of `Mode::Observe`, which all have the timestamp
    /// `FUTURE`).
    addr: u64,
    val: u128,
    /// For a cut observation: the value is poisoned (see `TState::poison`).
    poison: bool,
}

/// Exclusive monitor set by a load-exclusive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Monitor {
    addr: u64,
    size: Size,
    /// Timestamp each byte was read from.
    srcs: Vec<View>,
    /// The view of the read.
    view: View,
    /// The value read (for `Options::stutter`).
    val: u128,
}

/// State of one thread under the Promising-ARM model.
///
/// Views ending in `_p` include pick dependencies. Following Arm's model,
/// pick dependencies order later writes (and anything after an ISB) but not
/// later reads, so the views constraining reads (`v_rnew`) exclude them and
/// those constraining writes (`v_wnew`, `v_cap`) include them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TState {
    core: Core,
    /// Reads so far (excluding NoRet atomics' reads).
    v_rold: View,
    v_rold_p: View,
    /// Reads of NoRet atomics so far (ordered by DMB SY but not DMB LD).
    v_rnoret: View,
    v_rnoret_p: View,
    /// Writes so far.
    v_wold: View,
    /// Lower bounds for future reads / writes.
    v_rnew: View,
    v_wnew: View,
    /// Control and address dependencies of everything so far (with picks).
    v_cap: View,
    /// Latest release write (orders later LDAR, which is RCsc).
    v_rel: View,
    /// Per-byte coherence view.
    coh: ByteMap<View>,
    /// Forwarding bank: latest own write per byte.
    fwd: ByteMap<Fwd>,
    xcl: Option<Monitor>,
    /// Outstanding (unfulfilled) promises.
    promises: BTreeSet<View>,
    /// A `CAS` has failed on a later write by another thread (see
    /// `Solo::cas_fails_later`): the run may certify promises, but does not
    /// complete an execution.
    speculative: bool,
    /// For a cut observation (see `Explorer::observe_cut`): the registers
    /// (`live::Mask` bits) holding a value the run does not know, read from
    /// a later write that the analysis does not know (or computed from
    /// one). Instructions that read them are executed abstractly
    /// (`Abstract`).
    poison: crate::live::Mask,
    /// Store-exclusives that failed although they could have succeeded
    /// (counted only with `Options::spurious`).
    spurious: u32,
    /// Read-modify-writes that wrote back the value they read (counted only
    /// with `Options::stutter`).
    stutters: u32,
}

impl TState {
    fn new(regs: [u64; 31], tid: usize, threads: usize, entry: usize) -> TState {
        TState {
            core: Core::new(regs, tid, threads, entry),
            v_rold: 0,
            v_rold_p: 0,
            v_rnoret: 0,
            v_rnoret_p: 0,
            v_wold: 0,
            v_rnew: 0,
            v_wnew: 0,
            v_cap: 0,
            v_rel: 0,
            coh: ByteMap::new(),
            fwd: ByteMap::new(),
            xcl: None,
            promises: BTreeSet::new(),
            speculative: false,
            poison: 0,
            spurious: 0,
            stutters: 0,
        }
    }

    /// Mark register `r` as poisoned (see `poison`) if `on`.
    fn poison_reg(&mut self, r: crate::isa::RegOp, on: bool) {
        if on && r.idx < 31 {
            self.poison |= 1 << r.idx;
        }
    }

    fn coh(&self, b: u64) -> View {
        self.coh.get(b).unwrap_or(0)
    }

    /// The greatest coherence view of the bytes `a..a + n`.
    fn coh_max(&self, a: u64, n: u64) -> View {
        self.coh.values(a, a + n).max().unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Explore every run of the thread, and record:
    /// - the writes it might promise: every newly appended write. Up to the
    ///   point where a promisable write executes, this exploration is at
    ///   least as permissive as any run certifying that promise, so no
    ///   candidate is missed;
    /// - its final states (and faults) with the memory as it is: those of
    ///   the runs that fulfill all its promises and append no new write;
    /// - which candidates could certify (see `Solo::promising`).
    Collect,
    /// Search for a run that fulfills all outstanding promises; new writes
    /// may be appended to (a local copy of) memory.
    Certify,
    /// Explore every run of the thread in any execution whose memory
    /// extends this one (`.., x`), and record its accesses ordered after
    /// `x` (see `reduce::Observed`). Besides the messages of the memory and
    /// its own new writes, a read may return a later write by another
    /// thread: any value the reduction's analysis found such a write may
    /// store, with the timestamp `FUTURE`. That stands for every timestamp
    /// after the next message (`y`), and is above them all, so views that
    /// include it are at least the real ones; this allows at least the
    /// reads, fulfillments and writes the real ones would (a read with such
    /// a pre-view may still read the latest write in the memory), and drops
    /// a state only if a promise can no longer be fulfilled, which then
    /// holds for the real views too. Runs need not fulfill their promises,
    /// but those that can no longer do so are dropped (`doomed`): every
    /// execution fulfills them.
    Observe,
}

/// The timestamp of every later write by another thread (`Mode::Observe`).
const FUTURE: View = View::MAX / 2;

/// Most states `Explorer::observe` may visit for one memory, as a multiple
/// of the states of its `Mode::Collect` runs, and at least and at most.
/// Observing pays only when it prunes more than it costs, and on memories
/// whose observing explorations are far larger than their `Collect` runs
/// (as in ck's spinlocks, where a thread spins on values from later
/// writes) it seldom does.
const OBSERVE_STATES_PER_COLLECT: u64 = 10;
const MIN_OBSERVE_STATES: u64 = 1_000;
const MAX_OBSERVE_STATES: u64 = 200_000;

/// Fewest states `Explorer::observe_cut` may visit for one memory. On the
/// shallow memories of `ck_fifo_mpmc`, the cut takes up to about 7,000
/// states, many times its `Collect` runs; elsewhere, an observation that
/// needs more seldom prunes enough to pay for itself.
const MIN_CUT_STATES: u64 = 20_000;

/// Most values a read of later writes may return in `Mode::Observe`, where
/// the analysis does not know them, before it gives up.
const MAX_UNKNOWN_VALUES: usize = 4096;

/// Most rounds of `Explorer::observe` before it gives up. With values from
/// the parent memory, nearly every observation that converges does so in
/// one or two rounds; the rest are mostly counters that grow every round.
const MAX_OBSERVE_ROUNDS: usize = 3;

/// Values written, by footprint (address, length).
type Written = FxHashMap<(u64, u64), BTreeSet<u128>>;

/// How a read orders later accesses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ReadKind {
    load: LoadKind,
    /// A NoRet atomic's read (its value is discarded).
    noret: bool,
}

impl ReadKind {
    fn plain() -> ReadKind {
        ReadKind { load: LoadKind::Plain, noret: false }
    }
    fn of(load: LoadKind) -> ReadKind {
        ReadKind { load, noret: false }
    }
}

/// How a write is ordered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct WriteKind {
    release: bool,
    /// For the write of an exclusive pair or atomic: the view of its read.
    xview: View,
    /// The write of an acquire+release atomic, which orders everything after it.
    full: bool,
}

/// Outcome of one way of performing a read.
struct ReadResult {
    state: TState,
    val: u128,
    srcs: Vec<View>,
    /// Dependency views of the value read.
    dep: RV,
    /// For a cut observation: bit `i` is set if byte `i` of the value is
    /// unknown (read from a later write that the analysis does not know).
    poison: u16,
}

/// A candidate write to promise: (address, size, value).
type Candidate = (u64, Size, u128);

/// Exhaustive exploration of one thread running alone against a memory.
struct Solo<'a> {
    test: &'a Test,
    prog: &'a Code,
    tid: usize,
    base: Vec<Msg>,
    fuel: u32,
    lse2: bool,
    /// See `Options::spurious` and `Options::stutter`, and whether a run
    /// reached either limit.
    spurious: Option<u32>,
    stutter: Option<u32>,
    capped: std::cell::Cell<bool>,
    mode: Mode,
    /// Visited states (with the step counter zeroed, and dead registers
    /// cleared if `live` is given) -> fewest steps seen.
    visited: FxHashMap<(TState, Vec<Msg>), u32>,
    live: Option<Arc<crate::live::Liveness>>,
    /// A return the liveness did not expect was reached: the results are
    /// void, and the run must be repeated without it.
    live_failed: bool,
    candidates: BTreeSet<Candidate>,
    /// For `Mode::Observe`: the analysis, and what it found (reads in a
    /// `RefCell`, as `do_read` takes `&self`); set if it gave up.
    reduction: Option<Arc<crate::reduce::Reduction>>,
    observed_reads: std::cell::RefCell<crate::reduce::Footprints>,
    observed_writes: crate::reduce::Footprints,
    /// For `Mode::Observe`: the values other threads' later writes may
    /// store, for bytes whose values the analysis does not know (see
    /// `Explorer::observe`); whether a read used them; and the values of
    /// this thread's new writes.
    unknown: Arc<Written>,
    used_unknown: std::cell::Cell<bool>,
    written: Written,
    gave_up: std::cell::Cell<bool>,
    /// For `Mode::Observe`: whether to cut off the states past `x`, with
    /// unknown values of later writes poisoned instead of taken from
    /// `unknown` (see `Explorer::observe_cut`); and whether a run needed a
    /// poisoned value before it got past `x` (`Abstract::Fallback`), which
    /// voids the results.
    cut: bool,
    poison_used: std::cell::Cell<bool>,
    /// Whether a run was cut off, and its reads of `x` recorded.
    cut_reads: bool,
    /// Whether the store being performed writes a poisoned value.
    write_poison: bool,
    /// For `Mode::Observe`: most states before it gives up.
    max_states: u64,
    states: u64,
    /// Candidates written, in some run, where a promise right after `base`
    /// could be fulfilled: see `promising`.
    placeable: FxHashSet<Candidate>,
    /// Writes appended by some run that then has no outstanding promise.
    fulfilling: FxHashSet<Candidate>,
    finals: BTreeSet<Vec<u64>>,
    final_regs: &'a [u8],
    /// Observed locations private to the thread (see `Projection`).
    final_mems: &'a [u64],
    certified: bool,
    truncated: bool,
    /// Faults reached (in `Collect` mode, with every promise fulfilled and no
    /// new write).
    faults: BTreeSet<String>,
    /// Whether some state reached (in `Collect` mode) has written every one
    /// of the thread's messages in the memory and promises nothing more: the
    /// thread can stop there, so another thread's fault is real.
    fulfills: bool,
}

/// Split an access of `n` bytes at `a` into single-copy-atomic pieces of
/// (up to) `granule` bytes. A piece that is not aligned to `granule` is
/// split into bytes, unless FEAT_LSE2 applies and it stays within an
/// aligned 16-byte block.
fn pieces(a: u64, n: u64, granule: u64, lse2: bool) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut p = a;
    while p < a + n {
        let len = granule.min(a + n - p);
        if p % granule == 0 || (lse2 && p / 16 == (p + len - 1) / 16) {
            out.push((p, len));
            p += len;
        } else {
            out.push((p, 1));
            p += 1;
        }
    }
    out
}

/// A successor state, with any messages newly appended to memory.
type Succ = (TState, Vec<Msg>);

/// How a cut observation executes an instruction that reads a poisoned
/// register (see `TState::poison`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Abstract {
    /// It reads none.
    Clean,
    /// An integer operation: whatever it writes is poisoned.
    Data,
    /// Its outcome depends on the flags alone, which are poisoned: try
    /// each value (`Solo::forked_step`).
    Flags,
    /// A branch on a poisoned register: try both ways.
    Reg(u8),
    /// A store of a poisoned value (its address is clean): the write is
    /// poisoned (`Solo::write_poison`).
    Store,
    /// Anything else: the observation needs the values (see `Cut::Poisoned`).
    Fallback,
}

/// How to execute `i`, which reads the poisoned registers `p`.
fn abstract_step(i: &Instr, p: crate::live::Mask) -> Abstract {
    let flags_only = p & !crate::live::FLAGS == 0;
    let addr_clean = |a: &crate::isa::Addr| {
        let mut m = if a.base.idx < 31 { 1 << a.base.idx } else { 0 };
        if let crate::isa::Offset::Reg(r, _) = a.offset {
            if r.idx < 31 {
                m |= 1 << r.idx;
            }
        }
        m & p == 0
    };
    match *i {
        Instr::Cbz { rt, .. } | Instr::Tbz { rt, .. } => Abstract::Reg(rt.idx),
        Instr::BCond { .. } => Abstract::Flags,
        Instr::Csel { .. } | Instr::Adc { .. } | Instr::Ccmp { .. } if flags_only => Abstract::Flags,
        Instr::MovImm { .. }
        | Instr::Movk { .. }
        | Instr::MovReg { .. }
        | Instr::Alu { .. }
        | Instr::Csel { .. }
        | Instr::Bitfield { .. }
        | Instr::Madd { .. }
        | Instr::Unary { .. }
        | Instr::Extr { .. }
        | Instr::Adc { .. }
        | Instr::Mulh { .. }
        | Instr::Ccmp { .. }
            if !i.writes().contains(&crate::isa::SP) =>
        {
            Abstract::Data
        }
        Instr::Store { addr, .. }
        | Instr::StorePair { addr, .. }
        | Instr::StoreExcl { addr, .. }
        | Instr::StoreExclPair { addr, .. }
            if addr_clean(&addr) =>
        {
            Abstract::Store
        }
        _ => Abstract::Fallback,
    }
}

/// The address of a store.
fn store_addr(op: MemOp) -> u64 {
    match op {
        MemOp::Store { addr, .. }
        | MemOp::StorePair { addr, .. }
        | MemOp::StoreExcl { addr, .. }
        | MemOp::StoreExclPair { addr, .. } => addr.val,
        _ => 0,
    }
}

/// The dependency views of a register value read from memory.
fn dep(view: View, pick: View) -> RV {
    RV { val: 0, view, pick: pick.max(view) }
}

impl<'a> Solo<'a> {
    fn new(
        test: &'a Test,
        cfg: &Options,
        proj: &'a Projection,
        tid: usize,
        base: Vec<Msg>,
        mode: Mode,
        live: Option<Arc<crate::live::Liveness>>,
    ) -> Solo<'a> {
        Solo {
            live,
            live_failed: false,
            test,
            prog: &test.threads[tid].code,
            tid,
            base,
            fuel: cfg.fuel,
            lse2: cfg.lse2,
            spurious: cfg.spurious,
            stutter: cfg.stutter,
            capped: Default::default(),
            mode,
            visited: FxHashMap::default(),
            candidates: BTreeSet::new(),
            reduction: None,
            observed_reads: Default::default(),
            observed_writes: Default::default(),
            unknown: Default::default(),
            used_unknown: Default::default(),
            written: Default::default(),
            gave_up: Default::default(),
            cut: false,
            poison_used: Default::default(),
            cut_reads: false,
            write_poison: false,
            max_states: MAX_OBSERVE_STATES,
            states: 0,
            placeable: FxHashSet::default(),
            fulfilling: FxHashSet::default(),
            finals: BTreeSet::new(),
            final_regs: &proj.thread_regs[tid],
            final_mems: &proj.thread_mems[tid],
            certified: false,
            truncated: false,
            faults: BTreeSet::new(),
            fulfills: false,
        }
    }

    fn msg<'b>(&'b self, ext: &'b [Msg], t: View) -> &'b Msg {
        let t = t as usize - 1;
        if t < self.base.len() { &self.base[t] } else { &ext[t - self.base.len()] }
    }

    fn len(&self, ext: &[Msg]) -> View {
        (self.base.len() + ext.len()) as View
    }

    fn byte_value(&self, ext: &[Msg], b: u64, t: View) -> u8 {
        if t == 0 { self.test.init_byte(b) } else { self.msg(ext, t).byte(b) }
    }

    /// Is message `t` (0 = initial memory) written by another thread?
    fn external(&self, ext: &[Msg], t: View) -> bool {
        t == 0 || t != FUTURE && self.msg(ext, t).tid != self.tid
    }

    /// Timestamps byte `b` may be read from, given the lower bound `bound`:
    /// the last write covering `b` at or before `bound`, or any later one.
    fn byte_choices(&self, ext: &[Msg], b: u64, bound: View) -> Vec<View> {
        let n = self.len(ext);
        let last = (1..=bound.min(n)).rev().find(|&t| self.msg(ext, t).covers(b)).unwrap_or(0);
        let mut out = vec![last];
        out.extend((bound + 1..=n).filter(|&t| self.msg(ext, t).covers(b)));
        out
    }

    /// The first external write covering `b` after timestamp `t`.
    fn next_external(&self, ext: &[Msg], b: u64, t: View) -> Option<View> {
        (t + 1..=self.len(ext)).find(|&u| self.external(ext, u) && self.msg(ext, u).covers(b))
    }

    /// The latest external write (or 0 for initial memory) covering `b` at
    /// or before `t`.
    fn latest_external(&self, ext: &[Msg], b: u64, t: View) -> View {
        (1..=t).rev().find(|&u| self.external(ext, u) && self.msg(ext, u).covers(b)).unwrap_or(0)
    }

    /// Possible per-byte sources for a single-copy-atomic read of bytes
    /// `p..p+len`, each byte being allowed the sources in `cands`.
    ///
    /// Relative to other threads the read takes effect at one point `T` of
    /// the memory order: a byte read from an external write (or initial
    /// memory) reads the latest external write covering it at `T`, and a
    /// byte read from one of the thread's own writes must not have been
    /// overwritten externally by `T`. (Own writes impose no such
    /// constraint: in Arm's model, reading from them does not order the read.)
    fn atomic_choices(&self, ext: &[Msg], p: u64, len: u64, cands: &[Vec<View>]) -> Vec<Vec<View>> {
        if len == 1 {
            return cands[0].iter().map(|&t| vec![t]).collect();
        }
        let mut points = vec![0];
        points.extend((1..=self.len(ext)).filter(|&t| {
            let m = self.msg(ext, t);
            self.external(ext, t) && m.addr < p + len && p < m.addr + m.size.bytes()
        }));
        let mut out: Vec<Vec<View>> = Vec::new();
        for big_t in points {
            let mut combos: Vec<Vec<View>> = vec![Vec::new()];
            for (i, c) in cands.iter().enumerate() {
                let b = p + i as u64;
                let le = self.latest_external(ext, b, big_t);
                let opts: Vec<View> = c
                    .iter()
                    .copied()
                    .filter(|&t| {
                        if self.external(ext, t) {
                            t == le
                        } else {
                            self.next_external(ext, b, t).is_none_or(|u| u > big_t)
                        }
                    })
                    .collect();
                combos = combos
                    .iter()
                    .flat_map(|c| {
                        opts.iter().map(move |&t| {
                            let mut c2 = c.clone();
                            c2.push(t);
                            c2
                        })
                    })
                    .collect();
            }
            for c in combos {
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        out
    }

    /// Perform a read of `size` bytes at `addr.val`; one result per allowed
    /// choice of source writes. The read consists of single-copy-atomic
    /// pieces of `granule` bytes (or single bytes, where a piece is not
    /// aligned).
    fn do_read(&self, st: &TState, ext: &[Msg], addr: RV, kind: ReadKind, size: Size, granule: u64) -> Vec<ReadResult> {
        let a = addr.val;
        let n = size.bytes();
        let acquire = kind.load != LoadKind::Plain;
        let mut vpre = addr.view.max(st.v_rnew);
        if kind.load == LoadKind::Acquire {
            vpre = vpre.max(st.v_rel);
        }
        let mut combos: Vec<Vec<View>> = vec![Vec::new()];
        for (p, len) in pieces(a, n, granule, self.lse2) {
            // Reading one of our own unfulfilled promises makes it
            // unfulfillable (coherence would then require it to be later
            // than itself), so those are never candidates.
            let cands: Vec<Vec<View>> = (p..p + len)
                .map(|b| {
                    // In `Mode::Observe`, a byte the thread has written
                    // since is read from that write (or, in `observe_read`,
                    // a later one by another thread).
                    if st.fwd.get(b).is_some_and(|f| f.ts == FUTURE) {
                        return vec![FUTURE];
                    }
                    let mut c = self.byte_choices(ext, b, vpre.max(st.coh(b)));
                    c.retain(|t| !st.promises.contains(t));
                    c
                })
                .collect();
            let choices = self.atomic_choices(ext, p, len, &cands);
            combos = combos
                .iter()
                .flat_map(|c| {
                    choices.iter().map(move |t| {
                        let mut c2 = c.clone();
                        c2.extend_from_slice(t);
                        c2
                    })
                })
                .collect();
        }

        let mut out = Vec::new();
        for srcs in combos {
            let mut val = 0u128;
            let mut rview = 0;
            let mut rpick = 0;
            let mut poison = 0u16;
            for (i, &t) in srcs.iter().enumerate().rev() {
                let b = a + i as u64;
                let byte = match st.fwd.get(b) {
                    Some(f) if t == FUTURE => {
                        if f.poison {
                            poison |= 1 << i;
                        }
                        (f.val >> (8 * (b - f.addr))) as u8
                    }
                    _ => self.byte_value(ext, b, t),
                };
                val = (val << 8) | byte as u128;
                match st.fwd.get(b) {
                    Some(f) if f.ts == t => {
                        rview = rview.max(f.view);
                        if acquire {
                            rview = rview.max(f.xview);
                        }
                        rpick = rpick.max(f.pick);
                    }
                    _ => rview = rview.max(t),
                }
            }
            let vpost = vpre.max(rview);
            let ppost = vpost.max(rpick).max(addr.pick);
            let mut s = st.clone();
            s.coh.update(a, a + n, |v| Some(v.unwrap_or(0).max(vpost)));
            if kind.noret {
                s.v_rnoret = s.v_rnoret.max(vpost);
                s.v_rnoret_p = s.v_rnoret_p.max(ppost);
            } else {
                s.v_rold = s.v_rold.max(vpost);
                s.v_rold_p = s.v_rold_p.max(ppost);
            }
            if acquire {
                s.v_rnew = s.v_rnew.max(vpost);
                s.v_wnew = s.v_wnew.max(ppost);
            }
            s.v_cap = s.v_cap.max(addr.pick);
            out.push(ReadResult { state: s, val, srcs, dep: dep(vpost, ppost), poison });
        }
        if self.mode == Mode::Observe {
            let ps = pieces(a, n, granule, self.lse2);
            self.observe_read(st, addr, kind, n, &ps, vpre, &mut out);
        }
        out
    }

    /// For `Mode::Observe`: record the reads ordered after `x` (the last
    /// message of `base`) among `out`, the results of a read of `n` bytes
    /// (in single-copy-atomic pieces `ps`) with pre-view `vpre`; and add
    /// those that read a later write by another thread.
    #[allow(clippy::too_many_arguments)]
    fn observe_read(&self, st: &TState, addr: RV, kind: ReadKind, n: u64, ps: &[(u64, u64)], vpre: View, out: &mut Vec<ReadResult>) {
        let single = ps.len() == 1;
        let a = addr.val;
        let l = self.base.len() as View;
        let x = &self.base[l as usize - 1];
        let mut record = false;
        for r in out.iter() {
            record |= (vpre == l || r.srcs.contains(&l)) && r.srcs.iter().any(|&t| t <= l);
        }
        let Some(red) = &self.reduction else { return };
        let vals: Vec<(u128, u16)> = match red.future_values(self.tid, a, n, &|b| self.test.init_byte(b)) {
            crate::reduce::Future::None => Vec::new(),
            crate::reduce::Future::Unknown if self.cut => match self.poisoned_read_values(a, ps, out, red) {
                Some(v) => v,
                None => {
                    self.gave_up.set(true);
                    return;
                }
            },
            crate::reduce::Future::Unknown => {
                self.used_unknown.set(true);
                match self.unknown_read_values(a, ps, out, red) {
                    Some(v) => v.into_iter().map(|v| (v, 0)).collect(),
                    None => {
                        self.gave_up.set(true);
                        return;
                    }
                }
            }
            crate::reduce::Future::Values(v) => v.into_iter().map(|v| (v, 0)).collect(),
        };
        // A read taking some bytes from later writes and some from the
        // memory is represented by a read of later writes only (its views
        // are the same), so it is recorded if it might read `x` or is
        // ordered after it.
        let mixed_x = x.addr < a + n && a < x.addr + x.size.bytes() && (!single || red.may_mix(self.tid, a, n));
        if !vals.is_empty() && (vpre == l || mixed_x) {
            record = true;
        }
        if record {
            self.observed_reads.borrow_mut().insert((a, n));
        }
        let acquire = kind.load != LoadKind::Plain;
        for (val, poison) in vals {
            let (vpost, ppost) = (FUTURE, FUTURE);
            let mut s = st.clone();
            s.coh.update(a, a + n, |v| Some(v.unwrap_or(0).max(vpost)));
            if kind.noret {
                s.v_rnoret = vpost;
                s.v_rnoret_p = ppost;
            } else {
                s.v_rold = vpost;
                s.v_rold_p = ppost;
            }
            if acquire {
                s.v_rnew = vpost;
                s.v_wnew = ppost;
            }
            s.v_cap = s.v_cap.max(addr.pick);
            out.push(ReadResult { state: s, val, srcs: vec![FUTURE; n as usize], dep: dep(vpost, ppost), poison });
        }
    }

    /// The values a read at `a` in single-copy-atomic pieces `ps` may
    /// return when some piece reads a later write by another thread, where
    /// the analysis does not know what those may store, or None if that is
    /// not known either. Each piece is read on its own, from the memory
    /// (one of `concrete`'s values) or from a later write.
    fn unknown_read_values(&self, a: u64, ps: &[(u64, u64)], concrete: &[ReadResult], red: &crate::reduce::Reduction) -> Option<Vec<u128>> {
        if let [(p, len)] = *ps {
            return self.unknown_values(p, len, red);
        }
        let mut combos = vec![0u128];
        for &(p, len) in ps {
            let mask = if len >= 16 { u128::MAX } else { (1u128 << (8 * len)) - 1 };
            let mut vals: BTreeSet<u128> = concrete.iter().map(|r| (r.val >> (8 * (p - a))) & mask).collect();
            match red.future_values(self.tid, p, len, &|b| self.test.init_byte(b)) {
                crate::reduce::Future::None => {}
                crate::reduce::Future::Values(v) => vals.extend(v),
                crate::reduce::Future::Unknown => vals.extend(self.unknown_values(p, len, red)?),
            }
            if combos.len() * vals.len() > MAX_UNKNOWN_VALUES {
                return None;
            }
            combos = combos.iter().flat_map(|&c| vals.iter().map(move |&v| c | v << (8 * (p - a)))).collect();
        }
        Some(combos)
    }

    /// For a cut observation: the values a read at `a` in single-copy-atomic
    /// pieces `ps` may return when some piece reads a later write by
    /// another thread, where the analysis does not know what those may
    /// store, each with the bytes it leaves unknown (`ReadResult::poison`),
    /// or None if there are too many.
    /// A piece whose later writes the analysis does not know is unknown
    /// (which covers its values in the memory too); any other is read from
    /// the memory (one of `concrete`'s values) or from a later write.
    fn poisoned_read_values(&self, a: u64, ps: &[(u64, u64)], concrete: &[ReadResult], red: &crate::reduce::Reduction) -> Option<Vec<(u128, u16)>> {
        let mut combos = vec![(0u128, 0u16)];
        for &(p, len) in ps {
            let mask = if len >= 16 { u128::MAX } else { (1u128 << (8 * len)) - 1 };
            let shift = 8 * (p - a);
            let mut vals: BTreeSet<u128> = concrete.iter().map(|r| (r.val >> shift) & mask).collect();
            match red.future_values(self.tid, p, len, &|b| self.test.init_byte(b)) {
                crate::reduce::Future::None => {}
                crate::reduce::Future::Values(v) => vals.extend(v),
                crate::reduce::Future::Unknown => {
                    let bytes = (((1u32 << len) - 1) << (p - a)) as u16;
                    combos.iter_mut().for_each(|c| c.1 |= bytes);
                    continue;
                }
            }
            if combos.len() * vals.len() > MAX_UNKNOWN_VALUES {
                return None;
            }
            combos = combos.iter().flat_map(|&(c, m)| vals.iter().map(move |&v| (c | v << shift, m))).collect();
        }
        Some(combos)
    }

    /// The values that the later writes in `unknown` give a
    /// single-copy-atomic read of `n` bytes at `a`, or None if it might
    /// combine bytes of several writes.
    fn unknown_values(&self, a: u64, n: u64, red: &crate::reduce::Reduction) -> Option<Vec<u128>> {
        if red.may_mix(self.tid, a, n) {
            return None;
        }
        let mask = if n >= 16 { u128::MAX } else { (1u128 << (8 * n)) - 1 };
        let mut out = BTreeSet::new();
        for (&(wa, wn), vals) in self.unknown.iter() {
            if wa < a + n && a < wa + wn {
                // `may_mix` says the write contains the read.
                out.extend(vals.iter().map(|v| (v >> (8 * (a - wa))) & mask));
            }
        }
        Some(out.into_iter().collect())
    }

    /// No write to the footprint between each byte's source and `t`
    /// (by another thread, if `others_only`)?
    fn atomic(&self, ext: &[Msg], a: u64, srcs: &[View], t: View, others_only: bool) -> bool {
        srcs.iter().enumerate().all(|(i, &from)| {
            let b = a + i as u64;
            (from + 1..t).all(|u| {
                let m = self.msg(ext, u);
                !m.covers(b) || (others_only && m.tid == self.tid)
            })
        })
    }

    /// Perform a write of `val` (`size` bytes) to `addr.val`, whose data has
    /// dependencies `data`. `atomic` imposes an atomicity constraint relative
    /// to an earlier read (per-byte sources, and whether only other threads'
    /// writes count).
    #[allow(clippy::too_many_arguments)]
    fn do_write(
        &mut self,
        st: &TState,
        ext: &[Msg],
        addr: RV,
        size: Size,
        val: u128,
        data: RV,
        kind: WriteKind,
        atomic: Option<(&[View], bool)>,
    ) -> Vec<Succ> {
        let a = addr.val;
        let n = size.bytes();
        let new_msg = Msg { addr: a, size, val, tid: self.tid };
        let mut vpre = addr.pick.max(data.pick).max(st.v_wnew).max(st.v_cap);
        if kind.release {
            vpre = vpre.max(st.v_rold_p).max(st.v_rnoret_p).max(st.v_wold);
        }
        if self.mode == Mode::Observe && vpre == self.base.len() as View {
            self.observed_writes.insert((a, n));
        }
        if self.mode == Mode::Observe {
            self.written.entry((a, n)).or_default().insert(val);
        }
        let apply = |t: View| {
            let mut s = st.clone();
            let fwd = Fwd {
                ts: t,
                view: addr.view.max(data.view),
                pick: addr.pick.max(data.pick),
                xview: kind.xview,
                addr: a,
                val,
                poison: self.write_poison,
            };
            s.coh.set(a, a + n, t);
            s.fwd.set(a, a + n, fwd);
            s.v_wold = s.v_wold.max(t);
            s.v_cap = s.v_cap.max(addr.pick);
            if kind.release {
                s.v_rel = s.v_rel.max(t);
            }
            if kind.full {
                s.v_rnew = s.v_rnew.max(t);
                s.v_wnew = s.v_wnew.max(t);
            }
            s.promises.remove(&t);
            s
        };
        let ok_atomic = |t: View| match atomic {
            Some((srcs, others_only)) => self.atomic(ext, a, srcs, t, others_only),
            None => true,
        };
        let coh_ok = |t: View| st.coh_max(a, n) < t;
        let mut out = Vec::new();
        // Fulfill an outstanding promise (with any value, if the value
        // written is poisoned).
        let same = |m: &Msg| if self.write_poison { m.addr == a && m.size == size && m.tid == self.tid } else { *m == new_msg };
        for &t in &st.promises {
            if same(self.msg(ext, t)) && vpre < t && coh_ok(t) && ok_atomic(t) {
                out.push((apply(t), ext.to_vec()));
            }
        }
        // In `Mode::Observe`, a new write comes after `y`, like every other
        // write after the memory, and is kept only in the forwarding bank.
        if self.mode == Mode::Observe {
            if ok_atomic(self.len(ext) + 1) {
                out.push((apply(FUTURE), ext.to_vec()));
            }
            return out;
        }
        // Or write a new message at the end of memory.
        let t = self.len(ext) + 1;
        if ok_atomic(t) {
            if self.mode == Mode::Collect {
                self.candidates.insert((a, size, val));
                let l = self.base.len() as View;
                let at_l = match atomic {
                    Some((srcs, others_only)) => srcs.iter().all(|&f| f <= l) && self.atomic(ext, a, srcs, l + 1, others_only),
                    None => true,
                };
                if vpre <= l && st.coh_max(a, n) <= l && at_l {
                    self.placeable.insert((a, size, val));
                }
            }
            let mut e = ext.to_vec();
            e.push(new_msg);
            out.push((apply(t), e));
        }
        out
    }

    /// A plain or release store, split into single-copy-atomic pieces of up
    /// to `granule` bytes (each one a separate write message).
    #[allow(clippy::too_many_arguments)]
    fn do_store(
        &mut self,
        st: &TState,
        ext: &[Msg],
        addr: RV,
        size: Size,
        granule: u64,
        val: u128,
        data: RV,
        release: bool,
    ) -> Vec<Succ> {
        let a = addr.val;
        let kind = WriteKind { release, ..WriteKind::default() };
        let mut states: Vec<Succ> = vec![(st.clone(), ext.to_vec())];
        for (p, len) in pieces(a, size.bytes(), granule, self.lse2) {
            let psize = Size::from_bytes(len).expect("piece size");
            let shifted = val >> (8 * (p - a));
            let pval = if len >= 16 { shifted } else { shifted & ((1u128 << (8 * len)) - 1) };
            let paddr = RV { val: p, ..addr };
            let mut next = Vec::new();
            for (s, e) in states {
                next.extend(self.do_write(&s, &e, paddr, psize, pval, data, kind, None));
            }
            states = next;
        }
        states
    }

    /// The states after the next memory operation (or the end of the
    /// thread). Local steps have exactly one successor and touch no memory,
    /// so they are taken at once rather than each being a separate state of
    /// the search: compiled code, especially at -O0, is mostly local steps.
    /// A state whose fuel runs out partway is returned as it is.
    fn successors(&mut self, st: &TState, ext: &[Msg]) -> Vec<Succ> {
        let mut s = st.clone();
        loop {
            if self.live.as_ref().is_some_and(|l| !l.ret_ok(self.prog, &s.core)) {
                self.live_failed = true;
                return vec![];
            }
            let mut abs = Abstract::Clean;
            if s.poison != 0 {
                if let Some(i) = self.prog.instrs.get(s.core.pc) {
                    let p = crate::live::uses(i) & s.poison;
                    abs = if p == 0 { Abstract::Clean } else { abstract_step(i, p) };
                    match abs {
                        Abstract::Fallback => {
                            self.poison_used.set(true);
                            return vec![];
                        }
                        Abstract::Data => {}
                        _ => s.poison &= !crate::live::defs(i),
                    }
                    if let Abstract::Flags | Abstract::Reg(_) = abs {
                        return self.forked_step(&s, ext, abs);
                    }
                    if abs == Abstract::Data {
                        let defs = crate::live::defs(i);
                        match s.core.step(self.prog) {
                            Step::Local { ctrl, write: None } => {
                                if let Some(v) = ctrl {
                                    s.v_cap = s.v_cap.max(v);
                                }
                                s.poison |= defs;
                                if s.core.steps >= self.fuel {
                                    return vec![(s, ext.to_vec())];
                                }
                                continue;
                            }
                            _ => {
                                self.poison_used.set(true);
                                return vec![];
                            }
                        }
                    }
                }
            }
            if abs == Abstract::Store {
                return match s.core.step(self.prog) {
                    Step::Mem(op) if !s.core.stack.is_private(store_addr(op), 1) => {
                        self.write_poison = true;
                        let out = self.mem_successors(s, ext, op);
                        self.write_poison = false;
                        out
                    }
                    _ => {
                        self.poison_used.set(true);
                        vec![]
                    }
                };
            }
            match s.core.step(self.prog) {
                Step::Local { ctrl, write } => {
                    if let Some(w) = write {
                        // A stack store, placed as early as it may be: right
                        // after everything it must follow (as `do_write`'s
                        // `vpre`). Timestamps are whole numbers, so "just
                        // after `t`" compares like `t`.
                        s.v_wold = s.v_wold.max(w).max(s.v_wnew).max(s.v_cap);
                    }
                    if let Some(v) = ctrl {
                        s.v_cap = s.v_cap.max(v);
                    }
                    if s.core.steps >= self.fuel {
                        return vec![(s, ext.to_vec())];
                    }
                }
                step => return self.nonlocal_successors(s, ext, step),
            }
        }
    }

    /// For a cut observation: the states after `s`'s next instruction, a
    /// local one whose outcome depends on a poisoned value (`abs`): one for
    /// each value of the flags, or each of two values of the register (zero,
    /// and all ones), which between them take every way it may go.
    fn forked_step(&mut self, s: &TState, ext: &[Msg], abs: Abstract) -> Vec<Succ> {
        let subst: Vec<Box<dyn Fn(&mut Core)>> = match abs {
            Abstract::Flags => (0..16).map(|f| Box::new(move |c: &mut Core| c.flags.val = f) as Box<dyn Fn(&mut Core)>).collect(),
            Abstract::Reg(r) => [0, u64::MAX]
                .into_iter()
                .map(|v| Box::new(move |c: &mut Core| c.regs[r as usize].val = v) as Box<dyn Fn(&mut Core)>)
                .collect(),
            _ => unreachable!(),
        };
        let mut out: Vec<Succ> = Vec::new();
        for f in subst {
            let mut t = s.clone();
            f(&mut t.core);
            match t.core.step(self.prog) {
                Step::Local { ctrl, write: None } => {
                    if let Some(v) = ctrl {
                        t.v_cap = t.v_cap.max(v);
                    }
                    // With clean operands, the flags or register written
                    // are clean; a branch keeps its register poisoned.
                    if !out.iter().any(|(u, _)| *u == t) {
                        out.push((t, ext.to_vec()));
                    }
                }
                _ => {
                    self.poison_used.set(true);
                    return vec![];
                }
            }
        }
        out
    }

    /// Whether a run that ends in `s` completes an execution with the memory
    /// as it is: the thread has fulfilled all its promises and appended no
    /// new write.
    fn complete(&self, s: &TState, ext: &[Msg]) -> bool {
        self.mode == Mode::Collect && s.promises.is_empty() && ext.is_empty() && !s.speculative
    }

    fn nonlocal_successors(&mut self, s: TState, ext: &[Msg], step: Step) -> Vec<Succ> {
        match step {
            Step::Done => {
                if self.complete(&s, ext) {
                    let stack = &s.core.stack;
                    let byte = |b: u64| stack.byte(b).expect("private memory has a value");
                    let regs = self.final_regs
                        .iter()
                        .map(|&r| s.core.regs[r as usize].val)
                        .chain(self.final_mems.iter().map(|&a| self.test.loc_value(a, &byte)))
                        .collect();
                    self.finals.insert(regs);
                }
                vec![]
            }
            Step::Local { .. } => unreachable!("local steps are taken in `successors`"),
            Step::Mem(op) => self.mem_successors(s, ext, op),
            // A fault ends the execution. It is real if the thread's promises
            // are fulfilled, since the other threads' promises are certified.
            Step::Fault(e) => {
                if self.complete(&s, ext) {
                    self.faults.insert(format!("P{}: {}", self.tid, e));
                }
                vec![]
            }
        }
    }

    /// An exclusive or atomic access to memory private to the thread (see
    /// `Stack::is_private`), or None for any other operation. The core
    /// performs it on its private bytes, like a stack access: no message is
    /// written and nothing is read from memory. No other thread accesses
    /// these bytes, so the access is trivially atomic, and its outcomes are
    /// those of the shared case with every other thread's writes left out.
    /// The access neither acquires nor releases (the core faults on such
    /// accesses to private memory), and so needs none of the views that
    /// those use. A write is placed as early as it may be, as for a stack
    /// store (see `successors`).
    fn private_successors(&self, st: &TState, op: MemOp) -> Option<Vec<TState>> {
        let prog = self.prog;
        let status = |ok: bool| RV::imm(if ok { 0 } else { 1 });
        let (addr, len) = match op {
            MemOp::Load { addr, size, excl: true, .. }
            | MemOp::StoreExcl { addr, size, .. }
            | MemOp::Rmw { addr, size, .. } => (addr, size.bytes()),
            MemOp::LoadExclPair { addr, size, .. } | MemOp::StoreExclPair { addr, size, .. } => {
                (addr, 2 * size.bytes())
            }
            _ => return None,
        };
        if !st.core.stack.is_private(addr.val, len) {
            return None;
        }
        let mut base = st.clone();
        base.v_cap = base.v_cap.max(addr.pick);
        // Place a private write: after everything it must follow.
        let placed = |s: &mut TState, w: View| s.v_wold = s.v_wold.max(w).max(st.v_wnew).max(st.v_cap);
        let mut out = Vec::new();
        match op {
            MemOp::Load { rt, size, signed, .. } => {
                let (v, dep) = base.core.private_load(addr, size);
                base.core.set(rt, RV { val: size.extend(v as u64, signed), ..dep });
                base.xcl = Some(Monitor { addr: addr.val, size, srcs: Vec::new(), view: dep.view, val: v });
                out.push(base);
            }
            MemOp::LoadExclPair { rt, rt2, size, .. } => {
                let total = size.double();
                let (v, dep) = base.core.private_load(addr, total);
                let (lo, hi) = split_pair(v, size);
                base.core.set(rt, RV { val: lo, ..dep });
                base.core.set(rt2, RV { val: hi, ..dep });
                base.xcl = Some(Monitor { addr: addr.val, size: total, srcs: Vec::new(), view: dep.view, val: v });
                out.push(base);
            }
            MemOp::StoreExcl { data, rs, size, .. } | MemOp::StoreExclPair { data, rs, size, .. } => {
                // As for shared memory, failure is always possible.
                let (total, val, data) = match op {
                    MemOp::StoreExclPair { data2, .. } => {
                        (size.double(), pair_value(data.val, data2.val, size), RV::joined(0, data, data2))
                    }
                    _ => (size, size.mask(data.val) as u128, data),
                };
                let mut f = base.clone();
                f.xcl = None;
                f.core.set(rs, status(false));
                let can_succeed = st.xcl.as_ref().is_some_and(|m| m.addr == addr.val && m.size == total);
                if let Some(f) = self.store_excl_fails(f, can_succeed) {
                    out.push(f);
                }
                if can_succeed {
                    let mut s = base;
                    let w = s.core.private_store(addr, total, val, data);
                    placed(&mut s, w);
                    s.xcl = None;
                    s.core.set(rs, status(true));
                    out.push(s);
                }
            }
            MemOp::Rmw { op, operand, cmp, rt, size, .. } => {
                let (v, dep) = base.core.private_load(addr, size);
                let old = v as u64;
                let reg = RV { val: old, ..dep };
                match rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                    None => {
                        base.core.set(rt, reg);
                        out.push(base);
                    }
                    Some(new) => {
                        // As in `mem_successors`.
                        let data = match (cmp, op) {
                            (Some(c), _) => RV { pick: operand.pick.max(c.pick).max(dep.pick), ..operand },
                            (None, RmwOp::Swp) => operand,
                            (None, _) => RV::joined(0, operand, dep),
                        };
                        let mut outs = vec![reg];
                        if let Some(c) = cmp {
                            outs.push(RV { val: old, view: c.view, pick: c.pick.max(dep.pick) });
                        }
                        let w = base.core.private_store(addr, size, new as u128, data);
                        placed(&mut base, w);
                        for o in outs {
                            let mut s = base.clone();
                            s.core.set(rt, o);
                            out.push(s);
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
        for s in &mut out {
            s.core.retire(prog);
        }
        Some(out)
    }

    fn mem_successors(&mut self, st: TState, ext: &[Msg], op: MemOp) -> Vec<Succ> {
        let prog = self.prog;
        if let Some(states) = self.private_successors(&st, op) {
            return states.into_iter().map(|s| (s, ext.to_vec())).collect();
        }
        let mut out = Vec::new();
        let status = |ok: bool| RV::imm(if ok { 0 } else { 1 });
        match op {
            MemOp::Clrex => {
                let mut s = st;
                s.xcl = None;
                s.core.retire(prog);
                out.push((s, ext.to_vec()));
            }
            MemOp::Barrier(b) => {
                let mut s = st;
                match b {
                    Barrier::Full => {
                        s.v_rnew = s.v_rnew.max(s.v_rold).max(s.v_rnoret).max(s.v_wold);
                        s.v_wnew = s.v_wnew.max(s.v_rold_p).max(s.v_rnoret_p).max(s.v_wold);
                    }
                    Barrier::Ld => {
                        s.v_rnew = s.v_rnew.max(s.v_rold);
                        s.v_wnew = s.v_wnew.max(s.v_rold_p);
                    }
                    Barrier::St => s.v_wnew = s.v_wnew.max(s.v_wold),
                    Barrier::Isb => s.v_rnew = s.v_rnew.max(s.v_cap),
                }
                s.core.retire(prog);
                out.push((s, ext.to_vec()));
            }
            MemOp::Load { addr, rt, size, signed, kind, excl } => {
                for r in self.do_read(&st, ext, addr, ReadKind::of(kind), size, size.bytes()) {
                    let mut s = r.state;
                    s.core.set(rt, RV { val: size.extend(r.val as u64, signed), ..r.dep });
                    s.poison_reg(rt, r.poison != 0);
                    if excl {
                        s.xcl = Some(Monitor { addr: addr.val, size, srcs: r.srcs, view: r.dep.view, val: r.val });
                    }
                    s.core.retire(prog);
                    out.push((s, ext.to_vec()));
                }
            }
            MemOp::LoadExclPair { addr, rt, rt2, size, acquire } => {
                // A W pair is one 8-byte single-copy-atomic read; an X pair
                // is atomic only per 8-byte half (a successful STXP is what
                // guarantees the two halves were consistent).
                let kind = ReadKind::of(if acquire { LoadKind::Acquire } else { LoadKind::Plain });
                let total = size.double();
                let granule = total.bytes().min(8);
                for r in self.do_read(&st, ext, addr, kind, total, granule) {
                    let mut s = r.state;
                    let (lo, hi) = split_pair(r.val, size);
                    s.core.set(rt, RV { val: lo, ..r.dep });
                    s.core.set(rt2, RV { val: hi, ..r.dep });
                    let half = (1u16 << size.bytes()) - 1;
                    s.poison_reg(rt, r.poison & half != 0);
                    s.poison_reg(rt2, r.poison & !half != 0);
                    s.xcl = Some(Monitor { addr: addr.val, size: total, srcs: r.srcs, view: r.dep.view, val: r.val });
                    s.core.retire(prog);
                    out.push((s, ext.to_vec()));
                }
            }
            MemOp::StoreExclPair { addr, data, data2, rs, size, release } => {
                let total = size.double();
                let mut f = st.clone();
                f.xcl = None;
                f.core.set(rs, status(false));
                f.core.retire(prog);
                let mut succeeded = Vec::new();
                if let Some(m) = st.xcl.as_ref().filter(|m| m.addr == addr.val && m.size == total) {
                    let val = pair_value(data.val, data2.val, size);
                    if let Some(w) = if m.val == val { self.stutters(&st) } else { Some(st.clone()) } {
                        let srcs = m.srcs.clone();
                        let kind = WriteKind { release, xview: m.view, full: false };
                        let data = RV::joined(0, data, data2);
                        for (mut s, e) in self.do_write(&w, ext, addr, total, val, data, kind, Some((&srcs[..], true))) {
                            s.xcl = None;
                            s.core.set(rs, status(true));
                            s.core.retire(prog);
                            succeeded.push((s, e));
                        }
                    }
                }
                if let Some(f) = self.store_excl_fails(f, !succeeded.is_empty()) {
                    out.push((f, ext.to_vec()));
                }
                out.extend(succeeded);
            }
            MemOp::LoadPair { addr, rt, rt2, size, signed } => {
                let total = size.double();
                if self.lse2 && addr.val % total.bytes() == 0 {
                    // FEAT_LSE2: an aligned pair is one single-copy-atomic read.
                    for r in self.do_read(&st, ext, addr, ReadKind::plain(), total, total.bytes()) {
                        let mut s = r.state;
                        let (lo, hi) = split_pair(r.val, size);
                        s.core.set(rt, RV { val: size.extend(lo, signed), ..r.dep });
                        s.core.set(rt2, RV { val: size.extend(hi, signed), ..r.dep });
                        let half = (1u16 << size.bytes()) - 1;
                        s.poison_reg(rt, r.poison & half != 0);
                        s.poison_reg(rt2, r.poison & !half != 0);
                        s.core.retire(prog);
                        out.push((s, ext.to_vec()));
                    }
                } else {
                    // Two independent reads: the second does not depend on the first.
                    let addr2 = RV { val: addr.val + size.bytes(), ..addr };
                    for r1 in self.do_read(&st, ext, addr, ReadKind::plain(), size, size.bytes()) {
                        for r2 in self.do_read(&r1.state, ext, addr2, ReadKind::plain(), size, size.bytes()) {
                            let mut s = r2.state;
                            s.core.set(rt, RV { val: size.extend(r1.val as u64, signed), ..r1.dep });
                            s.core.set(rt2, RV { val: size.extend(r2.val as u64, signed), ..r2.dep });
                            s.poison_reg(rt, r1.poison != 0);
                            s.poison_reg(rt2, r2.poison != 0);
                            s.core.retire(prog);
                            out.push((s, ext.to_vec()));
                        }
                    }
                }
            }
            MemOp::VLoad { addr, vt, vt2, size } => {
                // Each register is an independent read; a 16-byte one is
                // single-copy atomic only per 8-byte half.
                let n = size.bytes();
                let mut states = vec![st];
                for (i, v) in std::iter::once(vt).chain(vt2).enumerate() {
                    let a = RV { val: addr.val + i as u64 * n, ..addr };
                    let mut next = Vec::new();
                    for s in &states {
                        for r in self.do_read(s, ext, a, ReadKind::plain(), size, n.min(8)) {
                            // SIMD&FP registers are not tracked.
                            if r.poison != 0 {
                                self.poison_used.set(true);
                                return vec![];
                            }
                            let mut s = r.state;
                            s.core.vset(v, VR::with(r.val, r.dep));
                            next.push(s);
                        }
                    }
                    states = next;
                }
                for mut s in states {
                    s.core.retire(prog);
                    out.push((s, ext.to_vec()));
                }
            }
            MemOp::VStore { addr, data, data2, size } => {
                let n = size.bytes();
                let mut states = vec![(st, ext.to_vec())];
                for (i, d) in std::iter::once(data).chain(data2).enumerate() {
                    let a = RV { val: addr.val + i as u64 * n, ..addr };
                    let mut next = Vec::new();
                    for (s, e) in &states {
                        next.extend(self.do_store(s, e, a, size, n.min(8), low_bytes(d.val, n), d.dep(), false));
                    }
                    states = next;
                }
                for (mut s, e) in states {
                    s.core.retire(prog);
                    out.push((s, e));
                }
            }
            MemOp::Store { addr, data, size, release } => {
                for (mut s, e) in self.do_store(&st, ext, addr, size, size.bytes(), size.mask(data.val) as u128, data, release) {
                    s.core.retire(prog);
                    out.push((s, e));
                }
            }
            MemOp::StorePair { addr, data, data2, size } => {
                let total = size.double();
                let states = if self.lse2 && addr.val % total.bytes() == 0 {
                    // FEAT_LSE2: an aligned pair is one single-copy-atomic write.
                    let val = pair_value(data.val, data2.val, size);
                    self.do_store(&st, ext, addr, total, total.bytes(), val, RV::joined(0, data, data2), false)
                } else {
                    let addr2 = RV { val: addr.val + size.bytes(), ..addr };
                    let mut states = Vec::new();
                    for (s1, e1) in self.do_store(&st, ext, addr, size, size.bytes(), size.mask(data.val) as u128, data, false) {
                        states.extend(self.do_store(&s1, &e1, addr2, size, size.bytes(), size.mask(data2.val) as u128, data2, false));
                    }
                    states
                };
                for (mut s, e) in states {
                    s.core.retire(prog);
                    out.push((s, e));
                }
            }
            MemOp::StoreExcl { addr, data, rs, size, release } => {
                // Failure is always possible (and certain without a matching
                // monitor). The status register carries no dependency.
                let mut f = st.clone();
                f.xcl = None;
                f.core.set(rs, status(false));
                f.core.retire(prog);
                let mut succeeded = Vec::new();
                if let Some(m) = st.xcl.as_ref().filter(|m| m.addr == addr.val && m.size == size) {
                    let val = size.mask(data.val) as u128;
                    if let Some(w) = if m.val == val { self.stutters(&st) } else { Some(st.clone()) } {
                        let srcs = m.srcs.clone();
                        let kind = WriteKind { release, xview: m.view, full: false };
                        for (mut s, e) in self.do_write(&w, ext, addr, size, val, data, kind, Some((&srcs[..], true))) {
                            s.xcl = None;
                            s.core.set(rs, status(true));
                            s.core.retire(prog);
                            succeeded.push((s, e));
                        }
                    }
                }
                if let Some(f) = self.store_excl_fails(f, !succeeded.is_empty()) {
                    out.push((f, ext.to_vec()));
                }
                out.extend(succeeded);
            }
            MemOp::Rmw { addr, op, operand, cmp, rt, size, acquire, release, noret } => {
                let load = if acquire { LoadKind::Acquire } else { LoadKind::Plain };
                if let Some(c) = cmp {
                    if let Some(mut s) = self.cas_fails_later(&st, addr, size, ReadKind { load, noret }) {
                        s.core.set(rt, RV { val: size.mask(c.val ^ 1), view: FUTURE, pick: FUTURE });
                        s.core.retire(prog);
                        out.push((s, ext.to_vec()));
                    }
                }
                for r in self.do_read(&st, ext, addr, ReadKind { load, noret }, size, size.bytes()) {
                    // The write's value (or whether it writes) may depend
                    // on the unknown value.
                    if r.poison != 0 {
                        self.poison_used.set(true);
                        return vec![];
                    }
                    let old = r.val as u64;
                    let reg = RV { val: old, ..r.dep };
                    match rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                        None => {
                            let mut s = r.state;
                            s.core.set(rt, reg);
                            s.core.retire(prog);
                            out.push((s, ext.to_vec()));
                        }
                        Some(new) => {
                            // CAS and SWP store a register; whether CAS stores
                            // at all depends on the comparison (a pick
                            // dependency). Other atomics compute from the old value.
                            let data = match (cmp, op) {
                                (Some(c), _) => {
                                    RV { pick: operand.pick.max(c.pick).max(r.dep.pick), ..operand }
                                }
                                (None, RmwOp::Swp) => operand,
                                (None, _) => RV::joined(0, operand, r.dep),
                            };
                            // A successful CAS leaves Rs holding the value it
                            // already had. Like herd7, allow that value to come
                            // either from the memory read or from the old Rs
                            // (then depending on the read only through the
                            // comparison: a pick dependency).
                            let mut outs = vec![reg];
                            if let Some(c) = cmp {
                                outs.push(RV { val: old, view: c.view, pick: c.pick.max(r.dep.pick) });
                            }
                            let kind = WriteKind { release, xview: r.dep.view, full: acquire && release };
                            let atomic = Some((&r.srcs[..], false));
                            let Some(w) = (if new == old { self.stutters(&r.state) } else { Some(r.state.clone()) }) else {
                                continue;
                            };
                            for (s, e) in self.do_write(&w, ext, addr, size, new as u128, data, kind, atomic) {
                                for &o in &outs {
                                    let mut s = s.clone();
                                    s.core.set(rt, o);
                                    s.core.retire(prog);
                                    out.push((s, e.clone()));
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// The state `f` after a store-exclusive fails, unless the failure is
    /// spurious (`can_succeed`) and the run has already failed as often as
    /// `Options::spurious` allows.
    fn store_excl_fails(&self, mut f: TState, can_succeed: bool) -> Option<TState> {
        if let (Some(limit), true) = (self.spurious, can_succeed) {
            if f.spurious >= limit {
                self.capped.set(true);
                return None;
            }
            f.spurious += 1;
        }
        Some(f)
    }

    /// The state `s` before a read-modify-write to shared memory writes
    /// back the value it read, unless the run has already done that as
    /// often as `Options::stutter` allows.
    fn stutters(&self, s: &TState) -> Option<TState> {
        let mut s = s.clone();
        if let Some(limit) = self.stutter {
            if s.stutters >= limit {
                self.capped.set(true);
                return None;
            }
            s.stutters += 1;
        }
        Some(s)
    }

    /// A `CAS` that fails on a later write by another thread, from `st`:
    /// the state after its read, or None if no other thread may write its
    /// footprint (or in `Mode::Observe`, whose reads of later writes already
    /// include this).
    ///
    /// Running alone, a thread sees only the memory so far, where the
    /// comparison may succeed. But in Arm's model a `CAS` that fails writes
    /// nothing, so it orders nothing after it as a release would, and its
    /// failure is no dependency. A thread may then need a later write to
    /// make its `CAS` fail before it can fulfill a promise that the
    /// execution places earlier (`CAS-fail-promise`). So the runs that
    /// certify promises and collect candidates may also let a `CAS` read a
    /// later write by another thread, with a value other than the expected
    /// one and the view `FUTURE`, which is above every timestamp. Whatever
    /// depends on that value is then ordered after every promise, and so
    /// cannot help to fulfill one: the value itself does not matter. Such a
    /// run only certifies; it never completes an execution, whose threads
    /// all run against the whole memory (`complete`), so no final state
    /// comes from it.
    ///
    /// An acquiring `CAS` that fails so gains nothing: its `v_wnew` becomes
    /// `FUTURE`, so a promise still outstanding can no longer be fulfilled
    /// (`doomed`), and a later write can never be placed among the
    /// promises. So only a `CAS` without acquire may fail on a later write.
    /// (An acquiring one would only lead the run on to no purpose: in a
    /// retry loop, such as `qemu_event_wait` in `c/qemu_event.c`, round
    /// and round until the fuel runs out.)
    fn cas_fails_later(&self, st: &TState, addr: RV, size: Size, kind: ReadKind) -> Option<TState> {
        let (a, n) = (addr.val, size.bytes());
        let others = match &self.reduction {
            Some(r) => !matches!(r.future_values(self.tid, a, n, &|b| self.test.init_byte(b)), crate::reduce::Future::None),
            None => true,
        };
        if self.mode == Mode::Observe || !others || kind.load != LoadKind::Plain {
            return None;
        }
        let mut s = st.clone();
        s.speculative = true;
        s.coh.set(a, a + n, FUTURE);
        if kind.noret {
            s.v_rnoret = FUTURE;
            s.v_rnoret_p = FUTURE;
        } else {
            s.v_rold = FUTURE;
            s.v_rold_p = FUTURE;
        }
        s.v_cap = s.v_cap.max(addr.pick);
        Some(s)
    }

    /// Can some outstanding promise no longer be fulfilled? Fulfilling
    /// promise `t` needs `v_wnew`, `v_cap` and the coherence view of each of
    /// its bytes to be below `t`, and none of them ever decreases.
    fn doomed(&self, st: &TState, ext: &[Msg]) -> bool {
        st.promises.iter().any(|&t| {
            let m = self.msg(ext, t);
            st.v_wnew >= t || st.v_cap >= t || st.coh_max(m.addr, m.size.bytes()) >= t
        })
    }

    /// Whether a promise of candidate `c` (collected against `base`) can
    /// certify: a necessary condition, so a candidate for which it fails
    /// need not be certified.
    ///
    /// A run certifying the promise, placed at `t = base.len() + 1`, writes
    /// it with every view it must exceed (the write's `vpre`, the
    /// coherence of its bytes, and an exclusive's atomicity) within
    /// `base`, then goes on until no promise is outstanding. Running the
    /// same steps in `Collect` mode, appending the write instead of
    /// fulfilling the promise, gives a run of this exploration: before the
    /// write, as `Mode::Collect` notes; after it, because the two runs
    /// differ only in where that one write sits among the thread's own new
    /// writes, and its bytes are disjoint from those of the writes before
    /// it (it could not be placed before them otherwise), so on each byte
    /// the order of the writes is the same, and every check against the
    /// older promises compares with a timestamp within `base`. That run
    /// writes `c` with those views within `base` (`placeable`), and ends up
    /// with no promise outstanding and `c` appended (`fulfilling`).
    fn promising(&self, c: &Candidate) -> bool {
        self.placeable.contains(c) && self.fulfilling.contains(c)
    }

    /// For `Mode::Observe`: if `st` is past `x` (the last message of
    /// `base`), erase its views, so that states that differ only in views
    /// are explored once, and return true. (In `ck_fifo_mpmc`, nearly all
    /// the states of an observing exploration are past `x`.)
    ///
    /// Views only grow, and in this mode each is at most `ts(x)` or
    /// `FUTURE`. A read's pre-view is at least `v_rnew`, and a write's at
    /// least `v_wnew` and `v_cap`, so once those are `FUTURE`, every later
    /// access has the pre-view `FUTURE`, whatever the other views are. The
    /// state has no outstanding promise (it would be `doomed`), so no write
    /// fulfills one: each is a new write at `FUTURE`. Each read returns the
    /// latest message to each byte, its own new write or a later write by
    /// another thread, and is recorded only if it might take bytes from `x`
    /// (see `observe_read`), which depends on the memory and the pieces of
    /// the read, not on views. So the run goes on as it would have, reading
    /// and writing the same values, and records the same accesses.
    fn past(&self, st: &mut TState) -> bool {
        let l = self.base.len() as View;
        if st.v_rnew <= l || st.v_wnew <= l && st.v_cap <= l {
            return false;
        }
        debug_assert!(st.promises.is_empty());
        st.core.erase_views();
        for v in [
            &mut st.v_rold,
            &mut st.v_rold_p,
            &mut st.v_rnoret,
            &mut st.v_rnoret_p,
            &mut st.v_wold,
            &mut st.v_rnew,
            &mut st.v_wnew,
            &mut st.v_cap,
            &mut st.v_rel,
        ] {
            *v = FUTURE;
        }
        // Coherence bounds a read only through `max` with its pre-view, and
        // the forwarding bank matters only for the new writes, which a read
        // of their bytes returns (a write in the memory is read from the
        // memory, as the latest message to its bytes).
        st.coh = ByteMap::new();
        st.fwd.map_values(|f| (f.ts == FUTURE).then_some(Fwd { view: 0, pick: 0, xview: 0, ..f }));
        if let Some(m) = &mut st.xcl {
            m.view = 0;
        }
        true
    }

    fn run(&mut self, start: TState) {
        let mut queue = StepQueue::new();
        queue.push(start.core.steps, (start, Vec::new()));
        while let Some((mut st, ext)) = queue.pop() {
            if self.mode == Mode::Observe {
                self.states += 1;
                if self.gave_up.get() || self.states > self.max_states {
                    self.gave_up.set(true);
                    return;
                }
                if self.poison_used.get() {
                    return;
                }
            }
            if self.mode == Mode::Certify && st.promises.is_empty() {
                self.certified = true;
                return;
            }
            // A run with a promise it can no longer fulfill can neither
            // certify nor complete, nor lead to a certifiable candidate.
            if self.doomed(&st, &ext) {
                continue;
            }
            if self.mode == Mode::Observe && self.past(&mut st) && self.cut {
                // What the run would record from here on: reads that take
                // bytes from `x` (see `past`).
                if !std::mem::replace(&mut self.cut_reads, true) {
                    let x = self.base.last().expect("an observed memory is not empty");
                    let red = self.reduction.as_ref().expect("observing needs the analysis");
                    self.observed_reads.get_mut().extend(red.reads_overlapping(self.tid, x));
                }
                continue;
            }
            // A state already reached with no more steps used has nothing new
            // to offer; this collapses spin loops.
            let mut key = st.clone();
            key.core.steps = 0;
            if let Some(l) = &self.live {
                l.canon(&mut key.core);
            }
            match self.visited.entry((key, ext.clone())) {
                Entry::Occupied(e) if *e.get() <= st.core.steps => continue,
                Entry::Occupied(mut e) => *e.get_mut() = st.core.steps,
                Entry::Vacant(e) => {
                    e.insert(st.core.steps);
                }
            }
            if self.mode == Mode::Collect && st.promises.is_empty() {
                self.fulfilling.extend(ext.iter().map(|m| (m.addr, m.size, m.val)));
            }
            self.fulfills |= self.complete(&st, &ext);
            if st.core.steps >= self.fuel && st.core.pc < self.prog.instrs.len() {
                self.truncated = true;
                continue;
            }
            for x in self.successors(&st, &ext) {
                queue.push(x.0.core.steps, x);
            }
            if self.live_failed {
                return;
            }
        }
    }
}

pub fn explore(test: &Test, cfg: &Options) -> Outcomes {
    if test.threads.len() == 1 {
        // A lone thread sees its own accesses in program order: its behaviors
        // are exactly the sequentially consistent ones.
        return crate::interleave::explore(test, crate::interleave::Kind::Sc, cfg.fuel, cfg.private, cfg.live);
    }
    let mut proj = Projection::new(test);
    let analysis = if cfg.por || cfg.private { crate::reduce::analyze(test, cfg.fuel, cfg.live) } else { None };
    let n = test.threads.len();
    let private = match &analysis {
        Some(r) if cfg.private => {
            let observed: Vec<(u64, u64)> = proj
                .obs
                .iter()
                .filter_map(|o| match *o {
                    Obs::Mem(a) => Some((a, test.loc_type(a).size.bytes())),
                    Obs::Reg(..) => None,
                })
                .collect();
            r.private(&observed)
        }
        _ => vec![Vec::new(); n],
    };
    for (t, ranges) in private.iter().enumerate() {
        proj.thread_mems[t] = proj
            .obs
            .iter()
            .filter_map(|o| match *o {
                Obs::Mem(a) if ranges.iter().any(|&(p, q)| p <= a && a < q) => Some(a),
                _ => None,
            })
            .collect();
    }
    let proj = proj;
    let mut ex = Explorer {
        test,
        cfg,
        proj: &proj,
        private: private.into_iter().map(Arc::new).collect(),
        live: (0..n)
            .map(|t| {
                let exit = proj.thread_regs[t].iter().filter(|&&r| r < 31).fold(0, |m, &r| m | 1 << r);
                cfg.live.then(|| Arc::new(crate::live::Liveness::new(&test.threads[t].code, exit)))
            })
            .collect(),
        reduction: if cfg.por { analysis.map(Arc::new) } else { None },
        seeds: FxHashMap::default(),
        failed: FxHashSet::default(),
        results: Outcomes::new(test.observables()),
    };
    match cfg.sample {
        None => ex.exhaustive(),
        Some(walks) => ex.sample(walks, cfg.seed),
    }
    ex.results
}

/// The outcome of `Explorer::observe_cut`.
enum Cut {
    Done(crate::reduce::Observed),
    GaveUp,
    /// A run used a poisoned value before it got past `x`.
    Poisoned,
}

/// The promise-first search over memories.
struct Explorer<'a> {
    test: &'a Test,
    cfg: &'a Options,
    proj: &'a Projection,
    /// Each thread's private memory (see `Reduction::private`).
    private: Vec<Arc<Vec<(u64, u64)>>>,
    /// Each thread's register liveness, with the observed registers live
    /// at the end; None once a run has returned where it did not expect.
    live: Vec<Option<Arc<crate::live::Liveness>>>,
    reduction: Option<Arc<crate::reduce::Reduction>>,
    /// For `observe`: the values that each thread's later writes may store
    /// after each memory observed, and the memories whose observation gave
    /// up (or was not tried, after its parent's gave up).
    seeds: FxHashMap<Vec<Msg>, Arc<Vec<Written>>>,
    failed: FxHashSet<Vec<Msg>>,
    results: Outcomes,
}

impl<'a> Explorer<'a> {
    /// Run thread `tid` alone against `mem`.
    fn solo(&mut self, tid: usize, mem: &[Msg], mode: Mode) -> Solo<'a> {
        self.solo_with(tid, mem, mode, Arc::default(), MAX_OBSERVE_STATES, false)
    }

    /// The same, with `Solo::unknown`, `Solo::max_states` and `Solo::cut` for
    /// `Mode::Observe`.
    fn solo_with(&mut self, tid: usize, mem: &[Msg], mode: Mode, unknown: Arc<Written>, max_states: u64, cut: bool) -> Solo<'a> {
        loop {
            let mut s = Solo::new(self.test, self.cfg, self.proj, tid, mem.to_vec(), mode, self.live[tid].clone());
            s.reduction = self.reduction.clone();
            s.max_states = max_states;
            s.cut = cut;
            if mode == Mode::Observe {
                s.unknown = unknown.clone();
            }
            s.run(self.start(tid, mem));
            if !s.live_failed {
                return s;
            }
            self.live[tid] = None;
        }
    }

    /// What each thread may do after `mem` (`Mode::Observe`), or None if
    /// that is not known, within `budget` states.
    ///
    /// It first tries `observe_cut`, which runs each thread once. Only if a
    /// run there needs a value the analysis does not know does it fall back
    /// to the rounds below. Where the analysis does not know what a later write may store (a
    /// counter, which it widens to any value), the values come from the
    /// explorations themselves, in rounds: in each, a read of those bytes
    /// returns the values the other threads wrote in the rounds before.
    /// Once a round adds no value, every later write's is among them: in
    /// Promising-ARM a write depends only on reads of earlier messages, so
    /// (by induction on the memory order) if the values of the first `k`
    /// later writes are among those a round starts from, the `k + 1`th is
    /// among those it finds, and so among those it started from.
    ///
    /// That holds whatever values the first round starts from, so it starts
    /// from those found for the parent memory (`mem` without its last
    /// message), if that was observed: every execution whose memory extends
    /// `mem` extends the parent too, and its writes after `mem` come after
    /// the parent. Then the first round usually adds nothing. If the
    /// parent's observation gave up, so does this one, without trying:
    /// deeper memories rarely converge where a shallower one did not, and
    /// trying them all costs more than they prune.
    fn observe(&mut self, mem: &[Msg], mut budget: u64) -> Option<crate::reduce::Observed> {
        let n = self.test.threads.len();
        let mut cut_budget = budget.max(MIN_CUT_STATES);
        match self.observe_cut(mem, &mut cut_budget) {
            Cut::Done(obs) => {
                self.results.stats.observed += 1;
                return Some(obs);
            }
            Cut::GaveUp => {
                self.results.stats.observe_failed += 1;
                return None;
            }
            Cut::Poisoned => {}
        }
        let parent = &mem[..mem.len() - 1];
        if self.failed.contains(parent) {
            self.failed.insert(mem.to_vec());
            self.results.stats.observe_failed += 1;
            return None;
        }
        let mut written: Vec<Written> = match self.seeds.get(parent) {
            Some(w) => (**w).clone(),
            None => vec![Written::default(); n],
        };
        for _ in 0..MAX_OBSERVE_ROUNDS {
            let mut obs = crate::reduce::Observed::default();
            let mut used_unknown = false;
            let mut grown = false;
            let mut new_written = written.clone();
            for tid in 0..n {
                let mut unknown = Written::default();
                for (t, w) in written.iter().enumerate() {
                    if t != tid {
                        for (fp, vals) in w {
                            unknown.entry(*fp).or_default().extend(vals);
                        }
                    }
                }
                let mut o = self.solo_with(tid, mem, Mode::Observe, Arc::new(unknown), budget, false);
                budget = budget.saturating_sub(o.states);
                if o.gave_up.get() {
                    self.failed.insert(mem.to_vec());
                    self.results.stats.observe_failed += 1;
                    return None;
                }
                used_unknown |= o.used_unknown.get();
                for (fp, vals) in std::mem::take(&mut o.written) {
                    let e = new_written[tid].entry(fp).or_default();
                    let before = e.len();
                    e.extend(vals);
                    grown |= e.len() > before;
                }
                obs.reads.push(o.observed_reads.take());
                obs.writes.push(std::mem::take(&mut o.observed_writes));
            }
            if !used_unknown || !grown {
                self.results.stats.observed += 1;
                self.seeds.insert(mem.to_vec(), Arc::new(new_written));
                return Some(obs);
            }
            written = new_written;
        }
        self.failed.insert(mem.to_vec());
        self.results.stats.observe_failed += 1;
        None
    }

    /// What each thread may do after `mem`, found with the states past `x`
    /// (the last message of `mem`) cut off (`Solo::cut`).
    ///
    /// A state past `x` makes no more accesses ordered after `x`, and needs
    /// no more than its reads of `x` recorded (see `Solo::past`). Only its
    /// writes would matter: their values are those that later writes give
    /// the other threads' reads, which the rounds of `observe` find. Instead,
    /// where the analysis does not know them, the value read is poisoned.
    /// A run that gets past `x` before it uses one needs no value at all;
    /// until then, the run computes with it, branches on it and stores it
    /// abstractly (`Abstract`). One that needs it for anything else, such
    /// as an address (`Cut::Poisoned`), needs the rounds.
    /// In `ck_fifo_mpmc`, whose loads are each followed by a fence, a read of
    /// a later write puts the run past `x` at once.
    fn observe_cut(&mut self, mem: &[Msg], budget: &mut u64) -> Cut {
        let mut obs = crate::reduce::Observed::default();
        for tid in 0..self.test.threads.len() {
            let mut o = self.solo_with(tid, mem, Mode::Observe, Arc::default(), *budget, true);
            *budget = budget.saturating_sub(o.states);
            if o.poison_used.get() {
                return Cut::Poisoned;
            }
            // Past the budget, or a read with too many values: the rounds,
            // which explore what the cut cuts off, would mostly give up too.
            if o.gave_up.get() {
                return Cut::GaveUp;
            }
            obs.reads.push(o.observed_reads.take());
            obs.writes.push(std::mem::take(&mut o.observed_writes));
        }
        Cut::Done(obs)
    }

    fn start(&self, tid: usize, mem: &[Msg]) -> TState {
        let test = self.test;
        let mut st = TState::new(test.init_regs[tid], tid, test.threads.len(), test.threads[tid].entry);
        st.core.stack.set_private(self.private[tid].clone(), &|b| test.init_byte(b));
        st.promises = (1..=mem.len() as View).filter(|&t| mem[t as usize - 1].tid == tid).collect();
        st
    }

    /// Explore `mem`: run each thread alone against it (`Mode::Collect`).
    ///
    /// Phase 2: record the final states of the executions that complete with
    /// this memory, each thread fulfilling exactly its promises. Phase 1:
    /// find every memory that extends `mem` by one certifiable promise,
    /// except those that `skip` rejects (before certifying them) and those
    /// the reduction prunes. Returns whether any execution completed (or
    /// faulted) with this memory, and the extended memories.
    fn visit(&mut self, mem: &[Msg], skip: &dyn Fn(&[Msg]) -> bool) -> (bool, Vec<Vec<Msg>>) {
        let test = self.test;
        self.results.stats.memories += 1;
        let mut per_thread = Vec::new();
        let mut faults = Vec::new();
        let mut fulfills = Vec::new();
        let mut candidates = Vec::new();
        let mut promising = Vec::new();
        let mut collect_states = 0u64;
        for tid in 0..test.threads.len() {
            let mut c = self.solo(tid, mem, Mode::Collect);
            collect_states += c.visited.len() as u64;
            let cands = std::mem::take(&mut c.candidates);
            promising.push(cands.iter().map(|x| !self.cfg.filter || c.promising(x)).collect::<Vec<_>>());
            self.results.truncated |= c.truncated;
            self.results.capped |= c.capped.get();
            faults.push(std::mem::take(&mut c.faults));
            fulfills.push(c.fulfills);
            per_thread.push(std::mem::take(&mut c.finals));
            candidates.push(cands);
        }
        // Phase 2. A thread's fault is real only if every other thread can
        // write all its messages in the memory (and then stop): otherwise a
        // promise that was certifiable when made can no longer be fulfilled,
        // for example because the faulting thread holds a lock forever.
        let mut found = false;
        for (tid, f) in faults.into_iter().enumerate() {
            if !f.is_empty() && fulfills.iter().enumerate().all(|(t, &ok)| ok || t == tid) {
                self.results.faults.extend(f);
                found = true;
            }
        }
        // If some thread cannot complete, neither can the execution.
        if per_thread.iter().all(|f| !f.is_empty()) {
            let byte =
                |b: u64| mem.iter().rev().find(|m| m.covers(b)).map(|m| m.byte(b)).unwrap_or_else(|| test.init_byte(b));
            for state in self.proj.combine(&per_thread, &|o| match o {
                Obs::Mem(a) => test.loc_value(a, &byte),
                Obs::Reg(..) => unreachable!(),
            }) {
                self.results.insert(state);
            }
            found = true;
        }
        // Phase 1.
        let need_observed = self.cfg.dynamic_por
            && match (&self.reduction, mem.last()) {
                (Some(r), Some(last)) => candidates.iter().enumerate().any(|(tid, cs)| {
                    cs.iter().any(|&(addr, size, val)| {
                        let m = Msg { addr, size, val, tid };
                        !r.prunable(last, &m) && r.may_prune_after(last, &m)
                    })
                }),
                _ => false,
            };
        let budget = (OBSERVE_STATES_PER_COLLECT * collect_states).clamp(MIN_OBSERVE_STATES, MAX_OBSERVE_STATES);
        let observed = if need_observed { self.observe(mem, budget) } else { None };
        let mut out = Vec::new();
        for (tid, cands) in candidates.into_iter().enumerate() {
            for ((addr, size, val), promising) in cands.into_iter().zip(&promising[tid]) {
                let msg = Msg { addr, size, val, tid };
                if let (Some(r), Some(last)) = (&self.reduction, mem.last()) {
                    if r.prunable(last, &msg) || observed.as_ref().is_some_and(|o| r.prunable_after(last, &msg, o)) {
                        self.results.stats.pruned += 1;
                        continue;
                    }
                }
                let mut m2 = mem.to_vec();
                m2.push(msg);
                if skip(&m2) {
                    continue;
                }
                if !promising && !self.cfg.check_filter {
                    self.results.stats.filtered += 1;
                    continue;
                }
                let cert = self.solo(tid, &m2, Mode::Certify);
                assert!(
                    *promising || !cert.certified,
                    "a candidate the filter rejects certifies: P{} writes {:#x} = {:#x} after {:?}",
                    tid, addr, val, mem
                );
                if cert.certified {
                    out.push(m2);
                } else {
                    self.results.truncated |= cert.truncated;
                    self.results.capped |= cert.capped.get();
                }
            }
        }
        (found, out)
    }

    /// Explore every memory.
    fn exhaustive(&mut self) {
        let mut seen: FxHashSet<Vec<Msg>> = FxHashSet::default();
        let mut stack: Vec<Vec<Msg>> = vec![Vec::new()];
        while let Some(mem) = stack.pop() {
            if !seen.insert(mem.clone()) {
                continue;
            }
            if self.cfg.max_memories.is_some_and(|m| self.results.stats.memories >= m) {
                self.results.over_budget = true;
                return;
            }
            let (_, children) = self.visit(&mem, &|m| seen.contains(m));
            stack.extend(children);
        }
    }

    /// Explore `walks` random paths through the memories, each from the
    /// empty memory until no promise can be added. A path that ends without
    /// a final state backtracks: such dead ends are common with the
    /// reduction, whose pruning can leave a memory with no way to complete.
    ///
    /// The paths are chosen as in PCT (Burckhardt et al., "A Randomized
    /// Scheduler with Probabilistic Guarantees of Finding Bugs", ASPLOS
    /// 2010): each walk gives the threads random priorities, and each step
    /// adds a promise of the highest-priority thread that has one (any of
    /// them, at random). At `pct_depth` random depths, the thread that would
    /// go next drops below all the others. So a thread runs ahead of the
    /// others for long stretches, as the promises that ARM-only behaviors
    /// need usually do (a write promised early, and the other threads' writes
    /// that a late read of the promising thread then reads). Choosing among
    /// all the promises at random instead favors whichever thread has the
    /// most of them at each step, and found the bugs in `c/bench` in up to
    /// ten times as many walks.
    fn sample(&mut self, walks: u64, seed: u64) {
        self.results.sampled = true;
        let mut rng = seed;
        let n = self.test.threads.len();
        // Memories already finished, and whether they had final states.
        let mut finished: FxHashMap<Vec<Msg>, bool> = FxHashMap::default();
        // The children of each memory that are not known to be dead ends.
        let mut children: FxHashMap<Vec<Msg>, Vec<Vec<Msg>>> = FxHashMap::default();
        // The longest memory so far, from which the depths of the changes
        // are drawn.
        let mut longest = 1u64;
        'walks: for walk in 1..=walks {
            self.results.stats.walks = walk;
            if self.results.stats.first_hit.is_none() && self.results.observed(self.test) == Some(true) {
                self.results.stats.first_hit = Some(walk - 1);
            }
            // Priorities, highest first; a dropped thread gets one below all
            // the initial ones and those dropped before.
            let mut prio: Vec<u64> = (0..n).map(|_| splitmix64(&mut rng) | 1 << 63).collect();
            let changes: Vec<usize> = (0..self.cfg.pct_depth).map(|_| (1 + splitmix64(&mut rng) % longest) as usize).collect();
            let mut low = 1u64 << 62;
            let mut path: Vec<Vec<Msg>> = vec![Vec::new()];
            while let Some(mem) = path.last().cloned() {
                longest = longest.max(mem.len() as u64);
                if !finished.contains_key(&mem) {
                    let (found, c) = self.visit(&mem, &|_| false);
                    finished.insert(mem.clone(), found);
                    children.insert(mem.clone(), c);
                }
                let c = &children[&mem];
                let first = |prio: &[u64]| c.iter().map(|m| m.last().unwrap().tid).max_by_key(|&t| prio[t]);
                if let Some(mut t) = first(&prio) {
                    if changes.contains(&mem.len()) {
                        prio[t] = low;
                        low -= 1;
                        t = first(&prio).unwrap();
                    }
                    let mine: Vec<&Vec<Msg>> = c.iter().filter(|m| m.last().unwrap().tid == t).collect();
                    path.push(mine[(splitmix64(&mut rng) % mine.len() as u64) as usize].clone());
                    continue;
                }
                if finished[&mem] {
                    continue 'walks;
                }
                // A dead end: never come here again.
                path.pop();
                match path.last() {
                    Some(parent) => children.get_mut(parent).unwrap().retain(|m| *m != mem),
                    // Every memory has been explored.
                    None => break 'walks,
                }
            }
        }
        if self.results.stats.first_hit.is_none() && self.results.observed(self.test) == Some(true) {
            self.results.stats.first_hit = Some(self.results.stats.walks);
        }
    }
}

/// A small, fast pseudo-random generator (SplitMix64).
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}
