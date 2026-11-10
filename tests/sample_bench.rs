//! A benchmark of `--sample` (run with `cargo test --release --test
//! sample_bench -- --ignored --nocapture`): for each test in
//! `c/bench/list.txt`, each an ARM-only bug, how many walks sampling takes
//! to find the bug, for several seeds. Each result is printed as `walk/
//! memories time`, where `walk` is the first walk after which the condition
//! held (`-` if none did). `WALKS` (default 100), `SEEDS` (default 10),
//! `PCT_DEPTH` (default `Options::pct_depth`) and `FILTER` (a substring of
//! the line) set what is run. `WALKS=0` explores exhaustively instead.

use armem::elf::Elf;
use armem::image::{Image, LoadOptions};
use armem::{Model, Options, litmus, run};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Bench {
    line: String,
    source: String,
    litmus: String,
    fuel: u32,
    cflags: Vec<String>,
}

fn benches() -> Vec<Bench> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("c");
    std::fs::read_to_string(dir.join("bench/list.txt"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Bench {
                line: l.to_string(),
                source: f[0].into(),
                litmus: f[1].into(),
                fuel: f[2].parse().unwrap(),
                cflags: f[3..].iter().map(|s| s.to_string()).collect(),
            }
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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sample_bench");
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

fn env(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn bench(clang: &str, i: usize, b: &Bench) -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("c");
    let obj = scratch(&format!("{}.o", i));
    let source = dir.join(&b.source);
    let out = Command::new(clang)
        .args(["--target=aarch64-linux-gnu", "-fno-pic", "-c", "-o"])
        .arg(&obj)
        .args(program_cflags(&source, &dir))
        .args(&b.cflags)
        .arg(&source)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}: clang failed: {}", b.line, String::from_utf8_lossy(&out.stderr));
    let data = std::fs::read(&obj).unwrap();
    let preload = Image::load(&Elf::parse(&data).unwrap(), &LoadOptions::default()).and_then(|i| i.preload()).unwrap();
    let src = std::fs::read_to_string(dir.join(&b.litmus)).unwrap();
    let test = litmus::parse_with(&src, &preload).unwrap();
    let walks = env("WALKS").unwrap_or(100);
    let mut r = b.line.clone();
    for seed in 0..env("SEEDS").unwrap_or(10) {
        let mut opts = Options { fuel: b.fuel, sample: (walks > 0).then_some(walks), seed, ..Default::default() };
        opts.pct_depth = env("PCT_DEPTH").unwrap_or(opts.pct_depth);
        let t = std::time::Instant::now();
        let o = run(&test, Model::Arm, &opts);
        let hit = match (walks, o.stats.first_hit) {
            (0, _) => o.observed(&test).unwrap().to_string(),
            (_, Some(h)) => h.to_string(),
            (_, None) => "-".into(),
        };
        r += &format!("\n  {}/{} {:.1}s", hit, o.stats.memories, t.elapsed().as_secs_f64());
    }
    r
}

#[test]
#[ignore]
fn sample_bench() {
    let Some(clang) = clang() else {
        eprintln!("skipping: clang not found");
        return;
    };
    let filter = std::env::var("FILTER").unwrap_or_default();
    // One at a time, so that contention does not inflate the times.
    for (i, b) in benches().iter().enumerate().filter(|(_, b)| b.line.contains(&filter)) {
        println!("{}", bench(&clang, i, b));
    }
}
