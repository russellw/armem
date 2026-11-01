//! Checks the ELF loader against LLVM.
//!
//! * Every litmus thread is assembled with `llvm-mc` into an object file,
//!   as a function placed after another one. Extracting and decoding it
//!   must give the program the assembly parser produces.
//! * The sections, symbols and relocations armem reads from assembled and
//!   compiled objects must match what `llvm-readobj` reports, including
//!   an object with more than 0xff00 sections (extended section numbers).
//! * Extracting a function that still needs relocation, or that contains
//!   data, is an error.
//!
//! The tests are skipped (with a message) when the LLVM tools are not
//! installed; the C test also needs `clang`. Set `ARMEM_LLVM_SUFFIX` to
//! choose a versioned install, e.g. `-21`.

use armem::asm::assemble;
use armem::elf::*;
use armem::litmus;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn llvm_suffix() -> Option<String> {
    let candidates: Vec<String> = match std::env::var("ARMEM_LLVM_SUFFIX") {
        Ok(s) => vec![s],
        Err(_) => std::iter::once(String::new()).chain((14..=30).rev().map(|v| format!("-{}", v))).collect(),
    };
    candidates.into_iter().find(|s| {
        ["llvm-mc", "llvm-readobj"]
            .iter()
            .all(|t| Command::new(format!("{}{}", t, s)).arg("--version").output().is_ok_and(|o| o.status.success()))
    })
}

fn tool(suffix: &str, name: &str) -> Command {
    Command::new(format!("{}{}", name, suffix))
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("elf_llvm");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{:?}: {}", cmd, String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Assemble `src` into an object file and return its contents.
fn assemble_obj(suffix: &str, src: &str, tag: &str) -> Vec<u8> {
    let (s, o) = (scratch(&format!("{}.s", tag)), scratch(&format!("{}.o", tag)));
    std::fs::write(&s, src).unwrap();
    run(tool(suffix, "llvm-mc").args(["-triple=aarch64", "-mattr=+all", "-filetype=obj", "-o"]).arg(&o).arg(&s));
    std::fs::read(&o).unwrap()
}

/// A block of `llvm-readobj` output: `Key: value` fields.
type Record = BTreeMap<String, String>;

/// Parse `llvm-readobj --file-header --sections --symbols --relocs
/// --expand-relocs` output into the header fields and the section, symbol
/// and relocation records (relocations get a `From` field: the index of
/// their relocation section).
fn readobj(suffix: &str, path: &Path) -> (Record, Vec<Record>, Vec<Record>, Vec<Record>) {
    let out = run(tool(suffix, "llvm-readobj")
        .args(["--file-header", "--sections", "--symbols", "--relocs", "--expand-relocs"])
        .arg(path));
    let text = String::from_utf8(out).unwrap();
    let (mut header, mut secs, mut syms, mut rels) = (Record::new(), Vec::new(), Vec::new(), Vec::new());
    let mut cur: Option<(&str, Record)> = None;
    let mut from = String::new();
    let mut depth = 0; // nesting of `Key [` lists inside a record
    for line in text.lines() {
        let t = line.trim();
        if let Some((kind, rec)) = &mut cur {
            if t == "}" && depth == 0 {
                let rec = std::mem::take(rec);
                match *kind {
                    "Section" => secs.push(rec),
                    "Symbol" => syms.push(rec),
                    _ => rels.push(rec),
                }
                cur = None;
            } else if t == "]" {
                depth -= 1;
            } else if depth == 0 {
                if let Some((k, v)) = t.split_once(':') {
                    rec.insert(k.trim().into(), v.trim().into());
                } else if let Some((k, v)) = t.split_once(" [") {
                    // `Flags [ (0x6)` or `Other [ (0x2)`: keep the value.
                    rec.insert(k.trim().into(), v.trim().into());
                    depth += 1;
                }
            }
            continue;
        }
        match t {
            "Section {" | "Symbol {" | "Relocation {" => {
                let mut rec = Record::new();
                if t == "Relocation {" {
                    rec.insert("From".into(), from.clone());
                }
                cur = Some((t.split(' ').next().unwrap(), rec));
            }
            _ if t.starts_with("Section (") && t.ends_with('{') => {
                from = t["Section (".len()..t.find(')').unwrap()].to_string();
            }
            _ => {
                if let Some((k, v)) = t.split_once(':') {
                    header.entry(k.trim().into()).or_insert_with(|| v.trim().into());
                }
            }
        }
    }
    (header, secs, syms, rels)
}

/// The number in a field such as `24`, `0x1B0`, `-0x4`, `Local (0x0)` or
/// `(0x6)`.
fn num(v: &str) -> i128 {
    let v = match (v.rfind('('), v.ends_with(')')) {
        (Some(i), true) => &v[i + 1..v.len() - 1],
        _ => v,
    };
    let (neg, v) = match v.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, v),
    };
    let n = match v.strip_prefix("0x") {
        Some(h) => i128::from_str_radix(h, 16),
        None => v.parse(),
    }
    .unwrap_or_else(|_| panic!("bad number '{}'", v));
    if neg { -n } else { n }
}

/// The text of a field such as `.text (9)`, without the number.
fn name(v: &str) -> &str {
    match v.rfind(" (") {
        Some(i) if v.ends_with(')') => &v[..i],
        _ if v.starts_with('(') => "",
        _ => v,
    }
}

/// Compare everything armem reads from `obj` with llvm-readobj.
fn check_against_readobj(suffix: &str, path: &Path) -> Elf<'static> {
    let data: &'static [u8] = Vec::leak(std::fs::read(path).unwrap());
    let elf = Elf::parse(data).unwrap_or_else(|e| panic!("{}: {}", path.display(), e));
    let (header, secs, syms, rels) = readobj(suffix, path);
    let ctx = path.display();
    let kind = match elf.kind {
        FileKind::Relocatable => 1,
        FileKind::Executable => 2,
        FileKind::Shared => 3,
    };
    assert_eq!(kind, num(&header["Type"]), "{}: file type", ctx);
    assert_eq!(elf.entry as i128, num(&header["Entry"]), "{}: entry", ctx);

    assert_eq!(elf.sections.len(), secs.len(), "{}: section count", ctx);
    for (i, (s, r)) in elf.sections.iter().zip(&secs).enumerate() {
        let got = (
            i as i128,
            s.name.as_str(),
            s.kind as i128,
            s.flags as i128,
            s.addr as i128,
            s.offset as i128,
            s.size as i128,
            s.link as i128,
            s.info as i128,
            s.align as i128,
            s.entsize as i128,
        );
        let f = |k: &str| num(&r[k]);
        let want = (
            f("Index"),
            name(&r["Name"]),
            f("Type"),
            f("Flags"),
            f("Address"),
            f("Offset"),
            f("Size"),
            f("Link"),
            f("Info"),
            f("AddressAlignment"),
            f("EntrySize"),
        );
        assert_eq!(got, want, "{}: section {}", ctx, i);
    }

    assert_eq!(elf.symbols.len(), syms.len(), "{}: symbol count", ctx);
    for (i, (s, r)) in elf.symbols.iter().zip(&syms).enumerate() {
        let section = match s.section {
            SymSection::Undefined => 0,
            SymSection::Absolute => 0xfff1,
            SymSection::Common => 0xfff2,
            SymSection::Reserved(x) => x as i128,
            SymSection::Index(x) => x as i128,
        };
        let kind = match s.kind {
            SymKind::NoType => 0,
            SymKind::Object => 1,
            SymKind::Func => 2,
            SymKind::Section => 3,
            SymKind::File => 4,
            SymKind::Common => 5,
            SymKind::Tls => 6,
            SymKind::Other(t) => t as i128,
        };
        let bind = match s.bind {
            Binding::Local => 0,
            Binding::Global => 1,
            Binding::Weak => 2,
            Binding::Other(b) => b as i128,
        };
        // llvm-readobj shows a section symbol by its section's name.
        let sym_name = match (s.kind, s.section) {
            (SymKind::Section, SymSection::Index(x)) if s.name.is_empty() => elf.sections[x].name.as_str(),
            _ => s.name.as_str(),
        };
        let got = (sym_name, s.value as i128, s.size as i128, bind, kind, s.other as i128, section);
        let f = |k: &str| num(&r[k]);
        let want = (name(&r["Name"]), f("Value"), f("Size"), f("Binding"), f("Type"), f("Other"), f("Section"));
        assert_eq!(got, want, "{}: symbol {}", ctx, i);
    }

    // llvm-readobj lists relocations by section, in file order, as armem does.
    assert_eq!(elf.relocs.len(), rels.len(), "{}: relocation count", ctx);
    for (i, (x, r)) in elf.relocs.iter().zip(&rels).enumerate() {
        let got = (x.from as i128, x.offset as i128, x.kind as i128, x.symbol as i128, x.addend.map(|a| a as i128));
        let want = (
            num(&r["From"]),
            num(&r["Offset"]),
            num(&r["Type"]),
            num(&r["Symbol"]),
            r.get("Addend").map(|a| num(a) as i64 as i128),
        );
        assert_eq!(got, want, "{}: relocation {}", ctx, i);
        if let Some(n) = reloc_name(x.kind) {
            assert_eq!(n, name(&r["Type"]), "{}: relocation {} name", ctx, i);
        }
    }
    elf
}

/// An assembly file with one function for each litmus thread would need
/// unique labels, so each thread gets its own object, placed after a
/// filler function so that it does not start at offset 0.
#[test]
fn litmus_threads_from_objects() {
    let Some(suffix) = llvm_suffix() else {
        eprintln!("skipping: llvm-mc/llvm-readobj not found");
        return;
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("litmus");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "litmus"))
        .collect();
    paths.sort();
    let mut failures = Vec::new();
    let mut threads = 0;
    for path in &paths {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let test = litmus::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        for (t, (code, prog)) in test.code.iter().zip(&test.threads).enumerate() {
            let src = format!(
                "\t.text\n\t.type filler,%function\nfiller:\n\tnop\n\tnop\n\t.size filler, .-filler\n\
                 \t.globl thread\n\t.type thread,%function\nthread:\n{}\n\t.size thread, .-thread\n\
                 \t.word 0\n",
                code.join("\n")
            );
            let tag = format!("{}-P{}", stem, t);
            let data = assemble_obj(&suffix, &src, &tag);
            threads += 1;
            let elf = match Elf::parse(&data) {
                Ok(e) => e,
                Err(e) => {
                    failures.push(format!("{}: {}", tag, e));
                    continue;
                }
            };
            match elf.decode_function("thread") {
                Err(e) => failures.push(format!("{}: {}", tag, e)),
                Ok(got) if got != *prog => failures.push(format!("{}: decoded {:?}, parsed {:?}", tag, got, prog)),
                Ok(_) => {}
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(threads > 100, "only {} threads checked", threads);
}

#[test]
fn assembled_object() {
    let Some(suffix) = llvm_suffix() else {
        eprintln!("skipping: llvm-mc/llvm-readobj not found");
        return;
    };
    let writer = ["MOV W0,#1", "STR W0,[X1]", "DMB ISH", "STR W0,[X2]"];
    let reader = ["L0:", "LDAR W0,[X1]", "CBZ W0,L0", "LDR W2,[X2]"];
    let src = format!(
        "\t.text
\t.globl writer
\t.type writer,%function
writer:
{}
\t.size writer, .-writer
\t.type reader,%function
reader:
{}
\t.size reader, .-reader
\t.globl pool
\t.type pool,%function
pool:
\tnop
\t.word 0x12345678
\tnop
\t.size pool, .-pool
\t.globl calls
\t.type calls,%function
calls:
\tnop
\tb external
\t.size calls, .-calls
\t.section .text.other,\"ax\",%progbits
\t.weak other
\t.type other,%function
other:
\tnop
\t.size other, .-other
\t.data
\t.globl datum
\t.type datum,%object
datum:
\t.quad writer
\t.size datum, 8
",
        writer.join("\n"),
        reader.join("\n")
    );
    assemble_obj(&suffix, &src, "assembled");
    let elf = check_against_readobj(&suffix, &scratch("assembled.o"));
    let lines = |l: &[&str]| assemble(&l.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    assert_eq!(elf.decode_function("writer").unwrap(), lines(&writer));
    assert_eq!(elf.decode_function("reader").unwrap(), lines(&reader));
    let reader_fn = elf.function("reader").unwrap();
    assert_eq!((reader_fn.load_addr, elf.function_range("reader").unwrap().start), (None, 16));
    assert_eq!(elf.decode_function("other").unwrap(), lines(&["NOP"]));
    let err = elf.function("pool").unwrap_err();
    assert!(err.contains("contains data") && err.contains("offset 0x4"), "{}", err);
    let err = elf.function("calls").unwrap_err();
    assert!(err.contains("offset 0x4") && err.contains("R_AARCH64_JUMP26") && err.contains("'external'"), "{}", err);
    assert!(elf.function("datum").unwrap_err().contains("not a function"));
    assert!(elf.function("external").unwrap_err().contains("not a function"));
}

/// More than 0xff00 sections: the section count, the section name table
/// index and the symbols' section indexes need the extended forms.
#[test]
fn extended_section_numbers() {
    let Some(suffix) = llvm_suffix() else {
        eprintln!("skipping: llvm-mc/llvm-readobj not found");
        return;
    };
    const N: usize = 0xff10;
    let mut src = String::new();
    for i in 0..N {
        src += &format!(
            "\t.section .text.f{i},\"ax\",%progbits\n\t.globl f{i}\n\t.type f{i},%function\nf{i}:\n\tmov x0, #{}\n\t.size f{i}, .-f{i}\n",
            i % 65536
        );
    }
    assemble_obj(&suffix, &src, "extended");
    let elf = check_against_readobj(&suffix, &scratch("extended.o"));
    assert!(elf.sections.len() > 0xff00);
    let last = format!("f{}", N - 1);
    let f = elf.function(&last).unwrap();
    assert!(f.section >= 0xff00);
    let x0 = armem::isa::RegOp { idx: 0, wide: true };
    assert_eq!(f.decode().unwrap(), [armem::isa::Instr::MovImm { rd: x0, imm: (N - 1) as u64 }]);
}

fn clang(suffix: &str) -> Option<Command> {
    let name = format!("clang{}", suffix);
    Command::new(&name).arg("--version").output().ok()?.status.success().then(|| Command::new(name))
}

#[test]
fn compiled_object() {
    let Some(suffix) = llvm_suffix() else {
        eprintln!("skipping: llvm-mc/llvm-readobj not found");
        return;
    };
    let Some(mut cc) = clang(&suffix) else {
        eprintln!("skipping: clang{} not found", suffix);
        return;
    };
    let src = r#"
long g = 1;
long common_var;
static long local_data[4];
__thread long tls_var;
const char *msg = "hello";
__attribute__((weak)) long weak_fn(long x) { return x + 1; }
__attribute__((visibility("hidden"))) long hidden_fn(long x) { return x * 3; }
static long helper(long *p) { return p[0] ^ local_data[1]; }
long uses_global(long *p) { return *p + g + helper(p); }
long jump_table(int x) {
    switch (x) {
    case 0: return 11; case 1: return g; case 2: return 37; case 3: return 41;
    case 4: return 43; case 5: return 47; default: return 0;
    }
}
long calls_out(long x) { extern long ext(long); return ext(x) + weak_fn(x) + tls_var; }
"#;
    let (c, o) = (scratch("compiled.c"), scratch("compiled.o"));
    std::fs::write(&c, src).unwrap();
    for (flags, tag) in [(&["-O2"][..], "O2"), (&["-O0", "-g", "-fcommon", "-ffunction-sections", "-fdata-sections"][..], "O0")] {
        let o = o.with_extension(format!("{}.o", tag));
        run(cc.args(["--target=aarch64-linux-gnu", "-c", "-o"]).arg(&o).args(flags).arg(&c));
        let elf = check_against_readobj(&suffix, &o);
        let err = elf.function("uses_global").unwrap_err();
        assert!(err.contains("needs relocation R_AARCH64_"), "{}: {}", tag, err);
        assert!(elf.function("ext").unwrap_err().contains("not a function"), "{}", tag);
        assert!(elf.function("tls_var").unwrap_err().contains("not a function"), "{}", tag);
        // Without relocations, a function is extracted; decoding then
        // stops at the first unsupported instruction (RET, or SP at -O0).
        let err = elf.decode_function("hidden_fn").unwrap_err();
        assert!(err.contains("unsupported") || err.contains("SP"), "{}: {}", tag, err);
        cc = clang(&suffix).unwrap();
    }
}
