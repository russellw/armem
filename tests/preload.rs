//! Tests that start from the memory of a loaded ELF image.
//!
//! The ELF tests assemble a data-only object with `llvm-mc` and are
//! skipped (with a message) when it is not installed.

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
