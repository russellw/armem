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
//! per byte. An aligned read is single-copy atomic: it takes effect at one
//! point of the memory order, and each byte reads the latest write covering
//! it at that point, so different bytes may come from different writes but
//! no aligned write is ever seen torn. An unaligned read behaves as
//! independent byte reads. (This mixed-size extension is ours; it is not
//! part of the published Promising-ARM model.)

use crate::core::{Core, MemOp, RV, Step, View, rmw_result};
use crate::isa::{Barrier, Instr, LoadKind, Size};
use crate::litmus::{Obs, Test};
use crate::outcome::{Outcomes, Projection};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Msg {
    pub addr: u64,
    pub size: Size,
    pub val: u64,
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
    view: View,
    xcl: bool,
}

/// Exclusive monitor set by a load-exclusive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Monitor {
    addr: u64,
    size: Size,
    /// Timestamp each byte was read from.
    srcs: Vec<View>,
}

/// State of one thread under the Promising-ARM model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TState {
    core: Core,
    /// Views: reads/writes so far (old), lower bounds for future reads/writes (new).
    v_rold: View,
    v_wold: View,
    v_rnew: View,
    v_wnew: View,
    /// Control and address dependencies of everything so far.
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
    fn new(regs: [u64; 31]) -> TState {
        TState {
            core: Core::new(regs),
            v_rold: 0,
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

/// Outcome of one way of performing a read.
struct ReadResult {
    state: TState,
    val: u64,
    srcs: Vec<View>,
    vpost: View,
}

/// A candidate write to promise: (address, size, value).
type Candidate = (u64, Size, u64);

/// Exhaustive exploration of one thread running alone against a memory.
struct Solo<'a> {
    test: &'a Test,
    prog: &'a [Instr],
    tid: usize,
    base: Vec<Msg>,
    fuel: u32,
    mode: Mode,
    /// Visited states (with the step counter zeroed) -> fewest steps seen.
    visited: HashMap<(TState, Vec<Msg>), u32>,
    candidates: BTreeSet<Candidate>,
    finals: BTreeSet<Vec<u64>>,
    final_regs: &'a [u8],
    certified: bool,
    truncated: bool,
}

/// A successor state, with any messages newly appended to memory.
type Succ = (TState, Vec<Msg>);

impl<'a> Solo<'a> {
    fn new(test: &'a Test, cfg: &Config, proj: &'a Projection, tid: usize, base: Vec<Msg>, mode: Mode) -> Solo<'a> {
        Solo {
            test,
            prog: &test.threads[tid],
            tid,
            base,
            fuel: cfg.fuel,
            mode,
            visited: HashMap::new(),
            candidates: BTreeSet::new(),
            finals: BTreeSet::new(),
            final_regs: &proj.thread_regs[tid],
            certified: false,
            truncated: false,
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

    /// The latest write covering byte `b` at or before timestamp `t`.
    fn latest(&self, ext: &[Msg], b: u64, t: View) -> View {
        (1..=t).rev().find(|&u| self.msg(ext, u).covers(b)).unwrap_or(0)
    }

    /// Possible per-byte sources for a single-copy-atomic read of bytes
    /// `a..a+n`. The read takes effect at one point `T` in the memory order
    /// (at or after `bound`), and every byte reads the latest write covering
    /// it at or before `T`. Only values of `T` where a covering write occurs
    /// give distinct results.
    fn atomic_read_choices(&self, ext: &[Msg], a: u64, n: u64, bound: View) -> Vec<Vec<View>> {
        let len = self.len(ext);
        let bound = bound.min(len);
        let mut points = vec![bound];
        points.extend((bound + 1..=len).filter(|&t| {
            let m = self.msg(ext, t);
            m.addr < a + n && a < m.addr + m.size.bytes()
        }));
        let mut out: Vec<Vec<View>> = Vec::new();
        for t in points {
            let srcs: Vec<View> = (a..a + n).map(|b| self.latest(ext, b, t)).collect();
            if !out.contains(&srcs) {
                out.push(srcs);
            }
        }
        out
    }

    /// Perform a read of `size` bytes at `addr.val`; one result per allowed
    /// choice of source writes.
    fn do_read(&self, st: &TState, ext: &[Msg], addr: RV, kind: LoadKind, size: Size) -> Vec<ReadResult> {
        let a = addr.val;
        let n = size.bytes();
        let mut vpre = addr.view.max(st.v_rnew);
        if kind == LoadKind::Acquire {
            vpre = vpre.max(st.v_rel);
        }
        let combos: Vec<Vec<View>> = if a % n == 0 {
            let bound = (a..a + n).map(|b| st.coh(b)).fold(vpre, View::max);
            self.atomic_read_choices(ext, a, n, bound)
        } else {
            // Unaligned: each byte is an independent single-copy-atomic access.
            let mut combos: Vec<Vec<View>> = vec![Vec::new()];
            for b in a..a + n {
                let choices = self.atomic_read_choices(ext, b, 1, vpre.max(st.coh(b)));
                combos = combos
                    .iter()
                    .flat_map(|c| {
                        choices.iter().map(move |t| {
                            let mut c2 = c.clone();
                            c2.push(t[0]);
                            c2
                        })
                    })
                    .collect();
            }
            combos
        };

        let mut out = Vec::new();
        // Reading one of our own unfulfilled promises makes it unfulfillable
        // (coherence would then require it to be later than itself).
        for srcs in combos.into_iter().filter(|c| !c.iter().any(|t| st.promises.contains(t))) {
            let mut val = 0u64;
            let mut rview = 0;
            for (i, &t) in srcs.iter().enumerate().rev() {
                let b = a + i as u64;
                val = (val << 8) | self.byte_value(ext, b, t) as u64;
                let v = match st.fwd.get(&b) {
                    Some(f) if f.ts == t && !(f.xcl && kind == LoadKind::Acquire) => f.view,
                    _ => t,
                };
                rview = rview.max(v);
            }
            let vpost = vpre.max(rview);
            let mut s = st.clone();
            for i in 0..n {
                let b = a + i;
                s.coh.insert(b, st.coh(b).max(vpost));
            }
            s.v_rold = s.v_rold.max(vpost);
            if kind != LoadKind::Plain {
                s.v_rnew = s.v_rnew.max(vpost);
                s.v_wnew = s.v_wnew.max(vpost);
            }
            s.v_cap = s.v_cap.max(addr.view);
            out.push(ReadResult { state: s, val, srcs, vpost });
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

    /// Perform a write of `val` (`size` bytes) to `addr.val`. `atomic`
    /// imposes an atomicity constraint relative to an earlier read
    /// (per-byte sources, and whether only other threads' writes count).
    #[allow(clippy::too_many_arguments)]
    fn do_write(
        &mut self,
        st: &TState,
        ext: &[Msg],
        addr: RV,
        size: Size,
        val: u64,
        data_view: View,
        release: bool,
        xcl: bool,
        atomic: Option<(&[View], bool)>,
    ) -> Vec<Succ> {
        let a = addr.val;
        let n = size.bytes();
        let new_msg = Msg { addr: a, size, val, tid: self.tid };
        let mut vpre = addr.view.max(data_view).max(st.v_wnew).max(st.v_cap);
        if release {
            vpre = vpre.max(st.v_rold).max(st.v_wold);
        }
        let apply = |t: View| {
            let mut s = st.clone();
            let fwd = Fwd { ts: t, view: addr.view.max(data_view), xcl };
            for b in a..a + n {
                s.coh.insert(b, t);
                s.fwd.insert(b, fwd);
            }
            s.v_wold = s.v_wold.max(t);
            s.v_cap = s.v_cap.max(addr.view);
            if release {
                s.v_rel = s.v_rel.max(t);
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
        }
    }

    fn mem_successors(&mut self, st: TState, ext: &[Msg], op: MemOp) -> Vec<Succ> {
        let prog = self.prog;
        let mut out = Vec::new();
        match op {
            MemOp::Barrier(b) => {
                let mut s = st;
                let (rold, wold) = (s.v_rold, s.v_wold);
                match b {
                    Barrier::Full => {
                        let v = rold.max(wold);
                        s.v_rnew = s.v_rnew.max(v);
                        s.v_wnew = s.v_wnew.max(v);
                    }
                    Barrier::Ld => {
                        s.v_rnew = s.v_rnew.max(rold);
                        s.v_wnew = s.v_wnew.max(rold);
                    }
                    Barrier::St => s.v_wnew = s.v_wnew.max(wold),
                    Barrier::Isb => s.v_rnew = s.v_rnew.max(s.v_cap),
                }
                s.core.retire(prog);
                out.push((s, ext.to_vec()));
            }
            MemOp::Load { addr, rt, size, signed, kind, excl } => {
                for r in self.do_read(&st, ext, addr, kind, size) {
                    let mut s = r.state;
                    s.core.set(rt, RV { val: size.extend(r.val, signed), view: r.vpost });
                    if excl {
                        s.xcl = Some(Monitor { addr: addr.val, size, srcs: r.srcs });
                    }
                    s.core.retire(prog);
                    out.push((s, ext.to_vec()));
                }
            }
            MemOp::LoadPair { addr, rt, rt2, size, signed } => {
                // Two independent reads: the second does not depend on the first.
                let addr2 = RV { val: addr.val + size.bytes(), view: addr.view };
                for r1 in self.do_read(&st, ext, addr, LoadKind::Plain, size) {
                    for r2 in self.do_read(&r1.state, ext, addr2, LoadKind::Plain, size) {
                        let mut s = r2.state;
                        s.core.set(rt, RV { val: size.extend(r1.val, signed), view: r1.vpost });
                        s.core.set(rt2, RV { val: size.extend(r2.val, signed), view: r2.vpost });
                        s.core.retire(prog);
                        out.push((s, ext.to_vec()));
                    }
                }
            }
            MemOp::Store { addr, data, size, release } => {
                for (mut s, e) in self.do_write(&st, ext, addr, size, size.mask(data.val), data.view, release, false, None) {
                    s.core.retire(prog);
                    out.push((s, e));
                }
            }
            MemOp::StorePair { addr, data, data2, size } => {
                let addr2 = RV { val: addr.val + size.bytes(), view: addr.view };
                for (s1, e1) in self.do_write(&st, ext, addr, size, size.mask(data.val), data.view, false, false, None) {
                    for (mut s, e) in
                        self.do_write(&s1, &e1, addr2, size, size.mask(data2.val), data2.view, false, false, None)
                    {
                        s.core.retire(prog);
                        out.push((s, e));
                    }
                }
            }
            MemOp::StoreExcl { addr, data, rs, size, release } => {
                // Failure is always possible (and certain without a matching monitor).
                let mut f = st.clone();
                f.xcl = None;
                f.core.set(rs, RV { val: 1, view: 0 });
                f.core.retire(prog);
                out.push((f, ext.to_vec()));
                if let Some(m) = &st.xcl {
                    if m.addr == addr.val && m.size == size {
                        let srcs = m.srcs.clone();
                        let atomic = Some((&srcs[..], true));
                        for (mut s, e) in
                            self.do_write(&st, ext, addr, size, size.mask(data.val), data.view, release, true, atomic)
                        {
                            s.xcl = None;
                            s.core.set(rs, RV { val: 0, view: 0 });
                            s.core.retire(prog);
                            out.push((s, e));
                        }
                    }
                }
            }
            MemOp::Rmw { addr, op, operand, cmp, rt, size, acquire, release } => {
                let kind = if acquire { LoadKind::Acquire } else { LoadKind::Plain };
                for r in self.do_read(&st, ext, addr, kind, size) {
                    let reg = RV { val: r.val, view: r.vpost };
                    match rmw_result(op, r.val, operand.val, cmp.map(|c| c.val), size) {
                        None => {
                            let mut s = r.state;
                            s.core.set(rt, reg);
                            s.core.retire(prog);
                            out.push((s, ext.to_vec()));
                        }
                        Some(new) => {
                            let dview = r.vpost.max(operand.view).max(cmp.map_or(0, |c| c.view));
                            let atomic = Some((&r.srcs[..], false));
                            for (mut s, e) in self.do_write(&r.state, ext, addr, size, new, dview, release, false, atomic) {
                                s.core.set(rt, reg);
                                s.core.retire(prog);
                                out.push((s, e));
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
            if st.core.steps >= self.fuel && st.core.pc < self.prog.len() {
                self.truncated = true;
                continue;
            }
            let succs = self.successors(&st, &ext);
            stack.extend(succs);
        }
    }
}

pub struct Config {
    pub fuel: u32,
}

pub fn explore(test: &Test, cfg: &Config) -> Outcomes {
    let n = test.threads.len();
    if n == 1 {
        // A lone thread sees its own accesses in program order: its behaviors
        // are exactly the sequentially consistent ones.
        return crate::interleave::explore(test, crate::interleave::Kind::Sc, cfg.fuel);
    }
    let proj = Projection::new(test);
    let mut results = Outcomes::new(test.observables());
    let mut seen: HashSet<Vec<Msg>> = HashSet::new();
    let mut stack: Vec<Vec<Msg>> = vec![Vec::new()];

    let solo = |tid: usize, mem: &[Msg], mode: Mode| Solo::new(test, cfg, &proj, tid, mem.to_vec(), mode);
    let start = |tid: usize, mem: &[Msg]| {
        let mut st = TState::new(test.init_regs[tid]);
        st.promises = (1..=mem.len() as View).filter(|&t| mem[t as usize - 1].tid == tid).collect();
        st
    };

    while let Some(mem) = stack.pop() {
        if !seen.insert(mem.clone()) {
            continue;
        }
        results.stats.memories += 1;

        // Phase 2: run every thread to completion against this memory.
        let mut per_thread = Vec::new();
        for tid in 0..n {
            let mut s = solo(tid, &mem, Mode::Final);
            s.run(start(tid, &mem));
            results.truncated |= s.truncated;
            per_thread.push(s.finals);
        }
        if per_thread.iter().all(|f| !f.is_empty()) {
            let byte = |b: u64| {
                mem.iter().rev().find(|m| m.covers(b)).map(|m| m.byte(b)).unwrap_or_else(|| test.init_byte(b))
            };
            for state in proj.combine(&per_thread, &|o| match o {
                Obs::Mem(a) => test.loc_value(a, &byte),
                Obs::Reg(..) => unreachable!(),
            }) {
                results.insert(state);
            }
        }

        // Phase 1: try every certifiable promise.
        for tid in 0..n {
            let mut c = solo(tid, &mem, Mode::Collect);
            c.run(start(tid, &mem));
            results.truncated |= c.truncated;
            for (addr, size, val) in c.candidates {
                let mut m2 = mem.clone();
                m2.push(Msg { addr, size, val, tid });
                if seen.contains(&m2) {
                    continue;
                }
                let mut cert = solo(tid, &m2, Mode::Certify);
                cert.run(start(tid, &m2));
                if cert.certified {
                    stack.push(m2);
                } else {
                    results.truncated |= cert.truncated;
                }
            }
        }
    }
    results
}
