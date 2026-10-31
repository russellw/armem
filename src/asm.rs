//! Parser for a subset of AArch64 assembly (herd-style syntax).

use crate::isa::*;
use std::collections::HashMap;

pub type Result<T> = std::result::Result<T, String>;

/// Split operands on commas that are not inside brackets.
fn split_operands(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '[' => {
                depth += 1;
                cur.push(ch)
            }
            ']' => {
                depth -= 1;
                cur.push(ch)
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

pub fn parse_reg(s: &str) -> Result<RegOp> {
    let u = s.trim().to_ascii_uppercase();
    match u.as_str() {
        "XZR" => return Ok(RegOp { idx: ZR, wide: true }),
        "WZR" => return Ok(RegOp { idx: ZR, wide: false }),
        _ => {}
    }
    let wide = match u.chars().next() {
        Some('X') => true,
        Some('W') => false,
        _ => return Err(format!("bad register '{}'", s)),
    };
    let idx: u8 = u[1..].parse().map_err(|_| format!("bad register '{}'", s))?;
    if idx > 30 {
        return Err(format!("bad register '{}'", s));
    }
    Ok(RegOp { idx, wide })
}

pub fn parse_int(s: &str) -> Result<i128> {
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let v = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i128::from_str_radix(h, 16)
    } else {
        t.parse::<i128>()
    }
    .map_err(|_| format!("bad integer '{}'", s))?;
    Ok(if neg { -v } else { v })
}

fn parse_imm(s: &str) -> Result<i128> {
    let t = s.trim();
    let t = t.strip_prefix('#').ok_or_else(|| format!("expected immediate, got '{}'", s))?;
    parse_int(t)
}

fn parse_operand(s: &str) -> Result<Operand> {
    if s.trim().starts_with('#') {
        Ok(Operand::Imm(parse_imm(s)? as u64))
    } else {
        Ok(Operand::Reg(parse_reg(s)?))
    }
}

fn parse_shift_amount(s: &str) -> Result<u8> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    Ok(parse_imm(s)? as u8)
}

fn parse_addr(s: &str) -> Result<Addr> {
    let t = s.trim();
    let inner = t
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .ok_or_else(|| format!("bad address '{}' (pre/post-index not supported)", s))?;
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    let base = parse_reg(parts[0])?;
    if !base.wide {
        return Err(format!("base register must be X: '{}'", s));
    }
    let offset = match parts.len() {
        1 => Offset::Imm(0),
        2 if parts[1].starts_with('#') => Offset::Imm(parse_imm(parts[1])? as i64),
        2 => Offset::Reg(parse_reg(parts[1])?, Extend::Lsl(0)),
        3 => {
            let r = parse_reg(parts[1])?;
            let ext = parts[2].to_ascii_uppercase();
            let (kind, amt) = match ext.split_once(char::is_whitespace) {
                Some((k, a)) => (k.to_string(), a.to_string()),
                None => (ext.clone(), String::new()),
            };
            let amt = parse_shift_amount(&amt)?;
            let e = match kind.as_str() {
                "LSL" => Extend::Lsl(amt),
                "UXTW" => Extend::Uxtw(amt),
                "SXTW" => Extend::Sxtw(amt),
                _ => return Err(format!("bad extend in '{}'", s)),
            };
            Offset::Reg(r, e)
        }
        _ => return Err(format!("bad address '{}'", s)),
    };
    Ok(Addr { base, offset })
}

fn parse_cond(s: &str) -> Result<Cond> {
    Ok(match s.trim().to_ascii_uppercase().as_str() {
        "EQ" => Cond::Eq,
        "NE" => Cond::Ne,
        "CS" | "HS" => Cond::Cs,
        "CC" | "LO" => Cond::Cc,
        "MI" => Cond::Mi,
        "PL" => Cond::Pl,
        "VS" => Cond::Vs,
        "VC" => Cond::Vc,
        "HI" => Cond::Hi,
        "LS" => Cond::Ls,
        "GE" => Cond::Ge,
        "LT" => Cond::Lt,
        "GT" => Cond::Gt,
        "LE" => Cond::Le,
        "AL" => Cond::Al,
        _ => return Err(format!("bad condition '{}'", s)),
    })
}

fn invert(c: Cond) -> Cond {
    use Cond::*;
    match c {
        Eq => Ne,
        Ne => Eq,
        Cs => Cc,
        Cc => Cs,
        Mi => Pl,
        Pl => Mi,
        Vs => Vc,
        Vc => Vs,
        Hi => Ls,
        Ls => Hi,
        Ge => Lt,
        Lt => Ge,
        Gt => Le,
        Le => Gt,
        Al => Al,
    }
}

fn want(ops: &[String], n: usize, mnem: &str) -> Result<()> {
    if ops.len() != n {
        Err(format!("{} expects {} operands, got {}", mnem, n, ops.len()))
    } else {
        Ok(())
    }
}

fn label(labels: &HashMap<String, usize>, s: &str) -> Result<usize> {
    labels.get(s.trim()).copied().ok_or_else(|| format!("unknown label '{}'", s))
}

/// Split an LSE mnemonic suffix: returns (base, acquire, release).
fn ordering_suffix<'a>(m: &'a str, base: &str) -> Option<(bool, bool)> {
    let rest = m.strip_prefix(base)?;
    match rest {
        "" => Some((false, false)),
        "A" => Some((true, false)),
        "L" => Some((false, true)),
        "AL" => Some((true, true)),
        _ => None,
    }
}

const RMW_OPS: &[(&str, RmwOp)] = &[
    ("ADD", RmwOp::Add),
    ("CLR", RmwOp::Clr),
    ("EOR", RmwOp::Eor),
    ("SET", RmwOp::Set),
    ("SMAX", RmwOp::Smax),
    ("SMIN", RmwOp::Smin),
    ("UMAX", RmwOp::Umax),
    ("UMIN", RmwOp::Umin),
];

/// Parse one instruction (without label).
pub fn parse_instr(line: &str, labels: &HashMap<String, usize>) -> Result<Instr> {
    let line = line.trim();
    let (mnem, rest) = match line.split_once(char::is_whitespace) {
        Some((m, r)) => (m.to_ascii_uppercase(), r.trim()),
        None => (line.to_ascii_uppercase(), ""),
    };
    let ops = split_operands(rest);
    let m = mnem.as_str();
    let reg = |i: usize| parse_reg(&ops[i]);

    // Conditional branch B.cond
    if let Some(c) = m.strip_prefix("B.") {
        want(&ops, 1, m)?;
        return Ok(Instr::BCond { cond: parse_cond(c)?, target: label(labels, &ops[0])? });
    }

    let alu = |op: AluOp, flags: bool| -> Result<Instr> {
        want(&ops, 3, m)?;
        Ok(Instr::Alu { op, rd: reg(0)?, rn: reg(1)?, op2: parse_operand(&ops[2])?, flags })
    };

    let ins = match m {
        "NOP" => Instr::Nop,
        "MOV" | "MOVZ" => {
            want(&ops, 2, m)?;
            let rd = reg(0)?;
            if ops[1].starts_with('#') {
                Instr::MovImm { rd, imm: Size::of(rd).mask(parse_imm(&ops[1])? as u64) }
            } else {
                Instr::MovReg { rd, rn: reg(1)? }
            }
        }
        "ADD" => alu(AluOp::Add, false)?,
        "ADDS" => alu(AluOp::Add, true)?,
        "SUB" => alu(AluOp::Sub, false)?,
        "SUBS" => alu(AluOp::Sub, true)?,
        "AND" => alu(AluOp::And, false)?,
        "ANDS" => alu(AluOp::And, true)?,
        "ORR" => alu(AluOp::Orr, false)?,
        "EOR" => alu(AluOp::Eor, false)?,
        "LSL" => alu(AluOp::Lsl, false)?,
        "LSR" => alu(AluOp::Lsr, false)?,
        "ASR" => alu(AluOp::Asr, false)?,
        "MUL" => alu(AluOp::Mul, false)?,
        "CMP" | "CMN" | "TST" => {
            want(&ops, 2, m)?;
            let rn = reg(0)?;
            let op = match m {
                "CMP" => AluOp::Sub,
                "CMN" => AluOp::Add,
                _ => AluOp::And,
            };
            Instr::Alu {
                op,
                rd: RegOp { idx: ZR, wide: rn.wide },
                rn,
                op2: parse_operand(&ops[1])?,
                flags: true,
            }
        }
        "CSEL" | "CSINC" => {
            want(&ops, 4, m)?;
            Instr::Csel { rd: reg(0)?, rn: reg(1)?, rm: reg(2)?, cond: parse_cond(&ops[3])?, inc: m == "CSINC" }
        }
        "CSET" => {
            want(&ops, 2, m)?;
            let rd = reg(0)?;
            let z = RegOp { idx: ZR, wide: rd.wide };
            Instr::Csel { rd, rn: z, rm: z, cond: invert(parse_cond(&ops[1])?), inc: true }
        }
        "B" => {
            want(&ops, 1, m)?;
            Instr::B { target: label(labels, &ops[0])? }
        }
        "CBZ" | "CBNZ" => {
            want(&ops, 2, m)?;
            Instr::Cbz { rt: reg(0)?, nonzero: m == "CBNZ", target: label(labels, &ops[1])? }
        }
        "LDR" | "LDAR" | "LDAPR" | "LDXR" | "LDAXR" => {
            want(&ops, 2, m)?;
            let kind = match m {
                "LDAR" | "LDAXR" => LoadKind::Acquire,
                "LDAPR" => LoadKind::AcquirePC,
                _ => LoadKind::Plain,
            };
            Instr::Load { rt: reg(0)?, addr: parse_addr(&ops[1])?, kind, excl: m.contains('X') }
        }
        "STR" | "STLR" => {
            want(&ops, 2, m)?;
            Instr::Store { rt: reg(0)?, addr: parse_addr(&ops[1])?, release: m == "STLR" }
        }
        "STXR" | "STLXR" => {
            want(&ops, 3, m)?;
            Instr::StoreExcl { rs: reg(0)?, rt: reg(1)?, addr: parse_addr(&ops[2])?, release: m == "STLXR" }
        }
        "DMB" | "DSB" => {
            want(&ops, 1, m)?;
            let o = ops[0].to_ascii_uppercase();
            let b = match o.as_str() {
                "SY" | "ISH" | "OSH" | "NSH" => Barrier::Full,
                "LD" | "ISHLD" | "OSHLD" | "NSHLD" => Barrier::Ld,
                "ST" | "ISHST" | "OSHST" | "NSHST" => Barrier::St,
                _ => return Err(format!("bad barrier option '{}'", ops[0])),
            };
            Instr::Barrier(b)
        }
        "ISB" => Instr::Barrier(Barrier::Isb),
        _ => {
            // LSE atomics
            if let Some((a, l)) = ordering_suffix(m, "CAS") {
                want(&ops, 3, m)?;
                return Ok(Instr::Cas { rs: reg(0)?, rt: reg(1)?, addr: parse_addr(&ops[2])?, acquire: a, release: l });
            }
            if let Some((a, l)) = ordering_suffix(m, "SWP") {
                want(&ops, 3, m)?;
                return Ok(Instr::Rmw {
                    op: RmwOp::Swp,
                    rs: reg(0)?,
                    rt: reg(1)?,
                    addr: parse_addr(&ops[2])?,
                    acquire: a,
                    release: l,
                });
            }
            for (name, op) in RMW_OPS {
                if let Some((a, l)) = ordering_suffix(m, &format!("LD{}", name)) {
                    want(&ops, 3, m)?;
                    return Ok(Instr::Rmw {
                        op: *op,
                        rs: reg(0)?,
                        rt: reg(1)?,
                        addr: parse_addr(&ops[2])?,
                        acquire: a,
                        release: l,
                    });
                }
                if let Some((false, l)) = ordering_suffix(m, &format!("ST{}", name)) {
                    want(&ops, 2, m)?;
                    let rs = reg(0)?;
                    return Ok(Instr::Rmw {
                        op: *op,
                        rs,
                        rt: RegOp { idx: ZR, wide: rs.wide },
                        addr: parse_addr(&ops[1])?,
                        acquire: false,
                        release: l,
                    });
                }
            }
            return Err(format!("unsupported instruction '{}'", line));
        }
    };
    Ok(ins)
}

/// A line of source, possibly with a leading label.
pub fn split_label(line: &str) -> (Option<String>, String) {
    let t = line.trim();
    if let Some((l, rest)) = t.split_once(':') {
        let l = l.trim();
        if !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            return (Some(l.to_string()), rest.trim().to_string());
        }
    }
    (None, t.to_string())
}

/// Assemble a sequence of source lines into a program.
pub fn assemble(lines: &[String]) -> Result<Vec<Instr>> {
    let mut labels = HashMap::new();
    let mut body = Vec::new();
    for l in lines {
        let (lab, rest) = split_label(l);
        if let Some(lab) = lab {
            if labels.insert(lab.clone(), body.len()).is_some() {
                return Err(format!("duplicate label '{}'", lab));
            }
        }
        if !rest.is_empty() {
            body.push(rest);
        }
    }
    body.iter().map(|l| parse_instr(l, &labels)).collect()
}
