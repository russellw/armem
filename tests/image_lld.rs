//! Checks relocation against lld.
//!
//! Each object file is loaded by armem (`image::Image::load`), then linked
//! by lld with a linker script that puts every allocated section at the
//! address armem chose, and the same addresses for undefined symbols. Every
//! section's contents and every symbol's address must then agree, and each
//! function must decode the same way from both.
//!
//! The objects are an assembly file using every supported relocation type
//! (in two layouts: compact, and with data placed far from the code), and
//! C files compiled by clang with several options.
//!
//! The tests are skipped (with a message) when `llvm-mc` or lld is not
//! found. lld is looked for as `$ARMEM_LLD`, `ld.lld<suffix>`, and the
//! `rust-lld` shipped with Rust toolchains.

use armem::elf::*;
use armem::image::{Image, LoadOptions};
use armem::isa::{Instr, RegOp};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn llvm_suffix() -> Option<String> {
    let candidates: Vec<String> = match std::env::var("ARMEM_LLVM_SUFFIX") {
        Ok(s) => vec![s],
        Err(_) => std::iter::once(String::new()).chain((14..=30).rev().map(|v| format!("-{}", v))).collect(),
    };
    candidates
        .into_iter()
        .find(|s| Command::new(format!("llvm-mc{}", s)).arg("--version").output().is_ok_and(|o| o.status.success()))
}

/// An lld command, with `-flavor gnu` for rust-lld.
fn lld(suffix: &str) -> Option<Command> {
    let works = |c: &mut Command| c.arg("--version").output().is_ok_and(|o| o.status.success());
    if let Ok(p) = std::env::var("ARMEM_LLD") {
        let flavor = p.ends_with("rust-lld");
        let make = || {
            let mut c = Command::new(&p);
            if flavor {
                c.args(["-flavor", "gnu"]);
            }
            c
        };
        return works(&mut make()).then(make);
    }
    for name in [format!("ld.lld{}", suffix), "ld.lld".into()] {
        if works(&mut Command::new(&name)) {
            return Some(Command::new(name));
        }
    }
    // rust-lld from rustup toolchains or the active sysroot.
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("RUSTUP_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".rustup"))) {
        if let Ok(d) = std::fs::read_dir(home.join("toolchains")) {
            roots.extend(d.flatten().map(|e| e.path()));
        }
    }
    if let Ok(o) = Command::new("rustc").args(["--print", "sysroot"]).output() {
        roots.push(PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    }
    for root in roots {
        let Ok(hosts) = std::fs::read_dir(root.join("lib/rustlib")) else { continue };
        for h in hosts.flatten() {
            let p = h.path().join("bin/rust-lld");
            let make = || {
                let mut c = Command::new(&p);
                c.args(["-flavor", "gnu"]);
                c
            };
            if p.exists() && works(&mut make()) {
                return Some(make());
            }
        }
    }
    None
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("image_lld");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().unwrap();
    if out.status.success() { Ok(()) } else { Err(format!("{:?}: {}", cmd, String::from_utf8_lossy(&out.stderr))) }
}

struct Tools {
    suffix: String,
}

impl Tools {
    fn get() -> Option<Tools> {
        let Some(suffix) = llvm_suffix() else {
            eprintln!("skipping: llvm-mc not found");
            return None;
        };
        if lld(&suffix).is_none() {
            eprintln!("skipping: lld not found (set ARMEM_LLD)");
            return None;
        }
        Some(Tools { suffix })
    }

    fn assemble(&self, src: &str, tag: &str) -> PathBuf {
        let (s, o) = (scratch(&format!("{}.s", tag)), scratch(&format!("{}.o", tag)));
        std::fs::write(&s, src).unwrap();
        run(Command::new(format!("llvm-mc{}", self.suffix)).args(["-triple=aarch64", "-mattr=+all", "-filetype=obj", "-o"]).arg(&o).arg(&s)).unwrap();
        o
    }

    fn clang(&self) -> Option<Command> {
        let name = format!("clang{}", self.suffix);
        Command::new(&name).arg("--version").output().ok()?.status.success().then(|| Command::new(name))
    }

    /// Link `obj` with lld at the addresses `image` uses.
    fn link_like(&self, obj: &Path, image: &Image, opts: &LoadOptions, tag: &str) -> Result<Vec<u8>> {
        let elf = image.elf;
        let mut placed: Vec<(u64, &str)> = (0..elf.sections.len())
            .filter_map(|i| Some((image.section_addrs[i]?, elf.sections[i].name.as_str())))
            .chain(image.got.map(|g| (g, ".got")))
            .collect();
        placed.sort();
        let mut script = String::from("SECTIONS {\n");
        for (a, n) in placed {
            script += &format!("  {} {:#x} : {{ *({}) }}\n", n, a, n);
        }
        script += "}\n";
        let (ld, out) = (scratch(&format!("{}.ld", tag)), scratch(&format!("{}.out", tag)));
        std::fs::write(&ld, script).unwrap();
        let mut cmd = lld(&self.suffix).unwrap();
        cmd.args(["-m", "aarch64linux", "--no-relax", "-O0", "-static", "-e", "0", "-T"]).arg(&ld).arg(obj).arg("-o").arg(&out);
        for (name, a) in &opts.externs {
            cmd.arg(format!("--defsym={}={:#x}", name, a));
        }
        run(&mut cmd)?;
        Ok(std::fs::read(&out).unwrap())
    }
}

/// Load `obj` with armem and lld and compare everything. Returns the
/// number of functions that decoded.
fn compare(t: &Tools, obj: &Path, opts: &LoadOptions, tag: &str) -> usize {
    let data = std::fs::read(obj).unwrap();
    let elf = Elf::parse(&data).unwrap();
    let image = Image::load(&elf, opts).unwrap_or_else(|e| panic!("{}: {}", tag, e));
    let linked = t.link_like(obj, &image, opts, tag).unwrap_or_else(|e| panic!("{}: {}", tag, e));
    let out = Elf::parse(&linked).unwrap();
    let mut failures = Vec::new();

    for (i, s) in elf.sections.iter().enumerate() {
        let Some(a) = image.section_addrs[i] else { continue };
        let Some(j) = out.section_by_name(&s.name) else {
            failures.push(format!("lld output has no section '{}'", s.name));
            continue;
        };
        let o = &out.sections[j];
        if (o.addr, o.size, o.kind) != (a, s.size, s.kind) {
            failures.push(format!("section '{}': armem {:#x}+{:#x}, lld {:#x}+{:#x}", s.name, a, s.size, o.addr, o.size));
            continue;
        }
        let ours = image.read(a, s.size).unwrap_or(&[]);
        let theirs = if o.kind == SHT_NOBITS { &vec![0; o.size as usize][..] } else { out.section_data(j).unwrap() };
        if ours != theirs {
            let k = ours.iter().zip(theirs).position(|(x, y)| x != y).unwrap_or(0);
            failures.push(format!("section '{}' differs at offset {:#x}: armem {:02x?}, lld {:02x?}", s.name, k, &ours[k & !3..], &theirs[k & !3..]).chars().take(300).collect());
        }
    }

    // Symbol addresses, for names that are unique in both files.
    let unique = |e: &Elf, n: &str| e.symbols.iter().filter(|s| s.name == n).count() == 1;
    let mut symbols = 0;
    for (s, a) in elf.symbols.iter().zip(&image.symbol_addrs) {
        if s.name.is_empty() || s.mapping().is_some() || s.kind == SymKind::File || !unique(&elf, &s.name) {
            continue;
        }
        let Some(a) = a else { continue };
        if !unique(&out, &s.name) {
            continue;
        }
        symbols += 1;
        let o = out.symbols.iter().find(|o| o.name == s.name).unwrap();
        if o.value != *a {
            failures.push(format!("symbol '{}': armem {:#x}, lld {:#x}", s.name, a, o.value));
        }
    }
    assert!(symbols > 0, "{}: no symbols compared", tag);

    // Functions decode the same from both.
    let mut decoded = 0;
    for s in elf.symbols.iter().filter(|s| s.kind == SymKind::Func && s.section != SymSection::Undefined) {
        let ours = image.decode_function(&s.name);
        let theirs = out.decode_function(&s.name);
        match (&ours, &theirs) {
            (Ok(x), Ok(y)) if x == y => decoded += 1,
            (Err(_), Err(_)) => {}
            _ => failures.push(format!("function '{}': armem {:?}, lld {:?}", s.name, ours, theirs)),
        }
    }
    assert!(failures.is_empty(), "{}:\n{}", tag, failures.join("\n"));
    decoded
}

const RELOCS: &str = "
	.text
	.globl f
	.type f,%function
f:
	adrp x0, data_sym
	add x0, x0, :lo12:data_sym
	ldrb w1, [x0, :lo12:data_sym]
	ldrh w1, [x0, :lo12:hsym]
	ldr w1, [x0, :lo12:wsym]
	ldr x1, [x0, :lo12:xsym]
	ldr q0, [x0, :lo12:qsym]
	adr x2, g
	ldr x3, far_lit
	b.eq g
	cbz x0, g
	tbz x0, #3, g
	bl ext_fn
	b g
	movz x4, #:abs_g3:data_sym
	movk x4, #:abs_g2_nc:data_sym
	movk x4, #:abs_g1_nc:data_sym
	movk x4, #:abs_g0_nc:data_sym
	movz x5, #:abs_g0:ext_small
	movz x5, #:abs_g1:data_sym
	movz x5, #:abs_g2:data_sym
	adrp x6, weak_undef
	add x6, x6, :lo12:weak_undef
	.size f, .-f

	.section .text.g,\"ax\",%progbits
	.globl g
	.type g,%function
g:	nop
	b f
	.size g, .-g
	.p2align 3
far_lit: .quad 7

	.data
	.p2align 4
data_sym: .byte 1
	.p2align 1
hsym:	.hword 2
	.p2align 2
wsym:	.word 3
	.p2align 3
xsym:	.quad 4
	.p2align 4
qsym:	.quad 5, 6
	.quad ext_data + 8
	.quad f - .
	.word data_sym + 3
	.word f - . - 4
	.word ext_fn@PLT - . + 8
	.hword ext_small

	.section .data.near,\"aw\",%progbits
	.hword g - .

	.bss
	.p2align 3
zeros:	.zero 24

	.weak weak_undef
";

fn relocs_externs() -> BTreeMap<String, u64> {
    [("ext_fn", 0x80_0000), ("ext_data", 0x1234_5678_9abc), ("ext_small", 0x1234)].into_iter().map(|(n, a)| (n.to_string(), a)).collect()
}

#[test]
fn every_relocation_type() {
    let Some(t) = Tools::get() else { return };
    let obj = t.assemble(RELOCS, "relocs");
    let data = std::fs::read(&obj).unwrap();
    let elf = Elf::parse(&data).unwrap();
    let kinds: std::collections::BTreeSet<u32> = elf.relocs.iter().map(|r| r.kind).collect();
    let want = [257, 258, 259, 260, 261, 262, 263, 264, 265, 266, 267, 268, 269, 273, 274, 275, 277, 278, 279, 280, 282, 283, 284, 285, 286, 299, 314];
    for k in want {
        assert!(kinds.contains(&k), "the test does not use {}", reloc_display(k));
    }
    // Compact layout.
    let opts = LoadOptions { externs: relocs_externs(), ..LoadOptions::default() };
    compare(&t, &obj, &opts, "relocs");
    // Data far from the code, and code in a different order.
    let mut far = opts.clone();
    far.section_addrs = [(".data", 0x7000_0000), (".text.g", 0x40_0000), (".text", 0x40_1000), (".data.near", 0x40_8000), (".bss", 0xf000_0000)]
        .into_iter()
        .map(|(n, a)| (n.to_string(), a))
        .collect();
    compare(&t, &obj, &far, "relocs-far");
}

#[test]
fn relocation_errors() {
    let Some(t) = Tools::get() else { return };
    let obj = t.assemble(RELOCS, "relocs-err");
    let data = std::fs::read(&obj).unwrap();
    let elf = Elf::parse(&data).unwrap();
    let err = |opts: &LoadOptions| Image::load(&elf, opts).unwrap_err();
    let mut opts = LoadOptions { externs: relocs_externs(), ..LoadOptions::default() };
    opts.externs.remove("ext_fn");
    let e = err(&opts);
    assert!(e.contains("R_AARCH64_CALL26") && e.contains("'ext_fn'") && e.contains("undefined"), "{}", e);
    // A call more than 128 MiB away.
    let mut opts = LoadOptions { externs: relocs_externs(), ..LoadOptions::default() };
    opts.externs.insert("ext_fn".into(), 0x1000_0000);
    let e = err(&opts);
    assert!(e.contains("R_AARCH64_CALL26") && e.contains("out of range"), "{}", e);
    // :abs_g1: of an address above 4 GiB.
    let mut opts = LoadOptions { externs: relocs_externs(), ..LoadOptions::default() };
    opts.section_addrs.insert(".data".into(), 0x1_0000_0000);
    let e = err(&opts);
    assert!(e.contains("R_AARCH64_MOVW_UABS_G1 ") && e.contains("out of range"), "{}", e);
    // Misaligned :lo12: for an 8-byte load.
    let mut opts = LoadOptions { externs: relocs_externs(), ..LoadOptions::default() };
    opts.section_addrs.insert(".data".into(), 0x2_0004);
    assert!(err(&opts).contains("not aligned"));
    opts.section_addrs.insert(".data".into(), 0x10_0000);
    opts.section_addrs.insert(".data.near".into(), 0x10_0000);
    let e = err(&opts);
    assert!(e.contains("overlaps"), "{}", e);
    opts.section_addrs.clear();
    opts.section_addrs.insert(".nope".into(), 0);
    assert!(err(&opts).contains("'.nope'"));
}

/// Functions made of supported instructions, using globals through ADRP.
#[test]
fn relocated_functions_run() {
    let Some(t) = Tools::get() else { return };
    let src = "
	.text
	.globl writer
	.type writer,%function
writer:
	adrp x1, data
	add x1, x1, :lo12:data
	adrp x2, flag
	mov w0, #42
	str w0, [x1]
	mov w3, #1
	stlr w3, [x2]
	.size writer, .-writer
	.globl reader
	.type reader,%function
reader:
	adrp x1, flag
	add x1, x1, :lo12:flag
1:	ldar w0, [x1]
	cbz w0, 1b
	adr x3, data
	ldr w2, [x3]
	.size reader, .-reader
	.data
	.p2align 12
	.globl data
data:	.word 0
	.zero 4092
	.globl flag
flag:	.word 0
";
    let obj = t.assemble(src, "run");
    let opts = LoadOptions { base: 0x10_0000, ..LoadOptions::default() };
    assert_eq!(compare(&t, &obj, &opts, "run"), 2);
    let data = std::fs::read(&obj).unwrap();
    let elf = Elf::parse(&data).unwrap();
    // Without relocation, ADRP cannot be decoded.
    assert!(elf.decode_function("writer").unwrap_err().contains("R_AARCH64_ADR_PREL_PG_HI21"));
    let image = Image::load(&elf, &opts).unwrap();
    let (d, f) = (image.symbol("data").unwrap(), image.symbol("flag").unwrap());
    let text = elf.section_by_name(".text").unwrap();
    assert_eq!((d % 4096, f, image.section_addrs[text]), (0, d + 4096, Some(0x10_0000)));
    let x = |i| RegOp { idx: i, wide: true };
    let w = image.decode_function("writer").unwrap();
    assert_eq!(&w[..3], [Instr::MovImm { rd: x(1), imm: d }, w[1], Instr::MovImm { rd: x(2), imm: f }]);
    let r = image.decode_function("reader").unwrap();
    assert_eq!(r[0], Instr::MovImm { rd: x(1), imm: f });
    assert_eq!(r[4], Instr::MovImm { rd: x(3), imm: d });
}

const C_SRC: &str = r#"
long g = 1;
long arr[8] = {1, 2, 3};
static long local_data[4];
static int counter;
const char *msg = "hello";
const char *msgs[] = {"a", "bb", "hello"};
long *gp = &arr[3];
extern long ext_var;
extern long ext(long);
__attribute__((weak)) extern long maybe(long);
__attribute__((weak)) long weak_fn(long x) { return x + 1; }
static long helper(long *p) { return p[0] ^ local_data[1]; }
long uses_global(long *p) { counter++; return *p + g + helper(p) + arr[5]; }
long jump_table(int x) {
    switch (x) {
    case 0: return 11; case 1: return g; case 2: return 37; case 3: return 41;
    case 4: return 43; case 5: return 47; case 6: return ext_var; default: return 0;
    }
}
long calls_out(long x) { return ext(x) + weak_fn(x) + (maybe ? maybe(x) : 0); }
long tail(long x) { return ext(x + 1); }
const char *pick(int i) { return msgs[i] ? msgs[i] : msg; }
"#;

#[test]
fn compiled_objects() {
    let Some(t) = Tools::get() else { return };
    if t.clang().is_none() {
        eprintln!("skipping: clang{} not found", t.suffix);
        return;
    }
    let c = scratch("c.c");
    std::fs::write(&c, C_SRC).unwrap();
    // `ext` must be within branch range of the code.
    let externs = |base: u64| -> BTreeMap<String, u64> {
        [("ext", base + 0x10_0000), ("ext_var", 0x7654_3210)].into_iter().map(|(n, a)| (n.to_string(), a)).collect()
    };
    let common = ["--target=aarch64-linux-gnu", "-fno-pic", "-fno-asynchronous-unwind-tables", "-c"];
    let variants: [(&str, &[&str]); 5] = [
        ("O2", &["-O2"]),
        ("O0", &["-O0"]),
        ("sections", &["-O2", "-ffunction-sections", "-fdata-sections"]),
        ("large", &["-O2", "-mcmodel=large"]),
        ("large-O0", &["-O0", "-mcmodel=large", "-ffunction-sections", "-fdata-sections"]),
    ];
    for (tag, flags) in variants {
        let o = scratch(&format!("c-{}.o", tag));
        run(t.clang().unwrap().args(common).args(flags).arg(&c).arg("-o").arg(&o)).unwrap();
        let opts = LoadOptions { externs: externs(0x1_0000), ..LoadOptions::default() };
        compare(&t, &o, &opts, tag);
        let opts = LoadOptions { base: 0x4000_0000, externs: externs(0x4000_0000), ..LoadOptions::default() };
        compare(&t, &o, &opts, &format!("{}-high", tag));
    }
    // Position-independent code reaches globals through the GOT.
    let o = scratch("c-pic.o");
    run(t.clang().unwrap().args(common).args(["-O2", "-fPIC"]).arg(&c).arg("-o").arg(&o)).unwrap();
    let opts = LoadOptions { externs: externs(0x1_0000), ..LoadOptions::default() };
    compare(&t, &o, &opts, "pic");
    let data = std::fs::read(&o).unwrap();
    let elf = Elf::parse(&data).unwrap();
    assert!(Image::load(&elf, &opts).unwrap().got.is_some());
}
