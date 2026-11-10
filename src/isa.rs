//! Instruction set representation.
//!
//! This is the decoded form of AArch64 instructions that the emulator
//! executes. It is produced by the assembly parser in `asm.rs` and by the
//! machine-code decoder in `decode.rs`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// A general-purpose register operand. `idx` 0..=30 are X0..X30,
/// 31 is the zero register (XZR/WZR) and 32 the stack pointer (SP/WSP).
/// `wide` selects X (64-bit) vs W (32-bit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegOp {
    pub idx: u8,
    pub wide: bool,
}

pub const ZR: u8 = 31;
pub const SP: u8 = 32;

impl fmt::Display for RegOp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let p = if self.wide { 'X' } else { 'W' };
        if self.idx == SP {
            write!(f, "{}", if self.wide { "SP" } else { "WSP" })
        } else if self.idx == ZR {
            write!(f, "{}ZR", p)
        } else {
            write!(f, "{}{}", p, self.idx)
        }
    }
}

/// Access size in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Size {
    B1,
    B2,
    B4,
    B8,
    /// A Q register, or the footprint of an X-register exclusive pair.
    B16,
}

impl Size {
    pub fn bytes(self) -> u64 {
        match self {
            Size::B1 => 1,
            Size::B2 => 2,
            Size::B4 => 4,
            Size::B8 => 8,
            Size::B16 => 16,
        }
    }
    pub fn from_bytes(n: u64) -> Option<Size> {
        Some(match n {
            1 => Size::B1,
            2 => Size::B2,
            4 => Size::B4,
            8 => Size::B8,
            16 => Size::B16,
            _ => return None,
        })
    }
    pub fn mask(self, v: u64) -> u64 {
        match self {
            Size::B8 | Size::B16 => v,
            s => v & ((1u64 << (8 * s.bytes())) - 1),
        }
    }
    /// Zero- or sign-extend a `self`-sized value to 64 bits.
    pub fn extend(self, v: u64, signed: bool) -> u64 {
        let v = self.mask(v);
        if !signed || self >= Size::B8 {
            return v;
        }
        let shift = 64 - 8 * self.bytes();
        (((v << shift) as i64) >> shift) as u64
    }
    /// Size of a register operand (W = 4 bytes, X = 8 bytes).
    pub fn of(r: RegOp) -> Size {
        if r.wide { Size::B8 } else { Size::B4 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Extend {
    Lsl(u8),
    Uxtw(u8),
    Sxtw(u8),
    /// Extends of an extended register operand of ADD/SUB only.
    Uxtb(u8),
    Uxth(u8),
    Sxtb(u8),
    Sxth(u8),
    /// Shifts of a shifted register operand only (ROR only for logical
    /// operations), within the operation's size.
    Lsr(u8),
    Asr(u8),
    Ror(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Offset {
    Imm(i64),
    Reg(RegOp, Extend),
}

/// Base register writeback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Writeback {
    /// `[Xn, off]`: no writeback.
    None,
    /// `[Xn, #imm]!`: access at Xn+imm, then Xn := Xn+imm.
    Pre,
    /// `[Xn], #imm`: access at Xn, then Xn := Xn+imm.
    Post(i64),
}

/// Addressing mode `[base, offset]`, possibly with writeback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Addr {
    pub base: RegOp,
    pub offset: Offset,
    pub writeback: Writeback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operand {
    Reg(RegOp),
    Imm(u64),
    /// Extended or shifted register, e.g. `W4, SXTW` or `X2, LSL #3`.
    RegExt(RegOp, Extend),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AluOp {
    Add,
    Sub,
    And,
    Orr,
    Eor,
    /// AND, ORR and EOR with the second operand inverted (BIC, ORN, EON).
    Bic,
    Orn,
    Eon,
    Lsl,
    Lsr,
    Asr,
    /// Rotate right (RORV); ROR by an immediate is `Instr::Extr`.
    Ror,
    Mul,
    /// Division rounds toward zero; dividing by zero gives 0.
    Udiv,
    Sdiv,
}

/// Bitfield moves: UBFM, SBFM and BFM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BfKind {
    /// UBFM: the other bits of the result are zero.
    Unsigned,
    /// SBFM: the bits above the field are copies of its top bit, those
    /// below it zero.
    Signed,
    /// BFM: the other bits of the result are those of the destination.
    Insert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cond {
    Eq,
    Ne,
    Cs,
    Cc,
    Mi,
    Pl,
    Vs,
    Vc,
    Hi,
    Ls,
    Ge,
    Lt,
    Gt,
    Le,
    Al,
}

/// Ordering strength of a load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoadKind {
    Plain,
    /// LDAPR: RCpc acquire.
    AcquirePC,
    /// LDAR: RCsc acquire.
    Acquire,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Barrier {
    /// DMB SY / ISH / OSH / NSH: orders everything.
    Full,
    /// DMB LD / ISHLD ...: orders loads before against loads and stores after.
    Ld,
    /// DMB ST / ISHST ...: orders stores against stores.
    St,
    /// ISB
    Isb,
}

/// Read-modify-write operations (ARMv8.1 LSE atomics).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RmwOp {
    Add,
    Clr,
    Eor,
    Set,
    Smax,
    Smin,
    Umax,
    Umin,
    Swp,
}

/// Data-processing (1 source) operations, on the whole register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// RBIT: reverse the bits.
    Rbit,
    /// REV16: reverse the bytes in each halfword.
    Rev16,
    /// REV32 (X registers only): reverse the bytes in each word.
    Rev32,
    /// REV (REV64 for X registers): reverse all the bytes.
    Rev,
    /// CLZ: count leading zero bits.
    Clz,
    /// CLS: count the leading bits below the top bit that equal it.
    Cls,
}

/// A SIMD&FP register, V0..V31. Its scalar names B, H, S, D and Q refer
/// to its low 1, 2, 4, 8 and 16 bytes.
pub type VReg = u8;

/// Multiply-add kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MulKind {
    /// MADD / MSUB: all registers are the same size.
    Plain,
    /// UMADDL / UMSUBL, SMADDL / SMSUBL: W registers rn and rm are zero-
    /// (sign-) extended, and their 64-bit product added to X register ra.
    UnsignedLong,
    SignedLong,
}

/// Kinds of branch to a register. They differ only in that `BLR` writes
/// the return address to X30 (`RET` merely hints that it is a return).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BrKind {
    Br,
    Blr,
    Ret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Instr {
    Nop,
    /// An instruction that could not be decoded; executing it is a fault
    /// (the reason is in `Code::errors`).
    Invalid,
    /// MOV/MOVZ rd, #imm
    MovImm { rd: RegOp, imm: u64 },
    /// MOVK rd, #imm, LSL #shift: replace 16 bits of rd.
    Movk { rd: RegOp, imm: u16, shift: u8 },
    /// MOV rd, rn
    MovReg { rd: RegOp, rn: RegOp },
    /// ADD/SUB/AND/...; `flags` for ADDS/SUBS/ANDS (and CMP/CMN/TST).
    Alu { op: AluOp, rd: RegOp, rn: RegOp, op2: Operand, flags: bool },
    /// CSEL rd, rn, rm, cond: rd := cond ? rn : rm', where rm' is rm, or
    /// for CSINC rm + 1 (`inc`), CSINV !rm (`inv`) and CSNEG !rm + 1
    /// (both).
    Csel { rd: RegOp, rn: RegOp, rm: RegOp, cond: Cond, inv: bool, inc: bool },
    /// UBFM / SBFM / BFM rd, rn, #immr, #imms, and their aliases (UBFX,
    /// SBFIZ, BFI, UXTB, ...). If imms >= immr, bits imms..immr of rn go to
    /// the bottom of rd; otherwise the low imms + 1 bits of rn go to bit
    /// (datasize - immr). The aliases LSL, LSR, ASR (immediate) and SXTW
    /// are `Alu` instead (see `Instr::bitfield`).
    Bitfield { kind: BfKind, rd: RegOp, rn: RegOp, immr: u8, imms: u8 },
    /// MADD / MSUB rd, rn, rm, ra: rd := ra + rn * rm (ra - rn * rm when
    /// `sub`), and the long forms (`kind`). MADD with ra = the zero
    /// register is `Alu` MUL instead (see `Instr::madd`).
    Madd { kind: MulKind, rd: RegOp, rn: RegOp, rm: RegOp, ra: RegOp, sub: bool },
    /// RBIT / REV16 / REV32 / REV / CLZ / CLS rd, rn.
    Unary { op: UnaryOp, rd: RegOp, rn: RegOp },
    /// EXTR rd, rn, rm, #lsb: bits lsb + datasize - 1 .. lsb of the
    /// concatenation rn:rm. ROR (immediate) is EXTR with rn = rm.
    Extr { rd: RegOp, rn: RegOp, rm: RegOp, lsb: u8 },
    /// ADC / SBC rd, rn, rm: rd := rn + rm + C (rn + !rm + C when `sub`),
    /// where C is the carry flag; `flags` for ADCS / SBCS. NGC(S) rd, rm
    /// is SBC(S) rd, ZR, rm.
    Adc { rd: RegOp, rn: RegOp, rm: RegOp, sub: bool, flags: bool },
    /// UMULH / SMULH rd, rn, rm: the high 64 bits of the 128-bit product.
    Mulh { rd: RegOp, rn: RegOp, rm: RegOp, signed: bool },
    /// CCMP / CCMN rn, op2, #nzcv, cond: if cond holds, set the flags as
    /// CMP (CMN when `cmn`) rn, op2 would; otherwise set them to `nzcv`.
    /// `op2` is a register or an immediate 0..=31.
    Ccmp { rn: RegOp, op2: Operand, nzcv: u8, cond: Cond, cmn: bool },
    /// B label
    B { target: usize },
    /// BL label: X30 := the address of the next instruction.
    Bl { target: usize },
    /// BR / BLR / RET to the address in `rn`.
    BrReg { rn: RegOp, kind: BrKind },
    /// B.cond label
    BCond { cond: Cond, target: usize },
    /// CBZ / CBNZ
    Cbz { rt: RegOp, nonzero: bool, target: usize },
    /// TBZ / TBNZ: branch if bit `bit` of rt is zero (nonzero).
    Tbz { rt: RegOp, bit: u8, nonzero: bool, target: usize },
    /// LDR{B,H,SB,SH,SW} / LDUR* / LDAR* / LDAPR* / LDXR* / LDAXR*.
    /// `signed` sign-extends the loaded value into `rt`.
    Load { rt: RegOp, addr: Addr, size: Size, signed: bool, kind: LoadKind, excl: bool },
    /// STR{B,H} / STUR* / STLR*
    Store { rt: RegOp, addr: Addr, size: Size, release: bool },
    /// STXR* / STLXR*
    StoreExcl { rs: RegOp, rt: RegOp, addr: Addr, size: Size, release: bool },
    /// LDXP / LDAXP: exclusive load of a register pair.
    LoadExclPair { rt: RegOp, rt2: RegOp, addr: Addr, acquire: bool },
    /// STXP / STLXP: exclusive store of a register pair; status in `rs`.
    StoreExclPair { rs: RegOp, rt: RegOp, rt2: RegOp, addr: Addr, release: bool },
    /// LDP / LDNP / LDPSW: two independent loads from consecutive addresses.
    LoadPair { rt: RegOp, rt2: RegOp, addr: Addr, size: Size, signed: bool },
    /// STP / STNP: two independent stores to consecutive addresses.
    StorePair { rt: RegOp, rt2: RegOp, addr: Addr, size: Size },
    /// LDADD / SWP / ... (rs = operand, rt = destination of old value)
    Rmw { op: RmwOp, rs: RegOp, rt: RegOp, addr: Addr, size: Size, acquire: bool, release: bool },
    /// CAS rs, rt, [addr]: compare with rs, store rt, old value into rs.
    Cas { rs: RegOp, rt: RegOp, addr: Addr, size: Size, acquire: bool, release: bool },
    Barrier(Barrier),
    /// CLREX: clear the exclusive monitor.
    Clrex,
    /// LDR / LDUR Bt, Ht, St, Dt or Qt, or with `vt2` LDP / LDNP St, Dt or
    /// Qt. Each register is zeroed above the bytes loaded into it.
    VLoad { vt: VReg, vt2: Option<VReg>, addr: Addr, size: Size },
    /// STR / STUR, or with `vt2` STP / STNP, of SIMD&FP registers.
    VStore { vt: VReg, vt2: Option<VReg>, addr: Addr, size: Size },
    /// MOVI / MVNI vd, with the immediate expanded to the register's 128
    /// bits (the upper 64 zero for the 64-bit forms).
    VMovImm { vd: VReg, imm: u128 },
    /// FMOV Sd, Sn / Dd, Dn: the low `size` bytes of vn, the rest zero.
    VMov { vd: VReg, vn: VReg, size: Size },
    /// AND / BIC / ORR / ORN / EOR Vd.T, Vn.T, Vm.T, T = 16B if `q`, else
    /// 8B (the upper 64 bits are then zeroed). MOV Vd.T, Vn.T is ORR with
    /// vm = vn.
    VLogic { op: AluOp, vd: VReg, vn: VReg, vm: VReg, q: bool },
    /// INS Vd.Ts[index], Rn (MOV), and FMOV Sd, Wn / Dd, Xn / Vd.D[1], Xn:
    /// the low `size` bytes of rn go to element `index` of vd. The other
    /// elements are kept, or zeroed if `zero` (FMOV Sd / Dd).
    VIns { vd: VReg, index: u8, size: Size, rn: RegOp, zero: bool },
    /// INS Vd.Ts[index], Vn.Ts[index2] (MOV): copy one element.
    VInsElem { vd: VReg, index: u8, vn: VReg, index2: u8, size: Size },
    /// DUP Vd.T, Vn.Ts[index] (`lanes` = 8 or 16 bytes' worth of
    /// elements), and DUP (MOV) Bd/Hd/Sd/Dd, Vn.Ts[index] (`lanes` = 1):
    /// copies of element `index` of vn fill the low `lanes` elements of vd,
    /// the rest zeroed.
    VDupElem { vd: VReg, vn: VReg, index: u8, size: Size, lanes: u8 },
    /// DUP Vd.T, Rn: copies of the low `size` bytes of rn fill the low 16
    /// bytes of vd if `q`, else the low 8 (the rest zeroed).
    VDup { vd: VReg, size: Size, q: bool, rn: RegOp },
    /// UMOV / SMOV Rd, Vn.Ts[index] (MOV for S and D elements), and FMOV
    /// Wd, Sn / Xd, Dn / Xd, Vn.D[1]: element `index` of vn, zero- or
    /// (`signed`) sign-extended.
    VMovToGp { rd: RegOp, vn: VReg, index: u8, size: Size, signed: bool },
    /// CNT Vd.T, Vn.T: the number of bits set in each byte (T = 16B if
    /// `q`, else 8B).
    VCnt { vd: VReg, vn: VReg, q: bool },
    /// ADDV Bd, Vn.T, or UADDLV Hd, Vn.T if `long`: the sum of the bytes of
    /// vn (T = 16B if `q`, else 8B), as a byte (halfword if `long`).
    VAddv { vd: VReg, vn: VReg, q: bool, long: bool },
}

impl Cond {
    /// Evaluate against NZCV bits (N=8, Z=4, C=2, V=1).
    pub fn holds(self, nzcv: u64) -> bool {
        let n = nzcv & 8 != 0;
        let z = nzcv & 4 != 0;
        let c = nzcv & 2 != 0;
        let v = nzcv & 1 != 0;
        match self {
            Cond::Eq => z,
            Cond::Ne => !z,
            Cond::Cs => c,
            Cond::Cc => !c,
            Cond::Mi => n,
            Cond::Pl => !n,
            Cond::Vs => v,
            Cond::Vc => !v,
            Cond::Hi => c && !z,
            Cond::Ls => !(c && !z),
            Cond::Ge => n == v,
            Cond::Lt => n != v,
            Cond::Gt => !z && n == v,
            Cond::Le => !(!z && n == v),
            Cond::Al => true,
        }
    }
}

impl RmwOp {
    pub fn apply(self, old: u64, operand: u64, size: Size) -> u64 {
        let (sold, sop) = (size.extend(old, true) as i64, size.extend(operand, true) as i64);
        let r = match self {
            RmwOp::Add => old.wrapping_add(operand),
            RmwOp::Clr => old & !operand,
            RmwOp::Eor => old ^ operand,
            RmwOp::Set => old | operand,
            RmwOp::Smax => sold.max(sop) as u64,
            RmwOp::Smin => sold.min(sop) as u64,
            RmwOp::Umax => size.mask(old).max(size.mask(operand)),
            RmwOp::Umin => size.mask(old).min(size.mask(operand)),
            RmwOp::Swp => operand,
        };
        size.mask(r)
    }
}

impl Instr {
    /// UBFM / SBFM / BFM rd, rn, #immr, #imms, as the `Alu` instruction of
    /// its LSL, LSR, ASR or SXTW alias if it has one. `immr` and `imms`
    /// must be less than the register size. The assembler and the decoder
    /// both use this, so they agree.
    pub fn bitfield(kind: BfKind, rd: RegOp, rn: RegOp, immr: u8, imms: u8) -> Instr {
        let top = 8 * Size::of(rd).bytes() as u8 - 1;
        let shift = |op: AluOp, amt: u8| Instr::Alu { op, rd, rn, op2: Operand::Imm(amt as u64), flags: false };
        match kind {
            BfKind::Unsigned if imms == top => shift(AluOp::Lsr, immr),
            BfKind::Unsigned if imms + 1 == immr => shift(AluOp::Lsl, top - imms),
            BfKind::Signed if imms == top => shift(AluOp::Asr, immr),
            BfKind::Signed if rd.wide && immr == 0 && imms == 31 => Instr::Alu {
                op: AluOp::Orr,
                rd,
                rn: RegOp { idx: ZR, wide: true },
                op2: Operand::RegExt(RegOp { idx: rn.idx, wide: false }, Extend::Sxtw(0)),
                flags: false,
            },
            _ => Instr::Bitfield { kind, rd, rn, immr, imms },
        }
    }

    /// MADD / MSUB rd, rn, rm, ra (or a long form), as `Alu` MUL if it is
    /// one.
    pub fn madd(kind: MulKind, rd: RegOp, rn: RegOp, rm: RegOp, ra: RegOp, sub: bool) -> Instr {
        if kind == MulKind::Plain && ra.idx == ZR && !sub {
            Instr::Alu { op: AluOp::Mul, rd, rn, op2: Operand::Reg(rm), flags: false }
        } else {
            Instr::Madd { kind, rd, rn, rm, ra, sub }
        }
    }

    /// The general-purpose registers (0..=30, or `SP`) the instruction
    /// reads, including address registers and destinations it only
    /// partly replaces (`MOVK`, `BFM`). A register branch (`BR`, `BLR`,
    /// `RET`) reads only its target here, though where it goes, any
    /// register may be read.
    pub fn reads(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut reg = |r: RegOp| {
            if r.idx != ZR {
                out.push(r.idx)
            }
        };
        let op = |o: Operand, reg: &mut dyn FnMut(RegOp)| match o {
            Operand::Reg(r) | Operand::RegExt(r, _) => reg(r),
            Operand::Imm(_) => {}
        };
        let mut addr = None;
        match *self {
            Instr::Nop
            | Instr::Invalid
            | Instr::MovImm { .. }
            | Instr::B { .. }
            | Instr::Bl { .. }
            | Instr::BCond { .. }
            | Instr::Barrier(_)
            | Instr::Clrex
            | Instr::VMovImm { .. }
            | Instr::VMov { .. }
            | Instr::VLogic { .. }
            | Instr::VInsElem { .. }
            | Instr::VDupElem { .. }
            | Instr::VMovToGp { .. }
            | Instr::VCnt { .. }
            | Instr::VAddv { .. } => {}
            Instr::Movk { rd, .. } => reg(rd),
            Instr::MovReg { rn, .. } | Instr::Unary { rn, .. } => reg(rn),
            Instr::Alu { rn, op2, .. } | Instr::Ccmp { rn, op2, .. } => {
                reg(rn);
                op(op2, &mut reg);
            }
            Instr::Csel { rn, rm, .. }
            | Instr::Extr { rn, rm, .. }
            | Instr::Adc { rn, rm, .. }
            | Instr::Mulh { rn, rm, .. } => {
                reg(rn);
                reg(rm);
            }
            Instr::Bitfield { kind, rd, rn, .. } => {
                reg(rn);
                if kind == BfKind::Insert {
                    reg(rd);
                }
            }
            Instr::Madd { rn, rm, ra, .. } => {
                reg(rn);
                reg(rm);
                reg(ra);
            }
            Instr::BrReg { rn, .. } => reg(rn),
            Instr::Cbz { rt, .. } | Instr::Tbz { rt, .. } => reg(rt),
            Instr::Load { addr: a, .. }
            | Instr::LoadExclPair { addr: a, .. }
            | Instr::LoadPair { addr: a, .. }
            | Instr::VLoad { addr: a, .. }
            | Instr::VStore { addr: a, .. } => addr = Some(a),
            Instr::Store { rt, addr: a, .. } | Instr::StoreExcl { rt, addr: a, .. } => {
                reg(rt);
                addr = Some(a);
            }
            Instr::StoreExclPair { rt, rt2, addr: a, .. } | Instr::StorePair { rt, rt2, addr: a, .. } => {
                reg(rt);
                reg(rt2);
                addr = Some(a);
            }
            Instr::Rmw { rs, addr: a, .. } => {
                reg(rs);
                addr = Some(a);
            }
            Instr::Cas { rs, rt, addr: a, .. } => {
                reg(rs);
                reg(rt);
                addr = Some(a);
            }
            Instr::VIns { rn, .. } | Instr::VDup { rn, .. } => reg(rn),
        }
        if let Some(a) = addr {
            reg(a.base);
            if let Offset::Reg(r, _) = a.offset {
                reg(r);
            }
        }
        out
    }

    /// Whether the instruction reads the condition flags.
    pub fn reads_flags(&self) -> bool {
        matches!(self, Instr::Csel { .. } | Instr::Ccmp { .. } | Instr::Adc { .. } | Instr::BCond { .. })
    }

    /// Whether the instruction sets the condition flags.
    pub fn writes_flags(&self) -> bool {
        matches!(
            self,
            Instr::Alu { flags: true, .. } | Instr::Adc { flags: true, .. } | Instr::Ccmp { .. }
        )
    }

    /// The general-purpose registers (0..=30, or `SP`) the instruction
    /// writes, including base register writeback.
    pub fn writes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut reg = |r: RegOp| {
            if r.idx != ZR {
                out.push(r.idx)
            }
        };
        let wb = |a: Addr| (a.writeback != Writeback::None).then_some(a.base);
        let mut base = None;
        match *self {
            Instr::Nop
            | Instr::Invalid
            | Instr::B { .. }
            | Instr::BCond { .. }
            | Instr::Cbz { .. }
            | Instr::Tbz { .. }
            | Instr::BrReg { kind: BrKind::Br | BrKind::Ret, .. }
            | Instr::Ccmp { .. }
            | Instr::Barrier(_)
            | Instr::Clrex
            | Instr::VMovImm { .. }
            | Instr::VMov { .. }
            | Instr::VLogic { .. }
            | Instr::VIns { .. }
            | Instr::VInsElem { .. }
            | Instr::VDup { .. }
            | Instr::VDupElem { .. }
            | Instr::VCnt { .. }
            | Instr::VAddv { .. } => {}
            Instr::VLoad { addr, .. } | Instr::VStore { addr, .. } => base = wb(addr),
            Instr::VMovToGp { rd, .. } => reg(rd),
            Instr::Bl { .. } | Instr::BrReg { kind: BrKind::Blr, .. } => reg(RegOp { idx: 30, wide: true }),
            Instr::MovImm { rd, .. }
            | Instr::Movk { rd, .. }
            | Instr::MovReg { rd, .. }
            | Instr::Alu { rd, .. }
            | Instr::Csel { rd, .. }
            | Instr::Bitfield { rd, .. }
            | Instr::Madd { rd, .. }
            | Instr::Mulh { rd, .. }
            | Instr::Unary { rd, .. }
            | Instr::Extr { rd, .. }
            | Instr::Adc { rd, .. } => reg(rd),
            Instr::Load { rt, addr, .. } => {
                reg(rt);
                base = wb(addr);
            }
            Instr::Store { addr, .. } | Instr::StorePair { addr, .. } => base = wb(addr),
            Instr::StoreExcl { rs, .. } | Instr::StoreExclPair { rs, .. } | Instr::Cas { rs, .. } => reg(rs),
            Instr::LoadExclPair { rt, rt2, .. } => {
                reg(rt);
                reg(rt2);
            }
            Instr::LoadPair { rt, rt2, addr, .. } => {
                reg(rt);
                reg(rt2);
                base = wb(addr);
            }
            Instr::Rmw { rt, .. } => reg(rt),
        }
        if let Some(b) = base {
            reg(b);
        }
        out
    }
}

/// Code a thread runs: instructions at consecutive addresses from `base`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Code {
    pub base: u64,
    pub instrs: Vec<Instr>,
    /// Why each `Instr::Invalid` could not be decoded, by index.
    pub errors: BTreeMap<usize, String>,
}

impl Code {
    pub fn new(base: u64, instrs: Vec<Instr>) -> Code {
        Code { base, instrs, errors: BTreeMap::new() }
    }

    /// The address of instruction `i`.
    pub fn addr(&self, i: usize) -> u64 {
        self.base.wrapping_add(4 * i as u64)
    }

    /// The address just past the code. A thread that reaches it has
    /// finished, so it is every thread's initial return address (X30).
    pub fn exit(&self) -> u64 {
        self.addr(self.instrs.len())
    }

    /// The index of the instruction at `addr`, which may be the exit.
    pub fn index(&self, addr: u64) -> Option<usize> {
        let off = addr.checked_sub(self.base)?;
        if off % 4 != 0 {
            return None;
        }
        let i = usize::try_from(off / 4).ok()?;
        (i <= self.instrs.len()).then_some(i)
    }
}

/// What a thread runs: shared code and the index it starts at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub code: Arc<Code>,
    pub entry: usize,
}

impl Program {
    /// Run `instrs`, loaded at `base`, from the start.
    pub fn new(base: u64, instrs: Vec<Instr>) -> Program {
        Program { code: Arc::new(Code::new(base, instrs)), entry: 0 }
    }
}
