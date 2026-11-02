//! Tests of SP and the threads' private stacks.
//!
//! Plain stack accesses bypass the memory models (see `core::Stack`). To
//! check that this changes no behavior, each test also runs with its stack
//! replaced by an ordinary memory location that only its thread uses (SP
//! becomes X9, pointing into it), which the models handle in full: the
//! final states must be the same under every model.

use armem::core::{STACK_AREA, STACK_SIZE, stack_range};
use armem::litmus::{self, Preload};
use armem::memory::{InitMem, Region};
use armem::outcome::Outcomes;
use armem::{Model, Options, run};
use std::collections::BTreeSet;

const MODELS: [Model; 3] = [Model::Arm, Model::Tso, Model::Sc];

/// A litmus test with the given init statements, thread columns and final
/// condition.
fn source(init: &str, threads: &[&[&str]], cond: &str) -> String {
    let n = threads.len();
    let header: Vec<String> = (0..n).map(|i| format!("P{}", i)).collect();
    let mut s = format!("AArch64 t\n{{ {} }}\n {} ;\n", init, header.join(" | "));
    let rows = threads.iter().map(|t| t.len()).max().unwrap();
    for r in 0..rows {
        let cells: Vec<&str> = threads.iter().map(|t| t.get(r).copied().unwrap_or("")).collect();
        s.push_str(&format!(" {} ;\n", cells.join(" | ")));
    }
    s + cond + "\n"
}

/// The same test with each thread's stack replaced by a private ordinary
/// location `stk<i>`, addressed through X9.
fn without_stack(init: &str, threads: &[&[&str]], cond: &str) -> String {
    let mut init = init.to_string();
    for i in 0..threads.len() {
        init.push_str(&format!(" {}:X9=stk{}+128;", i, i));
    }
    let code: Vec<Vec<String>> = threads.iter().map(|t| t.iter().map(|l| l.replace("SP", "X9")).collect()).collect();
    let code: Vec<Vec<&str>> = code.iter().map(|t| t.iter().map(String::as_str).collect()).collect();
    let code: Vec<&[&str]> = code.iter().map(Vec::as_slice).collect();
    source(&init, &code, cond)
}

fn states(o: &Outcomes) -> BTreeSet<Vec<u64>> {
    o.states.keys().cloned().collect()
}

/// Run the test and its stackless version under every model; they must
/// agree. Returns whether the condition is observed under each model.
fn check(init: &str, threads: &[&[&str]], cond: &str) -> Vec<bool> {
    let src = source(init, threads, cond);
    let test = litmus::parse(&src).unwrap_or_else(|e| panic!("{}\n{}", e, src));
    let alt = litmus::parse(&without_stack(init, threads, cond)).unwrap();
    let opts = Options::default();
    MODELS
        .iter()
        .map(|&m| {
            let (a, b) = (run(&test, m, &opts), run(&alt, m, &opts));
            assert!(a.faults.is_empty() && !a.truncated, "{}: {:?}\n{}", m.name(), a.faults, src);
            assert_eq!(states(&a), states(&b), "{} differs without the stack:\n{}", m.name(), src);
            a.observed(&test).unwrap()
        })
        .collect()
}

#[test]
fn stack_values() {
    // Sub-word, signed and pair accesses, with writeback, round trip.
    let p0: &[&str] = &[
        "MOVN X1,#0",
        "MOV X2,#0x1234",
        "STP X1,X2,[SP,#-32]!",
        "STRB W2,[SP,#16]",
        "LDRSB X3,[SP,#16]",
        "LDRH W4,[SP,#7]",
        "LDP W5,W6,[SP],#32",
        "LDR X7,[SP,#-24]",
    ];
    let r = check(
        "",
        &[p0, &["MOV W0,#1"]],
        "forall (0:X3=0x34 /\\ 0:X4=0x34ff /\\ 0:X5=0xffffffff /\\ 0:X6=0xffffffff /\\ 0:X7=0x1234)",
    );
    assert_eq!(r, [true; 3]);
}

#[test]
fn initial_stack_pointers() {
    let src = source("", &[&["MOV X0,SP"], &["MOV X0,SP", "ADD W1,WSP,#1"]], "locations [1:X0; 1:X1;]\nexists (0:X0=0)");
    let test = litmus::parse(&src).unwrap();
    for m in MODELS {
        let o = run(&test, m, &Options::default());
        let st: Vec<_> = o.states.keys().collect();
        assert_eq!(st.len(), 1);
        let get = |t, r| o.get(st[0], litmus::Obs::Reg(t, r));
        assert_eq!([get(0, 0), get(1, 0), get(1, 1)], [stack_range(0).1, stack_range(1).1, (stack_range(1).1 + 1) & 0xffff_ffff]);
        assert_eq!(stack_range(1).0, stack_range(0).1);
    }
}

#[test]
fn dependencies_through_the_stack() {
    let mp_writer: &[&str] = &["MOV W0,#1", "STR W0,[X1]", "DMB ST", "STR W0,[X2]"];
    let mp = |reader: &[&str]| check("0:X1=x; 0:X2=y; 1:X1=y; 1:X5=x;", &[mp_writer, reader], "exists (1:X0=1 /\\ 1:X4=0)");
    // An address dependency through a spilled value keeps the loads in order.
    let r = mp(&["LDR W0,[X1]", "STR W0,[SP,#-16]!", "LDR W2,[SP],#16", "EOR W3,W2,W2", "LDR W4,[X5,W3,SXTW]"]);
    assert_eq!(r, [false, false, false]);
    // Without the dependency, they may be reordered.
    let r = mp(&["LDR W0,[X1]", "STR W0,[SP,#-16]!", "LDR W2,[SP],#16", "EOR W3,W0,W0", "MOV W3,#0", "LDR W4,[X5,W3,SXTW]"]);
    assert_eq!(r, [true, false, false]);

    // Load buffering. P1 has a data dependency; P0 orders its write after
    // its read with a data dependency through the stack, or with a stack
    // access whose address depends on the read.
    let lb = |p0: &[&str]| {
        let p1: &[&str] = &["LDR W0,[X1]", "EOR W2,W0,W0", "ADD W2,W2,#1", "STR W2,[X3]"];
        check("0:X1=x; 0:X3=y; 1:X1=y; 1:X3=x;", &[p0, p1], "exists (0:X0=1 /\\ 1:X0=1)")
    };
    let data = lb(&["LDR W0,[X1]", "STR W0,[SP,#-8]", "LDR W4,[SP,#-8]", "EOR W2,W4,W4", "ADD W2,W2,#1", "STR W2,[X3]"]);
    assert_eq!(data, [false, false, false]);
    let addr = lb(&["LDR W0,[X1]", "SUB SP,SP,#16", "EOR W4,W0,W0", "STR WZR,[SP,W4,SXTW]", "MOV W2,#1", "STR W2,[X3]"]);
    assert_eq!(addr, [false, false, false]);
    let none = lb(&["LDR W0,[X1]", "STR W0,[SP,#-8]", "LDR W4,[SP,#-8]", "MOV W2,#1", "STR W2,[X3]"]);
    assert_eq!(none, [true, false, false]);

    // A stack store that depends on the read, then DMB ST: the barrier
    // orders the store, and so the read it depends on, before the write.
    // Each kind of store, by its data or its address.
    let dmb_st = |store: &[&str]| {
        let mut p0 = vec!["LDR W0,[X1]", "SUB SP,SP,#32"];
        p0.extend(store);
        p0.extend(["DMB ST", "MOV W2,#1", "STR W2,[X3]"]);
        lb(&p0)
    };
    for store in [
        &["STR W0,[SP]"][..],
        &["STP W5,W0,[SP]"],
        &["FMOV S0,W0", "STR Q0,[SP]"],
        &["EOR W4,W0,W0", "STR WZR,[SP,W4,SXTW]"],
    ] {
        assert_eq!(dmb_st(store), [false, false, false], "{:?}", store);
    }
    // Without the dependency, the read and the write may be reordered.
    assert_eq!(dmb_st(&["STR W5,[SP]"]), [true, false, false]);
}

/// The faults each model reports for a test.
fn faults(src: &str) -> Vec<BTreeSet<String>> {
    let test = litmus::parse(src).unwrap();
    MODELS.iter().map(|&m| run(&test, m, &Options::default()).faults).collect()
}

#[test]
fn stack_faults() {
    // P1 dereferences a pointer into P0's stack, which P0 published.
    let src = source(
        "0:X1=p; 1:X1=p;",
        &[&["MOV X0,SP", "STR X0,[X1]"], &["LDR X0,[X1]", "CBZ X0,end", "LDR X2,[X0,#-8]", "end:"]],
        "exists (1:X0=0)",
    );
    for f in faults(&src) {
        let f: Vec<_> = f.into_iter().collect();
        assert_eq!(f.len(), 1, "{:?}", f);
        assert!(f[0].starts_with("P1: 8-byte access at") && f[0].contains("outside its stack"), "{}", f[0]);
    }
    // The executions that do not fault still reach final states.
    let test = litmus::parse(&src).unwrap();
    assert!(run(&test, Model::Arm, &Options::default()).observed(&test).unwrap());

    // A stack overflow, an access across the top of the stack, and an
    // exclusive access to the stack.
    for (code, msg) in [
        (&["SUB SP,SP,#0x100000", "STR X0,[SP,#-8]!"][..], "outside its stack"),
        (&["LDR X0,[SP,#-4]"][..], "outside its stack"),
        (&["SUB SP,SP,#16", "LDAXR X0,[SP]"][..], "exclusive, atomic, acquire or release"),
        (&["SUB SP,SP,#16", "MOV X1,#1", "LDADD X1,X0,[SP]"][..], "exclusive, atomic, acquire or release"),
    ] {
        let src = source("", &[code, &["NOP"]], "exists (0:X0=0)");
        for f in faults(&src) {
            assert!(f.len() == 1 && f.iter().all(|f| f.starts_with("P0: ") && f.contains(msg)), "{:?}\n{}", f, src);
        }
    }
}

#[test]
fn stacks_do_not_overlap_preloaded_memory() {
    let region = Region { addr: STACK_AREA + 3 * STACK_SIZE - 8, bytes: vec![0; 16].into(), writable: true, origin: "data".into() };
    let pre = Preload { mem: InitMem::from_regions(vec![region]).unwrap(), ..Default::default() };
    let two = "AArch64 t\n{ }\n P0 | P1 ;\n NOP | NOP ;\nexists (0:X0=0)\n";
    let e = litmus::parse_with(two, &pre).unwrap_err();
    assert!(e.contains("overlaps the stacks"), "{}", e);
    // One thread needs only two guard slots and its stack.
    let one = "AArch64 t\n{ }\n P0 ;\n NOP ;\nexists (0:X0=0)\n";
    let e = litmus::parse_with(one, &pre).unwrap_err();
    assert!(e.contains("overlaps the stacks"), "{}", e);
    let pre = Preload {
        mem: InitMem::from_regions(vec![Region { addr: STACK_AREA - 16, bytes: vec![0; 16].into(), writable: true, origin: "data".into() }]).unwrap(),
        ..Default::default()
    };
    litmus::parse_with(two, &pre).unwrap();
}
