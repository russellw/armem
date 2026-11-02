//! Compiles the C programs in `c/` with clang, runs their functions as
//! threads, and checks the verdicts in `c/expected.txt`.
//!
//! Each line of `expected.txt` gives a program, an optimization level, the
//! expected ARM and TSO verdicts for its litmus test (`c/NAME.litmus`), and
//! any further clang flags (usually a `-D` that weakens one ordering). The
//! test is skipped (with a message) when clang is not installed.

use armem::elf::Elf;
use armem::image::{Image, LoadOptions};
use armem::{Model, Options, litmus, run};
use std::path::{Path, PathBuf};
use std::process::Command;

const FUEL: u32 = 1000;

struct Expectation {
    line: String,
    program: String,
    cflags: Vec<String>,
    arm: bool,
    tso: bool,
}

fn expectations() -> Vec<Expectation> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("c/expected.txt");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let verdict = |s: &str| match s {
                "yes" => true,
                "no" => false,
                _ => panic!("expected.txt: bad verdict '{}' in: {}", s, l),
            };
            let mut cflags = vec![f[1].to_string()];
            cflags.extend(f[4..].iter().map(|s| s.to_string()));
            Expectation { line: l.to_string(), program: f[0].to_string(), cflags, arm: verdict(f[2]), tso: verdict(f[3]) }
        })
        .collect()
}

fn clang() -> Option<String> {
    let candidates: Vec<String> = match std::env::var("ARMEM_LLVM_SUFFIX") {
        Ok(s) => vec![s],
        Err(_) => std::iter::once(String::new()).chain((14..=30).rev().map(|v| format!("-{}", v))).collect(),
    };
    candidates
        .into_iter()
        .map(|s| format!("clang{}", s))
        .find(|c| Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success()))
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("c_programs");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Compile, load and run one line; returns a failure message.
fn check(clang: &str, i: usize, e: &Expectation) -> Option<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("c");
    let obj = scratch(&format!("{}-{}.o", e.program, i));
    let out = Command::new(clang)
        .args(["--target=aarch64-linux-gnu", "-fno-pic", "-c", "-o"])
        .arg(&obj)
        .args(&e.cflags)
        .arg(dir.join(format!("{}.c", e.program)))
        .output()
        .unwrap();
    if !out.status.success() {
        return Some(format!("{}: clang failed: {}", e.line, String::from_utf8_lossy(&out.stderr)));
    }
    let data = std::fs::read(&obj).unwrap();
    let preload = Image::load(&Elf::parse(&data).unwrap(), &LoadOptions::default()).and_then(|i| i.preload()).unwrap();
    let src = std::fs::read_to_string(dir.join(format!("{}.litmus", e.program))).unwrap();
    let test = litmus::parse_with(&src, &preload).unwrap();
    let opts = Options { fuel: FUEL, ..Default::default() };
    let mut errors = Vec::new();
    for (model, want) in [(Model::Arm, e.arm), (Model::Tso, e.tso)] {
        let o = run(&test, model, &opts);
        if o.truncated {
            errors.push(format!("{} ran out of fuel", model.name()));
        }
        if !o.faults.is_empty() {
            errors.push(format!("{} faulted: {:?}", model.name(), o.faults));
        }
        let got = o.observed(&test).unwrap();
        if got != want {
            errors.push(format!("{}: expected {}, got {}", model.name(), want, got));
        }
    }
    (!errors.is_empty()).then(|| format!("{}: {}", e.line, errors.join("; ")))
}

#[test]
fn expected_verdicts() {
    let Some(clang) = clang() else {
        eprintln!("skipping: clang not found");
        return;
    };
    let (all, clang) = (expectations(), clang.as_str());
    let failures: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = all.iter().enumerate().map(|(i, e)| s.spawn(move || check(clang, i, e))).collect();
        handles.into_iter().filter_map(|h| h.join().unwrap()).collect()
    });
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
