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

use crate::core::{Core, MemOp, RV, Step, VR, View, low_bytes, pair_value, rmw_result, split_pair};
use crate::isa::{Barrier, Code, LoadKind, RmwOp, Size};
use crate::litmus::{Obs, Test};
use crate::Options;
use crate::outcome::{Outcomes, Projection};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
    coh: BTreeMap<u64, View>,
    /// Forwarding bank: latest own write per byte.
    fwd: BTreeMap<u64, Fwd>,
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
            coh: BTreeMap::new(),
            fwd: BTreeMap::new(),
            xcl: None,
            promises: BTreeSet::new(),
        }
    }

    fn coh(&self, b: u64) -> View {
        self.coh.get(&b).copied().unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Find the writes the thread might promise: explore like `Certify`
    /// (without stopping) and record every newly appended write. Up to the
    /// point where a promisable write executes, this exploration is at least
    /// as permissive as any run certifying that promise, so no candidate is
    /// missed.
    Collect,
    /// Search for a run that fulfills all outstanding promises; new writes
    /// may be appended to (a local copy of) memory.
    Certify,
    /// Run to completion fulfilling all promises, with no new writes.
    Final,
}

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
    /// Visited states (with the step counter zeroed) -> fewest steps seen.
    visited: HashMap<(TState, Vec<Msg>), u32>,
    candidates: BTreeSet<Candidate>,
    finals: BTreeSet<Vec<u64>>,
    final_regs: &'a [u8],
    certified: bool,
    truncated: bool,
    /// Faults reached (in `Final` mode, with every promise fulfilled).
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
    fn new(test: &'a Test, cfg: &Options, proj: &'a Projection, tid: usize, base: Vec<Msg>, mode: Mode) -> Solo<'a> {
        Solo {
            test,
            prog: &test.threads[tid].code,
            tid,
            base,
            fuel: cfg.fuel,
            lse2: cfg.lse2,
            mode,
            visited: HashMap::new(),
            candidates: BTreeSet::new(),
            finals: BTreeSet::new(),
            final_regs: &proj.thread_regs[tid],
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
        t == 0 || self.msg(ext, t).tid != self.tid
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
                val = (val << 8) | self.byte_value(ext, b, t) as u128;
                match st.fwd.get(&b) {
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
            for i in 0..n {
                let b = a + i;
                s.coh.insert(b, st.coh(b).max(vpost));
            }
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
        out
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
        let apply = |t: View| {
            let mut s = st.clone();
            let fwd = Fwd {
                ts: t,
                view: addr.view.max(data.view),
                pick: addr.pick.max(data.pick),
                xview: kind.xview,
            };
            for b in a..a + n {
                s.coh.insert(b, t);
                s.fwd.insert(b, fwd);
            }
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
        let coh_ok = |t: View| (a..a + n).all(|b| st.coh(b) < t);
        let mut out = Vec::new();
        // Fulfill an outstanding promise.
        for &t in &st.promises {
            if *self.msg(ext, t) == new_msg && vpre < t && coh_ok(t) && ok_atomic(t) {
                out.push((apply(t), ext.to_vec()));
            }
        }
        // Or (except in the final phase) write a new message at the end of memory.
        if self.mode != Mode::Final {
            let t = self.len(ext) + 1;
            if ok_atomic(t) {
                if self.mode == Mode::Collect {
                    self.candidates.insert((a, size, val));
                }
                let mut e = ext.to_vec();
                e.push(new_msg);
                out.push((apply(t), e));
            }
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

    fn successors(&mut self, st: &TState, ext: &[Msg]) -> Vec<Succ> {
        let mut s = st.clone();
        match s.core.step(self.prog) {
            Step::Done => {
                if self.mode == Mode::Final && s.promises.is_empty() {
                    let regs = self.final_regs.iter().map(|&r| s.core.regs[r as usize].val).collect();
                    self.finals.insert(regs);
                }
                vec![]
            }
            Step::Local { ctrl } => {
                if let Some(v) = ctrl {
                    s.v_cap = s.v_cap.max(v);
                }
                vec![(s, ext.to_vec())]
            }
            Step::Mem(op) => self.mem_successors(s, ext, op),
            // A fault ends the execution. It is real if the thread's promises
            // are fulfilled, since the other threads' promises are certified.
            Step::Fault(e) => {
                if self.mode == Mode::Final && s.promises.is_empty() {
                    self.faults.insert(format!("P{}: {}", self.tid, e));
                }
                vec![]
            }
        }
    }

    fn mem_successors(&mut self, st: TState, ext: &[Msg], op: MemOp) -> Vec<Succ> {
        let prog = self.prog;
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

    fn run(&mut self, start: TState) {
        let mut stack: Vec<Succ> = vec![(start, Vec::new())];
        while let Some((st, ext)) = stack.pop() {
            if self.mode == Mode::Certify && st.promises.is_empty() {
                self.certified = true;
                return;
            }
            // A state already reached with no more steps used has nothing new
            // to offer; this collapses spin loops.
            let mut key = st.clone();
            key.core.steps = 0;
            let key = (key, ext.clone());
            match self.visited.get(&key) {
                Some(&n) if n <= st.core.steps => continue,
                _ => {
                    self.visited.insert(key, st.core.steps);
                }
            }
            if st.core.steps >= self.fuel && st.core.pc < self.prog.instrs.len() {
                self.truncated = true;
                continue;
            }
            let succs = self.successors(&st, &ext);
            stack.extend(succs);
        }
    }
}

pub fn explore(test: &Test, cfg: &Options) -> Outcomes {
    if test.threads.len() == 1 {
        // A lone thread sees its own accesses in program order: its behaviors
        // are exactly the sequentially consistent ones.
        return crate::interleave::explore(test, crate::interleave::Kind::Sc, cfg.fuel);
    }
    let proj = Projection::new(test);
    let mut ex = Explorer {
        test,
        cfg,
        proj: &proj,
        reduction: if cfg.por { crate::reduce::analyze(test, cfg.fuel) } else { None },
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
    reduction: Option<crate::reduce::Reduction>,
    results: Outcomes,
}

impl<'a> Explorer<'a> {
    fn solo(&self, tid: usize, mem: &[Msg], mode: Mode) -> Solo<'a> {
        Solo::new(self.test, self.cfg, self.proj, tid, mem.to_vec(), mode)
    }

    fn start(&self, tid: usize, mem: &[Msg]) -> TState {
        let test = self.test;
        let mut st = TState::new(test.init_regs[tid], tid, test.threads.len(), test.threads[tid].entry);
        st.promises = (1..=mem.len() as View).filter(|&t| mem[t as usize - 1].tid == tid).collect();
        st
    }

    /// Phase 2: run every thread to completion against `mem`, fulfilling
    /// exactly its promises, and record the final states. Returns whether
    /// any execution completed (or faulted) with this memory.
    fn finish(&mut self, mem: &[Msg]) -> bool {
        let test = self.test;
        self.results.stats.memories += 1;
        let mut per_thread = Vec::new();
        let mut faulted = false;
        for tid in 0..test.threads.len() {
            let mut s = self.solo(tid, mem, Mode::Final);
            s.run(self.start(tid, mem));
            self.results.truncated |= s.truncated;
            faulted |= !s.faults.is_empty();
            self.results.faults.extend(std::mem::take(&mut s.faults));
            per_thread.push(s.finals);
        }
        if per_thread.iter().any(|f| f.is_empty()) {
            // Some thread cannot complete, so neither can the execution.
            return faulted;
        }
        let byte =
            |b: u64| mem.iter().rev().find(|m| m.covers(b)).map(|m| m.byte(b)).unwrap_or_else(|| test.init_byte(b));
        for state in self.proj.combine(&per_thread, &|o| match o {
            Obs::Mem(a) => test.loc_value(a, &byte),
            Obs::Reg(..) => unreachable!(),
        }) {
            self.results.insert(state);
        }
        true
    }

    /// Phase 1: every memory that extends `mem` by one certifiable promise,
    /// except those that `skip` rejects (before certifying them) and those
    /// the reduction prunes.
    fn children(&mut self, mem: &[Msg], skip: &dyn Fn(&[Msg]) -> bool) -> Vec<Vec<Msg>> {
        let mut out = Vec::new();
        for tid in 0..self.test.threads.len() {
            let mut c = self.solo(tid, mem, Mode::Collect);
            c.run(self.start(tid, mem));
            self.results.truncated |= c.truncated;
            for (addr, size, val) in c.candidates {
                let msg = Msg { addr, size, val, tid };
                if let (Some(r), Some(last)) = (&self.reduction, mem.last()) {
                    if r.prunable(last, &msg) {
                        self.results.stats.pruned += 1;
                        continue;
                    }
                }
                let mut m2 = mem.to_vec();
                m2.push(msg);
                if skip(&m2) {
                    continue;
                }
                let mut cert = self.solo(tid, &m2, Mode::Certify);
                cert.run(self.start(tid, &m2));
                if cert.certified {
                    out.push(m2);
                } else {
                    self.results.truncated |= cert.truncated;
                }
            }
        }
        out
    }

    /// Explore every memory.
    fn exhaustive(&mut self) {
        let mut seen: HashSet<Vec<Msg>> = HashSet::new();
        let mut stack: Vec<Vec<Msg>> = vec![Vec::new()];
        while let Some(mem) = stack.pop() {
            if !seen.insert(mem.clone()) {
                continue;
            }
            if self.cfg.max_memories.is_some_and(|m| self.results.stats.memories >= m) {
                self.results.over_budget = true;
                return;
            }
            self.finish(&mem);
            let children = self.children(&mem, &|m| seen.contains(m));
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
        let mut finished: HashMap<Vec<Msg>, bool> = HashMap::new();
        // The children of each memory that are not known to be dead ends.
        let mut children: HashMap<Vec<Msg>, Vec<Vec<Msg>>> = HashMap::new();
        'walks: for _ in 0..walks {
            let mut path: Vec<Vec<Msg>> = vec![Vec::new()];
            while let Some(mem) = path.last().cloned() {
                if !finished.contains_key(&mem) {
                    let found = self.finish(&mem);
                    finished.insert(mem.clone(), found);
                }
                if !children.contains_key(&mem) {
                    let c = self.children(&mem, &|_| false);
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
