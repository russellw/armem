//! The ARMv8 memory model, in the operational "Promising-ARM" style
//! (Pulte, Pichon-Pharabod, Kang, Lee, Hur: "Promising-ARM/RISC-V: a
//! simpler and faster operational concurrency model", PLDI 2019).
//!
//! Memory is a list of write messages; a message's timestamp is its
//! 1-based position in the list (0 denotes the initial value of every
//! location). Each thread keeps *views* (timestamps) that bound which
//! messages it may still read and where its writes may be placed.
//! Out-of-order writes are modelled by *promises*: a thread may append a
//! write to memory early, provided it can later fulfil it (certification).
//!
//! Exploration uses the promise-first strategy: in phase 1 threads make
//! promises (each one certified by a solo run of the promising thread);
//! in phase 2, with memory fixed, every thread runs independently and must
//! fulfil exactly its promises. Every behaviour of the model is reachable
//! this way, and in phase 2 the threads cannot affect each other, so the
//! final states are simply the product of each thread's final states.

use crate::core::{Core, MemOp, RV, Step, View, rmw_result};
use crate::isa::{Barrier, Instr, LoadKind, Size};
use crate::litmus::{Obs, Test};
use crate::outcome::{Outcomes, Projection};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Msg {
    pub loc: u64,
    pub val: u64,
    pub tid: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Fwd {
    ts: View,
    view: View,
    xcl: bool,
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
    /// Per-location coherence view.
    coh: BTreeMap<u64, View>,
    /// Forwarding bank: latest own write per location.
    fwd: BTreeMap<u64, Fwd>,
    /// Exclusive monitor: (location, timestamp read, post view).
    xcl: Option<(u64, View, View)>,
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
    /// Forget all view information (used where only values matter).
    fn erase_views(mut self) -> TState {
        for r in self.core.regs.iter_mut() {
            r.view = 0;
        }
        self.core.flags.view = 0;
        let pc = self.core.clone();
        let mut fresh = TState::new([0; 31]);
        fresh.core = pc;
        fresh.xcl = self.xcl.map(|(l, _, _)| (l, 0, 0));
        fresh.promises = std::mem::take(&mut self.promises);
        fresh
    }

    fn coh(&self, loc: u64) -> View {
        self.coh.get(&loc).copied().unwrap_or(0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Over-approximate the writes the thread might promise: reads may see
    /// any value written to the location, every write is a new message.
    Collect,
    /// Search for a run that fulfils all outstanding promises; new writes
    /// may be appended to (a local copy of) memory.
    Certify,
    /// Run to completion fulfilling all promises, with no new writes.
    Final,
}

/// Exhaustive exploration of one thread running alone against a memory.
struct Solo<'a> {
    prog: &'a [Instr],
    tid: usize,
    init: &'a BTreeMap<u64, u64>,
    base: Vec<Msg>,
    fuel: u32,
    mode: Mode,
    /// Visited states (with the step counter zeroed) -> fewest steps seen.
    visited: HashMap<(TState, Vec<Msg>), u32>,
    candidates: BTreeSet<(u64, u64)>,
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
            prog: &test.threads[tid],
            tid,
            init: &test.init_mem,
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

    fn value(&self, ext: &[Msg], loc: u64, t: View) -> u64 {
        if t == 0 { self.init.get(&loc).copied().unwrap_or(0) } else { self.msg(ext, t).val }
    }

    /// Timestamps a read of `loc` may read from, given the lower bound `bound`:
    /// the last write to `loc` at or before `bound`, or any later write.
    fn read_choices(&self, ext: &[Msg], loc: u64, bound: View) -> Vec<View> {
        let n = self.len(ext);
        let mut out = Vec::new();
        if self.mode == Mode::Collect {
            out.push(0);
            out.extend((1..=n).filter(|&t| self.msg(ext, t).loc == loc));
            return out;
        }
        let last = (1..=bound.min(n)).rev().find(|&t| self.msg(ext, t).loc == loc).unwrap_or(0);
        out.push(last);
        out.extend((bound + 1..=n).filter(|&t| self.msg(ext, t).loc == loc));
        out
    }

    /// Is there no write to `loc` strictly between `from` and `to`
    /// (by a thread other than us, if `others_only`)?
    fn atomic(&self, ext: &[Msg], loc: u64, from: View, to: View, others_only: bool) -> bool {
        (from + 1..to).all(|t| {
            let m = self.msg(ext, t);
            m.loc != loc || (others_only && m.tid == self.tid)
        })
    }

    /// Perform a read; returns the post-states (one per choice of message)
    /// with the value read and its timestamp.
    fn do_read(&self, st: &TState, ext: &[Msg], addr: RV, kind: LoadKind, size: Size) -> Vec<(TState, u64, View, View)> {
        let loc = addr.val;
        let mut vpre = addr.view.max(st.v_rnew);
        if kind == LoadKind::Acquire {
            vpre = vpre.max(st.v_rel);
        }
        let bound = vpre.max(st.coh(loc));
        let mut out = Vec::new();
        for t in self.read_choices(ext, loc, bound) {
            let val = size.mask(self.value(ext, loc, t));
            let rview = match st.fwd.get(&loc) {
                Some(f) if f.ts == t && !(f.xcl && kind == LoadKind::Acquire) => f.view,
                _ => t,
            };
            let vpost = vpre.max(rview);
            let mut s = st.clone();
            s.coh.insert(loc, st.coh(loc).max(vpost));
            s.v_rold = s.v_rold.max(vpost);
            if kind != LoadKind::Plain {
                s.v_rnew = s.v_rnew.max(vpost);
                s.v_wnew = s.v_wnew.max(vpost);
            }
            s.v_cap = s.v_cap.max(addr.view);
            out.push((s, val, t, vpost));
        }
        out
    }

    /// Perform a write of `val` to `addr.val`. `atomic_from` imposes an
    /// atomicity constraint relative to an earlier read at that timestamp
    /// (`others_only` for exclusives). Returns post-states.
    #[allow(clippy::too_many_arguments)]
    fn do_write(
        &mut self,
        st: &TState,
        ext: &[Msg],
        addr: RV,
        val: u64,
        data_view: View,
        release: bool,
        xcl: bool,
        atomic_from: Option<(View, bool)>,
    ) -> Vec<Succ> {
        let loc = addr.val;
        let mut out = Vec::new();
        if self.mode == Mode::Collect {
            self.candidates.insert((loc, val));
            let mut e = ext.to_vec();
            e.push(Msg { loc, val, tid: self.tid });
            return vec![(st.clone(), e)];
        }
        let mut vpre = addr.view.max(data_view).max(st.v_wnew).max(st.v_cap);
        if release {
            vpre = vpre.max(st.v_rold).max(st.v_wold);
        }
        let apply = |t: View| {
            let mut s = st.clone();
            s.coh.insert(loc, t);
            s.v_wold = s.v_wold.max(t);
            s.v_cap = s.v_cap.max(addr.view);
            if release {
                s.v_rel = s.v_rel.max(t);
            }
            s.fwd.insert(loc, Fwd { ts: t, view: addr.view.max(data_view), xcl });
            s.promises.remove(&t);
            s
        };
        let ok_atomic = |t: View| match atomic_from {
            Some((from, others_only)) => self.atomic(ext, loc, from, t, others_only),
            None => true,
        };
        // Fulfil an outstanding promise.
        for &t in &st.promises {
            let m = self.msg(ext, t);
            if m.loc == loc && m.val == val && vpre < t && st.coh(loc) < t && ok_atomic(t) {
                out.push((apply(t), ext.to_vec()));
            }
        }
        // Or (while certifying) write a new message at the end of memory.
        if self.mode == Mode::Certify {
            let t = self.len(ext) + 1;
            if ok_atomic(t) {
                let mut e = ext.to_vec();
                e.push(Msg { loc, val, tid: self.tid });
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
                s.core.retire();
                out.push((s, ext.to_vec()));
            }
            MemOp::Load { addr, rt, size, kind, excl } => {
                for (mut s, val, t, vpost) in self.do_read(&st, ext, addr, kind, size) {
                    s.core.set(rt, RV { val, view: vpost });
                    if excl {
                        s.xcl = Some((addr.val, t, vpost));
                    }
                    s.core.retire();
                    out.push((s, ext.to_vec()));
                }
            }
            MemOp::Store { addr, data, size, release } => {
                for (mut s, e) in self.do_write(&st, ext, addr, size.mask(data.val), data.view, release, false, None) {
                    s.core.retire();
                    out.push((s, e));
                }
            }
            MemOp::StoreExcl { addr, data, rs, size, release } => {
                // Failure is always possible (and certain without a matching monitor).
                let mut f = st.clone();
                f.xcl = None;
                f.core.set(rs, RV { val: 1, view: 0 });
                f.core.retire();
                out.push((f, ext.to_vec()));
                if let Some((loc, tr, _)) = st.xcl {
                    if loc == addr.val {
                        let atomic = if self.mode == Mode::Collect { None } else { Some((tr, true)) };
                        for (mut s, e) in self.do_write(&st, ext, addr, size.mask(data.val), data.view, release, true, atomic) {
                            s.xcl = None;
                            s.core.set(rs, RV { val: 0, view: 0 });
                            s.core.retire();
                            out.push((s, e));
                        }
                    }
                }
            }
            MemOp::Rmw { addr, op, operand, cmp, rt, size, acquire, release } => {
                let kind = if acquire { LoadKind::Acquire } else { LoadKind::Plain };
                for (s, old, tr, vpost) in self.do_read(&st, ext, addr, kind, size) {
                    let reg = RV { val: old, view: vpost };
                    match rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                        None => {
                            let mut s = s;
                            s.core.set(rt, reg);
                            s.core.retire();
                            out.push((s, ext.to_vec()));
                        }
                        Some(new) => {
                            let dview = vpost.max(operand.view).max(cmp.map_or(0, |c| c.view));
                            let atomic = if self.mode == Mode::Collect { None } else { Some((tr, false)) };
                            for (mut s2, e) in self.do_write(&s, ext, addr, new, dview, release, false, atomic) {
                                s2.core.set(rt, reg);
                                s2.core.retire();
                                out.push((s2, e));
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
            if self.mode == Mode::Collect {
                stack.extend(succs.into_iter().map(|(s, e)| (s.erase_views(), e)));
            } else {
                stack.extend(succs);
            }
        }
    }
}

pub struct Config {
    pub fuel: u32,
}

pub fn explore(test: &Test, cfg: &Config) -> Outcomes {
    let proj = Projection::new(test);
    let n = test.threads.len();
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
            let memval = |a: u64| {
                mem.iter()
                    .rev()
                    .find(|m| m.loc == a)
                    .map(|m| m.val)
                    .unwrap_or_else(|| test.init_mem.get(&a).copied().unwrap_or(0))
            };
            for state in proj.combine(&per_thread, &|o| match o {
                Obs::Mem(a) => memval(a),
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
            for (loc, val) in c.candidates {
                let mut m2 = mem.clone();
                m2.push(Msg { loc, val, tid });
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
