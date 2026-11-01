//! Decoder for A64 machine code.
//!
//! Produces the same `isa::Instr` as the assembly parser in `asm.rs`, so
//! compiled code runs under the same models as litmus tests. It covers
//! exactly the instructions the emulator supports. Any other encoding,
//! including an unallocated or UNPREDICTABLE one, is an error rather
//! than a guess.
//!
//! Register number 31 is the zero register or the stack pointer, depending
//! on the operand: base registers, the destination and first source of
//! `ADD`/`SUB` (immediate and extended register), and the destination of
//! a logical immediate are SP.
//!
//! Branches (including `BL`) decode to instruction indexes, relative to
//! the instruction's own index. `BL` and `BLR` write a return address,
//! which is computed when they run (`isa::Code::addr`).
//!
//! `ADR` and `ADRP` compute an address from the PC, so they decode only when
//! the code's load address is known (`decode_at`, `decode_program_at`), and
//! become `MovImm` of the computed address: the result is a constant with
//! no dependencies.
//!
//! Some encodings decode to the same `Instr` as a different but equivalent
//! source form: `ORR Xd, XZR, ...` becomes `MovImm` or `MovReg`, and `UXTX`
//! or `SXTX` extends become `LSL`. Bitfield moves and `MADD` become the
//! `Alu` form of an alias where they have one (`Instr::bitfield`,
//! `Instr::madd`), as the assembler's do. The hints `YIELD`, `WFE`, `WFI`, `SEV`
//! and `SEVL` become `Nop`, which is a valid implementation of each.

use crate::isa::*;

pub type Result<T> = std::result::Result<T, String>;

/// Bits `hi..=lo` of `w`.
fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}

fn bit(w: u32, n: u32) -> bool {
    w >> n & 1 != 0
}

/// Sign-extend the low `n` bits of `v`.
fn sext(v: u32, n: u32) -> i64 {
    let s = 64 - n;
    ((v as i64) << s) >> s
}

/// A register field in which 31 means the zero register.
fn zr(i: u32, wide: bool) -> RegOp {
    RegOp { idx: i as u8, wide }
}

/// A register field in which 31 means SP.
fn sp(i: u32, wide: bool) -> RegOp {
    RegOp { idx: if i == 31 { SP } else { i as u8 }, wide }
}

fn unsupported<T>() -> Result<T> {
    Err("unsupported or unallocated instruction".into())
}

fn unpredictable<T>() -> Result<T> {
    Err("UNPREDICTABLE register combination".into())
}

const CONDS: [Cond; 16] = [
    Cond::Eq,
    Cond::Ne,
    Cond::Cs,
    Cond::Cc,
    Cond::Mi,
    Cond::Pl,
    Cond::Vs,
    Cond::Vc,
    Cond::Hi,
    Cond::Ls,
    Cond::Ge,
    Cond::Lt,
    Cond::Gt,
    Cond::Le,
    Cond::Al,
    // NV behaves as AL.
    Cond::Al,
];

/// The architecture's DecodeBitMasks, for a logical immediate.
fn bitmask_imm(n: bool, immr: u32, imms: u32, wide: bool) -> Option<u64> {
    let combined = (n as u32) << 6 | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 || (!wide && n) {
        return None;
    }
    let esize = 1u32 << len;
    let levels = esize - 1;
    let (s, r) = (imms & levels, immr & levels);
    if s == levels {
        return None;
    }
    let emask = if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 };
    let welem = (1u64 << (s + 1)) - 1;
    let elem = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & emask };
    let mut v = 0u64;
    let mut i = 0;
    while i < 64 {
        v |= elem << i;
        i += esize;
    }
    Some(if wide { v } else { v & 0xffff_ffff })
}

/// Decode one instruction. `index` is its position in the program, in
/// instructions; branch targets are returned as instruction indexes.
pub fn decode(word: u32, index: usize) -> Result<Instr> {
    decode_at(word, index, None)
}

/// Decode one instruction of code loaded at `base` (if known): the
/// instruction's address is `base + 4 * index`.
pub fn decode_at(word: u32, index: usize, base: Option<u64>) -> Result<Instr> {
    decode_inner(word, index, base).map_err(|e| format!("{:#010x}: {}", word, e))
}

/// Decode little-endian A64 code. Branches must stay within the code
/// (a branch to just past the end is allowed: it ends the thread).
pub fn decode_program(code: &[u8]) -> Result<Vec<Instr>> {
    decode_program_at(code, None)
}

/// Decode little-endian A64 code loaded at address `base`, if known
/// (needed for `ADR`/`ADRP`).
pub fn decode_program_at(code: &[u8], base: Option<u64>) -> Result<Vec<Instr>> {
    decode_code(code, base)
}

fn decode_code(code: &[u8], base: Option<u64>) -> Result<Vec<Instr>> {
    if code.len() % 4 != 0 {
        return Err(format!("code size {} is not a multiple of 4", code.len()));
    }
    let prog: Vec<Instr> = code
        .chunks_exact(4)
        .enumerate()
        .map(|(i, c)| decode_at(u32::from_le_bytes(c.try_into().unwrap()), i, base))
        .collect::<Result<_>>()?;
    for (i, ins) in prog.iter().enumerate() {
        if let Some(target) = branch_target(ins) {
            if target > prog.len() {
                return Err(format!("instruction {}: branch target {} is outside the code", i, target));
            }
        }
    }
    Ok(prog)
}

/// The target of a direct branch.
fn branch_target(ins: &Instr) -> Option<usize> {
    match *ins {
        Instr::B { target }
        | Instr::Bl { target }
        | Instr::BCond { target, .. }
        | Instr::Cbz { target, .. }
        | Instr::Tbz { target, .. } => {
            Some(target)
        }
        _ => None,
    }
}

/// Decode code loaded at `base` to run, such as all of an image's code
/// (see `Image::code`). Unlike `decode_program_at`, a word that cannot be
/// decoded is not an error: it becomes `Instr::Invalid`, a fault only if
/// it runs, as is a direct branch out of the code. `gaps` are byte ranges
/// (relative to `base`) that hold no code, such as the space between
/// sections, or data in a code section.
pub fn decode_code_at(code: &[u8], base: u64, gaps: &[(usize, usize)]) -> Code {
    let n = code.len() / 4;
    let mut out = Code::new(base, Vec::with_capacity(n));
    for (i, c) in code.chunks_exact(4).enumerate() {
        let r = if gaps.iter().any(|&(lo, hi)| 4 * i < hi && 4 * i + 4 > lo) {
            Err("not code".to_string())
        } else {
            decode_at(u32::from_le_bytes(c.try_into().unwrap()), i, Some(base)).and_then(|ins| match branch_target(&ins) {
                Some(t) if t > n => Err(format!("branch to {:#x}, which is not in the code", out.addr(t))),
                _ => Ok(ins),
            })
        };
        out.instrs.push(match r {
            Ok(ins) => ins,
            Err(e) => {
                out.errors.insert(i, e);
                Instr::Invalid
            }
        });
    }
    out
}

fn decode_inner(w: u32, index: usize, base: Option<u64>) -> Result<Instr> {
    let op0 = bits(w, 28, 25);
    match op0 {
        0b1000 | 0b1001 if bits(w, 25, 24) == 0 => pc_rel(w, index, base),
        0b1000 | 0b1001 => data_imm(w),
        0b1010 | 0b1011 => branch_system(w, index),
        0b0101 | 0b1101 => data_reg(w),
        _ if op0 & 0b0101 == 0b0100 => load_store(w),
        _ => unsupported(),
    }
}

/// ADR / ADRP.
fn pc_rel(w: u32, index: usize, base: Option<u64>) -> Result<Instr> {
    let Some(base) = base else {
        return Err("ADR/ADRP needs the code's load address".into());
    };
    let pc = base.wrapping_add(4 * index as u64);
    let imm = sext(bits(w, 23, 5) << 2 | bits(w, 30, 29), 21) as u64;
    let imm = if bit(w, 31) { (pc & !0xfff).wrapping_add(imm << 12) } else { pc.wrapping_add(imm) };
    Ok(Instr::MovImm { rd: zr(bits(w, 4, 0), true), imm })
}

fn data_imm(w: u32) -> Result<Instr> {
    let wide = bit(w, 31);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    match bits(w, 25, 23) {
        // ADD/SUB (immediate)
        0b010 => {
            let flags = bit(w, 29);
            let imm = (bits(w, 21, 10) as u64) << if bit(w, 22) { 12 } else { 0 };
            let rd = if flags { zr(rd, wide) } else { sp(rd, wide) };
            let op = if bit(w, 30) { AluOp::Sub } else { AluOp::Add };
            Ok(Instr::Alu { op, rd, rn: sp(rn, wide), op2: Operand::Imm(imm), flags })
        }
        // Logical (immediate)
        0b100 => {
            let opc = bits(w, 30, 29);
            let Some(imm) = bitmask_imm(bit(w, 22), bits(w, 21, 16), bits(w, 15, 10), wide) else {
                return unsupported();
            };
            let rd = if opc == 0b11 { zr(rd, wide) } else { sp(rd, wide) };
            let rn = zr(rn, wide);
            let op = [AluOp::And, AluOp::Orr, AluOp::Eor, AluOp::And][opc as usize];
            if op == AluOp::Orr && rn.idx == ZR {
                return Ok(Instr::MovImm { rd, imm });
            }
            Ok(Instr::Alu { op, rd, rn, op2: Operand::Imm(imm), flags: opc == 0b11 })
        }
        // Move wide (immediate)
        0b101 => {
            let hw = bits(w, 22, 21);
            if !wide && hw >= 2 {
                return unsupported();
            }
            let v = (bits(w, 20, 5) as u64) << (16 * hw);
            let size = if wide { Size::B8 } else { Size::B4 };
            let imm = match bits(w, 30, 29) {
                0b00 => size.mask(!v), // MOVN
                0b10 => v,             // MOVZ
                0b11 => return Ok(Instr::Movk { rd: zr(rd, wide), imm: bits(w, 20, 5) as u16, shift: 16 * hw as u8 }),
                _ => return unsupported(),
            };
            Ok(Instr::MovImm { rd: zr(rd, wide), imm })
        }
        // Bitfield: SBFM, BFM, UBFM.
        0b110 => {
            let (immr, imms) = (bits(w, 21, 16), bits(w, 15, 10));
            if bit(w, 22) != wide || (!wide && (immr > 31 || imms > 31)) {
                return unsupported();
            }
            let kind = match bits(w, 30, 29) {
                0b00 => BfKind::Signed,
                0b01 => BfKind::Insert,
                0b10 => BfKind::Unsigned,
                _ => return unsupported(),
            };
            Ok(Instr::bitfield(kind, zr(rd, wide), zr(rn, wide), immr as u8, imms as u8))
        }
        // Extract: EXTR, and its alias ROR (immediate).
        0b111 => {
            let lsb = bits(w, 15, 10);
            if bits(w, 30, 29) != 0 || bit(w, 21) || bit(w, 22) != wide || (!wide && lsb > 31) {
                return unsupported();
            }
            Ok(Instr::Extr { rd: zr(rd, wide), rn: zr(rn, wide), rm: zr(bits(w, 20, 16), wide), lsb: lsb as u8 })
        }
        _ => unsupported(),
    }
}

fn branch_system(w: u32, index: usize) -> Result<Instr> {
    let rel = |off: i64| -> Result<usize> {
        let t = index as i64 + off;
        usize::try_from(t).map_err(|_| "branch target before the start of the code".to_string())
    };
    if bits(w, 30, 26) == 0b00101 {
        let target = rel(sext(bits(w, 25, 0), 26))?;
        return Ok(if bit(w, 31) { Instr::Bl { target } } else { Instr::B { target } });
    }
    if bits(w, 30, 25) == 0b011011 {
        let wide = bit(w, 31);
        return Ok(Instr::Tbz {
            rt: zr(bits(w, 4, 0), wide),
            bit: (bits(w, 31, 31) << 5 | bits(w, 23, 19)) as u8,
            nonzero: bit(w, 24),
            target: rel(sext(bits(w, 18, 5), 14))?,
        });
    }
    // BR, BLR, RET (without pointer authentication).
    if w & 0xff9f_fc1f == 0xd61f_0000 {
        let kind = match bits(w, 22, 21) {
            0b00 => BrKind::Br,
            0b01 => BrKind::Blr,
            0b10 => BrKind::Ret,
            _ => return unsupported(),
        };
        return Ok(Instr::BrReg { rn: zr(bits(w, 9, 5), true), kind });
    }
    if bits(w, 31, 24) == 0x54 && !bit(w, 4) {
        let cond = CONDS[bits(w, 3, 0) as usize];
        return Ok(Instr::BCond { cond, target: rel(sext(bits(w, 23, 5), 19))? });
    }
    if bits(w, 30, 25) == 0b011010 {
        return Ok(Instr::Cbz {
            rt: zr(bits(w, 4, 0), bit(w, 31)),
            nonzero: bit(w, 24),
            target: rel(sext(bits(w, 23, 5), 19))?,
        });
    }
    // Hints: NOP, YIELD, WFE, WFI, SEV, SEVL.
    if w & 0xffff_f01f == 0xd503_201f {
        return if bits(w, 11, 5) <= 5 { Ok(Instr::Nop) } else { unsupported() };
    }
    // Barriers.
    if w & 0xffff_f01f == 0xd503_301f {
        let crm = bits(w, 11, 8);
        return match bits(w, 7, 5) {
            // DSB, DMB. DSB is modeled as DMB. CRm<1:0> = 0 is reserved for
            // DMB and encodes speculation barriers (SSBB, PSSBB) for DSB.
            0b100 | 0b101 => match crm & 3 {
                0b11 => Ok(Instr::Barrier(Barrier::Full)),
                0b10 => Ok(Instr::Barrier(Barrier::St)),
                0b01 => Ok(Instr::Barrier(Barrier::Ld)),
                _ => unsupported(),
            },
            0b110 if crm == 0b1111 => Ok(Instr::Barrier(Barrier::Isb)),
            // CLREX #imm: the immediate is ignored.
            0b010 => Ok(Instr::Clrex),
            _ => unsupported(),
        };
    }
    unsupported()
}

/// The second operand of a shifted-register instruction. ROR is allowed
/// only for logical operations.
fn shifted(w: u32, wide: bool, logical: bool) -> Result<Operand> {
    let amt = bits(w, 15, 10);
    if (!wide && amt >= 32) || (bits(w, 23, 22) == 0b11 && !logical) {
        return unsupported();
    }
    let rm = zr(bits(w, 20, 16), wide);
    let amt = amt as u8;
    let shift = [Extend::Lsl(amt), Extend::Lsr(amt), Extend::Asr(amt), Extend::Ror(amt)][bits(w, 23, 22) as usize];
    Ok(if amt == 0 { Operand::Reg(rm) } else { Operand::RegExt(rm, shift) })
}

fn data_reg(w: u32) -> Result<Instr> {
    let wide = bit(w, 31);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    // Logical (shifted register); N (bit 21) inverts the second operand.
    if bits(w, 28, 24) == 0b01010 {
        let opc = bits(w, 30, 29);
        let op2 = shifted(w, wide, true)?;
        let (rd, rn) = (zr(rd, wide), zr(rn, wide));
        let op = match (opc, bit(w, 21)) {
            (0b00 | 0b11, false) => AluOp::And,
            (0b01, false) => AluOp::Orr,
            (0b10, false) => AluOp::Eor,
            (0b00 | 0b11, true) => AluOp::Bic,
            (0b01, true) => AluOp::Orn,
            _ => AluOp::Eon,
        };
        if let (AluOp::Orr, ZR, Operand::Reg(rm)) = (op, rn.idx, op2) {
            return Ok(Instr::MovReg { rd, rn: rm });
        }
        return Ok(Instr::Alu { op, rd, rn, op2, flags: opc == 0b11 });
    }
    // ADD/SUB (shifted or extended register)
    if bits(w, 28, 24) == 0b01011 {
        let flags = bit(w, 29);
        let op = if bit(w, 30) { AluOp::Sub } else { AluOp::Add };
        if !bit(w, 21) {
            let op2 = shifted(w, wide, false)?;
            return Ok(Instr::Alu { op, rd: zr(rd, wide), rn: zr(rn, wide), op2, flags });
        }
        let amt = bits(w, 12, 10);
        if bits(w, 23, 22) != 0 || amt > 4 {
            return unsupported();
        }
        let option = bits(w, 15, 13);
        let rm = zr(rm, wide && option & 3 == 3);
        let ext = match option {
            0b010 => Extend::Uxtw(amt as u8),
            0b110 => Extend::Sxtw(amt as u8),
            0b011 | 0b111 => Extend::Lsl(amt as u8),
            _ => return unsupported(),
        };
        let rd = if flags { zr(rd, wide) } else { sp(rd, wide) };
        return Ok(Instr::Alu { op, rd, rn: sp(rn, wide), op2: Operand::RegExt(rm, ext), flags });
    }
    // Add/subtract with carry: ADC, ADCS, SBC, SBCS.
    if bits(w, 28, 21) == 0b11010000 && bits(w, 15, 10) == 0 {
        let (rd, rn, rm) = (zr(rd, wide), zr(rn, wide), zr(rm, wide));
        return Ok(Instr::Adc { rd, rn, rm, sub: bit(w, 30), flags: bit(w, 29) });
    }
    // Data-processing (1 source): RBIT, REV16, REV32, REV, CLZ, CLS.
    if bits(w, 30, 16) == 0b101101011000000 {
        let op = match (bits(w, 15, 10), wide) {
            (0b000000, _) => UnaryOp::Rbit,
            (0b000001, _) => UnaryOp::Rev16,
            (0b000010, true) => UnaryOp::Rev32,
            (0b000010, false) | (0b000011, true) => UnaryOp::Rev,
            (0b000100, _) => UnaryOp::Clz,
            (0b000101, _) => UnaryOp::Cls,
            _ => return unsupported(),
        };
        return Ok(Instr::Unary { op, rd: zr(rd, wide), rn: zr(rn, wide) });
    }
    // Data-processing (2 source): UDIV, SDIV, LSLV, LSRV, ASRV, RORV.
    if bits(w, 30, 21) == 0b0011010110 {
        let op = match bits(w, 15, 10) {
            0b000010 => AluOp::Udiv,
            0b000011 => AluOp::Sdiv,
            0b001000 => AluOp::Lsl,
            0b001001 => AluOp::Lsr,
            0b001010 => AluOp::Asr,
            0b001011 => AluOp::Ror,
            _ => return unsupported(),
        };
        return Ok(Instr::Alu { op, rd: zr(rd, wide), rn: zr(rn, wide), op2: Operand::Reg(zr(rm, wide)), flags: false });
    }
    // Data-processing (3 source): MADD, MSUB, the long multiply-adds and
    // the high multiplies.
    if bits(w, 30, 24) == 0b0011011 {
        let (ra, sub) = (bits(w, 14, 10), bit(w, 15));
        let (x, wr) = (|i| zr(i, true), |i| zr(i, false));
        let long = |kind| Ok(Instr::madd(kind, x(rd), wr(rn), wr(rm), x(ra), sub));
        return match (wide, bits(w, 23, 21)) {
            (_, 0b000) => Ok(Instr::madd(MulKind::Plain, zr(rd, wide), zr(rn, wide), zr(rm, wide), zr(ra, wide), sub)),
            (true, 0b001) => long(MulKind::SignedLong),
            (true, 0b101) => long(MulKind::UnsignedLong),
            (true, op @ (0b010 | 0b110)) if !sub => {
                // Ra is should-be-one.
                if ra != 31 {
                    return unpredictable();
                }
                Ok(Instr::Mulh { rd: x(rd), rn: x(rn), rm: x(rm), signed: op == 0b010 })
            }
            _ => unsupported(),
        };
    }
    // Conditional select: CSEL, CSINC, CSINV, CSNEG.
    if bits(w, 29, 21) == 0b011010100 {
        let op2 = bits(w, 11, 10);
        if op2 > 1 {
            return unsupported();
        }
        return Ok(Instr::Csel {
            rd: zr(rd, wide),
            rn: zr(rn, wide),
            rm: zr(rm, wide),
            cond: CONDS[bits(w, 15, 12) as usize],
            inv: bit(w, 30),
            inc: op2 == 1,
        });
    }
    // Conditional compare (register or immediate): CCMN, CCMP.
    if bits(w, 29, 21) == 0b111010010 {
        if bit(w, 10) || bit(w, 4) {
            return unsupported();
        }
        let op2 = if bit(w, 11) { Operand::Imm(rm as u64) } else { Operand::Reg(zr(rm, wide)) };
        return Ok(Instr::Ccmp {
            rn: zr(rn, wide),
            op2,
            nzcv: bits(w, 3, 0) as u8,
            cond: CONDS[bits(w, 15, 12) as usize],
            cmn: !bit(w, 30),
        });
    }
    unsupported()
}

/// Access size for a 2-bit size field.
fn size_field(s: u32) -> Size {
    [Size::B1, Size::B2, Size::B4, Size::B8][s as usize]
}

fn plain_addr(rn: u32) -> Addr {
    Addr { base: sp(rn, true), offset: Offset::Imm(0), writeback: Writeback::None }
}

/// Is base register `rn` (in which 31 is SP) the same as data register
/// `rt` (in which 31 is the zero register)?
fn same_base(rt: u32, rn: u32) -> bool {
    rt == rn && rn != 31
}

fn load_store(w: u32) -> Result<Instr> {
    if bit(w, 26) {
        return Err("SIMD/FP loads and stores are not supported".into());
    }
    let (rt, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    match bits(w, 29, 27) {
        0b001 if bits(w, 25, 24) == 0 => exclusive_ordered(w),
        0b011 if !bit(w, 24) => Err("PC-relative literal loads are not supported".into()),
        0b101 if !bit(w, 25) => pair(w),
        0b111 => {
            let size = bits(w, 31, 30);
            if bits(w, 25, 24) == 0b01 {
                let off = (bits(w, 21, 10) as i64) << size;
                let addr = Addr { base: sp(rn, true), offset: Offset::Imm(off), writeback: Writeback::None };
                return single(w, addr);
            }
            if bits(w, 25, 24) != 0 {
                return unsupported();
            }
            if !bit(w, 21) {
                let imm = sext(bits(w, 20, 12), 9);
                let base = sp(rn, true);
                let addr = match bits(w, 11, 10) {
                    0b00 => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::None },
                    0b01 => Addr { base, offset: Offset::Imm(0), writeback: Writeback::Post(imm) },
                    0b11 => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::Pre },
                    _ => return unsupported(), // LDTR/STTR
                };
                if addr.writeback != Writeback::None && same_base(rt, rn) {
                    return unpredictable();
                }
                return single(w, addr);
            }
            match bits(w, 11, 10) {
                0b10 => {
                    let option = bits(w, 15, 13);
                    let amt = if bit(w, 12) { size as u8 } else { 0 };
                    let ext = match option {
                        0b010 => Extend::Uxtw(amt),
                        0b110 => Extend::Sxtw(amt),
                        0b011 | 0b111 => Extend::Lsl(amt),
                        _ => return unsupported(),
                    };
                    let rm = zr(bits(w, 20, 16), option & 1 == 1);
                    let addr = Addr { base: sp(rn, true), offset: Offset::Reg(rm, ext), writeback: Writeback::None };
                    single(w, addr)
                }
                0b00 => atomic(w),
                _ => unsupported(),
            }
        }
        _ => unsupported(),
    }
}

/// LDR/STR and their sized and signed forms, given the decoded address.
fn single(w: u32, addr: Addr) -> Result<Instr> {
    let size = bits(w, 31, 30);
    let rt = bits(w, 4, 0);
    let sz = size_field(size);
    let load = |wide: bool, signed: bool| Instr::Load {
        rt: zr(rt, wide),
        addr,
        size: sz,
        signed,
        kind: LoadKind::Plain,
        excl: false,
    };
    match bits(w, 23, 22) {
        0b00 => Ok(Instr::Store { rt: zr(rt, size == 3), addr, size: sz, release: false }),
        0b01 => Ok(load(size == 3, false)),
        0b10 if size < 3 => Ok(load(true, true)),
        0b11 if size < 2 => Ok(load(false, true)),
        _ => unsupported(), // PRFM and unallocated
    }
}

/// LSE atomics (including the ST<op> aliases) and LDAPR.
fn atomic(w: u32) -> Result<Instr> {
    let size = bits(w, 31, 30);
    let wide = size == 3;
    let (acquire, release) = (bit(w, 23), bit(w, 22));
    let (rs, rt) = (zr(bits(w, 20, 16), wide), zr(bits(w, 4, 0), wide));
    let addr = plain_addr(bits(w, 9, 5));
    let size = size_field(size);
    let opc = bits(w, 14, 12);
    let op = if !bit(w, 15) {
        [RmwOp::Add, RmwOp::Clr, RmwOp::Eor, RmwOp::Set, RmwOp::Smax, RmwOp::Smin, RmwOp::Umax, RmwOp::Umin]
            [opc as usize]
    } else if opc == 0 {
        RmwOp::Swp
    } else if opc == 0b100 && acquire && !release && rs.idx == ZR {
        return Ok(Instr::Load { rt, addr, size, signed: false, kind: LoadKind::AcquirePC, excl: false });
    } else {
        return unsupported();
    };
    Ok(Instr::Rmw { op, rs, rt, addr, size, acquire, release })
}

/// Exclusives, LDAR/STLR and CAS. Unused register fields are
/// should-be-one; other values are rejected as CONSTRAINED UNPREDICTABLE.
fn exclusive_ordered(w: u32) -> Result<Instr> {
    let size = bits(w, 31, 30);
    let (o2, l, o1, o0) = (bit(w, 23), bit(w, 22), bit(w, 21), bit(w, 15));
    let (rs, rt2, rn, rt) = (bits(w, 20, 16), bits(w, 14, 10), bits(w, 9, 5), bits(w, 4, 0));
    let addr = plain_addr(rn);
    let wide = size == 3;
    let sz = size_field(size);
    match (o2, o1) {
        // LDXR, LDAXR, STXR, STLXR
        (false, false) => {
            if rt2 != 31 {
                return unpredictable();
            }
            if l {
                if rs != 31 {
                    return unpredictable();
                }
                let kind = if o0 { LoadKind::Acquire } else { LoadKind::Plain };
                Ok(Instr::Load { rt: zr(rt, wide), addr, size: sz, signed: false, kind, excl: true })
            } else {
                if rs == rt || same_base(rs, rn) {
                    return unpredictable();
                }
                Ok(Instr::StoreExcl { rs: zr(rs, false), rt: zr(rt, wide), addr, size: sz, release: o0 })
            }
        }
        // LDXP, LDAXP, STXP, STLXP (size 0x here is CASP)
        (false, true) => {
            if size < 2 {
                return unsupported();
            }
            let wide = size == 3;
            let (rt1, rt2) = (zr(rt, wide), zr(rt2, wide));
            if l {
                if rs != 31 {
                    return unpredictable();
                }
                if rt == rt2.idx as u32 {
                    return unpredictable();
                }
                Ok(Instr::LoadExclPair { rt: rt1, rt2, addr, acquire: o0 })
            } else {
                if rs == rt || rs == rt2.idx as u32 || same_base(rs, rn) {
                    return unpredictable();
                }
                Ok(Instr::StoreExclPair { rs: zr(rs, false), rt: rt1, rt2, addr, release: o0 })
            }
        }
        // LDAR, STLR (o0 = 0 is LDLAR/STLLR, from FEAT_LOR)
        (true, false) => {
            if !o0 {
                return unsupported();
            }
            if rs != 31 || rt2 != 31 {
                return unpredictable();
            }
            Ok(if l {
                Instr::Load { rt: zr(rt, wide), addr, size: sz, signed: false, kind: LoadKind::Acquire, excl: false }
            } else {
                Instr::Store { rt: zr(rt, wide), addr, size: sz, release: true }
            })
        }
        // CAS, CASA, CASL, CASAL
        (true, true) => {
            if rt2 != 31 {
                return unpredictable();
            }
            Ok(Instr::Cas { rs: zr(rs, wide), rt: zr(rt, wide), addr, size: sz, acquire: l, release: o0 })
        }
    }
}

/// LDP, STP, LDNP, STNP, LDPSW.
fn pair(w: u32) -> Result<Instr> {
    let opc = bits(w, 31, 30);
    let mode = bits(w, 24, 23);
    let load = bit(w, 22);
    let (rt2, rn, rt) = (bits(w, 14, 10), bits(w, 9, 5), bits(w, 4, 0));
    let (size, wide, signed) = match opc {
        0b00 => (Size::B4, false, false),
        0b01 if load && mode != 0 => (Size::B4, true, true),
        0b10 => (Size::B8, true, false),
        _ => return unsupported(),
    };
    let imm = sext(bits(w, 21, 15), 7) << if opc == 0b10 { 3 } else { 2 };
    let base = sp(rn, true);
    let addr = match mode {
        0b00 | 0b10 => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::None },
        0b01 => Addr { base, offset: Offset::Imm(0), writeback: Writeback::Post(imm) },
        _ => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::Pre },
    };
    if (load && rt == rt2) || (addr.writeback != Writeback::None && (same_base(rt, rn) || same_base(rt2, rn))) {
        return unpredictable();
    }
    let (rt, rt2) = (zr(rt, wide), zr(rt2, wide));
    Ok(if load {
        Instr::LoadPair { rt, rt2, addr, size, signed }
    } else {
        Instr::StorePair { rt, rt2, addr, size }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmask_immediates() {
        // ORR X0, XZR, #0x5555555555555555 etc., checked against llvm-mc.
        assert_eq!(bitmask_imm(false, 0, 0b111100, true), Some(0x5555_5555_5555_5555));
        assert_eq!(bitmask_imm(true, 0, 0, true), Some(1));
        assert_eq!(bitmask_imm(false, 1, 0, false), Some(0x8000_0000));
        assert_eq!(bitmask_imm(true, 0, 0b111111, true), None);
        assert_eq!(bitmask_imm(true, 0, 0, false), None);
    }

    #[test]
    fn pc_relative_addresses() {
        let x = |i| RegOp { idx: i, wide: true };
        // Encodings from llvm-mc: ADRP X1,#0x3000; ADR X2,#-4; ADR XZR,#0.
        let code: Vec<u8> = [0xf000_0001u32, 0x10ff_ffe2, 0x1000_001f].iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(
            decode_program_at(&code, Some(0x1_0ffc)).unwrap(),
            [
                Instr::MovImm { rd: x(1), imm: 0x1_3000 },
                Instr::MovImm { rd: x(2), imm: 0x1_0ffc },
                Instr::MovImm { rd: x(ZR), imm: 0x1_1004 },
            ]
        );
        assert!(decode_program(&code).unwrap_err().contains("load address"));
    }

    #[test]
    fn calls_returns_and_movk() {
        let x = |i| RegOp { idx: i, wide: true };
        // Encodings from llvm-mc.
        let cases = [
            (0x9400_0002u32, Instr::Bl { target: 7 }),
            (0x97ff_ffff, Instr::Bl { target: 4 }),
            (0xd65f_03c0, Instr::BrReg { rn: x(30), kind: BrKind::Ret }),
            (0xd65f_0020, Instr::BrReg { rn: x(1), kind: BrKind::Ret }),
            (0xd61f_0040, Instr::BrReg { rn: x(2), kind: BrKind::Br }),
            (0xd63f_0060, Instr::BrReg { rn: x(3), kind: BrKind::Blr }),
            (0xd61f_03e0, Instr::BrReg { rn: x(ZR), kind: BrKind::Br }),
            (0xf2e2_4680, Instr::Movk { rd: x(0), imm: 0x1234, shift: 48 }),
            (0x72bf_ffe5, Instr::Movk { rd: RegOp { idx: 5, wide: false }, imm: 0xffff, shift: 16 }),
            (0xf280_00e1, Instr::Movk { rd: x(1), imm: 7, shift: 0 }),
        ];
        for (w, want) in cases {
            assert_eq!(decode(w, 5), Ok(want), "{:#x}", w);
        }
        // Unallocated: opc 0011 between RET and ERET; RETAA (pointer
        // authentication); MOVK W with LSL #32.
        for w in [0xd67f_0000u32, 0xd65f_0bff, 0x72c0_0000] {
            assert!(decode(w, 0).is_err(), "{:#x}", w);
        }
    }

    #[test]
    fn integer_instructions() {
        let (x, w) = (|i| RegOp { idx: i, wide: true }, |i| RegOp { idx: i, wide: false });
        // Encodings from llvm-mc.
        let cases = [
            (0xd343_1820u32, Instr::Bitfield { kind: BfKind::Unsigned, rd: x(0), rn: x(1), immr: 3, imms: 6 }),
            (0xb37d_0fe0, Instr::Bitfield { kind: BfKind::Insert, rd: x(0), rn: x(ZR), immr: 61, imms: 3 }),
            (0x937c_fc02, Instr::Alu { op: AluOp::Asr, rd: x(2), rn: x(0), op2: Operand::Imm(60), flags: false }),
            (0xda82_b420, Instr::Csel { rd: x(0), rn: x(1), rm: x(2), cond: Cond::Lt, inv: true, inc: true }),
            (0x5a81_1020, Instr::Csel { rd: w(0), rn: w(1), rm: w(1), cond: Cond::Ne, inv: true, inc: false }),
            (0x9b02_0c20, Instr::Madd { kind: MulKind::Plain, rd: x(0), rn: x(1), rm: x(2), ra: x(3), sub: false }),
            (0x9b2b_b149, Instr::Madd { kind: MulKind::SignedLong, rd: x(9), rn: w(10), rm: w(11), ra: x(12), sub: true }),
            (0x9b42_7c20, Instr::Mulh { rd: x(0), rn: x(1), rm: x(2), signed: true }),
            (0x9ac2_0c20, Instr::Alu { op: AluOp::Sdiv, rd: x(0), rn: x(1), op2: Operand::Reg(x(2)), flags: false }),
            (0x2a21_03e0, Instr::Alu { op: AluOp::Orn, rd: w(0), rn: w(ZR), op2: Operand::Reg(w(1)), flags: false }),
            (0xcac2_3420, Instr::Alu { op: AluOp::Eor, rd: x(0), rn: x(1), op2: Operand::RegExt(x(2), Extend::Ror(13)), flags: false }),
            (0xfa45_1824, Instr::Ccmp { rn: x(1), op2: Operand::Imm(5), nzcv: 4, cond: Cond::Ne, cmn: false }),
            (0x3a42_a02f, Instr::Ccmp { rn: w(1), op2: Operand::Reg(w(2)), nzcv: 15, cond: Cond::Ge, cmn: true }),
        ];
        for (word, want) in cases {
            assert_eq!(decode(word, 0), Ok(want), "{:#x}", word);
        }
        // The assembler gives the same instructions for llvm-mc's text.
        let text = [
            (0x131d_0c20u32, "SBFIZ W0,W1,#3,#4"),
            (0xb344_0c20, "BFI X0,X1,#60,#4"),
            (0x3300_7c62, "BFXIL W2,W3,#0,#32"),
            (0xb37d_0fe0, "BFC X0,#3,#4"),
            (0x5300_1c20, "UXTB W0,W1"),
            (0x9340_3c20, "SXTH X0,W1"),
            (0x5a81_1420, "CNEG W0,W1,EQ"),
            (0xda9f_93e0, "CSETM X0,HI"),
            (0x1b02_8c20, "MSUB W0,W1,W2,W3"),
            (0x9b02_fc20, "MNEG X0,X1,X2"),
            (0x9b02_7c20, "MADD X0,X1,X2,XZR"),
            (0x1ac2_0820, "UDIV W0,W1,W2"),
            (0x8a22_0c20, "BIC X0,X1,X2,LSL #3"),
            (0x6a22_0020, "BICS W0,W1,W2"),
            (0xca22_0020, "EON X0,X1,X2"),
            (0x9bc5_7c83, "UMULH X3,X4,X5"),
            (0x9ba8_7ce6, "UMULL X6,W7,W8"),
            (0x0b82_1c20, "ADD W0,W1,W2,ASR #7"),
            (0xcb42_fc20, "SUB X0,X1,X2,LSR #63"),
            (0x5ac0_0020, "RBIT W0,W1"),
            (0xdac0_0462, "REV16 X2,X3"),
            (0xdac0_08a4, "REV32 X4,X5"),
            (0x5ac0_08e6, "REV W6,W7"),
            (0xdac0_0d28, "REV X8,X9"),
            (0x5ac0_116a, "CLZ W10,W11"),
            (0xdac0_15ac, "CLS X12,X13"),
            (0x93c2_3420, "EXTR X0,X1,X2,#13"),
            (0x1384_1c83, "ROR W3,W4,#7"),
            (0x9ac7_2cc5, "ROR X5,X6,X7"),
            (0x9a02_0020, "ADC X0,X1,X2"),
            (0x3a05_0083, "ADCS W3,W4,W5"),
            (0xda08_00e6, "SBC X6,X7,X8"),
            (0x7a0b_0149, "SBCS W9,W10,W11"),
            (0xda0d_03ec, "NGC X12,X13"),
            (0x7a0f_03ee, "NGCS W14,W15"),
        ];
        for (word, src) in text {
            assert_eq!(decode(word, 0), crate::asm::parse_instr(src, &Default::default()), "{}", src);
        }
        // SMULH with Ra != 31; CCMP with o3 set; UMULL with sf = 0; CSEL
        // with op2 = 10; ADD with ROR; REV64 with sf = 0; EXTR with N != sf;
        // a W EXTR with lsb >= 32; RMIF (not ADC).
        for word in [
            0x9b42_7820u32,
            0xfa45_1834,
            0x1ba8_7ce6,
            0x9a82_0820,
            0x8bc2_3420,
            0x5ac0_0c00,
            0x9382_3420,
            0x1384_9c83,
            0xba00_0420,
        ] {
            assert!(decode(word, 0).is_err(), "{:#x}", word);
        }
    }

    #[test]
    fn code_decodes_lazily() {
        const NOP: u32 = 0xd503_201f;
        let words = [NOP, 0x9400_0001, 0xffff_ffff, 0x1400_0004, NOP, 0x97ff_fffb];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let c = decode_code_at(&code, 0x1000, &[(16, 20)]);
        assert_eq!(c.base, 0x1000);
        assert_eq!(
            c.instrs,
            [Instr::Nop, Instr::Bl { target: 2 }, Instr::Invalid, Instr::Invalid, Instr::Invalid, Instr::Bl { target: 0 }]
        );
        assert!(c.errors[&2].contains("unsupported"));
        assert!(c.errors[&3].contains("branch to 0x101c"), "{}", c.errors[&3]);
        assert_eq!(c.errors[&4], "not code");
        assert_eq!((c.exit(), c.index(0x1014), c.index(0x1018), c.index(0x1019), c.index(0xffc)), (0x1018, Some(5), Some(6), None, None));
    }

    #[test]
    fn branches_are_relative() {
        // b #-8 at index 5, cbz w0, #4 at index 0
        assert_eq!(decode(0x17ff_fffe, 5), Ok(Instr::B { target: 3 }));
        assert_eq!(decode(0x3400_0020, 0).unwrap(), Instr::Cbz { rt: zr(0, false), nonzero: false, target: 1 });
        assert!(decode(0x17ff_fffe, 1).is_err());
        assert!(decode_program(&0x1400_0002u32.to_le_bytes()).is_err());
        assert!(decode_program(&0x1400_0001u32.to_le_bytes()).is_ok());
    }

    #[test]
    fn stack_pointer() {
        let x = |i| RegOp { idx: i, wide: true };
        let sp = x(SP);
        // Encodings from llvm-mc.
        let cases = [
            // ldr x0, [sp]
            (0xf940_03e0u32, Instr::Load { rt: x(0), addr: Addr { base: sp, offset: Offset::Imm(0), writeback: Writeback::None }, size: Size::B8, signed: false, kind: LoadKind::Plain, excl: false }),
            // add sp, sp, #16
            (0x9100_43ff, Instr::Alu { op: AluOp::Add, rd: sp, rn: sp, op2: Operand::Imm(16), flags: false }),
            // stp x29, x30, [sp, #-16]!
            (0xa9bf_7bfd, Instr::StorePair { rt: x(29), rt2: x(30), addr: Addr { base: sp, offset: Offset::Imm(-16), writeback: Writeback::Pre }, size: Size::B8 }),
            // mov x29, sp
            (0x9100_03fd, Instr::Alu { op: AluOp::Add, rd: x(29), rn: sp, op2: Operand::Imm(0), flags: false }),
            // cmp sp, #16 (the destination is XZR)
            (0xf100_43ff, Instr::Alu { op: AluOp::Sub, rd: x(ZR), rn: sp, op2: Operand::Imm(16), flags: true }),
            // ldr xzr, [sp], #8: XZR and SP are different registers
            (0xf840_87ff, Instr::Load { rt: x(ZR), addr: Addr { base: sp, offset: Offset::Imm(0), writeback: Writeback::Post(8) }, size: Size::B8, signed: false, kind: LoadKind::Plain, excl: false }),
        ];
        for (w, want) in cases {
            assert_eq!(decode(w, 0), Ok(want), "{:#x}", w);
        }
        // ldr x0, [x0], #8 is UNPREDICTABLE.
        assert!(decode(0xf840_8400, 0).unwrap_err().contains("UNPREDICTABLE"));
    }
}
