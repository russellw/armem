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
use crate::isa::{Barrier, Code, LoadKind, RmwOp, Size};
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

/// Most states `Mode::Observe` may visit before it gives up.
const MAX_OBSERVE_STATES: u64 = 200_000;

/// Most values a read of later writes may return in `Mode::Observe`, where
/// the analysis does not know them, before it gives up.
const MAX_UNKNOWN_VALUES: usize = 4096;

/// Most rounds of `Explorer::observe` before it gives up.
const MAX_OBSERVE_ROUNDS: usize = 8;

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
    reads_x: std::cell::Cell<bool>,
    /// For `Mode::Observe`: the values other threads' later writes may
    /// store, for bytes whose values the analysis does not know (see
    /// `Explorer::observe`); whether a read used them; and the values of
    /// this thread's new writes.
    unknown: Arc<Written>,
    used_unknown: std::cell::Cell<bool>,
    written: Written,
    gave_up: std::cell::Cell<bool>,
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
            mode,
            visited: FxHashMap::default(),
            candidates: BTreeSet::new(),
            reduction: None,
            observed_reads: Default::default(),
            observed_writes: Default::default(),
            reads_x: Default::default(),
            unknown: Default::default(),
            used_unknown: Default::default(),
            written: Default::default(),
            gave_up: Default::default(),
            states: 0,
            placeable: FxHashSet::default(),
            fulfilling: FxHashSet::default(),
            finals: BTreeSet::new(),
            final_regs: &proj.thread_regs[tid],
            final_mems: &proj.thread_mems[tid],
            certified: false,
            truncated: false,
            faults: BTreeSet::new(),
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
            for (i, &t) in srcs.iter().enumerate().rev() {
                let b = a + i as u64;
                let byte = match st.fwd.get(b) {
                    Some(f) if t == FUTURE => (f.val >> (8 * (b - f.addr))) as u8,
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
            out.push(ReadResult { state: s, val, srcs, dep: dep(vpost, ppost) });
        }
        if self.mode == Mode::Observe {
            let ps = pieces(a, n, granule, self.lse2);
            self.observe_read(st, addr, kind, n, &ps, vpre, &mut out);
        }
        out
    }

    /// For `Mode::Observe`: record the reads ordered after `x` (the last
    /// message of `base`) among `out`, the results of a read of `n` bytes
    /// (in single-copy-atomic pieces `ps`) with pre-view `vpre`, and whether
    /// one reads `x`; and add those that read a later write by another
    /// thread.
    #[allow(clippy::too_many_arguments)]
    fn observe_read(&self, st: &TState, addr: RV, kind: ReadKind, n: u64, ps: &[(u64, u64)], vpre: View, out: &mut Vec<ReadResult>) {
        let single = ps.len() == 1;
        let a = addr.val;
        let l = self.base.len() as View;
        let x = &self.base[l as usize - 1];
        let mut record = false;
        for r in out.iter() {
            record |= (vpre == l || r.srcs.contains(&l)) && r.srcs.iter().any(|&t| t <= l);
            if r.srcs.contains(&l) {
                self.reads_x.set(true);
            }
        }
        let Some(red) = &self.reduction else { return };
        let vals = match red.future_values(self.tid, a, n, &|b| self.test.init_byte(b)) {
            crate::reduce::Future::None => Vec::new(),
            crate::reduce::Future::Unknown => {
                self.used_unknown.set(true);
                match self.unknown_read_values(a, ps, out, red) {
                    Some(v) => v,
                    None => {
                        self.gave_up.set(true);
                        return;
                    }
                }
            }
            crate::reduce::Future::Values(v) => v,
        };
        // A read taking some bytes from later writes and some from the
        // memory is represented by a read of later writes only (its views
        // are the same), so it counts as reading `x` if it might, and is
        // recorded if it might read `x` or is ordered after it.
        let mixed_x = x.addr < a + n && a < x.addr + x.size.bytes() && (!single || red.may_mix(self.tid, a, n));
        if !vals.is_empty() && mixed_x {
            self.reads_x.set(true);
        }
        if !vals.is_empty() && (vpre == l || mixed_x) {
            record = true;
        }
        if record {
            self.observed_reads.borrow_mut().insert((a, n));
        }
        let acquire = kind.load != LoadKind::Plain;
        for val in vals {
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
            out.push(ReadResult { state: s, val, srcs: vec![FUTURE; n as usize], dep: dep(vpost, ppost) });
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
        // Fulfill an outstanding promise.
        for &t in &st.promises {
            if *self.msg(ext, t) == new_msg && vpre < t && coh_ok(t) && ok_atomic(t) {
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

    /// Whether a run that ends in `s` completes an execution with the memory
    /// as it is: the thread has fulfilled all its promises and appended no
    /// new write.
    fn complete(&self, s: &TState, ext: &[Msg]) -> bool {
        self.mode == Mode::Collect && s.promises.is_empty() && ext.is_empty()
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
                base.xcl = Some(Monitor { addr: addr.val, size, srcs: Vec::new(), view: dep.view });
                out.push(base);
            }
            MemOp::LoadExclPair { rt, rt2, size, .. } => {
                let total = size.double();
                let (v, dep) = base.core.private_load(addr, total);
                let (lo, hi) = split_pair(v, size);
                base.core.set(rt, RV { val: lo, ..dep });
                base.core.set(rt2, RV { val: hi, ..dep });
                base.xcl = Some(Monitor { addr: addr.val, size: total, srcs: Vec::new(), view: dep.view });
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
                out.push(f);
                if st.xcl.as_ref().is_some_and(|m| m.addr == addr.val && m.size == total) {
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
                    if excl {
                        s.xcl = Some(Monitor { addr: addr.val, size, srcs: r.srcs, view: r.dep.view });
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
                    s.xcl = Some(Monitor { addr: addr.val, size: total, srcs: r.srcs, view: r.dep.view });
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
                out.push((f, ext.to_vec()));
                if let Some(m) = &st.xcl {
                    if m.addr == addr.val && m.size == total {
                        let srcs = m.srcs.clone();
                        let val = pair_value(data.val, data2.val, size);
                        let kind = WriteKind { release, xview: m.view, full: false };
                        let data = RV::joined(0, data, data2);
                        for (mut s, e) in self.do_write(&st, ext, addr, total, val, data, kind, Some((&srcs[..], true))) {
                            s.xcl = None;
                            s.core.set(rs, status(true));
                            s.core.retire(prog);
                            out.push((s, e));
                        }
                    }
                }
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
                out.push((f, ext.to_vec()));
                if let Some(m) = &st.xcl {
                    if m.addr == addr.val && m.size == size {
                        let srcs = m.srcs.clone();
                        let kind = WriteKind { release, xview: m.view, full: false };
                        let val = size.mask(data.val) as u128;
                        for (mut s, e) in self.do_write(&st, ext, addr, size, val, data, kind, Some((&srcs[..], true))) {
                            s.xcl = None;
                            s.core.set(rs, status(true));
                            s.core.retire(prog);
                            out.push((s, e));
                        }
                    }
                }
            }
            MemOp::Rmw { addr, op, operand, cmp, rt, size, acquire, release, noret } => {
                let load = if acquire { LoadKind::Acquire } else { LoadKind::Plain };
                for r in self.do_read(&st, ext, addr, ReadKind { load, noret }, size, size.bytes()) {
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
                            for (s, e) in self.do_write(&r.state, ext, addr, size, new as u128, data, kind, atomic) {
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

    fn run(&mut self, start: TState) {
        let mut queue = StepQueue::new();
        queue.push(start.core.steps, (start, Vec::new()));
        while let Some((st, ext)) = queue.pop() {
            if self.mode == Mode::Observe {
                self.states += 1;
                if self.gave_up.get() || self.states > MAX_OBSERVE_STATES {
                    self.gave_up.set(true);
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
        results: Outcomes::new(test.observables()),
    };
    match cfg.sample {
        None => ex.exhaustive(),
        Some(walks) => ex.sample(walks, cfg.seed),
    }
    ex.results
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
    results: Outcomes,
}

impl<'a> Explorer<'a> {
    /// Run thread `tid` alone against `mem`.
    fn solo(&mut self, tid: usize, mem: &[Msg], mode: Mode) -> Solo<'a> {
        self.solo_with(tid, mem, mode, Arc::default())
    }

    /// The same, with `Solo::unknown` for `Mode::Observe`.
    fn solo_with(&mut self, tid: usize, mem: &[Msg], mode: Mode, unknown: Arc<Written>) -> Solo<'a> {
        loop {
            let mut s = Solo::new(self.test, self.cfg, self.proj, tid, mem.to_vec(), mode, self.live[tid].clone());
            if mode == Mode::Observe {
                s.reduction = self.reduction.clone();
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
    /// that is not known.
    ///
    /// Where the analysis does not know what a later write may store (a
    /// counter, which it widens to any value), the values come from the
    /// explorations themselves, in rounds: in the first, a read of those
    /// bytes returns no later write; in each next one, it returns the
    /// values the other threads wrote in the rounds before. Once a round
    /// adds no value, every later write's is among them: in Promising-ARM
    /// a write depends only on reads of earlier messages, so (by induction
    /// on the memory order) the `k`th later write's value is found by
    /// round `k`, and that round has added it.
    fn observe(&mut self, mem: &[Msg]) -> Option<crate::reduce::Observed> {
        let n = self.test.threads.len();
        let mut written: Vec<Written> = vec![Written::default(); n];
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
                let mut o = self.solo_with(tid, mem, Mode::Observe, Arc::new(unknown));
                if o.gave_up.get() {
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
                obs.reads_x.push(o.reads_x.get());
            }
            if !used_unknown || !grown {
                self.results.stats.observed += 1;
                return Some(obs);
            }
            written = new_written;
        }
        self.results.stats.observe_failed += 1;
        None
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
        let mut faulted = false;
        let mut candidates = Vec::new();
        let mut promising = Vec::new();
        for tid in 0..test.threads.len() {
            let mut c = self.solo(tid, mem, Mode::Collect);
            let cands = std::mem::take(&mut c.candidates);
            promising.push(cands.iter().map(|x| !self.cfg.filter || c.promising(x)).collect::<Vec<_>>());
            self.results.truncated |= c.truncated;
            faulted |= !c.faults.is_empty();
            self.results.faults.extend(std::mem::take(&mut c.faults));
            per_thread.push(std::mem::take(&mut c.finals));
            candidates.push(cands);
        }
        // Phase 2. If some thread cannot complete, neither can the execution.
        let mut found = faulted;
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
        let observed = if need_observed { self.observe(mem) } else { None };
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
    fn sample(&mut self, walks: u64, seed: u64) {
        self.results.sampled = true;
        let mut rng = seed;
        // Memories already finished, and whether they had final states.
        let mut finished: FxHashMap<Vec<Msg>, bool> = FxHashMap::default();
        // The children of each memory that are not known to be dead ends.
        let mut children: FxHashMap<Vec<Msg>, Vec<Vec<Msg>>> = FxHashMap::default();
        'walks: for _ in 0..walks {
            let mut path: Vec<Vec<Msg>> = vec![Vec::new()];
            while let Some(mem) = path.last().cloned() {
                if !finished.contains_key(&mem) {
                    let (found, c) = self.visit(&mem, &|_| false);
                    finished.insert(mem.clone(), found);
                    children.insert(mem.clone(), c);
                }
                let c = &children[&mem];
                if !c.is_empty() {
                    path.push(c[(splitmix64(&mut rng) % c.len() as u64) as usize].clone());
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
