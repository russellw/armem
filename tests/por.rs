//! Checks the partial-order reduction (`src/reduce.rs`), and the handling
//! of memory private to one thread that relies on its analysis: on every
//! litmus test, and on randomly generated tests, the ARM model must reach
//! exactly the same final states with and without each of them (and
//! without register liveness, see `src/live.rs`), and the TSO and SC models
//! with and without private memory and liveness.

use armem::{Model, litmus, run};
use std::path::Path;

fn options(por: bool, private: bool, lse2: bool) -> armem::Options {
    armem::Options { por, private, lse2, ..Default::default() }
}

/// Explore `t` with neither the reduction nor private memory nor register
/// liveness, and check that the reduction and private memory together,
/// private memory alone, and liveness alone (which the others use too)
/// give the same results.
/// Returns the memories explored without and with both, or None if the
/// search without them would explore more than `budget` memories.
fn check_within(t: &litmus::Test, src: &str, lse2: bool, budget: Option<u64>) -> Option<(u64, u64)> {
    let full = run(t, Model::Arm, &armem::Options { max_memories: budget, live: false, ..options(false, false, lse2) });
    if full.over_budget {
        return None;
    }
    let mut reduced_memories = 0;
    for (por, private) in [(true, true), (false, true), (false, false)] {
        let reduced = run(t, Model::Arm, &options(por, private, lse2));
        let what = format!("por={} private={} lse2={}", por, private, lse2);
        assert_eq!(full.states.keys().collect::<Vec<_>>(), reduced.states.keys().collect::<Vec<_>>(), "{}\n{}", what, src);
        assert_eq!(full.faults, reduced.faults, "{}\n{}", what, src);
        assert_eq!(full.truncated, reduced.truncated, "{}\n{}", what, src);
        if por {
            reduced_memories = reduced.stats.memories;
        }
    }
    for model in [Model::Tso, Model::Sc] {
        let plain = run(t, model, &armem::Options { live: false, ..options(false, false, lse2) });
        let private = run(t, model, &options(false, true, lse2));
        let what = format!("{:?} private lse2={}", model, lse2);
        assert_eq!(plain.states.keys().collect::<Vec<_>>(), private.states.keys().collect::<Vec<_>>(), "{}\n{}", what, src);
        assert_eq!(plain.faults, private.faults, "{}\n{}", what, src);
        assert_eq!(plain.truncated, private.truncated, "{}\n{}", what, src);
    }
    Some((full.stats.memories, reduced_memories))
}

fn check(t: &litmus::Test, src: &str, lse2: bool) -> (u64, u64) {
    check_within(t, src, lse2, None).unwrap()
}

#[test]
fn litmus_suite() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("litmus");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    for p in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "litmus")) {
        let src = std::fs::read_to_string(p).unwrap();
        let t = litmus::parse(&src).unwrap();
        for lse2 in [false, true] {
            check(&t, &src, lse2);
        }
    }
}

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A random test: 2 or 3 threads of 2 to 4 accesses (fewer unless `large`) to three 32-bit
/// locations, mixing sizes, barriers, dependencies, acquire/release,
/// exclusives and atomics. Every register and location is observed.
fn random_test(rng: &mut Rng, n: usize, large: bool) -> String {
    let threads = 2 + rng.below(2);
    let mut cols: Vec<Vec<String>> = Vec::new();
    let mut observed = Vec::new();
    for tid in 0..threads {
        let mut code: Vec<String> = Vec::new();
        let mut loaded: Vec<String> = Vec::new();
        let mut next_reg = 10;
        let mut label = 0;
        let accesses = 2 + rng.below(if large { 3 } else { 4 - threads });
        for k in 0..accesses {
            // Sometimes the thread's own location (X4), which only it
            // accesses: private memory, even for its atomics.
            let loc = format!("X{}", if rng.below(5) == 0 { 4 } else { 1 + rng.below(3) });
            let val = 1 + tid * 4 + k;
            let mut fresh = || {
                let r = format!("W{}", next_reg);
                observed.push(format!("{}:X{}", tid, next_reg));
                next_reg += 1;
                r
            };
            // Something that may order this access after earlier ones.
            match rng.below(12) {
                0 => code.push("DMB SY".into()),
                1 => code.push("DMB LD".into()),
                2 => code.push("DMB ST".into()),
                3 if !loaded.is_empty() => {
                    code.push(format!("CBNZ {},L{}{}", loaded[rng.below(loaded.len())], tid, label));
                    code.push(format!("L{}{}:", tid, label));
                    label += 1;
                    if rng.below(2) == 0 {
                        code.push("ISB".into());
                    }
                }
                _ => {}
            }
            // An address dependency on an earlier load, sometimes.
            let dep = (!loaded.is_empty() && rng.below(3) == 0).then(|| loaded[rng.below(loaded.len())].clone());
            let addr = match &dep {
                Some(r) => {
                    code.push(format!("EOR W8,{},{}", r, r));
                    format!("[{},W8,SXTW]", loc)
                }
                None => format!("[{}]", loc),
            };
            match rng.below(13) {
                // An exclusive pair, on the thread's own location.
                12 if loc == "X4" && rng.below(2) == 0 => {
                    let (r, r2, s) = (fresh(), fresh(), fresh());
                    code.push(format!("{} {},{},[X4]", ["LDXP", "LDAXP"][rng.below(2)], r, r2));
                    code.push(format!("ADD W9,{},#{}", r, val));
                    code.push(format!("{} {},W9,{},[X4]", ["STXP", "STLXP"][rng.below(2)], s, r2));
                    loaded.push(r);
                }
                0..=3 => {
                    // Store, sometimes data-dependent on an earlier load.
                    if !loaded.is_empty() && rng.below(3) == 0 {
                        let r = &loaded[rng.below(loaded.len())];
                        code.push(format!("EOR W9,{},{}", r, r));
                        code.push(format!("ADD W9,W9,#{}", val));
                    } else {
                        code.push(format!("MOV W9,#{}", val));
                    }
                    let op = ["STR", "STR", "STLR", "STRB"][rng.below(4)];
                    let addr = if op == "STLR" { format!("[{}]", loc) } else { addr };
                    code.push(format!("{} W9,{}", op, addr));
                }
                4..=8 => {
                    let r = fresh();
                    let op = ["LDR", "LDR", "LDAR", "LDAPR", "LDRH"][rng.below(5)];
                    let addr = if op.starts_with("LDA") { format!("[{}]", loc) } else { addr };
                    code.push(format!("{} {},{}", op, r, addr));
                    loaded.push(r);
                }
                9 => {
                    let r = fresh();
                    let op = ["SWP", "SWPAL", "LDADD", "LDADDL"][rng.below(4)];
                    code.push(format!("MOV W9,#{}", val));
                    code.push(format!("{} W9,{},[{}]", op, r, loc));
                    loaded.push(r);
                }
                10 => {
                    let r = fresh();
                    code.push(format!("MOV {},#0", r));
                    code.push(format!("MOV W9,#{}", val));
                    code.push(format!("{} {},W9,[{}]", ["CAS", "CASA", "CASL"][rng.below(3)], r, loc));
                    loaded.push(r);
                }
                _ => {
                    let r = fresh();
                    let s = fresh();
                    code.push(format!("{} {},[{}]", ["LDXR", "LDAXR"][rng.below(2)], r, loc));
                    code.push(format!("ADD W9,{},#{}", r, val));
                    code.push(format!("{} {},W9,[{}]", ["STXR", "STLXR"][rng.below(2)], s, loc));
                    loaded.push(r);
                }
            }
        }
        cols.push(code);
    }
    let rows = cols.iter().map(Vec::len).max().unwrap();
    let mut src = format!("AArch64 random-{}\n{{ uint32_t x; uint32_t y; uint32_t z;", n);
    for tid in 0..threads {
        src.push_str(&format!(" uint64_t p{0}; {0}:X1=x; {0}:X2=y; {0}:X3=z; {0}:X4=p{0};", tid));
    }
    src.push_str(" }\n");
    src.push_str(&(0..threads).map(|t| format!("P{}", t)).collect::<Vec<_>>().join(" | "));
    src.push_str(" ;\n");
    for i in 0..rows {
        let row: Vec<&str> = cols.iter().map(|c| c.get(i).map_or("", String::as_str)).collect();
        src.push_str(&row.join(" | "));
        src.push_str(" ;\n");
    }
    let own: Vec<String> = (0..threads).map(|t| format!("p{}", t)).collect();
    src.push_str(&format!("locations [x; y; z; {}; {}]\n", own.join("; "), observed.join("; ")));
    src
}

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Compare `count` random tests (from `seed`) with and without the
/// reduction, skipping those whose full search exceeds `budget` memories.
/// `ARMEM_POR_TESTS`, `ARMEM_POR_SEED` and `ARMEM_POR_BUDGET` override the
/// arguments, `ARMEM_POR_VERBOSE` prints the size and time of each test,
/// and `ARMEM_POR_DUMP=n` prints test `n`.
fn random_tests(count: usize, seed: u64, large: bool, budget: u64) {
    let count = env("ARMEM_POR_TESTS", count);
    let budget = env("ARMEM_POR_BUDGET", budget);
    let mut rng = Rng(env("ARMEM_POR_SEED", seed));
    let (mut full, mut reduced, mut skipped) = (0, 0, 0);
    for n in 0..count {
        let src = random_test(&mut rng, n, large);
        let t = litmus::parse(&src).unwrap_or_else(|e| panic!("{}\n{}", e, src));
        let lse2 = rng.below(4) == 0;
        if std::env::var("ARMEM_POR_DUMP").is_ok_and(|d| d == n.to_string()) {
            eprintln!("{}", src);
        }
        let start = std::time::Instant::now();
        let result = check_within(&t, &src, lse2, Some(budget));
        if std::env::var("ARMEM_POR_VERBOSE").is_ok() {
            eprintln!("{:.3}s test {}: {:?}", start.elapsed().as_secs_f64(), n, result);
        }
        match result {
            Some((f, r)) => {
                full += f;
                reduced += r;
            }
            None => skipped += 1,
        }
    }
    eprintln!(
        "{} random tests ({} skipped as over budget): {} memories without the reduction and private memory, {} with them",
        count, skipped, full, reduced
    );
    assert!(skipped * 10 <= count, "too many tests over budget: {}", skipped);
}

/// Small random tests, part of the normal test run (about 10 s).
#[test]
fn random_small() {
    random_tests(200, 1, false, 2_000);
}

/// Larger random tests, which can take hours: run them with
/// `cargo test --release --test por -- --ignored` after changing the
/// model or the reduction.
#[test]
#[ignore]
fn random_large() {
    random_tests(1_200, 2, true, 100_000);
}

/// Two threads that each store to `k` locations, read by nobody, so all
/// orders of the stores to different locations are equivalent.
fn writes_test(k: usize) -> String {
    let init: Vec<String> = (0..k).flat_map(|i| [format!("0:X{0}=d{0}", i + 1), format!("1:X{0}=d{0}", i + 1)]).collect();
    let mut src = format!("AArch64 ww{}\n{{ {}; }}\n P0 | P1 ;\n MOV W0,#1 | MOV W0,#2 ;\n", k, init.join("; "));
    for i in 0..k {
        src.push_str(&format!(" STR W0,[X{}] | STR W0,[X{}] ;\n", i + 1, k - i));
    }
    src.push_str(&format!("exists ({})\n", (0..k).map(|i| format!("d{}=1", i)).collect::<Vec<_>>().join(" /\\ ")));
    src
}

/// Message passing of `k` data locations with no ordering on either side:
/// the reader cannot observe the order of the writer's stores.
fn mp_test(k: usize) -> String {
    let mut init = vec!["0:X9=f".to_string(), "1:X9=f".to_string()];
    init.extend((0..k).flat_map(|i| [format!("0:X{0}=d{0}", i + 1), format!("1:X{0}=d{0}", i + 1)]));
    let mut src = format!("AArch64 mp{}\n{{ {}; }}\n P0 | P1 ;\n MOV W0,#1 | LDR W10,[X9] ;\n", k, init.join("; "));
    for i in 0..k {
        src.push_str(&format!(" STR W0,[X{0}] | LDR W{1},[X{0}] ;\n", i + 1, 11 + i));
    }
    src.push_str(" STR W0,[X9] | ;\n");
    src.push_str(&format!("exists (1:X10=1 /\\ {})\n", (0..k).map(|i| format!("1:X{}=0", 11 + i)).collect::<Vec<_>>().join(" /\\ ")));
    src
}

/// The reduction prunes what it should: without it these explore every
/// order of every subset of the writes.
#[test]
fn reduction_prunes() {
    for (src, full_memories, at_most) in [(writes_test(3), 1957, 250), (mp_test(4), 326, 40)] {
        let t = litmus::parse(&src).unwrap();
        let (full, reduced) = check(&t, &src, false);
        assert_eq!(full, full_memories, "{}", src);
        assert!(reduced <= at_most, "{} memories with the reduction:\n{}", reduced, src);
    }
}

/// Sampling only finds final states the full exploration finds, and a few
/// walks find the ARM-only outcome of message passing with six data
/// locations.
#[test]
fn sampling() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("litmus");
    for name in ["MP", "IRIW", "spinlock-norelease", "LDXP-STXP-counter", "unaligned-in-block"] {
        let src = std::fs::read_to_string(dir.join(format!("{}.litmus", name))).unwrap();
        let t = litmus::parse(&src).unwrap();
        let full = run(&t, Model::Arm, &options(true, true, false));
        for seed in 0..3 {
            let sampled = run(&t, Model::Arm, &armem::Options { sample: Some(5), seed, ..Default::default() });
            assert!(sampled.sampled);
            assert!(!sampled.states.is_empty(), "{}: no final state sampled", name);
            for s in sampled.states.keys() {
                assert!(full.states.contains_key(s), "{}: sampled state {:?} is not reachable", name, s);
            }
        }
    }
    let src = mp_test(6);
    let t = litmus::parse(&src).unwrap();
    let sampled = run(&t, Model::Arm, &armem::Options { sample: Some(20), ..Default::default() });
    assert_eq!(sampled.observed(&t), Some(true));
}

/// A return to an address that is not after a `BL` (here, the return
/// address is changed): liveness assumes it cannot happen, so the searches
/// must notice it and start again without liveness. `X1` holds 0 or 1 from
/// the load, and is dead at the load in `f` if `RET` returns after the
/// `BL` (where `X1` is overwritten); it is read where `RET` really goes.
/// P1 reads `z` too, so that the load in `f` is not private memory: it is
/// a step of the search, where states differing only in `X1` could merge.
#[test]
fn unexpected_return() {
    let src = "AArch64 unexpected-return
{ 0:X2=x; 0:X3=y; 0:X4=z; 1:X2=x; 1:X4=z; }
 P0              | P1          ;
 LDR X1,[X2]     | MOV X0,#1   ;
 BL f            | STR X0,[X2] ;
 MOV X1,#0       | LDR X5,[X4] ;
 B end           |             ;
 STR X1,[X3]     |             ;
 B end           |             ;
 f:              |             ;
 LDR X5,[X4]     |             ;
 ADD X30,X30,#8  |             ;
 RET             |             ;
 end:            |             ;
exists (y=1)
";
    let t = litmus::parse(src).unwrap();
    for model in [Model::Arm, Model::Tso, Model::Sc] {
        let [dead, live] = [false, true].map(|live| run(&t, model, &armem::Options { live, ..Default::default() }));
        assert_eq!(dead.states.keys().collect::<Vec<_>>(), live.states.keys().collect::<Vec<_>>(), "{:?}", model);
        assert_eq!(live.states.len(), 2, "{:?}: y may be 0 or 1", model);
    }
    check(&t, src, false);
}
