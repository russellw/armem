//! Checks the A64 decoder against LLVM.
//!
//! * Every thread of every test in `litmus/` is assembled with `llvm-mc`;
//!   decoding the machine code must give the program the assembly parser
//!   produced.
//! * Random instruction words from the encoding classes the decoder handles
//!   are disassembled with `llvm-mc`, and the text is parsed with armem's
//!   assembler. The decoder must accept exactly the words whose disassembly
//!   the parser accepts, with the same result, and reject every word LLVM
//!   considers invalid or potentially undefined.
//!
//! The tests are skipped (with a message) when `llvm-mc` is not installed.
//! Set `ARMEM_LLVM_SUFFIX` to choose a versioned install, e.g. `-21`.

use armem::asm::parse_instr;
use armem::decode::{decode, decode_program};
use armem::isa::*;
use armem::litmus;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MATTR: &str = "-mattr=+all";

/// The suffix of the installed LLVM tools ("" or e.g. "-21"), if any.
fn llvm_suffix() -> Option<String> {
    let candidates: Vec<String> = match std::env::var("ARMEM_LLVM_SUFFIX") {
        Ok(s) => vec![s],
        Err(_) => std::iter::once(String::new()).chain((14..=30).rev().map(|v| format!("-{}", v))).collect(),
    };
    candidates.into_iter().find(|s| {
        Command::new(format!("llvm-mc{}", s)).arg("--version").output().is_ok_and(|o| o.status.success())
            && Command::new(format!("llvm-objcopy{}", s)).arg("--version").output().is_ok_and(|o| o.status.success())
    })
}

fn tool(suffix: &str, name: &str) -> Command {
    Command::new(format!("{}{}", name, suffix))
}

/// Make semantically identical forms compare equal.
fn normalize(i: Instr) -> Instr {
    let op2 = |o: Operand| match o {
        Operand::RegExt(r, Extend::Lsl(0)) => Operand::Reg(r),
        // UXTW of a W register is a plain shift (LLVM prints it as LSL when
        // SP is an operand).
        Operand::RegExt(r, Extend::Uxtw(0)) if !r.wide => Operand::Reg(r),
        Operand::RegExt(r, Extend::Uxtw(s)) if !r.wide => Operand::RegExt(r, Extend::Lsl(s)),
        o => o,
    };
    match i {
        Instr::Alu { op: AluOp::Orr, rd, rn, op2: Operand::Imm(v), flags: false } if rn.idx == ZR => {
            Instr::MovImm { rd, imm: Size::of(rd).mask(v) }
        }
        Instr::Alu { op, rd, rn, op2: o, flags } => match (op, rn.idx, op2(o), flags) {
            (AluOp::Orr, ZR, Operand::Reg(rm), false) if rm.wide == rd.wide => Instr::MovReg { rd, rn: rm },
            (_, _, o, _) => Instr::Alu { op, rd, rn, op2: o, flags },
        },
        i => i,
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("decode_llvm");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Assemble source lines with llvm-mc and return the raw `.text` bytes.
fn assemble(suffix: &str, lines: &[String], tag: &str) -> Result<Vec<u8>, String> {
    let (src, obj, bin) = (scratch(&format!("{}.s", tag)), scratch(&format!("{}.o", tag)), scratch(&format!("{}.bin", tag)));
    std::fs::write(&src, lines.join("\n") + "\n").unwrap();
    let out = tool(suffix, "llvm-mc")
        .args(["-triple=aarch64", MATTR, "-filetype=obj", "-o"])
        .arg(&obj)
        .arg(&src)
        .output()
        .unwrap();
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let out = tool(suffix, "llvm-objcopy")
        .args(["-O", "binary", "--only-section=.text"])
        .arg(&obj)
        .arg(&bin)
        .output()
        .unwrap();
    assert!(out.status.success(), "llvm-objcopy: {}", String::from_utf8_lossy(&out.stderr));
    Ok(std::fs::read(&bin).unwrap())
}

#[test]
fn litmus_threads_round_trip() {
    let Some(suffix) = llvm_suffix() else {
        eprintln!("skipping: llvm-mc/llvm-objcopy not found");
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
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let test = litmus::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        for (t, (code, prog)) in test.code.iter().zip(&test.threads).enumerate() {
            let tag = format!("{}-P{}", name, t);
            let bytes = match assemble(&suffix, code, &tag) {
                Ok(b) => b,
                Err(e) => {
                    failures.push(format!("{}: llvm-mc failed: {}", tag, e.trim()));
                    continue;
                }
            };
            threads += 1;
            match decode_program(&bytes) {
                Err(e) => failures.push(format!("{}: {}", tag, e)),
                Ok(got) => {
                    let got: Vec<Instr> = got.into_iter().map(normalize).collect();
                    let want: Vec<Instr> = prog.iter().copied().map(normalize).collect();
                    if got != want {
                        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                            if g != w {
                                failures.push(format!("{}: instruction {} ({}): decoded {:?}, parsed {:?}", tag, i, code_line(code, i), g, w));
                            }
                        }
                        if got.len() != want.len() {
                            failures.push(format!("{}: decoded {} instructions, parsed {}", tag, got.len(), want.len()));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(threads > 100, "only {} threads checked", threads);
}

/// The source text of instruction `i` (skipping label-only lines).
fn code_line(code: &[String], i: usize) -> String {
    code.iter()
        .map(|l| armem::asm::split_label(l).1)
        .filter(|l| !l.is_empty())
        .nth(i)
        .unwrap_or_default()
}

/// Encoding classes to sample: (mask, value) of the fixed bits.
const CLASSES: &[(&str, u32, u32)] = &[
    ("data-imm", 0x1c00_0000, 0x1000_0000),
    ("branch/system", 0x1c00_0000, 0x1400_0000),
    ("data-reg", 0x0e00_0000, 0x0a00_0000),
    ("load/store", 0x0a00_0000, 0x0800_0000),
    ("hint", 0xffff_f000, 0xd503_2000),
    ("barrier", 0xffff_f000, 0xd503_3000),
    ("b", 0x7c00_0000, 0x1400_0000),
    ("b.cond", 0xff00_0000, 0x5400_0000),
    ("cbz", 0x7e00_0000, 0x3400_0000),
    ("add/sub imm", 0x1f80_0000, 0x1100_0000),
    ("logical imm", 0x1f80_0000, 0x1200_0000),
    ("move wide", 0x1f80_0000, 0x1280_0000),
    ("bitfield", 0x1f80_0000, 0x1300_0000),
    ("logical reg", 0x1f00_0000, 0x0a00_0000),
    ("add/sub reg", 0x1f00_0000, 0x0b00_0000),
    ("dp 2-source", 0x7fe0_0000, 0x1ac0_0000),
    ("dp 3-source", 0x7f00_0000, 0x1b00_0000),
    ("csel", 0x3fe0_0000, 0x1a80_0000),
    ("exclusive/ordered", 0x3f00_0000, 0x0800_0000),
    ("atomic", 0x3f20_0c00, 0x3820_0000),
    ("ld/st reg offset", 0x3f20_0c00, 0x3820_0800),
    ("ld/st imm9", 0x3f20_0000, 0x3800_0000),
    ("ld/st uimm", 0x3f00_0000, 0x3900_0000),
    ("ld/st pair", 0x3e00_0000, 0x2800_0000),
];

/// Words sampled per class; `ARMEM_DECODE_SAMPLES` overrides it.
const SAMPLES_PER_CLASS: usize = 4000;

/// The instruction index branches are decoded at, so that backward
/// branches stay in range.
const BASE: usize = 1 << 30;

struct Llvm {
    /// None: LLVM rejects the encoding.
    text: Option<String>,
    /// LLVM decodes it but warns that it is potentially undefined.
    soft_fail: bool,
}

/// Disassemble words with llvm-mc.
fn disassemble(suffix: &str, words: &[u32]) -> Vec<Llvm> {
    let mut input = String::new();
    for w in words {
        let b = w.to_le_bytes();
        input.push_str(&format!("{:#04x},{:#04x},{:#04x},{:#04x}\n", b[0], b[1], b[2], b[3]));
    }
    let mut child = tool(suffix, "llvm-mc")
        .args(["--disassemble", "-triple=aarch64", MATTR])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()).unwrap());
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    // Warnings name the input line: "<stdin>:LINE:COL: warning: ...".
    let mut invalid = vec![false; words.len()];
    let mut soft = vec![false; words.len()];
    for l in String::from_utf8_lossy(&out.stderr).lines() {
        let Some(rest) = l.strip_prefix("<stdin>:") else { continue };
        let line: usize = rest.split(':').next().unwrap().parse().unwrap();
        if l.contains("invalid instruction encoding") {
            invalid[line - 1] = true;
        } else if l.contains("potentially undefined") {
            soft[line - 1] = true;
        } else {
            panic!("unexpected llvm-mc message: {}", l);
        }
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut texts = stdout.lines().filter(|l| l.starts_with('\t') && !l.starts_with("\t.")).map(|l| {
        let l = l.split("//").next().unwrap();
        l.trim().to_string()
    });
    let res: Vec<Llvm> = (0..words.len())
        .map(|i| Llvm { text: if invalid[i] { None } else { Some(texts.next().expect("missing llvm-mc output")) }, soft_fail: soft[i] })
        .collect();
    assert!(texts.next().is_none(), "extra llvm-mc output");
    res
}

/// Parse LLVM's disassembly. Branch offsets (`#imm`, in bytes) become a
/// label at `BASE + imm / 4`.
fn parse_llvm(text: &str) -> Result<Instr, String> {
    let lower = text.to_ascii_lowercase();
    let is_branch = ["b\t", "b.", "cbz\t", "cbnz\t"].iter().any(|p| lower.starts_with(p));
    let mut labels = HashMap::new();
    if is_branch {
        let (head, off) = text.rsplit_once('#').ok_or("branch without offset")?;
        let off: i64 = off.trim().parse().map_err(|_| "bad branch offset")?;
        labels.insert("T".to_string(), (BASE as i64 + off / 4) as usize);
        return parse_instr(&format!("{}T", head), &labels);
    }
    parse_instr(text, &labels)
}

/// An STXR/STLXR (any size) whose should-be-one Rt2 field is not all
/// ones. The decoder rejects it as CONSTRAINED UNPREDICTABLE; LLVM ignores
/// the field without a warning (unlike for LDXR, LDAR and STLR).
fn stxr_sbo_violation(w: u32) -> bool {
    w >> 21 & 0x1ff == 0b001000_000 && (w >> 10) & 31 != 31
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 16) as u32
    }
}

#[test]
fn random_words_match_llvm() {
    let Some(suffix) = llvm_suffix() else {
        eprintln!("skipping: llvm-mc not found");
        return;
    };
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut words = Vec::new();
    let n = std::env::var("ARMEM_DECODE_SAMPLES").map_or(SAMPLES_PER_CLASS, |v| v.parse().unwrap());
    for &(_, mask, value) in CLASSES {
        for _ in 0..n {
            words.push(rng.next() & !mask | value);
        }
    }
    let llvm = disassemble(&suffix, &words);
    // Discrepancies grouped by kind and mnemonic, with a few examples each.
    let mut problems: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    let mut accepted = 0;
    for (&w, l) in words.iter().zip(&llvm) {
        let ours = decode(w, BASE).map(normalize);
        let mnem = l.text.as_deref().map_or("-", |t| t.split_whitespace().next().unwrap()).to_string();
        let problem = match (&ours, &l.text) {
            (Ok(i), None) => Some(("decoded an encoding LLVM rejects", format!("{:#010x}: {:?}", w, i))),
            (Ok(i), Some(t)) if l.soft_fail => {
                Some(("decoded a potentially undefined encoding", format!("{:#010x} {}: {:?}", w, t, i)))
            }
            (Ok(i), Some(t)) => match parse_llvm(t).map(normalize) {
                Ok(p) if p == *i => {
                    accepted += 1;
                    None
                }
                Ok(p) => Some(("decoded differently", format!("{:#010x} {}: decoded {:?}, parsed {:?}", w, t, i, p))),
                Err(e) => Some(("decoded, but the parser rejects LLVM's text", format!("{:#010x} {}: {:?} ({})", w, t, i, e))),
            },
            (Err(_), Some(_)) if stxr_sbo_violation(w) => None,
            (Err(e), Some(t)) if !l.soft_fail => match parse_llvm(t) {
                Ok(p) => Some(("rejected, but the parser accepts LLVM's text", format!("{:#010x} {}: {:?} ({})", w, t, p, e))),
                Err(_) => None,
            },
            (Err(_), _) => None,
        };
        if let Some((kind, ex)) = problem {
            let e = problems.entry(format!("{}: {}", kind, mnem)).or_default();
            e.0 += 1;
            if e.1.len() < 3 {
                e.1.push(ex);
            }
        }
    }
    let report: Vec<String> =
        problems.iter().map(|(k, (n, ex))| format!("{} ({}x)\n    {}", k, n, ex.join("\n    "))).collect();
    assert!(report.is_empty(), "{} discrepancies:\n{}", problems.values().map(|p| p.0).sum::<usize>(), report.join("\n"));
    eprintln!("{} of {} words decoded and matched LLVM", accepted, words.len());
    assert!(accepted > 10_000, "only {} words decoded and matched", accepted);
}
