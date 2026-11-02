//! Runs every test in `litmus/` and checks the verdicts in `litmus/expected.txt`.

use armem::{Model, litmus, run};
use std::path::Path;

const FUEL: u32 = 200;

/// One line of `litmus/expected.txt`.
struct Expectation {
    name: String,
    arm: bool,
    tso: bool,
    opts: armem::Options,
}

fn load(name: &str) -> litmus::Test {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("litmus").join(format!("{}.litmus", name));
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {}", path.display(), e));
    litmus::parse(&src).unwrap_or_else(|e| panic!("{}: {}", path.display(), e))
}

fn expectations() -> Vec<Expectation> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("litmus/expected.txt");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let mut opts = armem::Options { fuel: FUEL, ..Default::default() };
            for o in &f[3..] {
                match *o {
                    "lse2" => opts.lse2 = true,
                    _ => panic!("unknown option '{}' in expected.txt", o),
                }
            }
            Expectation { name: f[0].to_string(), arm: f[1] == "yes", tso: f[2] == "yes", opts }
        })
        .collect()
}

#[test]
fn expected_verdicts() {
    let mut failures = Vec::new();
    for Expectation { name, arm, tso, opts } in expectations() {
        let t = load(&name);
        let label = if opts.lse2 { format!("{} [lse2]", name) } else { name.clone() };
        for (model, want) in [(Model::Arm, arm), (Model::Tso, tso)] {
            let o = run(&t, model, &opts);
            assert!(!o.truncated, "{} ({}) ran out of fuel", label, model.name());
            let got = o.observed(&t).expect("test has a condition");
            if got != want {
                failures.push(format!("{} under {}: expected {}, got {}", label, model.name(), want, got));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_test_has_an_expectation() {
    let names: Vec<String> = expectations().into_iter().map(|e| e.name).collect();
    for e in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("litmus")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "litmus") {
            let stem = p.file_stem().unwrap().to_string_lossy().to_string();
            assert!(names.contains(&stem), "{} has no entry in expected.txt", stem);
        }
    }
}

/// Every SC behavior must be possible under TSO and under ARM.
#[test]
fn sc_is_contained_in_weaker_models() {
    for Expectation { name, opts, .. } in expectations() {
        let t = load(&name);
        let sc = run(&t, Model::Sc, &opts);
        for m in [Model::Tso, Model::Arm] {
            let o = run(&t, m, &opts);
            for s in sc.states.keys() {
                assert!(
                    o.states.contains_key(s),
                    "{}: SC state {} missing under {}",
                    name,
                    sc.format_state(&t, s),
                    m.name()
                );
            }
        }
    }
}

/// Exploration is deterministic: repeated runs give identical results.
#[test]
fn deterministic() {
    for name in ["IRIW", "spinlock-norelease", "PPOCA"] {
        let t = load(&name);
        let opts = armem::Options { fuel: FUEL, ..Default::default() };
        let a = run(&t, Model::Arm, &opts);
        let b = run(&t, Model::Arm, &opts);
        assert_eq!(a.states, b.states);
    }
}
