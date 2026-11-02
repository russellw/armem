//! Thread-local execution shared by all memory models.
//!
//! Every register value carries a *view* (a timestamp) recording which
//! memory events it depends on. The ARM model uses views to track
//! dependencies; the reference models (SC/TSO) simply ignore them.

use crate::isa::*;
use std::collections::BTreeMap;
use std::sync::Arc;

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

/// A SIMD&FP register value with its dependency views, as in `RV`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VR {
    pub val: u128,
    pub view: View,
    pub pick: View,
}

impl VR {
    /// `val` with the dependencies of `d`.
    pub fn with(val: u128, d: RV) -> VR {
        VR { val, view: d.view, pick: d.pick }
    }
    /// The dependencies, as a general-purpose value of 0.
    pub fn dep(self) -> RV {
        RV { val: 0, view: self.view, pick: self.pick }
    }
}

/// The low `n` bytes of `v` (n <= 16).
pub fn low_bytes(v: u128, n: u64) -> u128 {
    if n >= 16 { v } else { v & ((1u128 << (8 * n)) - 1) }
}

/// Element `index` of `size` bytes of a vector value.
fn element(v: u128, index: u8, size: Size) -> u64 {
    let n = size.bytes();
    low_bytes(v >> (8 * n * index as u64), n) as u64
}

/// `v` with element `index` of `size` bytes replaced by the low bytes of `e`.
fn with_element(v: u128, index: u8, size: Size, e: u64) -> u128 {
    let n = size.bytes();
    let shift = 8 * n * index as u64;
    v & !(low_bytes(u128::MAX, n) << shift) | low_bytes(e as u128, n) << shift
}

/// Start of the stack area. It holds `threads + 2` slots of `STACK_SIZE`
/// bytes: thread `i`'s stack is slot `i + 1`, and the first and last slots
/// are guards.
pub const STACK_AREA: u64 = 0x7f00_0000_0000;
pub const STACK_SIZE: u64 = 0x10_0000;

/// The stack area of a test with `threads` threads: `[start, end)`.
pub fn stack_area(threads: usize) -> (u64, u64) {
    (STACK_AREA, STACK_AREA + (threads as u64 + 2) * STACK_SIZE)
}

/// Thread `tid`'s stack: `[lo, hi)`. Its initial SP is `hi`.
pub fn stack_range(tid: usize) -> (u64, u64) {
    let lo = STACK_AREA + (tid as u64 + 1) * STACK_SIZE;
    (lo, lo + STACK_SIZE)
}

/// A thread's stack, which is private to the thread.
///
/// Plain loads and stores within it are performed by the core itself,
/// without the memory model: no other thread can observe them, so they
/// need no place in the order of memory writes. This keeps compiled code's
/// stack traffic from multiplying the states the models explore. A load
/// reads the thread's latest store to each byte (or 0) and, as when a
/// load is satisfied by forwarding from a store, takes on the store's
/// address and data dependencies (Arm's `(addr | data); rfi`).
///
/// It is a fault for a thread to access the stack area outside its own
/// stack (another thread's stack, a guard slot, or across the edge of its
/// own), or to access its stack with an exclusive, atomic, acquire or
/// release instruction.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Stack {
    pub lo: u64,
    pub hi: u64,
    /// The whole stack area.
    pub area: (u64, u64),
    /// Bytes stored so far, with the views of the store (as in `RV`).
    bytes: BTreeMap<u64, (u8, View, View)>,
}

/// Where an access goes.
enum Place {
    Memory,
    Stack,
    Fault(String),
}

impl Stack {
    pub fn new(tid: usize, threads: usize) -> Stack {
        let (lo, hi) = stack_range(tid);
        Stack { lo, hi, area: stack_area(threads), bytes: BTreeMap::new() }
    }

    /// Classify an access of `len` bytes at `a`. `plain`: a load or store
    /// with no exclusive, atomic or ordering semantics.
    fn place(&self, a: u64, len: u64, plain: bool) -> Place {
        let Some(end) = a.checked_add(len) else {
            return Place::Memory;
        };
        if a >= self.lo && end <= self.hi {
            if plain {
                return Place::Stack;
            }
            return Place::Fault(format!(
                "exclusive, atomic, acquire or release access to its stack at {:#x}, which is not supported",
                a
            ));
        }
        if a < self.area.1 && end > self.area.0 {
            return Place::Fault(format!(
                "{}-byte access at {:#x} is outside its stack {:#x}..{:#x} (a stack overflow, or another thread's stack)",
                len, a, self.lo, self.hi
            ));
        }
        Place::Memory
    }

    /// Read `n` bytes at `a` (within the stack), with the dependencies
    /// of the stores they come from.
    fn read(&self, a: u64, n: u64) -> (u128, View, View) {
        let (mut val, mut view, mut pick) = (0u128, 0, 0);
        for b in (a..a + n).rev() {
            let (v, w, p) = self.bytes.get(&b).copied().unwrap_or((0, 0, 0));
            val = val << 8 | v as u128;
            view = view.max(w);
            pick = pick.max(p);
        }
        (val, view, pick)
    }

    fn write(&mut self, a: u64, n: u64, val: u128, view: View, pick: View) {
        for i in 0..n {
            self.bytes.insert(a + i, ((val >> (8 * i)) as u8, view, pick));
        }
    }
}

/// Architectural state of a thread that is independent of the memory model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Core {
    pub pc: usize,
    pub regs: [RV; 31],
    pub sp: RV,
    /// The SIMD&FP registers V0..V31: None until one is written (so that
    /// code without them costs nothing to copy and hash), then shared until
    /// written.
    pub vregs: Option<Arc<[VR; 32]>>,
    pub stack: Stack,
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
    /// Load SIMD&FP register `vt` (and `vt2` from `addr + size`, an
    /// independent access), each zeroed above `size` bytes. A 16-byte
    /// element is two single-copy-atomic 8-byte accesses.
    VLoad { addr: RV, vt: VReg, vt2: Option<VReg>, size: Size },
    /// Store `size` bytes of `data` (and of `data2` at `addr + size`), split
    /// into single-copy-atomic accesses as for `VLoad`.
    VStore { addr: RV, data: VR, data2: Option<VR>, size: Size },
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
    /// Clear the exclusive monitor, which belongs to the model.
    Clrex,
}

/// Result of attempting to execute the next instruction locally. Only a
/// `Local` step changes the core: after `Done`, `Fault` or `Mem` it is as
/// it was.
pub enum Step {
    /// The thread has finished.
    Done,
    /// A local instruction was executed. `ctrl` is a (pick) view that later
    /// writes must be ordered after: that of a conditional branch's
    /// condition, or of the address of a stack access.
    Local { ctrl: Option<View> },
    /// The thread cannot continue: the message says why.
    Fault(String),
    /// The next instruction needs the memory model. The pc has not been
    /// advanced; the model must call `Core::retire` after performing it.
    Mem(MemOp),
}

/// Apply a register extend/shift to `v`, a register value of the
/// operation's size (64 bits if `wide`, else 32).
fn extend(v: u64, e: Extend, wide: bool) -> u64 {
    match e {
        Extend::Lsl(s) => v << s,
        Extend::Uxtw(s) => (v & 0xffff_ffff) << s,
        Extend::Sxtw(s) => ((v as u32 as i32 as i64) as u64) << s,
        Extend::Lsr(s) => v >> s,
        Extend::Asr(s) if wide => ((v as i64) >> s) as u64,
        Extend::Asr(s) => ((v as u32 as i32) >> s) as u32 as u64,
        Extend::Ror(s) if wide => v.rotate_right(s as u32),
        Extend::Ror(s) => (v as u32).rotate_right(s as u32) as u64,
    }
}

/// The architecture's AddWithCarry: `x + y + carry` at the operation's
/// size (64 bits if `wide`, else 32; `x` and `y` masked to it), and the
/// NZCV flags it sets (N=8, Z=4, C=2, V=1).
fn add_with_carry(x: u64, y: u64, carry: bool, wide: bool) -> (u64, u64) {
    let bits = if wide { 64 } else { 32 };
    let sext = |v: u64| (v as i128) << (128 - bits) >> (128 - bits);
    let unsigned = x as u128 + y as u128 + carry as u128;
    let signed = sext(x) + sext(y) + carry as i128;
    let r = unsigned as u64 & (u64::MAX >> (64 - bits));
    let n = r >> (bits - 1) & 1;
    let z = (r == 0) as u64;
    let c = (unsigned >> bits != 0) as u64;
    let v = (sext(r) != signed) as u64;
    (r, n << 3 | z << 2 | c << 1 | v)
}

/// The NZCV flags set by `op` on `x` and `y`, whose result is `r`, all of
/// them masked to the operation's size. Only ADD and SUB set C and V.
fn nzcv(op: AluOp, x: u64, y: u64, r: u64, wide: bool) -> u64 {
    match op {
        AluOp::Add => add_with_carry(x, y, false, wide).1,
        AluOp::Sub => add_with_carry(x, !y & if wide { u64::MAX } else { 0xffff_ffff }, true, wide).1,
        _ => {
            let sign = if wide { 1u64 << 63 } else { 1 << 31 };
            ((r & sign != 0) as u64) << 3 | ((r == 0) as u64) << 2
        }
    }
}

/// The result of a data-processing (1 source) operation on `v`, a
/// register value of 64 bits if `wide`, else 32.
fn unary(op: UnaryOp, v: u64, wide: bool) -> u64 {
    let w = v as u32;
    match (op, wide) {
        (UnaryOp::Rbit, true) => v.reverse_bits(),
        (UnaryOp::Rbit, false) => w.reverse_bits() as u64,
        (UnaryOp::Rev16, _) => (v & 0x00ff_00ff_00ff_00ff) << 8 | (v >> 8) & 0x00ff_00ff_00ff_00ff,
        (UnaryOp::Rev32, _) => w.swap_bytes() as u64 | (((v >> 32) as u32).swap_bytes() as u64) << 32,
        (UnaryOp::Rev, true) => v.swap_bytes(),
        (UnaryOp::Rev, false) => w.swap_bytes() as u64,
        (UnaryOp::Clz, true) => v.leading_zeros() as u64,
        (UnaryOp::Clz, false) => w.leading_zeros() as u64,
        // The leading zeros of each bit XORed with the one below it,
        // ignoring the bottom bit.
        (UnaryOp::Cls, true) => ((v ^ v << 1) | 1).leading_zeros() as u64,
        (UnaryOp::Cls, false) => ((w ^ w << 1) | 1).leading_zeros() as u64,
    }
}

impl Core {
    /// Thread `tid` of `threads`, starting at instruction `entry`, with its
    /// stack pointer at the top of its (empty) stack.
    pub fn new(regs: [u64; 31], tid: usize, threads: usize, entry: usize) -> Core {
        let mut r = [RV::default(); 31];
        for (i, v) in regs.iter().enumerate() {
            r[i].val = *v;
        }
        let stack = Stack::new(tid, threads);
        Core {
            pc: entry,
            regs: r,
            sp: RV::imm(stack.hi),
            vregs: None,
            stack,
            flags: RV::default(),
            steps: 0,
        }
    }

    pub fn get(&self, r: RegOp) -> RV {
        let v = match r.idx {
            ZR => return RV::default(),
            SP => self.sp,
            i => self.regs[i as usize],
        };
        RV { val: Size::of(r).mask(v.val), ..v }
    }

    pub fn set(&mut self, r: RegOp, v: RV) {
        let v = RV { val: Size::of(r).mask(v.val), ..v };
        match r.idx {
            ZR => {}
            SP => self.sp = v,
            i => self.regs[i as usize] = v,
        }
    }

    pub fn vget(&self, v: VReg) -> VR {
        self.vregs.as_ref().map_or(VR::default(), |r| r[v as usize])
    }

    pub fn vset(&mut self, v: VReg, x: VR) {
        let regs = self.vregs.get_or_insert_with(|| Arc::new([VR::default(); 32]));
        Arc::make_mut(regs)[v as usize] = x;
    }

    fn operand(&self, o: Operand, wide: bool) -> RV {
        match o {
            Operand::Reg(r) => self.get(r),
            Operand::Imm(i) => RV::imm(if wide { i } else { i & 0xffff_ffff }),
            Operand::RegExt(r, e) => {
                let v = self.get(r);
                RV { val: extend(v.val, e, wide), ..v }
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
                RV { val: extend(v.val, e, true), ..v }
            }
        };
        RV::joined(b.val.wrapping_add(off.val), b, off)
    }

    /// Perform the base register writeback of `a`, if any.
    fn writeback(&mut self, a: Addr) {
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

    /// Advance past the current (memory) instruction, performing any base
    /// register writeback.
    pub fn retire(&mut self, code: &Code) {
        match code.instrs[self.pc] {
            Instr::Load { addr, .. }
            | Instr::Store { addr, .. }
            | Instr::LoadPair { addr, .. }
            | Instr::StorePair { addr, .. }
            | Instr::VLoad { addr, .. }
            | Instr::VStore { addr, .. } => self.writeback(addr),
            _ => {}
        }
        self.pc += 1;
        self.steps += 1;
    }

    /// Load `size` bytes at `a + off` from the stack.
    fn stack_load(&self, a: RV, off: u64, size: Size, signed: bool) -> RV {
        let (v, view, pick) = self.stack.read(a.val + off, size.bytes());
        let view = view.max(a.view);
        RV { val: size.extend(v as u64, signed), view, pick: pick.max(a.pick).max(view) }
    }

    /// Store `size` bytes of `data` at `a + off` on the stack.
    fn stack_store(&mut self, a: RV, off: u64, size: Size, data: RV) {
        self.stack.write(a.val + off, size.bytes(), data.val as u128, a.view.max(data.view), a.pick.max(data.pick));
    }

    /// Load SIMD&FP register `v` from `size` bytes at `a + off` on the stack.
    fn stack_vload(&mut self, a: RV, off: u64, size: Size, v: VReg) {
        let (val, view, pick) = self.stack.read(a.val + off, size.bytes());
        let view = view.max(a.view);
        self.vset(v, VR { val, view, pick: pick.max(a.pick).max(view) });
    }

    /// Execute a plain load or store that is within the thread's stack, or
    /// return what to do instead: `Some(step)` for an access to memory (or
    /// a fault).
    fn stack_access(&mut self, ins: Instr) -> Option<Step> {
        let (addr, len, plain) = match ins {
            Instr::Load { addr, size, kind, excl, .. } => (addr, size.bytes(), kind == LoadKind::Plain && !excl),
            Instr::Store { addr, size, release, .. } => (addr, size.bytes(), !release),
            Instr::LoadPair { addr, size, .. } | Instr::StorePair { addr, size, .. } => (addr, 2 * size.bytes(), true),
            Instr::StoreExcl { addr, size, .. } | Instr::Rmw { addr, size, .. } | Instr::Cas { addr, size, .. } => {
                (addr, size.bytes(), false)
            }
            Instr::LoadExclPair { addr, rt, .. } | Instr::StoreExclPair { addr, rt, .. } => {
                (addr, 2 * Size::of(rt).bytes(), false)
            }
            Instr::VLoad { addr, vt2, size, .. } | Instr::VStore { addr, vt2, size, .. } => {
                (addr, size.bytes() * if vt2.is_some() { 2 } else { 1 }, true)
            }
            _ => return None,
        };
        let a = self.addr(&addr);
        match self.stack.place(a.val, len, plain) {
            Place::Memory => return None,
            Place::Fault(e) => return Some(Step::Fault(e)),
            Place::Stack => {}
        }
        match ins {
            Instr::Load { rt, size, signed, .. } => {
                let v = self.stack_load(a, 0, size, signed);
                self.set(rt, v);
            }
            Instr::Store { rt, size, .. } => {
                let d = self.get(rt);
                self.stack_store(a, 0, size, d);
            }
            Instr::LoadPair { rt, rt2, size, signed, .. } => {
                let (v1, v2) = (self.stack_load(a, 0, size, signed), self.stack_load(a, size.bytes(), size, signed));
                self.set(rt, v1);
                self.set(rt2, v2);
            }
            Instr::StorePair { rt, rt2, size, .. } => {
                let (d1, d2) = (self.get(rt), self.get(rt2));
                self.stack_store(a, 0, size, d1);
                self.stack_store(a, size.bytes(), size, d2);
            }
            Instr::VLoad { vt, vt2, size, .. } => {
                self.stack_vload(a, 0, size, vt);
                if let Some(v2) = vt2 {
                    self.stack_vload(a, size.bytes(), size, v2);
                }
            }
            Instr::VStore { vt, vt2, size, .. } => {
                let n = size.bytes();
                for (i, v) in std::iter::once(vt).chain(vt2).enumerate() {
                    let d = self.vget(v);
                    self.stack.write(a.val + i as u64 * n, n, d.val, a.view.max(d.view), a.pick.max(d.pick));
                }
            }
            _ => unreachable!("only plain accesses go to the stack"),
        }
        self.writeback(addr);
        self.pc += 1;
        self.steps += 1;
        Some(Step::Local { ctrl: Some(a.pick) })
    }

    /// Execute the next instruction if it is local, or describe the memory
    /// operation it needs.
    pub fn step(&mut self, code: &Code) -> Step {
        let Some(ins) = code.instrs.get(self.pc) else {
            return Step::Done;
        };
        if let Some(step) = self.stack_access(*ins) {
            return step;
        }
        let mut ctrl = None;
        let mut next = self.pc + 1;
        match *ins {
            Instr::Nop => {}
            Instr::Invalid => {
                let e = code.errors.get(&self.pc).map_or("invalid instruction", String::as_str);
                return Step::Fault(format!("{:#x}: {}", code.addr(self.pc), e));
            }
            Instr::MovImm { rd, imm } => self.set(rd, RV::imm(imm)),
            Instr::Movk { rd, imm, shift } => {
                let old = self.get(rd);
                let val = old.val & !(0xffff << shift) | (imm as u64) << shift;
                self.set(rd, RV { val, ..old });
            }
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
                    AluOp::Bic => x & !y,
                    AluOp::Orn => x | !y,
                    AluOp::Eon => x ^ !y,
                    AluOp::Lsl => x << (y % bits),
                    AluOp::Lsr => x >> (y % bits),
                    AluOp::Asr => {
                        if wide {
                            ((x as i64) >> (y % 64)) as u64
                        } else {
                            ((x as u32 as i32) >> (y % 32)) as u32 as u64
                        }
                    }
                    AluOp::Ror => {
                        if wide {
                            x.rotate_right((y % 64) as u32)
                        } else {
                            (x as u32).rotate_right((y % 32) as u32) as u64
                        }
                    }
                    AluOp::Mul => x.wrapping_mul(y),
                    AluOp::Udiv => x.checked_div(y).unwrap_or(0),
                    AluOp::Sdiv => {
                        let (x, y) = (size.extend(x, true) as i64, size.extend(y, true) as i64);
                        // i64::MIN / -1 wraps, as INT_MIN / -1 does at either size.
                        if y == 0 { 0 } else { x.wrapping_div(y) as u64 }
                    }
                };
                let r = size.mask(r);
                if flags {
                    let nzcv = nzcv(op, x, y, r, wide);
                    self.flags = RV::joined(nzcv, a, b);
                }
                self.set(rd, RV::joined(r, a, b));
            }
            Instr::Csel { rd, rn, rm, cond, inv, inc } => {
                let f = self.flags;
                let v = if cond.holds(f.val) {
                    self.get(rn)
                } else {
                    let m = self.get(rm);
                    let val = if inv { !m.val } else { m.val };
                    RV { val: val.wrapping_add(inc as u64), ..m }
                };
                // The result has a data dependency on the chosen operand and
                // only a pick dependency on the flags.
                self.set(rd, RV { pick: v.pick.max(f.pick), ..v });
            }
            Instr::Bitfield { kind, rd, rn, immr, imms } => {
                let src = self.get(rn);
                let dst = self.get(rd);
                let bits = 8 * Size::of(rd).bytes() as u32;
                let (immr, imms) = (immr as u32, imms as u32);
                // The field's width and where it comes from and goes to.
                let (width, from, to) = if imms >= immr { (imms - immr + 1, immr, 0) } else { (imms + 1, 0, bits - immr) };
                let mask = u64::MAX >> (64 - width);
                let field = src.val >> from & mask;
                let mut r = match kind {
                    BfKind::Insert => dst.val & !(mask << to),
                    _ => 0,
                } | field << to;
                if kind == BfKind::Signed && field >> (width - 1) & 1 != 0 {
                    r |= u64::MAX << (to + width - 1);
                }
                let v = if kind == BfKind::Insert { RV::joined(r, src, dst) } else { RV { val: r, ..src } };
                self.set(rd, v);
            }
            Instr::Madd { kind, rd, rn, rm, ra, sub } => {
                let (a, b, c) = (self.get(rn), self.get(rm), self.get(ra));
                let signed = kind == MulKind::SignedLong;
                let (x, y) = (Size::of(rn).extend(a.val, signed), Size::of(rm).extend(b.val, signed));
                let p = x.wrapping_mul(y);
                let r = if sub { c.val.wrapping_sub(p) } else { c.val.wrapping_add(p) };
                self.set(rd, RV::joined(r, RV::joined(0, a, b), c));
            }
            Instr::Mulh { rd, rn, rm, signed } => {
                let (a, b) = (self.get(rn), self.get(rm));
                let r = if signed {
                    ((a.val as i64 as i128 * b.val as i64 as i128) >> 64) as u64
                } else {
                    ((a.val as u128 * b.val as u128) >> 64) as u64
                };
                self.set(rd, RV::joined(r, a, b));
            }
            Instr::Unary { op, rd, rn } => {
                let v = self.get(rn);
                self.set(rd, RV { val: unary(op, v.val, rd.wide), ..v });
            }
            Instr::Extr { rd, rn, rm, lsb } => {
                let (a, b) = (self.get(rn), self.get(rm));
                let bits = 8 * Size::of(rd).bytes() as u32;
                let r = ((a.val as u128) << bits | b.val as u128) >> lsb;
                self.set(rd, RV::joined(r as u64, a, b));
            }
            Instr::Adc { rd, rn, rm, sub, flags } => {
                // The carry flag is an input, so the result has a data
                // dependency on the flags.
                let (a, b, f) = (self.get(rn), self.get(rm), self.flags);
                let y = if sub { Size::of(rm).mask(!b.val) } else { b.val };
                let (r, nzcv) = add_with_carry(a.val, y, f.val & 2 != 0, rd.wide);
                let v = RV::joined(r, RV::joined(0, a, b), f);
                if flags {
                    self.flags = RV { val: nzcv, ..v };
                }
                self.set(rd, v);
            }
            Instr::Ccmp { rn, op2, nzcv: imm, cond, cmn } => {
                let f = self.flags;
                let v = if cond.holds(f.val) {
                    let (a, b) = (self.get(rn), self.operand(op2, rn.wide));
                    let size = Size::of(rn);
                    let (op, r) = if cmn { (AluOp::Add, a.val.wrapping_add(b.val)) } else { (AluOp::Sub, a.val.wrapping_sub(b.val)) };
                    RV::joined(nzcv(op, a.val, b.val, size.mask(r), rn.wide), a, b)
                } else {
                    RV::imm(imm as u64)
                };
                // Like CSEL, the new flags depend on the chosen operands and
                // are only picked by the old flags.
                self.flags = RV { pick: v.pick.max(f.pick), ..v };
            }
            Instr::B { target } => next = target,
            Instr::Tbz { rt, bit, nonzero, target } => {
                let v = self.get(rt);
                ctrl = Some(v.pick);
                if (v.val >> bit & 1 != 0) == nonzero {
                    next = target;
                }
            }
            Instr::Bl { target } => {
                self.regs[30] = RV::imm(code.addr(self.pc + 1));
                next = target;
            }
            Instr::BrReg { rn, kind } => {
                // The target is read before BLR writes X30.
                let t = self.get(rn);
                ctrl = Some(t.pick);
                let Some(i) = code.index(t.val) else {
                    return Step::Fault(format!(
                        "{:#x}: branch to {:#x}, which is not in the code {:#x}..{:#x}",
                        code.addr(self.pc),
                        t.val,
                        code.base,
                        code.exit()
                    ));
                };
                if kind == BrKind::Blr {
                    self.regs[30] = RV::imm(code.addr(self.pc + 1));
                }
                next = i;
            }
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
            Instr::Clrex => return Step::Mem(MemOp::Clrex),
            Instr::VLoad { vt, vt2, addr, size } => {
                return Step::Mem(MemOp::VLoad { addr: self.addr(&addr), vt, vt2, size });
            }
            Instr::VStore { vt, vt2, addr, size } => {
                return Step::Mem(MemOp::VStore {
                    addr: self.addr(&addr),
                    data: self.vget(vt),
                    data2: vt2.map(|v| self.vget(v)),
                    size,
                });
            }
            Instr::VMovImm { vd, imm } => self.vset(vd, VR { val: imm, view: 0, pick: 0 }),
            Instr::VMov { vd, vn, size } => {
                let v = self.vget(vn);
                self.vset(vd, VR { val: low_bytes(v.val, size.bytes()), ..v });
            }
            Instr::VLogic { op, vd, vn, vm, q } => {
                let (a, b) = (self.vget(vn), self.vget(vm));
                let r = match op {
                    AluOp::And => a.val & b.val,
                    AluOp::Bic => a.val & !b.val,
                    AluOp::Orr => a.val | b.val,
                    AluOp::Orn => a.val | !b.val,
                    AluOp::Eor => a.val ^ b.val,
                    _ => unreachable!("not a vector logical operation"),
                };
                let r = low_bytes(r, if q { 16 } else { 8 });
                self.vset(vd, VR::with(r, RV::joined(0, a.dep(), b.dep())));
            }
            Instr::VIns { vd, index, size, rn, zero } => {
                let x = self.get(rn);
                let old = if zero { VR::default() } else { self.vget(vd) };
                let r = with_element(old.val, index, size, x.val);
                self.vset(vd, VR::with(r, RV::joined(0, old.dep(), x)));
            }
            Instr::VInsElem { vd, index, vn, index2, size } => {
                let (old, src) = (self.vget(vd), self.vget(vn));
                let r = with_element(old.val, index, size, element(src.val, index2, size));
                self.vset(vd, VR::with(r, RV::joined(0, old.dep(), src.dep())));
            }
            Instr::VDup { vd, size, q, rn } => {
                let x = self.get(rn);
                let n = (if q { 16 } else { 8 }) / size.bytes() as u8;
                let r = (0..n).fold(0, |v, i| with_element(v, i, size, x.val));
                self.vset(vd, VR::with(r, x));
            }
            Instr::VDupElem { vd, vn, index, size, lanes } => {
                let v = self.vget(vn);
                let e = element(v.val, index, size);
                let r = (0..lanes).fold(0, |r, i| with_element(r, i, size, e));
                self.vset(vd, VR { val: r, ..v });
            }
            Instr::VMovToGp { rd, vn, index, size, signed } => {
                let v = self.vget(vn);
                self.set(rd, RV { val: size.extend(element(v.val, index, size), signed), ..v.dep() });
            }
            Instr::VCnt { vd, vn, q } => {
                let v = self.vget(vn);
                let n = if q { 16 } else { 8 };
                let r = (0..n).fold(0, |r, i| with_element(r, i, Size::B1, element(v.val, i, Size::B1).count_ones() as u64));
                self.vset(vd, VR { val: r, ..v });
            }
            Instr::VAddv { vd, vn, q, long } => {
                let v = self.vget(vn);
                let sum: u64 = (0..if q { 16 } else { 8 }).map(|i| element(v.val, i, Size::B1)).sum();
                let r = low_bytes(sum as u128, if long { 2 } else { 1 });
                self.vset(vd, VR { val: r, ..v });
            }
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

/// A work list for a search over a thread's states that expands them in
/// order of steps used (fewest first). Steps never decrease along a path,
/// so a state is first expanded with the fewest steps it can be reached
/// in, and a search that skips states already seen with no more steps
/// expands each state only once. (Depth-first order tends to reach a state
/// first by a long path, around a spin loop, and must then expand it again
/// for each shorter path found later.) The states reachable within the fuel
/// limit are the same in any order.
pub struct StepQueue<T> {
    buckets: BTreeMap<u32, Vec<T>>,
}

impl<T> StepQueue<T> {
    pub fn new() -> Self {
        StepQueue { buckets: BTreeMap::new() }
    }

    pub fn push(&mut self, steps: u32, x: T) {
        self.buckets.entry(steps).or_default().push(x);
    }

    pub fn pop(&mut self) -> Option<T> {
        let mut e = self.buckets.first_entry()?;
        let x = e.get_mut().pop();
        if e.get().is_empty() {
            e.remove();
        }
        x
    }
}
