//! Reference models: sequential consistency and x86-TSO.
//!
//! These explore every interleaving of the threads. Under TSO each thread
//! has a FIFO store buffer (loads may bypass buffered stores; a full DMB
//! drains the buffer, as MFENCE would; atomics are locked operations).
//! They answer the question "could this outcome also have happened on an
//! x86-like machine?", so that ARM-only outcomes can be singled out.

use crate::core::{Core, MemOp, RV, Step, rmw_result};
use crate::isa::Barrier;
use crate::litmus::{Obs, Test};
use crate::outcome::Outcomes;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sc,
    Tso,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Thread {
    core: Core,
    buf: Vec<(u64, u64)>,
    /// Exclusive monitor: the location of the last load-exclusive, if no
    /// other thread has written it since.
    mon: Option<u64>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Global {
    threads: Vec<Thread>,
    mem: BTreeMap<u64, u64>,
}

impl Global {
    fn read(&self, tid: usize, a: u64, init: &BTreeMap<u64, u64>) -> u64 {
        if let Some(&(_, v)) = self.threads[tid].buf.iter().rev().find(|(x, _)| *x == a) {
            return v;
        }
        self.mem.get(&a).or_else(|| init.get(&a)).copied().unwrap_or(0)
    }

    fn commit(&mut self, tid: usize, a: u64, v: u64) {
        self.mem.insert(a, v);
        for (i, t) in self.threads.iter_mut().enumerate() {
            if i != tid && t.mon == Some(a) {
                t.mon = None;
            }
        }
    }
}

pub fn explore(test: &Test, kind: Kind, fuel: u32) -> Outcomes {
    let obs = test.observables();
    let mut results = Outcomes::new(obs.clone());
    let start = Global {
        threads: test
            .threads
            .iter()
            .enumerate()
            .map(|(i, _)| Thread { core: Core::new(test.init_regs[i]), buf: Vec::new(), mon: None })
            .collect(),
        mem: test.init_mem.clone(),
    };
    // Visited states (step counters zeroed) -> per-thread step counts when
    // last explored. A state reached again with no thread having used fewer
    // steps cannot lead anywhere new; this collapses spin loops.
    let mut seen: HashMap<Global, Vec<u32>> = HashMap::new();
    let mut stack = vec![start];
    while let Some(g) = stack.pop() {
        let mut key = g.clone();
        let steps: Vec<u32> = key.threads.iter().map(|t| t.core.steps).collect();
        for t in key.threads.iter_mut() {
            t.core.steps = 0;
        }
        match seen.get(&key) {
            Some(prev) if prev.iter().zip(&steps).all(|(p, s)| p <= s) => continue,
            _ => {
                seen.insert(key, steps);
            }
        }
        results.stats.states += 1;
        let mut any = false;
        let mut stuck = false;
        for tid in 0..g.threads.len() {
            let th = &g.threads[tid];
            // Drain the oldest buffered store.
            if !th.buf.is_empty() {
                let mut n = g.clone();
                let (a, v) = n.threads[tid].buf.remove(0);
                n.commit(tid, a, v);
                stack.push(n);
                any = true;
            }
            if th.core.pc < test.threads[tid].len() && th.core.steps >= fuel {
                results.truncated = true;
                stuck = true;
                continue;
            }
            let mut n = g.clone();
            let buf_empty = n.threads[tid].buf.is_empty();
            let step = n.threads[tid].core.step(&test.threads[tid]);
            let ok = match step {
                Step::Done => false,
                Step::Local { .. } => true,
                Step::Mem(op) => match op {
                    MemOp::Load { addr, rt, size, excl, .. } => {
                        let v = size.mask(n.read(tid, addr.val, &test.init_mem));
                        let t = &mut n.threads[tid];
                        t.core.set(rt, RV { val: v, view: 0 });
                        if excl {
                            t.mon = Some(addr.val);
                        }
                        t.core.retire();
                        true
                    }
                    MemOp::Store { addr, data, size, .. } => {
                        let v = size.mask(data.val);
                        match kind {
                            Kind::Tso => n.threads[tid].buf.push((addr.val, v)),
                            Kind::Sc => n.commit(tid, addr.val, v),
                        }
                        n.threads[tid].core.retire();
                        true
                    }
                    MemOp::StoreExcl { addr, data, rs, size, .. } => {
                        if !buf_empty {
                            false
                        } else {
                            let success = n.threads[tid].mon == Some(addr.val);
                            if success {
                                n.commit(tid, addr.val, size.mask(data.val));
                            }
                            let t = &mut n.threads[tid];
                            t.mon = None;
                            t.core.set(rs, RV { val: if success { 0 } else { 1 }, view: 0 });
                            t.core.retire();
                            true
                        }
                    }
                    MemOp::Rmw { addr, op, operand, cmp, rt, size, .. } => {
                        if !buf_empty {
                            false
                        } else {
                            let old = size.mask(n.read(tid, addr.val, &test.init_mem));
                            if let Some(new) = rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                                n.commit(tid, addr.val, new);
                            }
                            let t = &mut n.threads[tid];
                            t.core.set(rt, RV { val: old, view: 0 });
                            t.core.retire();
                            true
                        }
                    }
                    MemOp::Barrier(b) => {
                        if b == Barrier::Full && !buf_empty {
                            false
                        } else {
                            n.threads[tid].core.retire();
                            true
                        }
                    }
                },
            };
            if ok {
                stack.push(n);
                any = true;
            }
        }
        if !any && !stuck {
            // Every thread has finished and drained its buffer.
            let state = obs
                .iter()
                .map(|o| match *o {
                    Obs::Reg(t, r) => g.threads[t].core.regs[r as usize].val,
                    Obs::Mem(a) => g.mem.get(&a).copied().unwrap_or(0),
                })
                .collect();
            results.insert(state);
        }
    }
    results
}
