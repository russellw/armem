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
    use armem::isa::{Instr, LoadKind};
    // load_b: LDR W0,[X1]
    let load = Instr::Load { rt: w(0), addr: addr(1), size: Size::B4, signed: false, kind: LoadKind::Plain, excl: false };
    pre.functions.insert("load_b".into(), Ok(vec![load]));
    pre.functions.insert("broken".into(), Err("function 'broken': unsupported".into()));
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
    assert!(err("AArch64 t\n{ }\n P0 = broken ;\n").contains("P0: function 'broken': unsupported"));
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
    let e = litmus::parse_with("AArch64 t\n{ }\n P0 = bad_ret ;\n", &pre).unwrap_err();
    assert!(e.contains("RET X1"), "{}", e);
    let e = litmus::parse_with("AArch64 t\n{ }\n P0 = clobbers_lr ;\n", &pre).unwrap_err();
    assert!(e.contains("writes X30"), "{}", e);
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
