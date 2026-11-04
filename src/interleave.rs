//! Reference models: sequential consistency and x86-TSO.
//!
//! These explore every interleaving of the threads. Under TSO each thread
//! has a FIFO store buffer (loads may bypass buffered stores; a full DMB
//! drains the buffer, as MFENCE would; atomics are locked operations).
//! They answer the question "could this outcome also have happened on an
//! x86-like machine?", so that ARM-only outcomes can be singled out.
//!
//! Only shared-memory operations are interleaving points. A thread's local
//! steps (register operations, branches and accesses to its private stack)
//! commute with every step of the other threads and with the draining of
//! store buffers, so after each step a thread runs on through its local
//! steps at once. This changes no outcome, and keeps compiled code
//! (especially at -O0, where most instructions are local) from multiplying
//! the interleavings.
//!
//! With `private`, memory that only one thread accesses (as found by the
//! reduction's pre-pass, `reduce::Reduction::private`) is treated like its
//! stack: the core performs plain accesses to it as local steps, and a
//! store to it bypasses the store buffer, which no other thread could
//! observe. An exclusive or atomic access to it is performed on the core's
//! private bytes, but keeps its effect on the thread's own ordering: it
//! waits for the store buffer to drain, as a locked operation does.

use crate::core::{Core, MemOp, RV, Step, StepQueue, VR, low_bytes, pair_value, rmw_result, split_pair};
use crate::hash::FxHashMap;
use crate::isa::{Barrier, Size};
use crate::litmus::{Obs, Test};
use crate::live::Liveness;
use crate::outcome::Outcomes;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::collections::hash_map::Entry;

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

#[derive(Clone)]
struct Global {
    /// Shared with the states it was reached from until changed.
    threads: Vec<Arc<Thread>>,
    /// The id under which each thread is interned in `explore`, if known
    /// (reset whenever the thread changes).
    ids: Vec<Option<u32>>,
    /// Bytes written so far (others hold their initial value).
    mem: BTreeMap<u64, u8>,
}

fn overlaps(a: u64, sa: Size, b: u64, sb: Size) -> bool {
    a < b + sb.bytes() && b < a + sa.bytes()
}

impl Global {
    /// Thread `tid`, to be changed.
    fn thread(&mut self, tid: usize) -> &mut Thread {
        self.ids[tid] = None;
        Arc::make_mut(&mut self.threads[tid])
    }

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
            Kind::Tso => self.thread(tid).buf.push(w),
            Kind::Sc => self.commit(tid, w),
        }
    }

    fn commit(&mut self, tid: usize, (a, size, v): Write) {
        for i in 0..size.bytes() {
            self.mem.insert(a + i, (v >> (8 * i)) as u8);
        }
        for i in 0..self.threads.len() {
            if let Some((ma, ms)) = self.threads[i].mon {
                if i != tid && overlaps(a, size, ma, ms) {
                    self.thread(i).mon = None;
                }
            }
        }
    }
}

/// Run the thread's local steps up to its next memory operation, the end of
/// its code, a fault or the fuel limit. `Core::step` leaves the core
/// unchanged unless the step is local, so the next instruction is left for
/// the explorer.
/// Take `core`'s local steps. False if it reached a return that `live`
/// does not expect (see `Liveness::ret_ok`).
fn run_local(core: &mut Core, prog: &crate::isa::Code, fuel: u32, live: Option<&Liveness>) -> bool {
    while core.steps < fuel {
        if live.is_some_and(|l| !l.ret_ok(prog, core)) {
            return false;
        }
        if !matches!(core.step(prog), Step::Local { .. }) {
            break;
        }
    }
    true
}

/// An exclusive or atomic access by thread `t` to its private memory, if
/// `op` is one: Some(whether it can be performed now), having performed it.
fn private_atomic(t: &mut Thread, op: MemOp, prog: &crate::isa::Code) -> Option<bool> {
    let (addr, len) = match op {
        MemOp::Load { addr, size, excl: true, .. } | MemOp::StoreExcl { addr, size, .. } | MemOp::Rmw { addr, size, .. } => {
            (addr, size.bytes())
        }
        MemOp::LoadExclPair { addr, size, .. } | MemOp::StoreExclPair { addr, size, .. } => (addr, 2 * size.bytes()),
        _ => return None,
    };
    if !t.core.stack.is_private(addr.val, len) {
        return None;
    }
    let reg = |val: u64| RV { val, pick: 0, view: 0 };
    let drained = t.buf.is_empty();
    match op {
        MemOp::Load { rt, size, signed, .. } => {
            let v = t.core.private_load(addr, size).0;
            t.core.set(rt, reg(size.extend(v as u64, signed)));
            t.mon = Some((addr.val, size));
        }
        MemOp::LoadExclPair { rt, rt2, size, .. } => {
            let total = size.double();
            let (lo, hi) = split_pair(t.core.private_load(addr, total).0, size);
            t.core.set(rt, reg(lo));
            t.core.set(rt2, reg(hi));
            t.mon = Some((addr.val, total));
        }
        MemOp::StoreExcl { .. } | MemOp::StoreExclPair { .. } | MemOp::Rmw { .. } if !drained => return Some(false),
        MemOp::StoreExcl { data, rs, size, .. } => {
            let success = t.mon == Some((addr.val, size));
            if success {
                t.core.private_store(addr, size, size.mask(data.val) as u128, RV::default());
            }
            t.mon = None;
            t.core.set(rs, reg(if success { 0 } else { 1 }));
        }
        MemOp::StoreExclPair { data, data2, rs, size, .. } => {
            let total = size.double();
            let success = t.mon == Some((addr.val, total));
            if success {
                t.core.private_store(addr, total, pair_value(data.val, data2.val, size), RV::default());
            }
            t.mon = None;
            t.core.set(rs, reg(if success { 0 } else { 1 }));
        }
        MemOp::Rmw { op, operand, cmp, rt, size, .. } => {
            let old = t.core.private_load(addr, size).0 as u64;
            if let Some(new) = rmw_result(op, old, operand.val, cmp.map(|c| c.val), size) {
                t.core.private_store(addr, size, new as u128, RV::default());
            }
            t.core.set(rt, reg(old));
        }
        _ => unreachable!(),
    }
    t.core.retire(prog);
    Some(true)
}

/// Explore every interleaving of `test` under `kind`. With `private`,
/// memory that only one thread accesses is treated like its stack (see
/// the module comment).
/// With `live`, thread states that differ only in registers never read
/// again are merged (see `crate::live`).
pub fn explore(test: &Test, kind: Kind, fuel: u32, private: bool, live: bool) -> Outcomes {
    live.then(|| explore_with(test, kind, fuel, private, true))
        .flatten()
        .unwrap_or_else(|| explore_with(test, kind, fuel, private, false).unwrap())
}

/// `explore`, merging thread states that differ only in dead registers if
/// `live`. None if a thread returned where the liveness did not expect.
fn explore_with(test: &Test, kind: Kind, fuel: u32, private: bool, live: bool) -> Option<Outcomes> {
    let obs = test.observables();
    let lives: Vec<Option<Liveness>> = (0..test.threads.len())
        .map(|t| {
            let exit = obs.iter().fold(0, |m, o| match *o {
                Obs::Reg(u, r) if u == t && r < 31 => m | 1 << r,
                _ => m,
            });
            live.then(|| Liveness::new(&test.threads[t].code, exit))
        })
        .collect();
    let mut results = Outcomes::new(obs.clone());
    let n = test.threads.len();
    let ranges: Vec<Arc<Vec<(u64, u64)>>> = match private.then(|| crate::reduce::analyze(test, fuel, true)).flatten() {
        Some(r) => {
            let observed: Vec<(u64, u64)> = obs
                .iter()
                .filter_map(|o| match *o {
                    Obs::Mem(a) => Some((a, test.loc_type(a).size.bytes())),
                    Obs::Reg(..) => None,
                })
                .collect();
            r.private(&observed).into_iter().map(Arc::new).collect()
        }
        None => vec![Arc::new(Vec::new()); n],
    };
    let mut ok = true;
    let start = Global {
        threads: test
            .threads
            .iter()
            .zip(ranges)
            .enumerate()
            .map(|(i, (p, r))| {
                let mut core = Core::new(test.init_regs[i], i, n, p.entry);
                core.stack.set_private(r, &|b| test.init_byte(b));
                ok &= run_local(&mut core, &p.code, fuel, lives[i].as_ref());
                Arc::new(Thread { core, buf: Vec::new(), mon: None })
            })
            .collect(),
        ids: vec![None; n],
        mem: BTreeMap::new(),
    };
    if !ok {
        return None;
    }
    // Visited states (step counters zeroed) -> the per-thread step counts
    // they have been explored with, none using at least as many steps as
    // another on every thread. A state reached again with no thread having
    // used fewer steps than in one of them cannot lead anywhere new; this
    // collapses spin loops. States are explored in order of total steps
    // used, so each is usually first reached by a path that spins least,
    // and later paths that spin more are cut off.
    // A state is keyed by the ids of its threads (with their steps taken
    // out), interned so that each thread state is stored once however many
    // states of the others it is combined with, and by memory.
    let mut seen: FxHashMap<(Vec<u32>, BTreeMap<u64, u8>), Vec<Vec<u32>>> = FxHashMap::default();
    let mut interned: FxHashMap<Thread, u32> = FxHashMap::default();
    let total = |g: &Global| g.threads.iter().map(|t| t.core.steps).sum::<u32>();
    let mut queue = StepQueue::new();
    queue.push(total(&start), start);
    while let Some(mut g) = queue.pop() {
        let steps: Vec<u32> = g.threads.iter().map(|t| t.core.steps).collect();
        for i in 0..n {
            if g.ids[i].is_none() {
                let t = Arc::make_mut(&mut g.threads[i]);
                t.core.steps = 0;
                // Interned with its dead registers cleared.
                let (regs, flags) = (t.core.regs, t.core.flags);
                if let Some(l) = &lives[i] {
                    l.canon(&mut t.core);
                }
                let id = match interned.get(t) {
                    Some(&id) => id,
                    None => {
                        let id = interned.len() as u32;
                        interned.insert(t.clone(), id);
                        id
                    }
                };
                (t.core.regs, t.core.flags) = (regs, flags);
                t.core.steps = steps[i];
                g.ids[i] = Some(id);
            }
        }
        let key = (g.ids.iter().map(|id| id.unwrap()).collect(), g.mem.clone());
        let covers = |p: &Vec<u32>, s: &Vec<u32>| p.iter().zip(s).all(|(p, s)| p <= s);
        match seen.entry(key) {
            Entry::Occupied(e) if e.get().iter().any(|p| covers(p, &steps)) => continue,
            Entry::Occupied(mut e) => {
                e.get_mut().retain(|p| !covers(&steps, p));
                e.get_mut().push(steps);
            }
            Entry::Vacant(e) => {
                e.insert(vec![steps]);
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
                let w = n.thread(tid).buf.remove(0);
                n.commit(tid, w);
                queue.push(total(&n), n);
                any = true;
            }
            if th.core.pc < test.threads[tid].code.instrs.len() && th.core.steps >= fuel {
                results.truncated = true;
                stuck = true;
                continue;
            }
            let mut n = g.clone();
            let buf_empty = n.thread(tid).buf.is_empty();
            let prog = &*test.threads[tid].code;
            if lives[tid].as_ref().is_some_and(|l| !l.ret_ok(prog, &n.thread(tid).core)) {
                return None;
            }
            let step = n.thread(tid).core.step(prog);
            let ok = match step {
                Step::Done => false,
                Step::Local { .. } => true,
                Step::Fault(e) => {
                    // The execution cannot complete.
                    results.faults.insert(format!("P{}: {}", tid, e));
                    stuck = true;
                    false
                }
                Step::Mem(op) => match private_atomic(n.thread(tid), op, prog) {
                    Some(ok) => ok,
                    None => match op {
                        MemOp::Load { addr, rt, size, signed, excl, .. } => {
                            let v = size.extend(n.read(tid, addr.val, size, test) as u64, signed);
                            let t = n.thread(tid);
                            t.core.set(rt, RV { val: v, pick: 0, view: 0 });
                            if excl {
                                t.mon = Some((addr.val, size));
                            }
                            t.core.retire(prog);
                            true
                        }
                        MemOp::LoadExclPair { addr, rt, rt2, size, .. } => {
                            let total = size.double();
                            let (lo, hi) = split_pair(n.read(tid, addr.val, total, test), size);
                            let t = n.thread(tid);
                            t.core.set(rt, RV { val: lo, pick: 0, view: 0 });
                            t.core.set(rt2, RV { val: hi, pick: 0, view: 0 });
                            t.mon = Some((addr.val, total));
                            t.core.retire(prog);
                            true
                        }
                        MemOp::StoreExclPair { addr, data, data2, rs, size, .. } => {
                            if !buf_empty {
                                false
                            } else {
                                let total = size.double();
                                let success = n.thread(tid).mon == Some((addr.val, total));
                                if success {
                                    n.commit(tid, (addr.val, total, pair_value(data.val, data2.val, size)));
                                }
                                let t = n.thread(tid);
                                t.mon = None;
                                t.core.set(rs, RV { val: if success { 0 } else { 1 }, pick: 0, view: 0 });
                                t.core.retire(prog);
                                true
                            }
                        }
                        MemOp::LoadPair { addr, rt, rt2, size, signed } => {
                            // Both halves are read in one step.
                            let v1 = size.extend(n.read(tid, addr.val, size, test) as u64, signed);
                            let v2 = size.extend(n.read(tid, addr.val + size.bytes(), size, test) as u64, signed);
                            let t = n.thread(tid);
                            t.core.set(rt, RV { val: v1, pick: 0, view: 0 });
                            t.core.set(rt2, RV { val: v2, pick: 0, view: 0 });
                            t.core.retire(prog);
                            true
                        }
                        MemOp::VLoad { addr, vt, vt2, size } => {
                            // All registers are read in one step.
                            for (i, v) in std::iter::once(vt).chain(vt2).enumerate() {
                                let val = n.read(tid, addr.val + i as u64 * size.bytes(), size, test);
                                n.thread(tid).core.vset(v, VR { val, view: 0, pick: 0 });
                            }
                            n.thread(tid).core.retire(prog);
                            true
                        }
                        MemOp::VStore { addr, data, data2, size } => {
                            let len = size.bytes();
                            for (i, d) in std::iter::once(data).chain(data2).enumerate() {
                                n.store(tid, kind, (addr.val + i as u64 * len, size, low_bytes(d.val, len)));
                            }
                            n.thread(tid).core.retire(prog);
                            true
                        }
                        MemOp::Store { addr, data, size, .. } => {
                            n.store(tid, kind, (addr.val, size, size.mask(data.val) as u128));
                            n.thread(tid).core.retire(prog);
                            true
                        }
                        MemOp::StorePair { addr, data, data2, size } => {
                            n.store(tid, kind, (addr.val, size, size.mask(data.val) as u128));
                            n.store(tid, kind, (addr.val + size.bytes(), size, size.mask(data2.val) as u128));
                            n.thread(tid).core.retire(prog);
                            true
                        }
                        MemOp::StoreExcl { addr, data, rs, size, .. } => {
                            if !buf_empty {
                                false
                            } else {
                                let success = n.thread(tid).mon == Some((addr.val, size));
                                if success {
                                    n.commit(tid, (addr.val, size, size.mask(data.val) as u128));
                                }
                                let t = n.thread(tid);
                                t.mon = None;
                                t.core.set(rs, RV { val: if success { 0 } else { 1 }, pick: 0, view: 0 });
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
                                let t = n.thread(tid);
                                t.core.set(rt, RV { val: old, pick: 0, view: 0 });
                                t.core.retire(prog);
                                true
                            }
                        }
                        MemOp::Clrex => {
                            let t = n.thread(tid);
                            t.mon = None;
                            t.core.retire(prog);
                            true
                        }
                        MemOp::Barrier(b) => {
                            if b == Barrier::Full && !buf_empty {
                                false
                            } else {
                                n.thread(tid).core.retire(prog);
                                true
                            }
                        }
                    },
                },
            };
            if ok {
                if !run_local(&mut n.thread(tid).core, prog, fuel, lives[tid].as_ref()) {
                    return None;
                }
                queue.push(total(&n), n);
                any = true;
            }
        }
        if !any && !stuck {
            // Every thread has finished and drained its buffer.
            let state = obs
                .iter()
                .map(|o| match *o {
                    Obs::Reg(t, r) => g.threads[t].core.regs[r as usize].val,
                    Obs::Mem(a) => test.loc_value(a, &|b| {
                        // A private byte's value is in its thread's core.
                        g.threads
                            .iter()
                            .find(|t| t.core.stack.is_private(b, 1))
                            .and_then(|t| t.core.stack.byte(b))
                            .unwrap_or_else(|| g.mem_byte(b, test))
                    }),
                })
                .collect();
            results.insert(state);
        }
    }
    Some(results)
}
