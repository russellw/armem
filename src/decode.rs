//! Decoder for A64 machine code.
//!
//! Produces the same `isa::Instr` as the assembly parser in `asm.rs`, so
//! compiled code runs under the same models as litmus tests. It covers
//! exactly the instructions the emulator supports. Any other encoding,
//! including an unallocated or UNPREDICTABLE one, is an error rather
//! than a guess.
//!
//! Where the architecture uses register number 31 as the stack pointer
//! (base registers, and the destination and first source of `ADD`/`SUB`
//! immediate), decoding fails, because the emulator does not model SP yet.
//!
//! Some encodings decode to the same `Instr` as a different but equivalent
//! source form: `ORR Xd, XZR, ...` becomes `MovImm` or `MovReg`, and `UXTX`
//! or `SXTX` extends become `LSL`. The hints `YIELD`, `WFE`, `WFI`, `SEV`
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
fn sp(i: u32, wide: bool) -> Result<RegOp> {
    if i == 31 { Err("uses SP, which is not modeled yet".into()) } else { Ok(zr(i, wide)) }
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
    decode_inner(word, index).map_err(|e| format!("{:#010x}: {}", word, e))
}

/// Decode little-endian A64 code. Branches must stay within the code
/// (a branch to just past the end is allowed: it ends the thread).
pub fn decode_program(code: &[u8]) -> Result<Vec<Instr>> {
    if code.len() % 4 != 0 {
        return Err(format!("code size {} is not a multiple of 4", code.len()));
    }
    let prog: Vec<Instr> = code
        .chunks_exact(4)
        .enumerate()
        .map(|(i, c)| decode(u32::from_le_bytes(c.try_into().unwrap()), i))
        .collect::<Result<_>>()?;
    for (i, ins) in prog.iter().enumerate() {
        if let Instr::B { target } | Instr::BCond { target, .. } | Instr::Cbz { target, .. } = *ins {
            if target > prog.len() {
                return Err(format!("instruction {}: branch target {} is outside the code", i, target));
            }
        }
    }
    Ok(prog)
}

fn decode_inner(w: u32, index: usize) -> Result<Instr> {
    let op0 = bits(w, 28, 25);
    match op0 {
        0b1000 | 0b1001 => data_imm(w),
        0b1010 | 0b1011 => branch_system(w, index),
        0b0101 | 0b1101 => data_reg(w),
        _ if op0 & 0b0101 == 0b0100 => load_store(w),
        _ => unsupported(),
    }
}

fn data_imm(w: u32) -> Result<Instr> {
    let wide = bit(w, 31);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    match bits(w, 25, 23) {
        // ADD/SUB (immediate)
        0b010 => {
            let flags = bit(w, 29);
            let imm = (bits(w, 21, 10) as u64) << if bit(w, 22) { 12 } else { 0 };
            let rd = if flags { zr(rd, wide) } else { sp(rd, wide)? };
            let op = if bit(w, 30) { AluOp::Sub } else { AluOp::Add };
            Ok(Instr::Alu { op, rd, rn: sp(rn, wide)?, op2: Operand::Imm(imm), flags })
        }
        // Logical (immediate)
        0b100 => {
            let opc = bits(w, 30, 29);
            let Some(imm) = bitmask_imm(bit(w, 22), bits(w, 21, 16), bits(w, 15, 10), wide) else {
                return unsupported();
            };
            let rd = if opc == 0b11 { zr(rd, wide) } else { sp(rd, wide)? };
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
                _ => return unsupported(),
            };
            Ok(Instr::MovImm { rd: zr(rd, wide), imm })
        }
        // Bitfield: only the LSL/LSR/ASR #imm and SXTW aliases.
        0b110 => {
            if bit(w, 22) != wide {
                return unsupported();
            }
            let (immr, imms) = (bits(w, 21, 16), bits(w, 15, 10));
            let top = if wide { 63 } else { 31 };
            if !wide && (immr > 31 || imms > 31) {
                return unsupported();
            }
            let (rd, rn) = (zr(rd, wide), zr(rn, wide));
            let shift = |op: AluOp, amt: u32| Instr::Alu { op, rd, rn, op2: Operand::Imm(amt as u64), flags: false };
            match bits(w, 30, 29) {
                0b10 if imms == top => Ok(shift(AluOp::Lsr, immr)),
                0b10 if imms + 1 == immr => Ok(shift(AluOp::Lsl, top - imms)),
                0b00 if imms == top => Ok(shift(AluOp::Asr, immr)),
                0b00 if wide && immr == 0 && imms == 31 => Ok(Instr::Alu {
                    op: AluOp::Orr,
                    rd,
                    rn: zr(31, true),
                    op2: Operand::RegExt(RegOp { idx: rn.idx, wide: false }, Extend::Sxtw(0)),
                    flags: false,
                }),
                _ => unsupported(),
            }
        }
        _ => unsupported(),
    }
}

fn branch_system(w: u32, index: usize) -> Result<Instr> {
    let rel = |off: i64| -> Result<usize> {
        let t = index as i64 + off;
        usize::try_from(t).map_err(|_| "branch target before the start of the code".to_string())
    };
    if bits(w, 31, 26) == 0b000101 {
        return Ok(Instr::B { target: rel(sext(bits(w, 25, 0), 26))? });
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
            _ => unsupported(),
        };
    }
    unsupported()
}

/// The second operand of a shifted-register instruction (LSL only).
fn shifted(w: u32, wide: bool) -> Result<Operand> {
    let amt = bits(w, 15, 10);
    if bits(w, 23, 22) != 0 || (!wide && amt >= 32) {
        return unsupported();
    }
    let rm = zr(bits(w, 20, 16), wide);
    Ok(if amt == 0 { Operand::Reg(rm) } else { Operand::RegExt(rm, Extend::Lsl(amt as u8)) })
}

fn data_reg(w: u32) -> Result<Instr> {
    let wide = bit(w, 31);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    // Logical (shifted register), without the inverting forms (BIC, ORN, ...).
    if bits(w, 28, 24) == 0b01010 {
        if bit(w, 21) {
            return unsupported();
        }
        let opc = bits(w, 30, 29);
        let op2 = shifted(w, wide)?;
        let (rd, rn) = (zr(rd, wide), zr(rn, wide));
        let op = [AluOp::And, AluOp::Orr, AluOp::Eor, AluOp::And][opc as usize];
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
            let op2 = shifted(w, wide)?;
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
        let rd = if flags { zr(rd, wide) } else { sp(rd, wide)? };
        return Ok(Instr::Alu { op, rd, rn: sp(rn, wide)?, op2: Operand::RegExt(rm, ext), flags });
    }
    // Data-processing (2 source): LSLV, LSRV, ASRV.
    if bits(w, 30, 21) == 0b0011010110 {
        let op = match bits(w, 15, 10) {
            0b001000 => AluOp::Lsl,
            0b001001 => AluOp::Lsr,
            0b001010 => AluOp::Asr,
            _ => return unsupported(),
        };
        return Ok(Instr::Alu { op, rd: zr(rd, wide), rn: zr(rn, wide), op2: Operand::Reg(zr(rm, wide)), flags: false });
    }
    // Data-processing (3 source): only MADD with XZR, i.e. MUL.
    if bits(w, 30, 24) == 0b0011011 {
        if bits(w, 23, 21) != 0 || bit(w, 15) || bits(w, 14, 10) != 31 {
            return unsupported();
        }
        return Ok(Instr::Alu {
            op: AluOp::Mul,
            rd: zr(rd, wide),
            rn: zr(rn, wide),
            op2: Operand::Reg(zr(rm, wide)),
            flags: false,
        });
    }
    // Conditional select: CSEL, CSINC.
    if bits(w, 29, 21) == 0b011010100 {
        let op2 = bits(w, 11, 10);
        if bit(w, 30) || op2 > 1 {
            return unsupported();
        }
        return Ok(Instr::Csel {
            rd: zr(rd, wide),
            rn: zr(rn, wide),
            rm: zr(rm, wide),
            cond: CONDS[bits(w, 15, 12) as usize],
            inc: op2 == 1,
        });
    }
    unsupported()
}

/// Access size for a 2-bit size field.
fn size_field(s: u32) -> Size {
    [Size::B1, Size::B2, Size::B4, Size::B8][s as usize]
}

fn plain_addr(rn: u32) -> Result<Addr> {
    Ok(Addr { base: sp(rn, true)?, offset: Offset::Imm(0), writeback: Writeback::None })
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
                let addr = Addr { base: sp(rn, true)?, offset: Offset::Imm(off), writeback: Writeback::None };
                return single(w, addr);
            }
            if bits(w, 25, 24) != 0 {
                return unsupported();
            }
            if !bit(w, 21) {
                let imm = sext(bits(w, 20, 12), 9);
                let base = sp(rn, true)?;
                let addr = match bits(w, 11, 10) {
                    0b00 => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::None },
                    0b01 => Addr { base, offset: Offset::Imm(0), writeback: Writeback::Post(imm) },
                    0b11 => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::Pre },
                    _ => return unsupported(), // LDTR/STTR
                };
                if addr.writeback != Writeback::None && rt == rn {
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
                    let addr = Addr { base: sp(rn, true)?, offset: Offset::Reg(rm, ext), writeback: Writeback::None };
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
    let addr = plain_addr(bits(w, 9, 5))?;
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
    let addr = plain_addr(rn)?;
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
                if rs == rt || rs == rn {
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
                if rs == rt || rs == rt2.idx as u32 || rs == rn {
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
    let base = sp(rn, true)?;
    let addr = match mode {
        0b00 | 0b10 => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::None },
        0b01 => Addr { base, offset: Offset::Imm(0), writeback: Writeback::Post(imm) },
        _ => Addr { base, offset: Offset::Imm(imm), writeback: Writeback::Pre },
    };
    if (load && rt == rt2) || (addr.writeback != Writeback::None && (rt == rn || rt2 == rn)) {
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
    fn branches_are_relative() {
        // b #-8 at index 5, cbz w0, #4 at index 0
        assert_eq!(decode(0x17ff_fffe, 5), Ok(Instr::B { target: 3 }));
        assert_eq!(decode(0x3400_0020, 0).unwrap(), Instr::Cbz { rt: zr(0, false), nonzero: false, target: 1 });
        assert!(decode(0x17ff_fffe, 1).is_err());
        assert!(decode_program(&0x1400_0002u32.to_le_bytes()).is_err());
        assert!(decode_program(&0x1400_0001u32.to_le_bytes()).is_ok());
    }

    #[test]
    fn rejects_sp() {
        // ldr x0, [sp]; add sp, sp, #16; stp x29, x30, [sp, #-16]!
        for w in [0xf940_03e0u32, 0x9100_43ff, 0xa9bf_7bfd] {
            assert!(decode(w, 0).unwrap_err().contains("SP"), "{:#x}", w);
        }
    }
}
