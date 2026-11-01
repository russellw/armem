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
/// `#imm`, `Rm, <extend or shift>` or `#imm, LSL #12`. A shift must be
/// less than the size of `rm`, and ROR is allowed only if `ror`.
fn parse_op2(ops: &[String], mnem: &str, ror: bool) -> Result<Operand> {
    let op2 = parse_op2_unchecked(ops, mnem)?;
    if let Operand::RegExt(r, e) = op2 {
        let ok = match e {
            Extend::Lsl(s) | Extend::Lsr(s) | Extend::Asr(s) => (s as u64) < 8 * Size::of(r).bytes(),
            Extend::Ror(s) => ror && (s as u64) < 8 * Size::of(r).bytes(),
            Extend::Uxtw(_) | Extend::Sxtw(_) => true,
        };
        if !ok {
            return Err(format!("{}: bad shift '{}'", mnem, ops[1].trim()));
        }
    }
    Ok(op2)
}

fn parse_op2_unchecked(ops: &[String], mnem: &str) -> Result<Operand> {
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
        "LSR" => Extend::Lsr(amt),
        "ASR" => Extend::Asr(amt),
        "ROR" => Extend::Ror(amt),
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
        3 => match parse_extend(parts[2])? {
            Extend::Lsr(_) | Extend::Asr(_) | Extend::Ror(_) => return Err(format!("bad address '{}'", s)),
            e => Offset::Reg(parse_reg(parts[1])?, e),
        },
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

/// The condition of a CSEL alias such as CINC, which is the inverse of
/// the one written. AL and NV are not allowed.
fn alias_cond(s: &str, mnem: &str) -> Result<Cond> {
    match parse_cond(s)? {
        Cond::Al => Err(format!("{}: condition '{}' is not allowed", mnem, s.trim())),
        c => Ok(invert(c)),
    }
}

fn same_width(regs: &[RegOp], mnem: &str) -> Result<()> {
    if regs.iter().all(|r| r.wide == regs[0].wide) {
        Ok(())
    } else {
        Err(format!("{}: registers must be the same width", mnem))
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
        let logical = matches!(op, AluOp::And | AluOp::Orr | AluOp::Eor | AluOp::Bic | AluOp::Orn | AluOp::Eon);
        let op2 = parse_op2(&ops[2..], m, logical)?;
        let add_sub = matches!(op, AluOp::Add | AluOp::Sub);
        let logical_imm = matches!(op, AluOp::And | AluOp::Orr | AluOp::Eor) && matches!(op2, Operand::Imm(_));
        let rd = if !flags && (add_sub || logical_imm) { parse_reg_sp(&ops[0])? } else { reg(0)? };
        let rn = if add_sub { parse_reg_sp(&ops[1])? } else { reg(1)? };
        // With an immediate, register 31 is SP: the zero register cannot be
        // encoded.
        if add_sub && rn.idx == ZR && matches!(op2, Operand::Imm(_)) {
            return Err(format!("{}: the first source cannot be the zero register with an immediate", m));
        }
        Ok(Instr::Alu { op, rd, rn, op2, flags })
    };

    if SIMD_MNEMONICS.contains(&m) && ops.iter().any(|o| parse_vop(o).is_some()) {
        return parse_simd(m, &ops);
    }

    let ins = match m {
        // Hints that may be implemented as NOP, which is all the emulator needs.
        "NOP" | "YIELD" | "WFE" | "WFI" | "SEV" | "SEVL" => Instr::Nop,
        "CLREX" => {
            match ops.len() {
                0 => {}
                1 if (0..=15).contains(&parse_imm(&ops[0])?) => {}
                _ => return Err(format!("CLREX: bad operands '{}'", rest)),
            }
            Instr::Clrex
        }
        "MOVZ" | "MOVN" | "MOVK" => {
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
            if m == "MOVK" {
                return Ok(Instr::Movk { rd, imm: imm as u16, shift });
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
        "ROR" if ops.len() == 3 && ops[2].starts_with('#') => {
            let (rd, rn) = (reg(0)?, reg(1)?);
            same_width(&[rd, rn], m)?;
            let lsb = parse_imm(&ops[2])?;
            if !(0..8 * Size::of(rd).bytes() as i128).contains(&lsb) {
                return Err(format!("{}: shift out of range", m));
            }
            Instr::Extr { rd, rn, rm: rn, lsb: lsb as u8 }
        }
        "ROR" => {
            want(&ops, 3, m)?;
            let (rd, rn, rm) = (reg(0)?, reg(1)?, reg(2)?);
            same_width(&[rd, rn, rm], m)?;
            Instr::Alu { op: AluOp::Ror, rd, rn, op2: Operand::Reg(rm), flags: false }
        }
        "EXTR" => {
            want(&ops, 4, m)?;
            let (rd, rn, rm) = (reg(0)?, reg(1)?, reg(2)?);
            same_width(&[rd, rn, rm], m)?;
            let lsb = parse_imm(&ops[3])?;
            if !(0..8 * Size::of(rd).bytes() as i128).contains(&lsb) {
                return Err(format!("{}: bit position out of range", m));
            }
            Instr::Extr { rd, rn, rm, lsb: lsb as u8 }
        }
        "RBIT" | "REV16" | "REV32" | "REV" | "REV64" | "CLZ" | "CLS" => {
            want(&ops, 2, m)?;
            let (rd, rn) = (reg(0)?, reg(1)?);
            same_width(&[rd, rn], m)?;
            let op = match m {
                "RBIT" => UnaryOp::Rbit,
                "REV16" => UnaryOp::Rev16,
                "REV32" => UnaryOp::Rev32,
                "REV" | "REV64" => UnaryOp::Rev,
                "CLZ" => UnaryOp::Clz,
                _ => UnaryOp::Cls,
            };
            if !rd.wide && matches!(m, "REV32" | "REV64") {
                return Err(format!("{}: registers must be X", m));
            }
            Instr::Unary { op, rd, rn }
        }
        "ADC" | "ADCS" | "SBC" | "SBCS" => {
            want(&ops, 3, m)?;
            let (rd, rn, rm) = (reg(0)?, reg(1)?, reg(2)?);
            same_width(&[rd, rn, rm], m)?;
            Instr::Adc { rd, rn, rm, sub: m.starts_with('S'), flags: m.ends_with('S') }
        }
        "NGC" | "NGCS" => {
            want(&ops, 2, m)?;
            let (rd, rm) = (reg(0)?, reg(1)?);
            same_width(&[rd, rm], m)?;
            Instr::Adc { rd, rn: RegOp { idx: ZR, wide: rd.wide }, rm, sub: true, flags: m == "NGCS" }
        }
        "BIC" | "BICS" | "ORN" | "EON" => {
            let op = match m {
                "ORN" => AluOp::Orn,
                "EON" => AluOp::Eon,
                _ => AluOp::Bic,
            };
            let ins = alu(op, m == "BICS")?;
            if let Instr::Alu { op2: Operand::Imm(_), .. } = ins {
                return Err(format!("{}: the second operand must be a register", m));
            }
            ins
        }
        "MVN" => {
            if !(2..=3).contains(&ops.len()) {
                return Err(format!("{} expects 2 or 3 operands, got {}", m, ops.len()));
            }
            let rd = reg(0)?;
            let op2 = match parse_op2(&ops[1..], m, true)? {
                Operand::Imm(_) => return Err(format!("{}: the operand must be a register", m)),
                o => o,
            };
            Instr::Alu { op: AluOp::Orn, rd, rn: RegOp { idx: ZR, wide: rd.wide }, op2, flags: false }
        }
        "UDIV" | "SDIV" | "MNEG" => {
            want(&ops, 3, m)?;
            let (rd, rn, rm) = (reg(0)?, reg(1)?, reg(2)?);
            same_width(&[rd, rn, rm], m)?;
            match m {
                "UDIV" => Instr::Alu { op: AluOp::Udiv, rd, rn, op2: Operand::Reg(rm), flags: false },
                "SDIV" => Instr::Alu { op: AluOp::Sdiv, rd, rn, op2: Operand::Reg(rm), flags: false },
                _ => Instr::madd(MulKind::Plain, rd, rn, rm, RegOp { idx: ZR, wide: rd.wide }, true),
            }
        }
        "MADD" | "MSUB" => {
            want(&ops, 4, m)?;
            let (rd, rn, rm, ra) = (reg(0)?, reg(1)?, reg(2)?, reg(3)?);
            same_width(&[rd, rn, rm, ra], m)?;
            Instr::madd(MulKind::Plain, rd, rn, rm, ra, m == "MSUB")
        }
        // Xd, Wn, Wm{, Xa}
        "SMADDL" | "SMSUBL" | "UMADDL" | "UMSUBL" | "SMULL" | "SMNEGL" | "UMULL" | "UMNEGL" => {
            let acc = m.ends_with("DDL") || m.ends_with("UBL");
            want(&ops, if acc { 4 } else { 3 }, m)?;
            let (rd, rn, rm) = (reg(0)?, reg(1)?, reg(2)?);
            let ra = if acc { reg(3)? } else { RegOp { idx: ZR, wide: true } };
            if !rd.wide || rn.wide || rm.wide || !ra.wide {
                return Err(format!("{}: bad register widths", m));
            }
            let kind = if m.starts_with('S') { MulKind::SignedLong } else { MulKind::UnsignedLong };
            Instr::madd(kind, rd, rn, rm, ra, m.contains("SUB") || m.contains("NEG"))
        }
        "SMULH" | "UMULH" => {
            want(&ops, 3, m)?;
            let (rd, rn, rm) = (reg(0)?, reg(1)?, reg(2)?);
            if !(rd.wide && rn.wide && rm.wide) {
                return Err(format!("{}: registers must be X", m));
            }
            Instr::Mulh { rd, rn, rm, signed: m == "SMULH" }
        }
        "SBFM" | "BFM" | "UBFM" | "SBFX" | "BFXIL" | "UBFX" | "SBFIZ" | "BFI" | "UBFIZ" | "BFC" => {
            let kind = match m.as_bytes()[0] {
                b'S' => BfKind::Signed,
                b'U' => BfKind::Unsigned,
                _ => BfKind::Insert,
            };
            let rd = reg(0)?;
            let (rn, imms) = if m == "BFC" {
                want(&ops, 3, m)?;
                (RegOp { idx: ZR, wide: rd.wide }, &ops[1..])
            } else {
                want(&ops, 4, m)?;
                (reg(1)?, &ops[2..])
            };
            same_width(&[rd, rn], m)?;
            let bits = 8 * Size::of(rd).bytes() as i128;
            let (a, b) = (parse_imm(&imms[0])?, parse_imm(&imms[1])?);
            let (immr, imms) = match m {
                "SBFM" | "BFM" | "UBFM" => (a, b),
                // #lsb, #width: extract a field to the bottom...
                "SBFX" | "BFXIL" | "UBFX" if a >= 0 && b >= 1 && a + b <= bits => (a, a + b - 1),
                // ... or insert one at bit lsb.
                "SBFIZ" | "BFI" | "UBFIZ" | "BFC" if a >= 0 && b >= 1 && a + b <= bits => ((bits - a) % bits, b - 1),
                _ => return Err(format!("{}: bad bitfield '{}'", m, rest)),
            };
            if !(0..bits).contains(&immr) || !(0..bits).contains(&imms) {
                return Err(format!("{}: bad bitfield '{}'", m, rest));
            }
            Instr::bitfield(kind, rd, rn, immr as u8, imms as u8)
        }
        "SXTB" | "SXTH" | "UXTB" | "UXTH" => {
            want(&ops, 2, m)?;
            let (rd, rn) = (reg(0)?, reg(1)?);
            if rn.wide || (rd.wide && m.starts_with('U')) {
                return Err(format!("{}: bad register widths", m));
            }
            let kind = if m.starts_with('S') { BfKind::Signed } else { BfKind::Unsigned };
            let imms = if m.ends_with('B') { 7 } else { 15 };
            Instr::bitfield(kind, rd, RegOp { idx: rn.idx, wide: rd.wide }, 0, imms)
        }
        "CCMP" | "CCMN" => {
            want(&ops, 4, m)?;
            let rn = reg(0)?;
            let op2 = if ops[1].starts_with('#') {
                let i = parse_imm(&ops[1])?;
                if !(0..=31).contains(&i) {
                    return Err(format!("{}: immediate out of range", m));
                }
                Operand::Imm(i as u64)
            } else {
                let rm = reg(1)?;
                same_width(&[rn, rm], m)?;
                Operand::Reg(rm)
            };
            let nzcv = parse_imm(&ops[2])?;
            if !(0..=15).contains(&nzcv) {
                return Err(format!("{}: bad flags '{}'", m, ops[2]));
            }
            Instr::Ccmp { rn, op2, nzcv: nzcv as u8, cond: parse_cond(&ops[3])?, cmn: m == "CCMN" }
        }
        "NEG" | "NEGS" => {
            if !(2..=3).contains(&ops.len()) {
                return Err(format!("{} expects 2 or 3 operands, got {}", m, ops.len()));
            }
            let rd = reg(0)?;
            let op2 = parse_op2(&ops[1..], m, false)?;
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
                op2: parse_op2(&ops[1..], m, m == "TST")?,
                flags: true,
            }
        }
        "CSEL" | "CSINC" | "CSINV" | "CSNEG" => {
            want(&ops, 4, m)?;
            let (rd, rn, rm) = (reg(0)?, reg(1)?, reg(2)?);
            same_width(&[rd, rn, rm], m)?;
            let (inv, inc) = (matches!(m, "CSINV" | "CSNEG"), matches!(m, "CSINC" | "CSNEG"));
            Instr::Csel { rd, rn, rm, cond: parse_cond(&ops[3])?, inv, inc }
        }
        // CINC rd, rn, cond is CSINC rd, rn, rn, !cond, and so on.
        "CINC" | "CINV" | "CNEG" => {
            want(&ops, 3, m)?;
            let (rd, rn) = (reg(0)?, reg(1)?);
            same_width(&[rd, rn], m)?;
            let (inv, inc) = (m != "CINC", m != "CINV");
            Instr::Csel { rd, rn, rm: rn, cond: alias_cond(&ops[2], m)?, inv, inc }
        }
        "CSET" | "CSETM" => {
            want(&ops, 2, m)?;
            let rd = reg(0)?;
            let z = RegOp { idx: ZR, wide: rd.wide };
            let m1 = m == "CSETM";
            Instr::Csel { rd, rn: z, rm: z, cond: alias_cond(&ops[1], m)?, inv: m1, inc: !m1 }
        }
        "B" | "BL" => {
            want(&ops, 1, m)?;
            let target = label(labels, &ops[0])?;
            if m == "BL" { Instr::Bl { target } } else { Instr::B { target } }
        }
        "BR" | "BLR" | "RET" => {
            let rn = match (m, ops.len()) {
                ("RET", 0) => RegOp { idx: 30, wide: true },
                (_, 1) => reg(0)?,
                _ => return Err(format!("{} expects {} operand", m, if m == "RET" { "at most one" } else { "one" })),
            };
            if !rn.wide {
                return Err(format!("{}: the target must be an X register", m));
            }
            let kind = match m {
                "BR" => BrKind::Br,
                "BLR" => BrKind::Blr,
                _ => BrKind::Ret,
            };
            Instr::BrReg { rn, kind }
        }
        "TBZ" | "TBNZ" => {
            want(&ops, 3, m)?;
            let rt = reg(0)?;
            let bit = parse_imm(&ops[1])?;
            if !(0..8 * Size::of(rt).bytes() as i128).contains(&(bit as i128)) {
                return Err(format!("{}: bit number out of range", m));
            }
            Instr::Tbz { rt, bit: bit as u8, nonzero: m == "TBNZ", target: label(labels, &ops[2])? }
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

/// Mnemonics that have forms with SIMD&FP register operands.
const SIMD_MNEMONICS: &[&str] = &[
    "LDR", "STR", "LDUR", "STUR", "LDP", "STP", "LDNP", "STNP", "MOVI", "MVNI", "FMOV", "MOV", "INS", "UMOV", "SMOV", "DUP",
    "AND", "BIC", "ORR", "ORN", "EOR", "CNT", "ADDV", "UADDLV",
];

/// A SIMD&FP register operand.
#[derive(Clone, Copy, Debug, PartialEq)]
enum VOp {
    /// Bn, Hn, Sn, Dn or Qn.
    Scalar(VReg, Size),
    /// Vn.T: the element size, and whether it is 128 bits (`Q`) or 64.
    Vector(VReg, Size, bool),
    /// Vn.Ts[index].
    Elem(VReg, Size, u8),
}

fn elem_size(c: char) -> Option<Size> {
    Some(match c {
        'B' => Size::B1,
        'H' => Size::B2,
        'S' => Size::B4,
        'D' => Size::B8,
        _ => return None,
    })
}

fn vreg_num(s: &str) -> Option<VReg> {
    let n: u8 = s.parse().ok()?;
    (n <= 31 && !s.starts_with('+')).then_some(n)
}

fn parse_vop(s: &str) -> Option<VOp> {
    let u = s.trim().to_ascii_uppercase();
    let mut chars = u.chars();
    let first = chars.next()?;
    let rest = chars.as_str();
    if first != 'V' {
        let size = match first {
            'Q' => Size::B16,
            c => elem_size(c)?,
        };
        return Some(VOp::Scalar(vreg_num(rest)?, size));
    }
    let (n, t) = rest.split_once('.')?;
    let n = vreg_num(n)?;
    if let Some((e, i)) = t.strip_suffix(']').and_then(|t| t.split_once('[')) {
        let mut ec = e.chars();
        let size = elem_size(ec.next()?)?;
        let i: u8 = i.parse().ok()?;
        return (ec.next().is_none() && (i as u64) < 16 / size.bytes()).then_some(VOp::Elem(n, size, i));
    }
    let (size, q) = match t {
        "8B" => (Size::B1, false),
        "16B" => (Size::B1, true),
        "4H" => (Size::B2, false),
        "8H" => (Size::B2, true),
        "2S" => (Size::B4, false),
        "4S" => (Size::B4, true),
        "2D" => (Size::B8, true),
        _ => return None,
    };
    Some(VOp::Vector(n, size, q))
}

/// An instruction with SIMD&FP register operands.
fn parse_simd(m: &str, ops: &[String]) -> Result<Instr> {
    let bad = || Err(format!("{}: unsupported operands '{}'", m, ops.join(", ")));
    let vop = |i: usize| ops.get(i).and_then(|o| parse_vop(o));
    let gp = |i: usize| ops.get(i).ok_or_else(|| format!("{}: missing operand", m)).and_then(|o| parse_reg(o));
    // A general-purpose register of the width that suits an element.
    let gp_for = |i: usize, size: Size| -> Result<RegOp> {
        let r = gp(i)?;
        if r.wide != (size == Size::B8) {
            return Err(format!("{}: register {} does not match the element size", m, r));
        }
        Ok(r)
    };
    match m {
        "LDR" | "STR" | "LDUR" | "STUR" => {
            let Some(VOp::Scalar(vt, size)) = vop(0) else { return bad() };
            let addr = parse_mem_operand(ops, 1, m)?;
            if m.ends_with("UR") && (addr.writeback != Writeback::None || !matches!(addr.offset, Offset::Imm(_))) {
                return Err(format!("{}: only an immediate offset is allowed", m));
            }
            if let Offset::Reg(_, Extend::Lsl(s) | Extend::Uxtw(s) | Extend::Sxtw(s)) = addr.offset {
                if s != 0 && 1u64 << s != size.bytes() {
                    return Err(format!("{}: bad index shift", m));
                }
            }
            Ok(if m.starts_with('L') {
                Instr::VLoad { vt, vt2: None, addr, size }
            } else {
                Instr::VStore { vt, vt2: None, addr, size }
            })
        }
        "LDP" | "STP" | "LDNP" | "STNP" => {
            let (Some(VOp::Scalar(vt, size)), Some(VOp::Scalar(vt2, size2))) = (vop(0), vop(1)) else { return bad() };
            if size != size2 || size < Size::B4 {
                return Err(format!("{}: registers must be S, D or Q, of the same size", m));
            }
            let addr = parse_mem_operand(ops, 2, m)?;
            if !matches!(addr.offset, Offset::Imm(_)) || (m.ends_with("NP") && addr.writeback != Writeback::None) {
                return Err(format!("{}: bad addressing mode", m));
            }
            if m.starts_with('L') {
                if vt == vt2 {
                    return Err(format!("UNPREDICTABLE register combination in '{} {}'", m, ops.join(", ")));
                }
                Ok(Instr::VLoad { vt, vt2: Some(vt2), addr, size })
            } else {
                Ok(Instr::VStore { vt, vt2: Some(vt2), addr, size })
            }
        }
        "MOVI" | "MVNI" => {
            let (vd, esize, q) = match vop(0) {
                Some(VOp::Scalar(vd, Size::B8)) if m == "MOVI" => (vd, Size::B8, false),
                Some(VOp::Vector(vd, esize, q)) => (vd, esize, q),
                _ => return bad(),
            };
            if !(2..=3).contains(&ops.len()) {
                return bad();
            }
            let imm = parse_imm(&ops[1])?;
            let shift = ops.get(2).map(|e| e.trim().to_ascii_uppercase());
            let (kind, amt) = match &shift {
                None => ("LSL", 0),
                Some(e) => {
                    let (k, a) = e.split_once(char::is_whitespace).ok_or_else(|| format!("{}: bad shift '{}'", m, e))?;
                    (if k == "MSL" { "MSL" } else if k == "LSL" { "LSL" } else { return bad() }, parse_imm(a)? as u32)
                }
            };
            let i = imm as u64;
            let rep32 = |v: u64| v | v << 32;
            let byte_ok = (0..=255).contains(&imm);
            let v = match (esize, kind, amt) {
                (Size::B8, "LSL", 0) if m == "MOVI" && (0..8).all(|b| matches!(i >> (8 * b) & 0xff, 0 | 0xff)) => i,
                (Size::B1, "LSL", 0) if m == "MOVI" && byte_ok => i * 0x0101_0101_0101_0101,
                (Size::B2, "LSL", 0 | 8) if byte_ok => (i << amt) * 0x0001_0001_0001_0001,
                (Size::B4, "LSL", 0 | 8 | 16 | 24) if byte_ok => rep32(i << amt),
                (Size::B4, "MSL", 8 | 16) if byte_ok => rep32(i << amt | ((1 << amt) - 1)),
                _ => return Err(format!("{}: bad immediate '{}'", m, ops[1..].join(", "))),
            };
            let v = if m == "MVNI" { !v } else { v };
            Ok(Instr::VMovImm { vd, imm: if q { v as u128 | (v as u128) << 64 } else { v as u128 } })
        }
        "FMOV" => {
            want(ops, 2, m)?;
            Ok(match (vop(0), vop(1)) {
                (Some(VOp::Scalar(vd, size @ (Size::B4 | Size::B8))), None) => {
                    Instr::VIns { vd, index: 0, size, rn: gp_for(1, size)?, zero: true }
                }
                (Some(VOp::Elem(vd, Size::B8, 1)), None) => Instr::VIns { vd, index: 1, size: Size::B8, rn: gp_for(1, Size::B8)?, zero: false },
                (None, Some(VOp::Scalar(vn, size @ (Size::B4 | Size::B8)))) => {
                    Instr::VMovToGp { rd: gp_for(0, size)?, vn, index: 0, size, signed: false }
                }
                (None, Some(VOp::Elem(vn, Size::B8, 1))) => {
                    Instr::VMovToGp { rd: gp_for(0, Size::B8)?, vn, index: 1, size: Size::B8, signed: false }
                }
                (Some(VOp::Scalar(vd, s1 @ (Size::B4 | Size::B8))), Some(VOp::Scalar(vn, s2))) if s1 == s2 => {
                    Instr::VMov { vd, vn, size: s1 }
                }
                _ => return bad(),
            })
        }
        "MOV" | "INS" | "UMOV" | "SMOV" => {
            want(ops, 2, m)?;
            Ok(match (m, vop(0), vop(1)) {
                ("MOV", Some(VOp::Vector(vd, Size::B1, q)), Some(VOp::Vector(vn, Size::B1, q2))) if q == q2 => {
                    Instr::VLogic { op: AluOp::Orr, vd, vn, vm: vn, q }
                }
                ("MOV" | "INS", Some(VOp::Elem(vd, size, index)), None) => Instr::VIns { vd, index, size, rn: gp_for(1, size)?, zero: false },
                ("MOV" | "INS", Some(VOp::Elem(vd, size, index)), Some(VOp::Elem(vn, s2, index2))) if size == s2 => {
                    Instr::VInsElem { vd, index, vn, index2, size }
                }
                ("MOV", Some(VOp::Scalar(vd, s1)), Some(VOp::Elem(vn, size, index))) if s1 == size => {
                    Instr::VDupElem { vd, vn, index, size, lanes: 1 }
                }
                ("MOV", None, Some(VOp::Elem(vn, size @ (Size::B4 | Size::B8), index)))
                | ("UMOV", None, Some(VOp::Elem(vn, size, index))) => {
                    Instr::VMovToGp { rd: gp_for(0, size)?, vn, index, size, signed: false }
                }
                ("SMOV", None, Some(VOp::Elem(vn, size @ (Size::B1 | Size::B2 | Size::B4), index))) => {
                    let rd = gp(0)?;
                    if size == Size::B4 && !rd.wide {
                        return bad();
                    }
                    Instr::VMovToGp { rd, vn, index, size, signed: true }
                }
                _ => return bad(),
            })
        }
        "DUP" => {
            want(ops, 2, m)?;
            match (vop(0), vop(1)) {
                (Some(VOp::Vector(vd, size, q)), None) => Ok(Instr::VDup { vd, size, q, rn: gp_for(1, size)? }),
                (Some(VOp::Vector(vd, size, q)), Some(VOp::Elem(vn, s2, index))) if size == s2 => {
                    let lanes = (if q { 16 } else { 8 }) / size.bytes() as u8;
                    Ok(Instr::VDupElem { vd, vn, index, size, lanes })
                }
                (Some(VOp::Scalar(vd, s1)), Some(VOp::Elem(vn, size, index))) if s1 == size => {
                    Ok(Instr::VDupElem { vd, vn, index, size, lanes: 1 })
                }
                _ => bad(),
            }
        }
        "AND" | "BIC" | "ORR" | "ORN" | "EOR" => {
            want(ops, 3, m)?;
            let (Some(VOp::Vector(vd, Size::B1, q)), Some(VOp::Vector(vn, Size::B1, q1)), Some(VOp::Vector(vm, Size::B1, q2))) =
                (vop(0), vop(1), vop(2))
            else {
                return bad();
            };
            if q != q1 || q != q2 {
                return bad();
            }
            let op = match m {
                "AND" => AluOp::And,
                "BIC" => AluOp::Bic,
                "ORR" => AluOp::Orr,
                "ORN" => AluOp::Orn,
                _ => AluOp::Eor,
            };
            Ok(Instr::VLogic { op, vd, vn, vm, q })
        }
        "CNT" => {
            want(ops, 2, m)?;
            match (vop(0), vop(1)) {
                (Some(VOp::Vector(vd, Size::B1, q)), Some(VOp::Vector(vn, Size::B1, q2))) if q == q2 => Ok(Instr::VCnt { vd, vn, q }),
                _ => bad(),
            }
        }
        _ => {
            // ADDV, UADDLV
            want(ops, 2, m)?;
            let long = m == "UADDLV";
            match (vop(0), vop(1)) {
                (Some(VOp::Scalar(vd, s)), Some(VOp::Vector(vn, Size::B1, q))) if s == if long { Size::B2 } else { Size::B1 } => {
                    Ok(Instr::VAddv { vd, vn, q, long })
                }
                _ => bad(),
            }
        }
    }
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
            Instr::Csel { rd: w(0), rn: w(1), rm: w(1), cond: Cond::Ne, inv: false, inc: true }
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
    fn calls_and_branches() {
        let x = |i| RegOp { idx: i, wide: true };
        let labels: HashMap<String, usize> = [("f".to_string(), 3)].into();
        let q = |s: &str| parse_instr(s, &labels).unwrap();
        assert_eq!(q("BL f"), Instr::Bl { target: 3 });
        assert_eq!(q("RET"), Instr::BrReg { rn: x(30), kind: BrKind::Ret });
        assert_eq!(q("RET X1"), Instr::BrReg { rn: x(1), kind: BrKind::Ret });
        assert_eq!(q("BLR X2"), Instr::BrReg { rn: x(2), kind: BrKind::Blr });
        assert_eq!(q("BR XZR"), Instr::BrReg { rn: x(ZR), kind: BrKind::Br });
        assert_eq!(q("TBNZ X3,#63,f"), Instr::Tbz { rt: x(3), bit: 63, nonzero: true, target: 3 });
        assert_eq!(q("MOVK X0,#0x1234,LSL #48"), Instr::Movk { rd: x(0), imm: 0x1234, shift: 48 });
        assert_eq!(q("CLREX"), Instr::Clrex);
        assert_eq!(q("CLREX #3"), Instr::Clrex);
        for bad in ["RET W0", "BR", "BLR X1,X2", "TBZ W0,#32,f", "MOVK X0,#1,LSL #8", "MOVK W0,#1,LSL #32", "CLREX #16"] {
            assert!(parse_instr(bad, &labels).is_err(), "accepted {}", bad);
        }
    }

    #[test]
    fn integer_aliases() {
        let x = |i| RegOp { idx: i, wide: true };
        let w = |i| RegOp { idx: i, wide: false };
        let bf = |kind, rd, rn, immr, imms| Instr::Bitfield { kind, rd, rn, immr, imms };
        assert_eq!(p("UBFX X0,X1,#4,#8").unwrap(), bf(BfKind::Unsigned, x(0), x(1), 4, 11));
        assert_eq!(p("UBFIZ W0,W1,#20,#8").unwrap(), bf(BfKind::Unsigned, w(0), w(1), 12, 7));
        assert_eq!(p("BFC X0,#4,#56").unwrap(), bf(BfKind::Insert, x(0), x(ZR), 60, 55));
        assert_eq!(p("BFI X0,X1,#0,#4").unwrap(), bf(BfKind::Insert, x(0), x(1), 0, 3));
        assert_eq!(p("SXTB X0,W1").unwrap(), bf(BfKind::Signed, x(0), x(1), 0, 7));
        // Aliases with an `Alu` form.
        assert_eq!(p("UBFIZ X0,X1,#3,#61").unwrap(), p("LSL X0,X1,#3").unwrap());
        assert_eq!(p("SBFX W0,W1,#3,#29").unwrap(), p("ASR W0,W1,#3").unwrap());
        assert_eq!(p("SBFM X0,X1,#0,#31").unwrap(), p("SXTW X0,W1").unwrap());
        assert_eq!(p("MADD W0,W1,W2,WZR").unwrap(), p("MUL W0,W1,W2").unwrap());
        assert_eq!(p("ROR X0,X1,#3").unwrap(), Instr::Extr { rd: x(0), rn: x(1), rm: x(1), lsb: 3 });
        assert_eq!(p("REV64 X0,X1").unwrap(), p("REV X0,X1").unwrap());
        assert_eq!(p("NGCS W0,W1").unwrap(), Instr::Adc { rd: w(0), rn: w(ZR), rm: w(1), sub: true, flags: true });
        assert_eq!(
            p("CSETM W0,EQ").unwrap(),
            Instr::Csel { rd: w(0), rn: w(ZR), rm: w(ZR), cond: Cond::Ne, inv: true, inc: false }
        );
        assert_eq!(
            p("SMNEGL X0,W1,W2").unwrap(),
            Instr::Madd { kind: MulKind::SignedLong, rd: x(0), rn: w(1), rm: w(2), ra: x(ZR), sub: true }
        );
        assert_eq!(
            p("CCMN X1,#31,#15,AL").unwrap(),
            Instr::Ccmp { rn: x(1), op2: Operand::Imm(31), nzcv: 15, cond: Cond::Al, cmn: true }
        );
        for bad in [
            "UBFX X0,X1,#60,#8",
            "UBFX W0,W1,#0,#0",
            "BFI W0,X1,#0,#4",
            "UBFM W0,W1,#32,#0",
            "UXTB X0,W1",
            "SXTH W0,X1",
            "CINC W0,W1,AL",
            "CSETM X0,NV",
            "CNEG W0,X1,EQ",
            "UDIV X0,X1,W2",
            "SDIV X0,X1,#2",
            "MADD X0,X1,X2",
            "SMULL X0,X1,X2",
            "UMULH W0,W1,W2",
            "BIC X0,X1,#1",
            "ADD X0,X1,X2,ROR #3",
            "AND W0,W1,W2,LSR #32",
            "LDR X0,[X1,X2,LSR #3]",
            "CCMP X0,#32,#0,EQ",
            "CCMP X0,X1,#16,EQ",
            "CCMP X0,W1,#0,EQ",
            "REV32 W0,W1",
            "REV64 W0,W1",
            "CLZ X0,W1",
            "ROR W0,W1,#32",
            "ROR X0,X1,W2",
            "EXTR X0,X1,X2,#64",
            "EXTR W0,W1,X2,#3",
            "ADC X0,X1,#1",
            "SBC W0,W1,X2",
            "NGC X0,W1",
            "SUBS X0,XZR,#1",
            "ADD W0,WZR,#1",
        ] {
            assert!(p(bad).is_err(), "accepted {}", bad);
        }
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
