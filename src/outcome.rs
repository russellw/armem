//! Final states of an exploration and their presentation.

use crate::litmus::{Obs, Quant, Test};
use std::collections::{BTreeMap, BTreeSet};

/// Maps observables onto per-thread register lists.
pub struct Projection {
    pub obs: Vec<Obs>,
    /// For each thread, the registers that are observed.
    pub thread_regs: Vec<Vec<u8>>,
}

impl Projection {
    pub fn new(test: &Test) -> Projection {
        let obs = test.observables();
        let mut thread_regs = vec![Vec::new(); test.threads.len()];
        for o in &obs {
            if let Obs::Reg(t, r) = *o {
                if t < thread_regs.len() && !thread_regs[t].contains(&r) {
                    thread_regs[t].push(r);
                }
            }
        }
        Projection { obs, thread_regs }
    }

    /// Combine per-thread register outcomes (each a set of vectors aligned
    /// with `thread_regs[t]`) and memory values into full final states.
    pub fn combine(&self, per_thread: &[BTreeSet<Vec<u64>>], mem: &dyn Fn(Obs) -> u64) -> Vec<Vec<u64>> {
        let mut partial: Vec<Vec<Vec<u64>>> = vec![Vec::new()];
        for f in per_thread {
            let mut next = Vec::new();
            for p in &partial {
                for regs in f {
                    let mut q = p.clone();
                    q.push(regs.clone());
                    next.push(q);
                }
            }
            partial = next;
        }
        partial
            .into_iter()
            .map(|p| {
                self.obs
                    .iter()
                    .map(|o| match *o {
                        Obs::Reg(t, r) => {
                            let i = self.thread_regs[t].iter().position(|&x| x == r).unwrap();
                            p[t][i]
                        }
                        Obs::Mem(_) => mem(*o),
                    })
                    .collect()
            })
            .collect()
    }
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    /// Promising: number of distinct promise memories explored.
    pub memories: u64,
    /// Interleaving models: number of distinct global states explored.
    pub states: u64,
}

/// The set of final states found by an exploration.
pub struct Outcomes {
    pub obs: Vec<Obs>,
    /// Final state (aligned with `obs`) -> number of witnesses found.
    pub states: BTreeMap<Vec<u64>, u64>,
    /// Some path was cut off by the fuel limit, so the set may be incomplete.
    pub truncated: bool,
    /// Faults that some execution reaches (such as an access outside the
    /// thread's stack), each prefixed with the thread. Such an execution
    /// has no final state.
    pub faults: BTreeSet<String>,
    pub stats: Stats,
}

impl Outcomes {
    pub fn new(obs: Vec<Obs>) -> Outcomes {
        Outcomes { obs, states: BTreeMap::new(), truncated: false, faults: BTreeSet::new(), stats: Stats::default() }
    }

    pub fn insert(&mut self, state: Vec<u64>) {
        *self.states.entry(state).or_insert(0) += 1;
    }

    pub fn get(&self, state: &[u64], o: Obs) -> u64 {
        let i = self.obs.iter().position(|x| *x == o).expect("observable not projected");
        state[i]
    }

    pub fn format_state(&self, test: &Test, state: &[u64]) -> String {
        self.obs
            .iter()
            .zip(state)
            .map(|(o, v)| format!("{}={};", test.obs_name(*o), test.value_name(*v)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Evaluate the test's condition: returns (condition holds, positive witnesses, negative witnesses).
    pub fn verdict(&self, test: &Test) -> Option<(bool, usize, usize)> {
        let c = test.condition.as_ref()?;
        let mut pos = 0;
        let mut neg = 0;
        for s in self.states.keys() {
            if c.prop.eval(&|o| self.get(s, o)) {
                pos += 1
            } else {
                neg += 1
            }
        }
        let ok = match c.quant {
            Quant::Exists => pos > 0,
            Quant::NotExists => pos == 0,
            Quant::Forall => neg == 0,
        };
        Some((ok, pos, neg))
    }

    /// Does any final state satisfy the condition's proposition?
    pub fn observed(&self, test: &Test) -> Option<bool> {
        self.verdict(test).map(|(_, pos, _)| pos > 0)
    }
}
