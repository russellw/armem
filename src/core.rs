//! Thread-local execution shared by all memory models.
//!
//! Every register value carries a *view* (a timestamp) recording which
//! memory events it depends on. The ARM model uses views to track
//! dependencies; the reference models (SC/TSO) simply ignore them.

use crate::isa::*;

/// A timestamp/view. 0 is the initial state of memory.
pub type View = u32;

/// A register value together with its dependency views.
///
/// `view` covers address and data dependencies (which order later reads
/// and writes). `pick` (always >= `view`) additionally covers "pick"
/// dependencies: values that were only *selected* by a condition, as in
/// `CSEL` or the comparison of a `CAS`. In Arm's model those order later
/// writes but not later reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RV {
    pub val: u64,
    pub view: View,
    pub pick: View,
}

impl RV {
    /// A value with no dependencies.
    pub fn imm(val: u64) -> RV {
        RV { val, view: 0, pick: 0 }
    }
    /// `val` with the dependencies of both `a` and `b`.
    pub fn joined(val: u64, a: RV, b: RV) -> RV {
        RV { val, view: a.view.max(b.view), pick: a.pick.max(b.pick) }
    }
}

/// Architectural state of a thread that is independent of the memory model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Core {
    pub pc: usize,
    pub regs: [RV; 31],
    /// NZCV flags in bits 3..0 of `val`.
    pub flags: RV,
    /// Number of instructions executed (bounded by the fuel limit).
    pub steps: u32,
}

/// A memory operation with its operands already evaluated.
#[derive(Clone, Copy, Debug)]
pub enum MemOp {
    /// `signed`: sign-extend the value into `rt` (otherwise zero-extend).
    Load { addr: RV, rt: RegOp, size: Size, signed: bool, kind: LoadKind, excl: bool },
    Store { addr: RV, data: RV, size: Size, release: bool },
    /// Exclusive pair load: `size` is the element size. A W pair is one
    /// 8-byte single-copy-atomic access; an X pair is a 16-byte access that
    /// is only single-copy atomic per 8-byte half.
    LoadExclPair { addr: RV, rt: RegOp, rt2: RegOp, size: Size, acquire: bool },
    /// Exclusive pair store: one write of `2 * size` bytes.
    StoreExclPair { addr: RV, data: RV, data2: RV, rs: RegOp, size: Size, release: bool },
    /// Two independent loads, at `addr` and `addr + size`.
    LoadPair { addr: RV, rt: RegOp, rt2: RegOp, size: Size, signed: bool },
    /// Two independent stores, at `addr` and `addr + size`.
    StorePair { addr: RV, data: RV, data2: RV, size: Size },
    StoreExcl { addr: RV, data: RV, rs: RegOp, size: Size, release: bool },
    /// Atomic read-modify-write. For CAS, `cmp` is Some(expected).
    /// `noret`: the loaded value is discarded (destination XZR/WZR). Such a
    /// read has no acquire semantics and is not ordered by `DMB LD`.
    Rmw {
        addr: RV,
        op: RmwOp,
        operand: RV,
        cmp: Option<RV>,
        rt: RegOp,
        size: Size,
        acquire: bool,
        release: bool,
        noret: bool,
    },
    Barrier(Barrier),
}

/// Result of attempting to execute the next instruction locally.
pub enum Step {
    /// The thread has finished.
    Done,
    /// A local instruction was executed. `ctrl` is the (pick) view of a
    /// branch condition, if the instruction was a conditional branch.
    Local { ctrl: Option<View> },
    /// The next instruction needs the memory model. The pc has not been
    /// advanced; the model must call `Core::retire` after performing it.
    Mem(MemOp),
}

/// Apply a register extend/shift.
fn extend(v: u64, e: Extend) -> u64 {
    match e {
        Extend::Lsl(s) => v << s,
        Extend::Uxtw(s) => (v & 0xffff_ffff) << s,
        Extend::Sxtw(s) => ((v as u32 as i32 as i64) as u64) << s,
    }
}

impl Core {
    pub fn new(regs: [u64; 31]) -> Core {
        let mut r = [RV::default(); 31];
        for (i, v) in regs.iter().enumerate() {
            r[i].val = *v;
        }
        Core { pc: 0, regs: r, flags: RV::default(), steps: 0 }
    }

    pub fn get(&self, r: RegOp) -> RV {
        if r.idx == ZR {
            return RV::default();
        }
        let v = self.regs[r.idx as usize];
        RV { val: Size::of(r).mask(v.val), ..v }
    }

    pub fn set(&mut self, r: RegOp, v: RV) {
        if r.idx == ZR {
            return;
        }
        self.regs[r.idx as usize] = RV { val: Size::of(r).mask(v.val), ..v };
    }

    fn operand(&self, o: Operand, wide: bool) -> RV {
        match o {
            Operand::Reg(r) => self.get(r),
            Operand::Imm(i) => RV::imm(if wide { i } else { i & 0xffff_ffff }),
            Operand::RegExt(r, e) => {
                let v = self.get(r);
                RV { val: extend(v.val, e), ..v }
            }
        }
    }

    /// The address accessed by `a`.
    pub fn addr(&self, a: &Addr) -> RV {
        let b = self.get(a.base);
        if let Writeback::Post(_) = a.writeback {
            return b;
        }
        let off = match a.offset {
            Offset::Imm(i) => RV::imm(i as u64),
            Offset::Reg(r, e) => {
                let v = self.get(r);
                RV { val: extend(v.val, e), ..v }
            }
        };
        RV::joined(b.val.wrapping_add(off.val), b, off)
    }

    /// Advance past the current (memory) instruction, performing any base
    /// register writeback.
    pub fn retire(&mut self, prog: &[Instr]) {
        let addr = match prog[self.pc] {
            Instr::Load { addr, .. }
            | Instr::Store { addr, .. }
            | Instr::LoadPair { addr, .. }
            | Instr::StorePair { addr, .. } => Some(addr),
            _ => None,
        };
        if let Some(a) = addr {
            let b = self.get(a.base);
            let new = match (a.writeback, a.offset) {
                (Writeback::Pre, Offset::Imm(i)) => Some(b.val.wrapping_add(i as u64)),
                (Writeback::Post(i), _) => Some(b.val.wrapping_add(i as u64)),
                _ => None,
            };
            if let Some(v) = new {
                self.set(a.base, RV { val: v, ..b });
            }
        }
        self.pc += 1;
        self.steps += 1;
    }

    /// Execute the next instruction if it is local, or describe the memory
    /// operation it needs.
    pub fn step(&mut self, prog: &[Instr]) -> Step {
        let Some(ins) = prog.get(self.pc) else {
            return Step::Done;
        };
        let mut ctrl = None;
        let mut next = self.pc + 1;
        match *ins {
            Instr::Nop => {}
            Instr::MovImm { rd, imm } => self.set(rd, RV::imm(imm)),
            Instr::MovReg { rd, rn } => {
                let v = self.get(rn);
                self.set(rd, v)
            }
            Instr::Alu { op, rd, rn, op2, flags } => {
                let wide = rd.wide || (rd.idx == ZR && rn.wide);
                let size = if wide { Size::B8 } else { Size::B4 };
                let a = self.get(rn);
                let b = self.operand(op2, wide);
                let bits = if wide { 64 } else { 32 };
                let (x, y) = (size.mask(a.val), size.mask(b.val));
                let r = match op {
                    AluOp::Add => x.wrapping_add(y),
                    AluOp::Sub => x.wrapping_sub(y),
                    AluOp::And => x & y,
                    AluOp::Orr => x | y,
                    AluOp::Eor => x ^ y,
                    AluOp::Lsl => x << (y % bits),
                    AluOp::Lsr => x >> (y % bits),
                    AluOp::Asr => {
                        if wide {
                            ((x as i64) >> (y % 64)) as u64
                        } else {
                            ((x as u32 as i32) >> (y % 32)) as u32 as u64
                        }
                    }
                    AluOp::Mul => x.wrapping_mul(y),
                };
                let r = size.mask(r);
                if flags {
                    let sign = 1u64 << (bits - 1);
                    let n = r & sign != 0;
                    let z = r == 0;
                    let (c, v) = match op {
                        AluOp::Add => {
                            let c = if wide { x.checked_add(y).is_none() } else { x + y > 0xffff_ffff };
                            let v = (x & sign == y & sign) && (r & sign != x & sign);
                            (c, v)
                        }
                        AluOp::Sub => {
                            let c = x >= y;
                            let v = (x & sign != y & sign) && (r & sign != x & sign);
                            (c, v)
                        }
                        _ => (false, false),
                    };
                    let nzcv = (n as u64) << 3 | (z as u64) << 2 | (c as u64) << 1 | v as u64;
                    self.flags = RV::joined(nzcv, a, b);
                }
                self.set(rd, RV::joined(r, a, b));
            }
            Instr::Csel { rd, rn, rm, cond, inc } => {
                let f = self.flags;
                let v = if cond.holds(f.val) {
                    self.get(rn)
                } else {
                    let m = self.get(rm);
                    RV { val: if inc { m.val.wrapping_add(1) } else { m.val }, ..m }
                };
                // The result has a data dependency on the chosen operand and
                // only a pick dependency on the flags.
                self.set(rd, RV { pick: v.pick.max(f.pick), ..v });
            }
            Instr::B { target } => next = target,
            Instr::BCond { cond, target } => {
                ctrl = Some(self.flags.pick);
                if cond.holds(self.flags.val) {
                    next = target;
                }
            }
            Instr::Cbz { rt, nonzero, target } => {
                let v = self.get(rt);
                ctrl = Some(v.pick);
                if (v.val != 0) == nonzero {
                    next = target;
                }
            }
            Instr::Load { rt, addr, size, signed, kind, excl } => {
                return Step::Mem(MemOp::Load { addr: self.addr(&addr), rt, size, signed, kind, excl });
            }
            Instr::Store { rt, addr, size, release } => {
                return Step::Mem(MemOp::Store { addr: self.addr(&addr), data: self.get(rt), size, release });
            }
            Instr::StoreExcl { rs, rt, addr, size, release } => {
                return Step::Mem(MemOp::StoreExcl { addr: self.addr(&addr), data: self.get(rt), rs, size, release });
            }
            Instr::LoadExclPair { rt, rt2, addr, acquire } => {
                return Step::Mem(MemOp::LoadExclPair { addr: self.addr(&addr), rt, rt2, size: Size::of(rt), acquire });
            }
            Instr::StoreExclPair { rs, rt, rt2, addr, release } => {
                return Step::Mem(MemOp::StoreExclPair {
                    addr: self.addr(&addr),
                    data: self.get(rt),
                    data2: self.get(rt2),
                    rs,
                    size: Size::of(rt),
                    release,
                });
            }
            Instr::LoadPair { rt, rt2, addr, size, signed } => {
                return Step::Mem(MemOp::LoadPair { addr: self.addr(&addr), rt, rt2, size, signed });
            }
            Instr::StorePair { rt, rt2, addr, size } => {
                return Step::Mem(MemOp::StorePair {
                    addr: self.addr(&addr),
                    data: self.get(rt),
                    data2: self.get(rt2),
                    size,
                });
            }
            Instr::Rmw { op, rs, rt, addr, size, acquire, release } => {
                let noret = rt.idx == ZR;
                return Step::Mem(MemOp::Rmw {
                    addr: self.addr(&addr),
                    op,
                    operand: self.get(rs),
                    cmp: None,
                    rt,
                    size,
                    acquire: acquire && !noret,
                    release,
                    noret,
                });
            }
            Instr::Cas { rs, rt, addr, size, acquire, release } => {
                return Step::Mem(MemOp::Rmw {
                    addr: self.addr(&addr),
                    op: RmwOp::Swp,
                    operand: self.get(rt),
                    cmp: Some(self.get(rs)),
                    rt: rs,
                    size,
                    acquire: acquire && rs.idx != ZR,
                    release,
                    noret: rs.idx == ZR,
                });
            }
            Instr::Barrier(b) => return Step::Mem(MemOp::Barrier(b)),
        }
        self.pc = next;
        self.steps += 1;
        Step::Local { ctrl }
    }
}

impl Size {
    /// The size of two elements of this size.
    pub fn double(self) -> Size {
        Size::from_bytes(2 * self.bytes()).expect("pair too large")
    }
}

/// Combine two elements of `size` bytes into one little-endian value.
pub fn pair_value(lo: u64, hi: u64, size: Size) -> u128 {
    size.mask(lo) as u128 | (size.mask(hi) as u128) << (8 * size.bytes())
}

/// Split a little-endian value into two elements of `size` bytes.
pub fn split_pair(v: u128, size: Size) -> (u64, u64) {
    (size.mask(v as u64), size.mask((v >> (8 * size.bytes())) as u64))
}

/// Compute the value an RMW stores, or None if a CAS comparison fails.
pub fn rmw_result(op: RmwOp, old: u64, operand: u64, cmp: Option<u64>, size: Size) -> Option<u64> {
    match cmp {
        Some(c) if size.mask(c) != size.mask(old) => None,
        Some(_) => Some(size.mask(operand)),
        None => Some(op.apply(old, operand, size)),
    }
}
