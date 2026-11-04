//! Human-readable reports.

use crate::litmus::Test;
use crate::outcome::Outcomes;
use crate::Model;
use std::fmt::Write;

pub fn observation(o: &Outcomes, test: &Test) -> &'static str {
    match o.verdict(test) {
        None => "-",
        Some((_, pos, neg)) => {
            if pos == 0 {
                "Never"
            } else if neg == 0 {
                "Always"
            } else {
                "Sometimes"
            }
        }
    }
}

/// Full report for one test. `results` holds one entry per model run;
/// when a reference model (TSO/SC) is present, ARM states it cannot
/// produce are marked.
pub fn full(test: &Test, results: &[(Model, Outcomes)]) -> String {
    let mut s = String::new();
    let reference = results.iter().find(|(m, _)| *m == Model::Tso).or_else(|| results.iter().find(|(m, _)| *m == Model::Sc));
    for (model, o) in results {
        writeln!(s, "Test {} ({})", test.name, model.name()).unwrap();
        writeln!(s, "States {}", o.states.len()).unwrap();
        for st in o.states.keys() {
            let mut line = format!("  {}", o.format_state(test, st));
            if let Some(c) = &test.condition {
                if c.prop.eval(&|ob| o.get(st, ob)) {
                    line.push_str("  [condition]");
                }
            }
            if *model == Model::Arm {
                if let Some((rm, r)) = reference {
                    if !r.states.contains_key(st) {
                        line.push_str(&format!("  <-- not possible under {}", rm.name()));
                    }
                }
            }
            writeln!(s, "{}", line).unwrap();
        }
        if let Some((ok, pos, neg)) = o.verdict(test) {
            let c = test.condition.as_ref().unwrap();
            writeln!(s, "{} (condition {}: {})", if ok { "Ok" } else { "No" }, c.quant, observation(o, test)).unwrap();
            writeln!(s, "Witnesses: Positive {} Negative {}", pos, neg).unwrap();
        }
        if o.stats.memories > 0 {
            write!(s, "Explored {} promise memories", o.stats.memories).unwrap();
            if o.stats.pruned > 0 {
                write!(s, " ({} pruned by partial-order reduction)", o.stats.pruned).unwrap();
            }
            if o.stats.observed + o.stats.observe_failed > 0 {
                write!(s, " (threads explored after {} memories, given up after {})", o.stats.observed, o.stats.observe_failed).unwrap();
            }
            if o.stats.filtered > 0 {
                write!(s, " ({} candidate promises left uncertified: they cannot certify)", o.stats.filtered).unwrap();
            }
            writeln!(s).unwrap();
        }
        if o.over_budget {
            writeln!(s, "WARNING: the search stopped at its memory budget; other final states may be possible").unwrap();
        }
        if o.sampled {
            writeln!(s, "WARNING: only sampled executions were explored; other final states may be possible").unwrap();
        }
        if o.stats.states > 0 {
            writeln!(s, "Explored {} states", o.stats.states).unwrap();
        }
        if o.truncated {
            writeln!(s, "WARNING: some executions exceeded the fuel limit; results may be incomplete").unwrap();
        }
        for f in &o.faults {
            writeln!(s, "FAULT: {}", f).unwrap();
        }
        writeln!(s).unwrap();
    }
    if let Some(line) = comparison(test, results) {
        writeln!(s, "{}", line).unwrap();
    }
    s
}

/// One-line conclusion comparing ARM with a reference model.
pub fn comparison(test: &Test, results: &[(Model, Outcomes)]) -> Option<String> {
    let (_, arm) = results.iter().find(|(m, _)| *m == Model::Arm)?;
    let (rm, r) = results.iter().find(|(m, _)| *m != Model::Arm)?;
    let extra: Vec<_> = arm.states.keys().filter(|k| !r.states.contains_key(*k)).collect();
    let mut msg = String::new();
    if extra.is_empty() {
        write!(msg, "No ARM-only final states (relative to {}).", rm.name()).unwrap();
    } else {
        write!(msg, "{} final state(s) possible on ARM but not under {}.", extra.len(), rm.name()).unwrap();
    }
    if let (Some(a), Some(b)) = (arm.observed(test), r.observed(test)) {
        if a && !b {
            write!(msg, " The condition is reachable on ARM only: likely ARM-specific bug.").unwrap();
        }
    }
    Some(msg)
}

/// Compact one-line summary.
pub fn summary(test: &Test, results: &[(Model, Outcomes)]) -> String {
    let mut s = format!("{:<24}", test.name);
    for (m, o) in results {
        write!(s, " {}:{:<9}", m.name(), observation(o, test)).unwrap();
        if o.truncated {
            s.push_str("(trunc)");
        }
        if o.sampled {
            s.push_str("(sampled)");
        }
        if !o.faults.is_empty() {
            s.push_str("(fault)");
        }
    }
    if let (Some((_, a)), Some((_, r))) = (
        results.iter().find(|(m, _)| *m == Model::Arm),
        results.iter().find(|(m, _)| *m != Model::Arm),
    ) {
        if a.observed(test) == Some(true) && r.observed(test) == Some(false) {
            s.push_str("  ARM-ONLY");
        }
    }
    s
}
