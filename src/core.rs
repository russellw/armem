//! Thread-local execution shared by all memory models.
//!
//! Every register value carries a *view* (a timestamp) recording which
//! memory events it depends on. The ARM model uses views to track
//! dependencies; the reference models (SC/TSO) simply ignore them.

use crate::isa::*;

/// A timestamp/view. 0 is the initial state of memory.
pub type View = u32;

/// A register value together with its dependency view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RV {
    pub val: u64,
    pub view: View,
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
    Load { addr: RV, rt: RegOp, size: Size, kind: LoadKind, excl: bool },
    Store { addr: RV, data: RV, size: Size, release: bool },
    StoreExcl { addr: RV, data: RV, rs: RegOp, size: Size, release: bool },
    /// Atomic read-modify-write. For CAS, `cmp` is Some(expected).
    Rmw { addr: RV, op: RmwOp, operand: RV, cmp: Option<RV>, rt: RegOp, size: Size, acquire: bool, release: bool },
    Barrier(Barrier),
}

/// Result of attempting to execute the next instruction locally.
pub enum Step {
    /// The thread has finished.
    Done,
    /// A local instruction was executed. `ctrl` is the view of a branch
    /// condition, if the instruction was a conditional branch.
    Local { ctrl: Option<View> },
    /// The next instruction needs the memory model. The pc has not been
    /// advanced; the model must call `Core::retire` after performing it.
    Mem(MemOp),
}

fn join(a: View, b: View) -> View {
    a.max(b)
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
        RV { val: Size::of(r).mask(v.val), view: v.view }
    }

    pub fn set(&mut self, r: RegOp, v: RV) {
        if r.idx == ZR {
            return;
        }
        self.regs[r.idx as usize] = RV { val: Size::of(r).mask(v.val), view: v.view };
    }

    fn operand(&self, o: Operand, wide: bool) -> RV {
        match o {
            Operand::Reg(r) => self.get(r),
            Operand::Imm(i) => RV { val: if wide { i } else { i & 0xffff_ffff }, view: 0 },
        }
    }

    pub fn addr(&self, a: &Addr) -> RV {
        let b = self.get(a.base);
        let (off, view) = match a.offset {
            Offset::Imm(i) => (i as u64, 0),
            Offset::Reg(r, e) => {
                let v = self.get(r);
                let o = match e {
                    Extend::Lsl(s) => v.val << s,
                    Extend::Uxtw(s) => (v.val & 0xffff_ffff) << s,
                    Extend::Sxtw(s) => ((v.val as u32 as i32 as i64) as u64) << s,
                };
                (o, v.view)
            }
        };
        RV { val: b.val.wrapping_add(off), view: join(b.view, view) }
    }

    /// Advance past the current (memory) instruction.
    pub fn retire(&mut self) {
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
            Instr::MovImm { rd, imm } => self.set(rd, RV { val: imm, view: 0 }),
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
                let view = join(a.view, b.view);
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
                    self.flags = RV { val: nzcv, view };
                }
                self.set(rd, RV { val: r, view });
            }
            Instr::Csel { rd, rn, rm, cond, inc } => {
                let f = self.flags;
                let v = if cond.holds(f.val) {
                    self.get(rn)
                } else {
                    let m = self.get(rm);
                    RV { val: if inc { m.val.wrapping_add(1) } else { m.val }, view: m.view }
                };
                // The result depends on the flags as well as the chosen operand.
                self.set(rd, RV { val: v.val, view: join(v.view, f.view) });
            }
            Instr::B { target } => next = target,
            Instr::BCond { cond, target } => {
                ctrl = Some(self.flags.view);
                if cond.holds(self.flags.val) {
                    next = target;
                }
            }
            Instr::Cbz { rt, nonzero, target } => {
                let v = self.get(rt);
                ctrl = Some(v.view);
                if (v.val != 0) == nonzero {
                    next = target;
                }
            }
            Instr::Load { rt, addr, kind, excl } => {
                return Step::Mem(MemOp::Load { addr: self.addr(&addr), rt, size: Size::of(rt), kind, excl });
            }
            Instr::Store { rt, addr, release } => {
                return Step::Mem(MemOp::Store {
                    addr: self.addr(&addr),
                    data: self.get(rt),
                    size: Size::of(rt),
                    release,
                });
            }
            Instr::StoreExcl { rs, rt, addr, release } => {
                return Step::Mem(MemOp::StoreExcl {
                    addr: self.addr(&addr),
                    data: self.get(rt),
                    rs,
                    size: Size::of(rt),
                    release,
                });
            }
            Instr::Rmw { op, rs, rt, addr, acquire, release } => {
                // ST<op> forms (rt = ZR) do not have acquire semantics.
                return Step::Mem(MemOp::Rmw {
                    addr: self.addr(&addr),
                    op,
                    operand: self.get(rs),
                    cmp: None,
                    rt,
                    size: Size::of(rs),
                    acquire: acquire && rt.idx != ZR,
                    release,
                });
            }
            Instr::Cas { rs, rt, addr, acquire, release } => {
                return Step::Mem(MemOp::Rmw {
                    addr: self.addr(&addr),
                    op: RmwOp::Swp,
                    operand: self.get(rt),
                    cmp: Some(self.get(rs)),
                    rt: rs,
                    size: Size::of(rs),
                    acquire,
                    release,
                });
            }
            Instr::Barrier(b) => return Step::Mem(MemOp::Barrier(b)),
        }
        self.pc = next;
        self.steps += 1;
        Step::Local { ctrl }
    }
}

/// Compute the value an RMW stores, or None if a CAS comparison fails.
pub fn rmw_result(op: RmwOp, old: u64, operand: u64, cmp: Option<u64>, size: Size) -> Option<u64> {
    match cmp {
        Some(c) if size.mask(c) != size.mask(old) => None,
        Some(_) => Some(size.mask(operand)),
        None => Some(op.apply(old, operand, size)),
    }
}
