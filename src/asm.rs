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

/// Parse a general-purpose register or the zero register (not SP).
pub fn parse_reg(s: &str) -> Result<RegOp> {
    let r = parse_reg_sp(s)?;
    if r.idx == SP {
        return Err(format!("SP is not allowed here: '{}'", s.trim()));
    }
    Ok(r)
}

/// Parse a general-purpose register, the zero register or SP.
pub fn parse_reg_sp(s: &str) -> Result<RegOp> {
    let u = s.trim().to_ascii_uppercase();
    match u.as_str() {
        "SP" => return Ok(RegOp { idx: SP, wide: true }),
        "WSP" => return Ok(RegOp { idx: SP, wide: false }),
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

/// Parse a second ALU operand from one or two operand strings: `Rm`,
/// `#imm`, `Rm, <extend or LSL>` or `#imm, LSL #12`.
fn parse_op2(ops: &[String], mnem: &str) -> Result<Operand> {
    match ops {
        [o] => parse_operand(o),
        [i, e] if i.starts_with('#') => match parse_extend(e)? {
            Extend::Lsl(s @ (0 | 12)) => Ok(Operand::Imm((parse_imm(i)? as u64) << s)),
            _ => Err(format!("{}: bad immediate shift '{}'", mnem, e)),
        },
        [r, e] => Ok(Operand::RegExt(parse_reg(r)?, parse_extend(e)?)),
        _ => Err(format!("{}: bad operands", mnem)),
    }
}

fn parse_shift_amount(s: &str) -> Result<u8> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    Ok(parse_imm(s)? as u8)
}

/// Parse an extend/shift specifier such as `SXTW`, `UXTW #2` or `LSL #3`.
fn parse_extend(s: &str) -> Result<Extend> {
    let u = s.trim().to_ascii_uppercase();
    let (kind, amt) = match u.split_once(char::is_whitespace) {
        Some((k, a)) => (k.to_string(), a.to_string()),
        None => (u.clone(), String::new()),
    };
    let amt = parse_shift_amount(&amt)?;
    Ok(match kind.as_str() {
        // UXTX/SXTX of a 64-bit register is a plain shift.
        "LSL" | "UXTX" | "SXTX" => Extend::Lsl(amt),
        "UXTW" => Extend::Uxtw(amt),
        "SXTW" => Extend::Sxtw(amt),
        _ => return Err(format!("bad extend '{}'", s)),
    })
}

fn parse_addr(s: &str) -> Result<Addr> {
    let t = s.trim();
    let (t, pre) = match t.strip_suffix('!') {
        Some(r) => (r.trim(), true),
        None => (t, false),
    };
    let inner = t
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .ok_or_else(|| format!("bad address '{}'", s))?;
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    let base = parse_reg_sp(parts[0])?;
    if base.idx == ZR {
        return Err(format!("base register cannot be XZR: '{}'", s));
    }
    if !base.wide {
        return Err(format!("base register must be X: '{}'", s));
    }
    let offset = match parts.len() {
        1 => Offset::Imm(0),
        2 if parts[1].starts_with('#') => Offset::Imm(parse_imm(parts[1])? as i64),
        2 => Offset::Reg(parse_reg(parts[1])?, Extend::Lsl(0)),
        3 => Offset::Reg(parse_reg(parts[1])?, parse_extend(parts[2])?),
        _ => return Err(format!("bad address '{}'", s)),
    };
    if pre && !matches!(offset, Offset::Imm(_)) {
        return Err(format!("pre-index needs an immediate offset: '{}'", s));
    }
    let writeback = if pre { Writeback::Pre } else { Writeback::None };
    Ok(Addr { base, offset, writeback })
}

/// Parse the address operand at `ops[i]`, plus an optional post-index
/// immediate at `ops[i + 1]`. The address must be the last operand.
fn parse_mem_operand(ops: &[String], i: usize, mnem: &str) -> Result<Addr> {
    let a = ops.get(i).ok_or_else(|| format!("{}: missing address operand", mnem))?;
    let mut addr = parse_addr(a)?;
    match ops.len() - i {
        1 => {}
        2 => {
            if addr.writeback != Writeback::None || addr.offset != Offset::Imm(0) {
                return Err(format!("{}: bad post-index form", mnem));
            }
            addr.writeback = Writeback::Post(parse_imm(&ops[i + 1])? as i64);
        }
        _ => return Err(format!("{}: too many operands", mnem)),
    }
    Ok(addr)
}

/// Split a load/store mnemonic such as `LDRSB` or `STLXRH` into its base
/// (ending in `R`) and its size suffix.
fn split_size_suffix(m: &str) -> Option<(&str, &str)> {
    let i = m.rfind('R')?;
    let (base, suffix) = m.split_at(i + 1);
    match suffix {
        "" | "B" | "H" | "SB" | "SH" | "SW" => Some((base, suffix)),
        _ => None,
    }
}

/// Decode a size suffix: (size, signed). None means "size of register".
fn suffix_size(suffix: &str) -> (Option<Size>, bool) {
    match suffix {
        "B" => (Some(Size::B1), false),
        "H" => (Some(Size::B2), false),
        "SB" => (Some(Size::B1), true),
        "SH" => (Some(Size::B2), true),
        "SW" => (Some(Size::B4), true),
        _ => (None, false),
    }
}

/// Check that a data register suits the access size.
fn check_data_reg(r: RegOp, size: Size, signed: bool, mnem: &str) -> Result<()> {
    let ok = match size {
        Size::B8 => r.wide,
        Size::B4 => r.wide == signed,
        _ => !r.wide || signed,
    };
    if ok { Ok(()) } else { Err(format!("{}: register {} does not match the access size", mnem, r)) }
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
        "AL" | "NV" => Cond::Al,
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
    let ins = parse_instr_unchecked(line, labels)?;
    if unpredictable(&ins) {
        return Err(format!("UNPREDICTABLE register combination in '{}'", line.trim()));
    }
    Ok(ins)
}

/// Register combinations the architecture makes CONSTRAINED UNPREDICTABLE.
fn unpredictable(ins: &Instr) -> bool {
    let wb = |a: &Addr| a.writeback != Writeback::None;
    let same = |a: RegOp, b: RegOp| a.idx == b.idx;
    match *ins {
        Instr::Load { rt, addr, .. } | Instr::Store { rt, addr, .. } => wb(&addr) && same(rt, addr.base),
        Instr::LoadPair { rt, rt2, addr, .. } => {
            same(rt, rt2) || (wb(&addr) && (same(rt, addr.base) || same(rt2, addr.base)))
        }
        Instr::StorePair { rt, rt2, addr, .. } => wb(&addr) && (same(rt, addr.base) || same(rt2, addr.base)),
        Instr::StoreExcl { rs, rt, addr, .. } => same(rs, rt) || same(rs, addr.base),
        Instr::LoadExclPair { rt, rt2, .. } => same(rt, rt2),
        Instr::StoreExclPair { rs, rt, rt2, addr, .. } => same(rs, rt) || same(rs, rt2) || same(rs, addr.base),
        _ => false,
    }
}

fn parse_instr_unchecked(line: &str, labels: &HashMap<String, usize>) -> Result<Instr> {
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

    // SP may be the first source of ADD/SUB, and the destination of ADD/SUB
    // and of AND/ORR/EOR with an immediate, unless they set the flags.
    let alu = |op: AluOp, flags: bool| -> Result<Instr> {
        if !(3..=4).contains(&ops.len()) {
            return Err(format!("{} expects 3 or 4 operands, got {}", m, ops.len()));
        }
        let op2 = parse_op2(&ops[2..], m)?;
        let add_sub = matches!(op, AluOp::Add | AluOp::Sub);
        let logical_imm = matches!(op, AluOp::And | AluOp::Orr | AluOp::Eor) && matches!(op2, Operand::Imm(_));
        let rd = if !flags && (add_sub || logical_imm) { parse_reg_sp(&ops[0])? } else { reg(0)? };
        let rn = if add_sub { parse_reg_sp(&ops[1])? } else { reg(1)? };
        Ok(Instr::Alu { op, rd, rn, op2, flags })
    };

    let ins = match m {
        // Hints that may be implemented as NOP, which is all the emulator needs.
        "NOP" | "YIELD" | "WFE" | "WFI" | "SEV" | "SEVL" => Instr::Nop,
        "MOVZ" | "MOVN" => {
            if !(2..=3).contains(&ops.len()) {
                return Err(format!("{} expects 2 or 3 operands, got {}", m, ops.len()));
            }
            let rd = reg(0)?;
            let imm = parse_imm(&ops[1])?;
            let shift = match ops.get(2).map(|e| parse_extend(e)).transpose()? {
                None => 0,
                Some(Extend::Lsl(s)) if s % 16 == 0 && (s as u64) < 8 * Size::of(rd).bytes() => s,
                Some(_) => return Err(format!("{}: bad shift '{}'", m, ops[2])),
            };
            if !(0..=0xffff).contains(&imm) {
                return Err(format!("{}: immediate out of range", m));
            }
            let v = (imm as u64) << shift;
            Instr::MovImm { rd, imm: Size::of(rd).mask(if m == "MOVN" { !v } else { v }) }
        }
        "MOV" => {
            want(&ops, 2, m)?;
            let rd = parse_reg_sp(&ops[0])?;
            if ops[1].starts_with('#') {
                Instr::MovImm { rd, imm: Size::of(rd).mask(parse_imm(&ops[1])? as u64) }
            } else {
                let rn = parse_reg_sp(&ops[1])?;
                if rd.idx == SP || rn.idx == SP {
                    // An alias of ADD (immediate), as the zero register is not
                    // allowed with SP.
                    if rd.idx == ZR || rn.idx == ZR {
                        return Err(format!("MOV: cannot move between SP and the zero register: '{}'", line));
                    }
                    Instr::Alu { op: AluOp::Add, rd, rn, op2: Operand::Imm(0), flags: false }
                } else {
                    Instr::MovReg { rd, rn }
                }
            }
        }
        "SXTW" => {
            // Alias of SBFM; modeled as XZR | sign-extended Wn.
            want(&ops, 2, m)?;
            let rd = reg(0)?;
            Instr::Alu {
                op: AluOp::Orr,
                rd,
                rn: RegOp { idx: ZR, wide: true },
                op2: Operand::RegExt(reg(1)?, Extend::Sxtw(0)),
                flags: false,
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
        "NEG" | "NEGS" => {
            if !(2..=3).contains(&ops.len()) {
                return Err(format!("{} expects 2 or 3 operands, got {}", m, ops.len()));
            }
            let rd = reg(0)?;
            let op2 = parse_op2(&ops[1..], m)?;
            Instr::Alu { op: AluOp::Sub, rd, rn: RegOp { idx: ZR, wide: rd.wide }, op2, flags: m == "NEGS" }
        }
        "CMP" | "CMN" | "TST" => {
            if !(2..=3).contains(&ops.len()) {
                return Err(format!("{} expects 2 or 3 operands, got {}", m, ops.len()));
            }
            let rn = if m == "TST" { reg(0)? } else { parse_reg_sp(&ops[0])? };
            let op = match m {
                "CMP" => AluOp::Sub,
                "CMN" => AluOp::Add,
                _ => AluOp::And,
            };
            Instr::Alu {
                op,
                rd: RegOp { idx: ZR, wide: rn.wide },
                rn,
                op2: parse_op2(&ops[1..], m)?,
                flags: true,
            }
        }
        "CSEL" | "CSINC" => {
            want(&ops, 4, m)?;
            Instr::Csel { rd: reg(0)?, rn: reg(1)?, rm: reg(2)?, cond: parse_cond(&ops[3])?, inc: m == "CSINC" }
        }
        "CINC" => {
            want(&ops, 3, m)?;
            let rn = reg(1)?;
            Instr::Csel { rd: reg(0)?, rn, rm: rn, cond: invert(parse_cond(&ops[2])?), inc: true }
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
        "LDXP" | "LDAXP" => {
            want(&ops, 3, m)?;
            let (rt, rt2) = (reg(0)?, reg(1)?);
            if rt.wide != rt2.wide {
                return Err(format!("{}: registers must be the same width", m));
            }
            let addr = parse_addr(&ops[2])?;
            if addr.offset != Offset::Imm(0) || addr.writeback != Writeback::None {
                return Err(format!("{}: only [Xn] addressing is allowed", m));
            }
            Instr::LoadExclPair { rt, rt2, addr, acquire: m == "LDAXP" }
        }
        "STXP" | "STLXP" => {
            want(&ops, 4, m)?;
            let (rs, rt, rt2) = (reg(0)?, reg(1)?, reg(2)?);
            if rt.wide != rt2.wide || rs.wide {
                return Err(format!("{}: bad register widths", m));
            }
            let addr = parse_addr(&ops[3])?;
            if addr.offset != Offset::Imm(0) || addr.writeback != Writeback::None {
                return Err(format!("{}: only [Xn] addressing is allowed", m));
            }
            Instr::StoreExclPair { rs, rt, rt2, addr, release: m == "STLXP" }
        }
        "LDP" | "LDNP" | "LDPSW" | "STP" | "STNP" => {
            if ops.len() < 3 {
                return Err(format!("{} expects at least 3 operands", m));
            }
            let (rt, rt2) = (reg(0)?, reg(1)?);
            if rt.wide != rt2.wide {
                return Err(format!("{}: registers must be the same width", m));
            }
            let addr = parse_mem_operand(&ops, 2, m)?;
            match m {
                "LDPSW" => {
                    if !rt.wide {
                        return Err("LDPSW needs X registers".into());
                    }
                    Instr::LoadPair { rt, rt2, addr, size: Size::B4, signed: true }
                }
                "LDP" | "LDNP" => Instr::LoadPair { rt, rt2, addr, size: Size::of(rt), signed: false },
                _ => Instr::StorePair { rt, rt2, addr, size: Size::of(rt) },
            }
        }
        _ if split_size_suffix(m).is_some_and(|(b, _)| LOADS.contains(&b) || STORES.contains(&b)) => {
            parse_load_store(m, &ops)?
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
        "ISB" if ops.is_empty() || (ops.len() == 1 && ops[0].eq_ignore_ascii_case("SY")) => {
            Instr::Barrier(Barrier::Isb)
        }
        _ => {
            // LSE atomics, with an optional B/H size suffix.
            let (m, size) = match m.strip_suffix('B') {
                Some(r) => (r, Some(Size::B1)),
                None => match m.strip_suffix('H') {
                    Some(r) => (r, Some(Size::B2)),
                    None => (m, None),
                },
            };
            let sized = |r: RegOp| -> Result<Size> {
                match size {
                    Some(_) if r.wide => Err(format!("{}: byte/halfword atomics take W registers", m)),
                    Some(s) => Ok(s),
                    None => Ok(Size::of(r)),
                }
            };
            if let Some((a, l)) = ordering_suffix(m, "CAS") {
                want(&ops, 3, m)?;
                let rs = reg(0)?;
                return Ok(Instr::Cas {
                    rs,
                    rt: reg(1)?,
                    addr: parse_addr(&ops[2])?,
                    size: sized(rs)?,
                    acquire: a,
                    release: l,
                });
            }
            let mut ops_list: Vec<(String, RmwOp)> = vec![("SWP".to_string(), RmwOp::Swp)];
            ops_list.extend(RMW_OPS.iter().map(|(n, o)| (format!("LD{}", n), *o)));
            for (name, op) in &ops_list {
                if let Some((a, l)) = ordering_suffix(m, name) {
                    want(&ops, 3, m)?;
                    let rs = reg(0)?;
                    return Ok(Instr::Rmw {
                        op: *op,
                        rs,
                        rt: reg(1)?,
                        addr: parse_addr(&ops[2])?,
                        size: sized(rs)?,
                        acquire: a,
                        release: l,
                    });
                }
            }
            for (name, op) in RMW_OPS {
                if let Some((false, l)) = ordering_suffix(m, &format!("ST{}", name)) {
                    want(&ops, 2, m)?;
                    let rs = reg(0)?;
                    return Ok(Instr::Rmw {
                        op: *op,
                        rs,
                        rt: RegOp { idx: ZR, wide: rs.wide },
                        addr: parse_addr(&ops[1])?,
                        size: sized(rs)?,
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

const LOADS: &[&str] = &["LDR", "LDUR", "LDAR", "LDAPR", "LDXR", "LDAXR"];
const STORES: &[&str] = &["STR", "STUR", "STLR", "STXR", "STLXR"];

fn parse_load_store(m: &str, ops: &[String]) -> Result<Instr> {
    let (base, suffix) = split_size_suffix(m).unwrap();
    let (fixed, signed) = suffix_size(suffix);
    if signed && !(base == "LDR" || base == "LDUR") {
        return Err(format!("unsupported instruction '{}'", m));
    }
    let wb_ok = matches!(base, "LDR" | "STR");
    let check_wb = |a: &Addr| -> Result<()> {
        if a.writeback != Writeback::None && !wb_ok {
            Err(format!("{}: writeback addressing not allowed", m))
        } else if !matches!(base, "LDR" | "STR" | "LDUR" | "STUR") && a.offset != Offset::Imm(0) {
            Err(format!("{}: only [Xn] addressing is allowed", m))
        } else {
            Ok(())
        }
    };
    if base == "STXR" || base == "STLXR" {
        if ops.len() != 3 {
            return Err(format!("{} expects 3 operands", m));
        }
        let rs = parse_reg(&ops[0])?;
        let rt = parse_reg(&ops[1])?;
        let size = fixed.unwrap_or(Size::of(rt));
        check_data_reg(rt, size, false, m)?;
        let addr = parse_addr(&ops[2])?;
        check_wb(&addr)?;
        return Ok(Instr::StoreExcl { rs, rt, addr, size, release: base == "STLXR" });
    }
    if ops.len() < 2 {
        return Err(format!("{} expects 2 operands", m));
    }
    let rt = parse_reg(&ops[0])?;
    let size = fixed.unwrap_or(Size::of(rt));
    check_data_reg(rt, size, signed, m)?;
    let addr = parse_mem_operand(ops, 1, m)?;
    check_wb(&addr)?;
    Ok(if LOADS.contains(&base) {
        let kind = match base {
            "LDAR" | "LDAXR" => LoadKind::Acquire,
            "LDAPR" => LoadKind::AcquirePC,
            _ => LoadKind::Plain,
        };
        Instr::Load { rt, addr, size, signed, kind, excl: base.contains('X') }
    } else {
        Instr::Store { rt, addr, size, release: base == "STLR" }
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Instr> {
        parse_instr(s, &HashMap::new())
    }

    #[test]
    fn sized_loads_and_stores() {
        let Instr::Load { size, signed, .. } = p("LDRSH X0,[X1]").unwrap() else { panic!() };
        assert_eq!((size, signed), (Size::B2, true));
        let Instr::Store { size, release, .. } = p("STLRB W0,[X1]").unwrap() else { panic!() };
        assert_eq!((size, release), (Size::B1, true));
        let Instr::StoreExcl { size, .. } = p("STLXRH W2,W0,[X1]").unwrap() else { panic!() };
        assert_eq!(size, Size::B2);
        let Instr::Rmw { size, acquire, release, .. } = p("LDADDALB W0,W1,[X2]").unwrap() else { panic!() };
        assert_eq!((size, acquire, release), (Size::B1, true, true));
        let Instr::Cas { size, .. } = p("CASH W0,W1,[X2]").unwrap() else { panic!() };
        assert_eq!(size, Size::B2);
    }

    #[test]
    fn exclusive_pairs() {
        let Instr::LoadExclPair { acquire, .. } = p("LDAXP X0,X1,[X2]").unwrap() else { panic!() };
        assert!(acquire);
        let Instr::StoreExclPair { release, .. } = p("STLXP W3,W0,W1,[X2]").unwrap() else { panic!() };
        assert!(release);
    }

    #[test]
    fn writeback_forms() {
        let Instr::Load { addr, .. } = p("LDR X0,[X1,#8]!").unwrap() else { panic!() };
        assert_eq!((addr.offset, addr.writeback), (Offset::Imm(8), Writeback::Pre));
        let Instr::StorePair { addr, .. } = p("STP X0,X1,[X2],#-16").unwrap() else { panic!() };
        assert_eq!(addr.writeback, Writeback::Post(-16));
    }

    #[test]
    fn extended_register_operands() {
        let Instr::Alu { op2, .. } = p("ADD X7,X6,W4,SXTW").unwrap() else { panic!() };
        assert!(matches!(op2, Operand::RegExt(_, Extend::Sxtw(0))));
        let Instr::Alu { op, rn, op2, .. } = p("SXTW X11,W6").unwrap() else { panic!() };
        assert_eq!((op, rn.idx), (AluOp::Orr, ZR));
        assert!(matches!(op2, Operand::RegExt(_, Extend::Sxtw(0))));
    }

    #[test]
    fn aliases() {
        let x = |i| RegOp { idx: i, wide: true };
        let w = |i| RegOp { idx: i, wide: false };
        assert_eq!(p("MOVN W0,#0xffff,LSL #16").unwrap(), Instr::MovImm { rd: w(0), imm: 0xffff });
        assert_eq!(p("MOVZ X0,#1,LSL #48").unwrap(), Instr::MovImm { rd: x(0), imm: 1 << 48 });
        assert_eq!(
            p("NEG X0,X1").unwrap(),
            Instr::Alu { op: AluOp::Sub, rd: x(0), rn: x(ZR), op2: Operand::Reg(x(1)), flags: false }
        );
        assert_eq!(
            p("CINC W0,W1,EQ").unwrap(),
            Instr::Csel { rd: w(0), rn: w(1), rm: w(1), cond: Cond::Ne, inc: true }
        );
        assert_eq!(
            p("CMP X0,#1,LSL #12").unwrap(),
            Instr::Alu { op: AluOp::Sub, rd: x(ZR), rn: x(0), op2: Operand::Imm(4096), flags: true }
        );
        assert!(matches!(p("CMP X0,X1,LSL #2").unwrap(), Instr::Alu { op2: Operand::RegExt(_, Extend::Lsl(2)), .. }));
        assert_eq!(p("WFE").unwrap(), Instr::Nop);
        assert_eq!(p("ISB SY").unwrap(), Instr::Barrier(Barrier::Isb));
    }

    #[test]
    fn rejects_bad_forms() {
        for bad in [
            "STRB X0,[X1]",
            "LDRSW W0,[X1]",
            "LDAR X0,[X1,#8]",
            "LDAR X0,[X1],#8",
            "LDP W0,X1,[X2]",
            "LDADDB X0,X1,[X2]",
            "LDR X0,[X1,X2]!",
            "LDXP W0,X1,[X2]",
            "STXP X0,X1,X2,[X3]",
            "LDXP X0,X1,[X2,#16]",
            "LDR X1,[X1],#8",
            "STRB W2,[X2,#1]!",
            "LDP X0,X0,[X1]",
            "STP X0,X1,[X1,#16]!",
            "STXR W0,W0,[X1]",
            "STXR W1,W0,[X1]",
            "STXP W1,X1,X2,[X3]",
            "LDXP X0,X0,[X2]",
            "LDR X0,[XZR]",
            "ISB #3",
            "MOVZ X0,#1,LSL #8",
            "MOVN W0,#1,LSL #32",
            "ADD X0,X1,#1,LSL #4",
        ] {
            assert!(p(bad).is_err(), "accepted {}", bad);
        }
    }
}
