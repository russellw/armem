//! Tests that start from the memory of a loaded ELF image, and threads that
//! run its functions.
//!
//! The ELF tests assemble objects with `llvm-mc` (and compile C with
//! `clang`) and are skipped (with a message) when they are not installed.

use armem::elf::Elf;
use armem::image::{Image, LoadOptions};
use armem::litmus::{self, LocType, PreSym, Preload};
use armem::memory::{InitMem, Region};
use armem::{Model, Options, isa::Size, run};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether `test`'s condition holds in some final state under `model`.
fn observed(test: &litmus::Test, model: Model) -> bool {
    run(test, model, &Options::default()).observed(test).unwrap()
}

fn manual_preload() -> Preload {
    let region = Region { addr: 0x8000, bytes: vec![1, 0, 0, 0, 9, 0, 0, 0].into(), writable: true, origin: "data".into() };
    let int = LocType { size: Size::B4, signed: true };
    Preload {
        mem: InitMem::from_regions(vec![region]).unwrap(),
        symbols: vec![
            PreSym { name: "a".into(), addr: 0x8000, size: 4, ty: int },
            PreSym { name: "b".into(), addr: 0x8004, size: 4, ty: int },
        ],
        code: None,
        functions: Default::default(),
    }
}

#[test]
fn preloaded_values_and_overrides() {
    let pre = manual_preload();
    let t = litmus::parse_with(
        "AArch64 t\n{ 0:X1=a; 0:X3=b; 0:X5=c; }\n P0 ;\n LDR W0,[X1] ;\n LDR W2,[X3] ;\n LDR X4,[X5] ;\nforall (0:X0=1 /\\ 0:X2=9 /\\ 0:X4=0)",
        &pre,
    )
    .unwrap();
    assert!(observed(&t, Model::Arm));
    // The test's own location `c` gets a fresh address outside the image.
    let c = t.symbols.iter().find(|(n, _)| n == "c").unwrap().1;
    assert_eq!(c, litmus::SYM_BASE);
    // Without a condition, all named locations are reported.
    let t = litmus::parse_with("AArch64 t\n{ }\n P0 ;\n NOP ;\n", &pre).unwrap();
    assert_eq!(t.observables().len(), 2);
    // `b=5` overrides; `uint8_t a` only changes the type.
    let t = litmus::parse_with(
        "AArch64 t\n{ b=5; uint8_t a; 0:X3=b; }\n P0 ;\n LDR W2,[X3] ;\nforall (0:X2=5 /\\ a=1 /\\ b=5)",
        &pre,
    )
    .unwrap();
    assert!(observed(&t, Model::Arm));
    assert_eq!(t.loc_type(0x8000).size, Size::B1);
    // A fresh location must not overlap the image.
    let mut low = manual_preload();
    let r = Region { addr: litmus::SYM_BASE + 0x80, bytes: vec![0].into(), writable: true, origin: ".data".into() };
    low.mem = InitMem::from_regions(vec![r]).unwrap();
    let e = litmus::parse_with("AArch64 t\n{ 0:X1=c; }\n P0 ;\n NOP ;\n", &low).unwrap_err();
    assert!(e.contains("overlap"), "{}", e);
}

fn llvm_mc() -> Option<String> {
    let candidates: Vec<String> = match std::env::var("ARMEM_LLVM_SUFFIX") {
        Ok(s) => vec![s],
        Err(_) => std::iter::once(String::new()).chain((14..=30).rev().map(|v| format!("-{}", v))).collect(),
    };
    let found = candidates
        .into_iter()
        .map(|s| format!("llvm-mc{}", s))
        .find(|t| Command::new(t).arg("--version").output().is_ok_and(|o| o.status.success()));
    if found.is_none() {
        eprintln!("skipping: llvm-mc not found");
    }
    found
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("preload");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

const DATA: &str = "
	.data
	.p2align 3
	.globl x, y, seven, ptr, small, arr
	.type x,%object
	.size x, 4
x:	.word 0
	.type y,%object
	.size y, 4
y:	.word 0
	.type seven,%object
	.size seven, 4
seven:	.word 7
	.p2align 3
	.type ptr,%object
	.size ptr, 8
ptr:	.quad y
	.type small,%object
	.size small, 1
small:	.byte 0xff
	.p2align 3
	.type arr,%object
	.size arr, 16
arr:	.quad 1, 2
	.section .rodata
	.globl msg
	.type msg,%object
	.size msg, 4
msg:	.asciz \"abc\"
	.comm buf, 8, 8
";

fn data_object(mc: &str) -> PathBuf {
    let (s, o) = (scratch("data.s"), scratch("data.o"));
    std::fs::write(&s, DATA).unwrap();
    let out = Command::new(mc).args(["-triple=aarch64", "-filetype=obj", "-o"]).arg(&o).arg(&s).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    o
}

const MP: &str = "AArch64 MP-elf
{ 0:X1=x; 0:X3=y; 1:X1=y; 1:X3=x; }
 P0          | P1          ;
 MOV W0,#1   | LDR W0,[X1] ;
 STR W0,[X1] | LDR W2,[X3] ;
 MOV W2,#1   |             ;
 STR W2,[X3] |             ;
exists (1:X0=1 /\\ 1:X2=0)
";

#[test]
fn elf_data_as_initial_memory() {
    let Some(mc) = llvm_mc() else { return };
    let data = std::fs::read(data_object(&mc)).unwrap();
    let elf = Elf::parse(&data).unwrap();
    let image = Image::load(&elf, &LoadOptions::default()).unwrap();
    let pre = image.preload().unwrap();
    let names: Vec<&str> = pre.symbols.iter().map(|p| p.name.as_str()).collect();
    for n in ["x", "y", "seven", "ptr", "small", "arr", "msg", "buf"] {
        assert!(names.contains(&n), "{} missing from {:?}", n, names);
    }
    let addr = |n: &str| image.symbol(n).unwrap();

    // Values from the image, including a relocated pointer and read-only data.
    let t = litmus::parse_with(
        "AArch64 init
{ 0:X1=ptr; 0:X3=seven; 0:X5=small; 0:X7=msg; 0:X9=arr; }
 P0 ;
 LDR X0,[X1] ;
 LDR W2,[X0] ;
 LDR W4,[X3] ;
 LDRB W6,[X5] ;
 LDR W8,[X7] ;
 LDR X10,[X9,#8] ;
forall (0:X0=y /\\ 0:X2=0 /\\ 0:X4=7 /\\ 0:X6=255 /\\ 0:X8=0x636261 /\\ 0:X10=2 /\\ small=-1 /\\ ptr=y)",
        &pre,
    )
    .unwrap();
    assert_eq!(t.init_regs[0][1], addr("ptr"));
    for m in [Model::Arm, Model::Tso, Model::Sc] {
        assert!(observed(&t, m), "{:?}", m);
    }

    // Message passing through the image's globals: ARM only.
    let t = litmus::parse_with(MP, &pre).unwrap();
    assert_eq!(t.init_regs[0][1], addr("x"));
    assert!(observed(&t, Model::Arm));
    assert!(!observed(&t, Model::Tso));
}

#[test]
fn command_line() {
    let Some(mc) = llvm_mc() else { return };
    let obj = data_object(&mc);
    let test = scratch("MP-elf.litmus");
    std::fs::write(&test, MP).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_armem")).arg("--elf").arg(&obj).arg(&test).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("not possible under TSO"), "{}", text);
    // With the image at the test's own addresses, its fresh locations overlap.
    let mut fresh = MP.replace("1:X3=x;", "1:X3=x; 1:X5=z;");
    fresh = fresh.replace("MP-elf", "MP-fresh");
    std::fs::write(&test, fresh).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_armem")).args(["--elf-base", "0x1000", "--elf"]).arg(&obj).arg(&test).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("overlap"));
}

#[test]
fn function_threads() {
    let mut pre = manual_preload();
    let x = |i| armem::isa::RegOp { idx: i, wide: true };
    let w = |i| armem::isa::RegOp { idx: i, wide: false };
    let addr = |r| armem::isa::Addr { base: x(r), offset: armem::isa::Offset::Imm(0), writeback: armem::isa::Writeback::None };
    use armem::isa::{BrKind, Code, Instr, LoadKind};
    // load_b: LDR W0,[X1]; RET. broken: an instruction that did not decode.
    let load = Instr::Load { rt: w(0), addr: addr(1), size: Size::B4, signed: false, kind: LoadKind::Plain, excl: false };
    let ret = Instr::BrReg { rn: x(30), kind: BrKind::Ret };
    let mut code = Code::new(0x9000, vec![load, ret, Instr::Invalid]);
    code.errors.insert(2, "unsupported".into());
    // Without code, no function can run.
    pre.functions.insert("load_b".into(), 0x9000);
    let e = litmus::parse_with("AArch64 t\n{ }\n P0 = load_b ;\n", &pre).unwrap_err();
    assert!(e.contains("not in the loaded code"), "{}", e);
    pre.code = Some(code.into());
    pre.functions.insert("broken".into(), 0x9008);
    pre.functions.insert("outside".into(), 0x900c);
    let parse = |src: &str| litmus::parse_with(src, &pre);
    let t = parse("AArch64 t\n{ 0:X1=b; 1:X3=a; }\n P0 = load_b | P1 ;\n | LDR W2,[X3] ;\nforall (0:X0=9 /\\ 1:X2=1)").unwrap();
    assert_eq!(t.functions, [Some("load_b".to_string()), None]);
    assert!(t.code[0].is_empty());
    assert!(observed(&t, Model::Arm));
    // Only a header and a condition.
    let t = parse("AArch64 t\n{ 0:X1=a; }\n P0=load_b ;\nforall (0:X0=1)").unwrap();
    assert!(observed(&t, Model::Tso));
    let err = |src: &str| parse(src).unwrap_err();
    assert!(err("AArch64 t\n{ }\n P0 = load_b ;\n NOP ;\n").contains("must be empty"));
    assert!(err("AArch64 t\n{ }\n P0 = nope ;\n").contains("no function 'nope'"));
    assert!(err("AArch64 t\n{ }\n P0 = outside ;\n").contains("not in the loaded code"));
    let t = parse("AArch64 t\n{ }\n P0 = broken ;\n").unwrap();
    let faults = run(&t, Model::Arm, &Options::default()).faults;
    assert_eq!(faults.into_iter().collect::<Vec<_>>(), ["P0: 0x9008: unsupported"]);
    // X30 starts at the code's exit unless the test sets it.
    assert_eq!(t.init_regs[0][30], 0x900c);
    let t = parse("AArch64 t\n{ 0:X30=0x9004; }\n P0 = broken ;\n").unwrap();
    assert_eq!(t.init_regs[0][30], 0x9004);
    assert!(err("AArch64 t\n{ }\n P0 = 1x ;\n").contains("bad thread header"));
    assert!(err("AArch64 t\n{ }\n P1 = load_b ;\n").contains("bad thread header"));
    let e = litmus::parse("AArch64 t\n{ }\n P0 = load_b ;\n").unwrap_err();
    assert!(e.contains("needs an ELF file"), "{}", e);
}

const FUNCS: &str = "
	.text
	.globl writer, reader, bad_ret, clobbers_lr
	.type writer,%function
writer:
	adrp x1, data
	add x1, x1, :lo12:data
	mov w0, #42
	str w0, [x1]
	adrp x2, flag
	mov w3, #1
	str w3, [x2, :lo12:flag]
	ret
	.size writer, .-writer
	.type reader,%function
reader:
	adrp x1, flag
	ldr w2, [x1, :lo12:flag]
	cbz w2, 1f
	adrp x3, data
	ldr w0, [x3, :lo12:data]
	ret
1:	mov w0, #-1
	ret
	.size reader, .-reader
	.type bad_ret,%function
bad_ret:
	ret x1
	.size bad_ret, .-bad_ret
	.type clobbers_lr,%function
clobbers_lr:
	mov x30, x0
	ret
	.size clobbers_lr, .-clobbers_lr
	.bss
	.globl data, flag
	.p2align 2
	.type data,%object
	.size data, 4
data:	.zero 4
	.type flag,%object
	.size flag, 4
flag:	.zero 4
";

#[test]
fn assembled_functions_as_threads() {
    let Some(mc) = llvm_mc() else { return };
    let (s, o) = (scratch("funcs.s"), scratch("funcs.o"));
    std::fs::write(&s, FUNCS).unwrap();
    let out = Command::new(&mc).args(["-triple=aarch64", "-filetype=obj", "-o"]).arg(&o).arg(&s).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let data = std::fs::read(&o).unwrap();
    let elf = Elf::parse(&data).unwrap();
    let image = Image::load(&elf, &LoadOptions::default()).unwrap();
    let pre = image.preload().unwrap();
    let t = litmus::parse_with("AArch64 MP-fn\n{ }\n P0 = writer | P1 = reader ;\nexists (1:X0=0)", &pre).unwrap();
    assert!(observed(&t, Model::Arm));
    assert!(!observed(&t, Model::Tso));
    // The reader sees the flag and then the data, or neither.
    let t = litmus::parse_with("AArch64 MP-fn\n{ }\n P0 = writer | P1 = reader ;\nforall (1:X0=-1 \\/ 1:X0=42 \\/ 1:X0=0)", &pre).unwrap();
    assert!(observed(&t, Model::Arm));
    // Returning somewhere other than the thread's return address faults.
    for f in ["bad_ret", "clobbers_lr"] {
        let t = litmus::parse_with(&format!("AArch64 t\n{{ }}\n P0 = {} ;\n", f), &pre).unwrap();
        let faults = run(&t, Model::Sc, &Options::default()).faults;
        assert!(faults.len() == 1 && faults.iter().all(|e| e.contains("branch to 0x0, which is not in the code")), "{:?}", faults);
    }
}

const C_MP: &str = r#"
int data, flag;
void writer(void) { data = 42; __atomic_store_n(&flag, 1, __ATOMIC_RELAXED); }
int reader(void) { while (!__atomic_load_n(&flag, __ATOMIC_RELAXED)) {} return data; }
void writer_rel(void) { data = 42; __atomic_store_n(&flag, 1, __ATOMIC_RELEASE); }
int reader_acq(void) { while (!__atomic_load_n(&flag, __ATOMIC_ACQUIRE)) {} return data; }
"#;

/// Compiled C: relaxed message passing goes wrong on ARM only, and
/// release/acquire fixes it. At -O0 the functions keep values on the stack.
#[test]
fn compiled_functions_as_threads() {
    let Some(mc) = llvm_mc() else { return };
    let clang = mc.replace("llvm-mc", "clang");
    if !Command::new(&clang).arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: {} not found", clang);
        return;
    }
    let c = scratch("mp.c");
    std::fs::write(&c, C_MP).unwrap();
    for opt in ["-O2", "-O0"] {
        let o = scratch(&format!("mp{}.o", opt));
        let out = Command::new(&clang)
            .args(["--target=aarch64-linux-gnu", opt, "-fno-pic", "-c", "-o"])
            .arg(&o)
            .arg(&c)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let check = |header: &str, arm_only: bool| {
            let test = scratch("mp-c.litmus");
            std::fs::write(&test, format!("AArch64 MP-c\n{{ }}\n {} ;\nexists (1:X0=0)\n", header)).unwrap();
            let out = Command::new(env!("CARGO_BIN_EXE_armem")).arg("--elf").arg(&o).arg(&test).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(text.contains("reachable on ARM only"), arm_only, "{} {}:\n{}", opt, header, text);
            assert!(!text.contains("fuel") && !text.contains("FAULT"), "{}", text);
        };
        check("P0 = writer | P1 = reader", true);
        check("P0 = writer_rel | P1 = reader_acq", false);
        check("P0 = writer_rel | P1 = reader", true);
    }
}

const C_LOCK: &str = r#"
int lock, counter;
__attribute__((noinline)) void acquire(int *l) {
  int z;
  do { z = 0; } while (!__atomic_compare_exchange_n(l, &z, 1, 0, ACQ, __ATOMIC_RELAXED));
}
__attribute__((noinline)) void release(int *l) { __atomic_store_n(l, 0, REL); }
__attribute__((noinline)) void incr(void) { counter = counter + 1; }
void (*volatile hook)(void) = incr;
void worker(void) { acquire(&lock); hook(); release(&lock); }
"#;

/// Compiled C with calls: `worker` calls `acquire` and `release` (BL, the
/// last as a tail call) and `incr` through a function pointer (BLR), and
/// saves X30 on the stack. With a relaxed lock, an increment can be lost on
/// ARM only. `-mcmodel=large` builds addresses with MOVZ/MOVK.
#[test]
fn compiled_calls() {
    let Some(mc) = llvm_mc() else { return };
    let clang = mc.replace("llvm-mc", "clang");
    if !Command::new(&clang).arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: {} not found", clang);
        return;
    }
    let c = scratch("lock.c");
    std::fs::write(&c, C_LOCK).unwrap();
    let test = scratch("lock-c.litmus");
    std::fs::write(&test, "AArch64 lock-c\n{ }\n P0 = worker | P1 = worker ;\nexists (counter=1)\n").unwrap();
    let relaxed = ["-DACQ=__ATOMIC_RELAXED", "-DREL=__ATOMIC_RELAXED"];
    let ordered = ["-DACQ=__ATOMIC_ACQUIRE", "-DREL=__ATOMIC_RELEASE"];
    for (opt, model) in [("-O0", "small"), ("-O2", "small"), ("-O2", "large")] {
        for (order, arm_only) in [(ordered, false), (relaxed, true)] {
            let tag = format!("{} {} {}", opt, model, order[0]);
            let o = scratch("lock.o");
            let out = Command::new(&clang)
                .args(["--target=aarch64-linux-gnu", opt, &format!("-mcmodel={}", model), "-fno-pic", "-c", "-o"])
                .arg(&o)
                .args(order)
                .arg(&c)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            if model == "large" {
                let data = std::fs::read(&o).unwrap();
                let elf = Elf::parse(&data).unwrap();
                let code = Image::load(&elf, &LoadOptions::default()).unwrap().code().unwrap().unwrap();
                assert!(code.instrs.iter().any(|i| matches!(i, armem::isa::Instr::Movk { .. })), "{}: no MOVK", tag);
            }
            // -O0 code spins for longer.
            let out = Command::new(env!("CARGO_BIN_EXE_armem")).args(["-f", "1000", "--elf"]).arg(&o).arg(&test).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(text.contains("reachable on ARM only"), arm_only, "{}:\n{}", tag, text);
            assert!(!text.contains("fuel") && !text.contains("FAULT"), "{}:\n{}", tag, text);
        }
    }
}

/// Arithmetic that compilers turn into bitfield moves (UBFX, BFI, SXTH,
/// ...), multiply-adds and high multiplies (division by a constant),
/// divisions, inverted logical operations, shifted operands, CSINV/CSNEG,
/// CCMP, byte and bit reversal (REV, RBIT), leading-bit counts (CLZ,
/// and CLS, though clang does not emit it), rotates and EXTR, and 128-bit addition and subtraction (ADC,
/// SBC). The hash of the results must be the same as when the code is
/// compiled for the host and run natively.
const C_ARITH: &str = r#"typedef __UINT8_TYPE__ uint8_t;
typedef __INT8_TYPE__ int8_t;
typedef __INT16_TYPE__ int16_t;
#define INT64_MIN (-__INT64_MAX__ - 1)
#define INT32_MIN (-__INT32_MAX__ - 1)
typedef __UINT64_TYPE__ u64;
typedef __INT64_TYPE__ i64;
typedef __UINT32_TYPE__ u32;
typedef __INT32_TYPE__ i32;
struct bits { u32 lo : 5, mid : 11, hi : 16; };
u64 in[] = { 0, 1, 7, 100, 0x8000000000f0a5c3, -1, -100, 0x7fffffff, 0x80000000, 0x123456789abcdef0 };
#define N (sizeof in / sizeof in[0])
u64 h;

/* FNV-1a style; x must not depend on *h, or the mix can lose its bits. */
static inline __attribute__((always_inline)) void mix(u64 *h, u64 x) { *h = (*h ^ x) * 0x100000001b3; }

__attribute__((noinline)) void ops(u64 a, u64 b, u64 *h) {
  i64 sa = a, sb = b; u32 wa = a, wb = b; i32 swa = a, swb = b;
  struct bits f; __builtin_memcpy(&f, &wa, 4);
  mix(h, (a >> 4) & 0xff);
  mix(h, (u64)((i64)(a << 2) >> 59));
  mix(h, (a & 0xff) << 24);
  mix(h, (u64)(i64)(int8_t)a << 8);
  mix(h, (b & ~0xf00ull) | ((a & 0xf) << 8));
  mix(h, f.lo + f.mid * 3 + f.hi);
  f.mid = wb; u32 fw; __builtin_memcpy(&fw, &f, 4); mix(h, fw);
  mix(h, (int16_t)a); mix(h, (uint8_t)b); mix(h, (u32)(int8_t)wa);
  mix(h, b ? a / b : 1); mix(h, b ? a % b : 2);
  mix(h, wb ? wa / wb : 3); mix(h, wb ? wa % wb : 4);
  mix(h, sb && !(sa == INT64_MIN && sb == -1) ? sa / sb : 5);
  mix(h, sb && !(sa == INT64_MIN && sb == -1) ? sa % sb : 6);
  mix(h, swb && !(swa == INT32_MIN && swb == -1) ? swa / swb : 7);
  mix(h, swb && !(swa == INT32_MIN && swb == -1) ? swa % swb : 8);
  mix(h, a / 10); mix(h, sa / 7); mix(h, wa / 3);
  mix(h, a * b + (b >> 3)); mix(h, (a << 5) - a * b); mix(h, -(a * b)); mix(h, wa * wb + wa);
  mix(h, (i64)swa * swb); mix(h, sa - (i64)swa * (i64)(i32)(b >> 32));
  mix(h, wa + (u32)(swb >> 7)); mix(h, wa ^ ((wb >> 13) | (wb << 19))); mix(h, a ^ ((b >> 13) | (b << 51)));
  mix(h, a & ~b); mix(h, a | ~b); mix(h, a ^ ~b); mix(h, wa & ~(wb << 3)); mix(h, ~a);
  mix(h, sa < 0 ? -sa : sa); mix(h, swa < 5 ? ~swb : swa); mix(h, a > b ? -a : b);
  mix(h, a == b ? -1 : 0); mix(h, a == 3 ? b : ~b); mix(h, (a < b) ? a + 1 : b);
  mix(h, (a > 5 && b < 9) ? 11 : 13); mix(h, (sa < sb || a == 100) ? 17 : 19);
  mix(h, (wa != 0 && wb == wa) ? 23 : 29); mix(h, (a == b || a == -b) ? 31 : 37);
  mix(h, __builtin_bswap64(a)); mix(h, __builtin_bswap32(wb)); mix(h, __builtin_bswap16(a));
  mix(h, __builtin_bitreverse64(a)); mix(h, __builtin_bitreverse32(wb));
  mix(h, a ? __builtin_clzll(a) : 64); mix(h, wb ? __builtin_clz(wb) : 32); mix(h, wb ? __builtin_ctz(wb) : 32);
  mix(h, __builtin_clrsbll(sa)); mix(h, __builtin_clrsb(swb));
  mix(h, (a >> (b & 63)) | (a << (-b & 63))); mix(h, (wa >> 7) | (wa << 25)); mix(h, (a << 9) | (b >> 55));
  /* No __int128 variables: at -O0 clang spills them through SIMD registers. */
  mix(h, (u64)((((unsigned __int128)a << 64 | b) + ((unsigned __int128)b << 64 | a)) >> 64));
  mix(h, (u64)((((unsigned __int128)a << 64 | b) - ((unsigned __int128)b << 64 | wa)) >> 64));
}

void run(void) {
  u64 x = 0xcbf29ce484222325;
  for (unsigned i = 0; i < N; i++)
    for (unsigned j = 0; j < N; j++)
      ops(in[i], in[j], &x);
  h = x;
}

#ifdef NATIVE
int printf(const char *, ...);
int main(void) { run(); printf("%lld\n", (long long)h); }
#endif
"#;

#[test]
fn compiled_arithmetic() {
    let Some(mc) = llvm_mc() else { return };
    let clang = mc.replace("llvm-mc", "clang");
    if !Command::new(&clang).arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: {} not found", clang);
        return;
    }
    let c = scratch("arith.c");
    std::fs::write(&c, C_ARITH).unwrap();
    let native = scratch("arith-native");
    let out = Command::new(&clang).args(["-O2", "-DNATIVE", "-o"]).arg(&native).arg(&c).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let Ok(out) = Command::new(&native).output() else {
        eprintln!("skipping: cannot run natively compiled code");
        return;
    };
    let want = String::from_utf8(out.stdout).unwrap();
    let test = scratch("arith.litmus");
    std::fs::write(&test, format!("AArch64 arith\n{{ }}\n P0 = run ;\nforall (h={})\n", want.trim())).unwrap();
    for opt in ["-O0", "-O1", "-O2", "-Os"] {
        let o = scratch(&format!("arith{}.o", opt));
        let out = Command::new(&clang)
            .args(["--target=aarch64-linux-gnu", opt, "-fno-pic", "-c", "-o"])
            .arg(&o)
            .arg(&c)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        if opt == "-O2" {
            use armem::isa::{AluOp, Extend, Instr, MulKind, Operand, UnaryOp};
            let data = std::fs::read(&o).unwrap();
            let elf = Elf::parse(&data).unwrap();
            let code = Image::load(&elf, &LoadOptions::default()).unwrap().code().unwrap().unwrap();
            let kinds: [(&str, fn(&Instr) -> bool); 19] = [
                ("bitfield", |i| matches!(i, Instr::Bitfield { .. })),
                ("MADD/MSUB", |i| matches!(i, Instr::Madd { kind: MulKind::Plain, .. })),
                ("UMULL", |i| matches!(i, Instr::Madd { kind: MulKind::UnsignedLong, .. })),
                ("SMULL/SMSUBL", |i| matches!(i, Instr::Madd { kind: MulKind::SignedLong, .. })),
                ("SMULH", |i| matches!(i, Instr::Mulh { signed: true, .. })),
                ("UMULH", |i| matches!(i, Instr::Mulh { signed: false, .. })),
                ("UDIV/SDIV", |i| matches!(i, Instr::Alu { op: AluOp::Udiv | AluOp::Sdiv, .. })),
                ("BIC/ORN/EON", |i| matches!(i, Instr::Alu { op: AluOp::Bic | AluOp::Orn | AluOp::Eon, .. })),
                ("CSINV/CSNEG", |i| matches!(i, Instr::Csel { inv: true, .. })),
                ("CCMP/CCMN", |i| matches!(i, Instr::Ccmp { .. })),
                ("ASR operand", |i| matches!(i, Instr::Alu { op2: Operand::RegExt(_, Extend::Asr(_)), .. })),
                ("ROR operand", |i| matches!(i, Instr::Alu { op2: Operand::RegExt(_, Extend::Ror(_)), .. })),
                ("REV", |i| matches!(i, Instr::Unary { op: UnaryOp::Rev, .. })),
                ("RBIT", |i| matches!(i, Instr::Unary { op: UnaryOp::Rbit, .. })),
                ("CLZ", |i| matches!(i, Instr::Unary { op: UnaryOp::Clz, .. })),
                ("EXTR", |i| matches!(i, Instr::Extr { rn, rm, .. } if rn != rm)),
                ("ROR immediate", |i| matches!(i, Instr::Extr { rn, rm, .. } if rn == rm)),
                ("RORV", |i| matches!(i, Instr::Alu { op: AluOp::Ror, .. })),
                ("ADC/SBC", |i| matches!(i, Instr::Adc { .. })),
            ];
            for (name, f) in kinds {
                assert!(code.instrs.iter().any(f), "no {} in the -O2 code", name);
            }
        }
        let out = Command::new(env!("CARGO_BIN_EXE_armem")).args(["-f", "300000", "--elf"]).arg(&o).arg(&test).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(text.contains("condition forall: Always"), "{}:\n{}", opt, text);
        assert!(!text.contains("fuel") && !text.contains("FAULT"), "{}:\n{}", opt, text);
    }
}
