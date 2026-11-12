//! Compiles the C programs in `c/` with clang, runs their functions as
//! threads, and checks the verdicts in `c/expected.txt`.
//!
//! Each line of `expected.txt` gives a program, an optimization level, the
//! expected ARM and TSO verdicts for its litmus test (`c/NAME.litmus`:
//! `yes`, `no`, or `fault` when some execution faults), and
//! any further clang flags (usually a `-D` that weakens one ordering). A
//! `sample=N` among the flags runs the ARM model on N random paths instead
//! of exhaustively, for variants too big to explore; it may only confirm
//! that the condition is reachable, as sampling may miss states. A
//! `fuel=N` gives the threads N instructions each rather than `FUEL`. A
//! `spurious=N` lets each run of a thread fail a store-exclusive that could
//! succeed at most N times (`Options::spurious`), and a `stutter=N` lets it
//! write back the value a read-modify-write read at most N times
//! (`Options::stutter`), so a `no` holds only up to those bounds. The test
//! is skipped (with a message) when clang is not installed.
//!
//! A program is `c/NAME.c`, or `c/NAME.cpp` for C++. One whose first line
//! is `/* cflags: ... */` is compiled with those flags too, with `-I` paths
//! relative to `c/`. They come after the line's flags, so that one such as
//! `-fno-vectorize` is not undone by the line's `-O2`.

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
    sample: Option<u64>,
    fuel: u32,
    spurious: Option<u32>,
    stutter: Option<u32>,
    arm: Verdict,
    tso: Verdict,
}

/// What a model is expected to find.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// The condition is reachable (`yes`).
    Yes,
    /// It is not, and no execution faults (`no`).
    No,
    /// Some execution faults (`fault`), as one that dereferences a null
    /// pointer does.
    Fault,
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
                "yes" => Verdict::Yes,
                "no" => Verdict::No,
                "fault" => Verdict::Fault,
                _ => panic!("expected.txt: bad verdict '{}' in: {}", s, l),
            };
            let mut cflags = vec![f[1].to_string()];
            let mut sample = None;
            let mut fuel = FUEL;
            let mut spurious = None;
            let mut stutter = None;
            for s in &f[4..] {
                if let Some(n) = s.strip_prefix("sample=") {
                    sample = Some(n.parse().unwrap());
                } else if let Some(n) = s.strip_prefix("fuel=") {
                    fuel = n.parse().unwrap();
                } else if let Some(n) = s.strip_prefix("spurious=") {
                    spurious = Some(n.parse().unwrap());
                } else if let Some(n) = s.strip_prefix("stutter=") {
                    stutter = Some(n.parse().unwrap());
                } else {
                    cflags.push(s.to_string());
                }
            }
            let arm = verdict(f[2]);
            assert!(arm != Verdict::No || sample.is_none(), "expected.txt: sampling cannot show that a condition is unreachable: {}", l);
            Expectation { line: l.to_string(), program: f[0].to_string(), cflags, sample, fuel, spurious, stutter, arm, tso: verdict(f[3]) }
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

/// The flags in a program's `/* cflags: ... */` first line, if any.
fn program_cflags(source: &Path, dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(source).unwrap();
    let first = text.lines().next().unwrap_or("");
    let Some(flags) = first.strip_prefix("/* cflags:").and_then(|f| f.strip_suffix("*/")) else {
        return Vec::new();
    };
    flags
        .split_whitespace()
        .map(|f| match f.strip_prefix("-I") {
            Some(p) => format!("-I{}", dir.join(p).display()),
            None => f.to_string(),
        })
        .collect()
}

/// Compile, load and run one line; returns a failure message.
fn check(clang: &str, i: usize, e: &Expectation) -> Option<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("c");
    let obj = scratch(&format!("{}-{}.o", e.program, i));
    let source = ["c", "cpp"].iter().map(|x| dir.join(format!("{}.{}", e.program, x))).find(|p| p.exists()).unwrap();
    let out = Command::new(clang)
        .args(["--target=aarch64-linux-gnu", "-fno-pic", "-c", "-o"])
        .arg(&obj)
        .args(&e.cflags)
        .args(program_cflags(&source, &dir))
        .arg(&source)
        .output()
        .unwrap();
    if !out.status.success() {
        return Some(format!("{}: clang failed: {}", e.line, String::from_utf8_lossy(&out.stderr)));
    }
    let data = std::fs::read(&obj).unwrap();
    let preload = Image::load(&Elf::parse(&data).unwrap(), &LoadOptions::default()).and_then(|i| i.preload()).unwrap();
    let src = std::fs::read_to_string(dir.join(format!("{}.litmus", e.program))).unwrap();
    let test = litmus::parse_with(&src, &preload).unwrap();
    let opts = Options { fuel: e.fuel, spurious: e.spurious, stutter: e.stutter, check_filter: true, ..Default::default() };
    let mut errors = Vec::new();
    for (model, want) in [(Model::Arm, e.arm), (Model::Tso, e.tso)] {
        let sampled = Options { sample: e.sample, ..opts };
        let o = run(&test, model, if model == Model::Arm { &sampled } else { &opts });
        if o.truncated {
            errors.push(format!("{} ran out of fuel", model.name()));
        }
        if want == Verdict::Fault {
            if o.faults.is_empty() {
                errors.push(format!("{}: expected a fault, got none", model.name()));
            }
            continue;
        }
        if !o.faults.is_empty() {
            errors.push(format!("{} faulted: {:?}", model.name(), o.faults));
        }
        let got = o.observed(&test).unwrap();
        if got != (want == Verdict::Yes) {
            errors.push(format!("{}: expected {}, got {}", model.name(), want == Verdict::Yes, got));
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
