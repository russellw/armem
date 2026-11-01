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
    /// Only used for the 16-byte footprint of an X-register exclusive pair.
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
    Lsl,
    Lsr,
    Asr,
    Mul,
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
    /// CSEL rd, rn, rm, cond (and CSINC when `inc`)
    Csel { rd: RegOp, rn: RegOp, rm: RegOp, cond: Cond, inc: bool },
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
    /// The registers (0..=30, or `SP`) the instruction writes, including
    /// base register writeback.
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
            | Instr::Barrier(_)
            | Instr::Clrex => {}
            Instr::Bl { .. } | Instr::BrReg { kind: BrKind::Blr, .. } => reg(RegOp { idx: 30, wide: true }),
            Instr::MovImm { rd, .. }
            | Instr::Movk { rd, .. } | Instr::MovReg { rd, .. } | Instr::Alu { rd, .. } | Instr::Csel { rd, .. } => reg(rd),
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
