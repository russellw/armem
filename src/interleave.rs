//! Reference models: sequential consistency and x86-TSO.
//!
//! These explore every interleaving of the threads. Under TSO each thread
//! has a FIFO store buffer (loads may bypass buffered stores; a full DMB
//! drains the buffer, as MFENCE would; atomics are locked operations).
//! They answer the question "could this outcome also have happened on an
//! x86-like machine?", so that ARM-only outcomes can be singled out.

use crate::core::{Core, MemOp, RV, Step, pair_value, rmw_result, split_pair};
use crate::isa::{Barrier, Size};
use crate::litmus::{Obs, Test};
use crate::outcome::Outcomes;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Sc,
    Tso,
}

/// A buffered or committed store: address, size, value.
type Write = (u64, Size, u128);

#[derive(Clone, PartialEq, Eq, Hash)]
struct Thread {
    core: Core,
    buf: Vec<Write>,
    /// Exclusive monitor: the footprint of the last load-exclusive, if no
    /// other thread has written to it since.
    mon: Option<(u64, Size)>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Global {
    threads: Vec<Thread>,
    /// Bytes written so far (others hold their initial value).
    mem: BTreeMap<u64, u8>,
}

fn overlaps(a: u64, sa: Size, b: u64, sb: Size) -> bool {
    a < b + sb.bytes() && b < a + sa.bytes()
}

impl Global {
    fn mem_byte(&self, b: u64, test: &Test) -> u8 {
        self.mem.get(&b).copied().unwrap_or_else(|| test.init_byte(b))
    }

    /// Read as seen by thread `tid`: each byte comes from the youngest
    /// buffered store covering it, or from memory.
    fn read(&self, tid: usize, a: u64, size: Size, test: &Test) -> u128 {
        let mut v = 0u128;
        for i in (0..size.bytes()).rev() {
            let b = a + i;
            let byte = self.threads[tid]
                .buf
                .iter()
                .rev()
                .find(|(wa, ws, _)| overlaps(*wa, *ws, b, Size::B1))
                .map(|(wa, _, wv)| (wv >> (8 * (b - wa))) as u8)
                .unwrap_or_else(|| self.mem_byte(b, test));
            v = (v << 8) | byte as u128;
        }
        v
    }

    fn store(&mut self, tid: usize, kind: Kind, w: Write) {
        match kind {
            Kind::Tso => self.threads[tid].buf.push(w),
            Kind::Sc => self.commit(tid, w),
        }
    }

    fn commit(&mut self, tid: usize, (a, size, v): Write) {
        for i in 0..size.bytes() {
            self.mem.insert(a + i, (v >> (8 * i)) as u8);
        }
        for (i, t) in self.threads.iter_mut().enumerate() {
            if let Some((ma, ms)) = t.mon {
                if i != tid && overlaps(a, size, ma, ms) {
                    t.mon = None;
                }
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
        mem: BTreeMap::new(),
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
                let w = n.threads[tid].buf.remove(0);
                n.commit(tid, w);
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
            let prog = &test.threads[tid];
            let step = n.threads[tid].core.step(prog);
            let ok = match step {
                Step::Done => false,
                Step::Local { .. } => true,
                Step::Mem(op) => match op {
                    MemOp::Load { addr, rt, size, signed, excl, .. } => {
                        let v = size.extend(n.read(tid, addr.val, size, test) as u64, signed);
                        let t = &mut n.threads[tid];
                        t.core.set(rt, RV { val: v, view: 0 });
                        if excl {
                            t.mon = Some((addr.val, size));
                        }
                        t.core.retire(prog);
                        true
                    }
                    MemOp::LoadExclPair { addr, rt, rt2, size, .. } => {
                        let total = size.double();
                        let (lo, hi) = split_pair(n.read(tid, addr.val, total, test), size);
                        let t = &mut n.threads[tid];
                        t.core.set(rt, RV { val: lo, view: 0 });
                        t.core.set(rt2, RV { val: hi, view: 0 });
                        t.mon = Some((addr.val, total));
                        t.core.retire(prog);
                        true
                    }
                    MemOp::StoreExclPair { addr, data, data2, rs, size, .. } => {
                        if !buf_empty {
                            false
                        } else {
                            let total = size.double();
                            let success = n.threads[tid].mon == Some((addr.val, total));
                            if success {
                                n.commit(tid, (addr.val, total, pair_value(data.val, data2.val, size)));
                            }
                            let t = &mut n.threads[tid];
                            t.mon = None;
                            t.core.set(rs, RV { val: if success { 0 } else { 1 }, view: 0 });
                            t.core.retire(prog);
                            true
                        }
                    }
                    MemOp::LoadPair { addr, rt, rt2, size, signed } => {
                        // Both halves are read in one step.
                        let v1 = size.extend(n.read(tid, addr.val, size, test) as u64, signed);
                        let v2 = size.extend(n.read(tid, addr.val + size.bytes(), size, test) as u64, signed);
                        let t = &mut n.threads[tid];
                        t.core.set(rt, RV { val: v1, view: 0 });
                        t.core.set(rt2, RV { val: v2, view: 0 });
                        t.core.retire(prog);
                        true
                    }
                    MemOp::Store { addr, data, size, .. } => {
                        n.store(tid, kind, (addr.val, size, size.mask(data.val) as u128));
                        n.threads[tid].core.retire(prog);
                        true
                    }
                    MemOp::StorePair { addr, data, data2, size } => {
                        n.store(tid, kind, (addr.val, size, size.mask(data.val) as u128));
                        n.store(tid, kind, (addr.val + size.bytes(), size, size.mask(data2.val) as u128));
                        n.threads[tid].core.retire(prog);
                        true
                    }
                    MemOp::StoreExcl { addr, data, rs, size, .. } => {
                        if !buf_empty {
                            false
                        } else {
                            let success = n.threads[tid].mon == Some((addr.val, size));
                            if success {
                                n.commit(tid, (addr.val, size, size.mask(data.val) as u128));
                            }
                            let t = &mut n.threads[tid];
                            t.mon = None;
                            t.core.set(rs, RV { val: if success { 0 } else { 1 }, view: 0 });
                            t.core.retire(prog);
                            true
                        }
                    }
                    MemOp::Rmw { addr, op, operand, cmp, rt, size, .. } => {
                        if !buf_empty {
                            false
                        } else {
                            let old = n.read(tid, addr.val, size, test) as u64;
                            if let Some(new) = rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                                n.commit(tid, (addr.val, size, new as u128));
                            }
                            let t = &mut n.threads[tid];
                            t.core.set(rt, RV { val: old, view: 0 });
                            t.core.retire(prog);
                            true
                        }
                    }
                    MemOp::Barrier(b) => {
                        if b == Barrier::Full && !buf_empty {
                            false
                        } else {
                            n.threads[tid].core.retire(prog);
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
                    Obs::Mem(a) => test.loc_value(a, &|b| g.mem_byte(b, test)),
                })
                .collect();
            results.insert(state);
        }
    }
    results
}
