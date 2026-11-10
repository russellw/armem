//! Register liveness, for merging states that differ only in registers
//! whose values are never used again.
//!
//! A search over a thread's states (the reduction's pre-pass, the solo
//! explorations, the reference models) keeps every state it has visited.
//! Compiled code leaves stale values in registers it no longer needs: a
//! retry loop's old comparison results, a value loaded only to be thrown
//! away (a volatile read), the status of a store-exclusive. States that
//! differ only there behave alike, so `canon` clears such registers before
//! a state is looked up. A dead register is never read before it is
//! written, so neither its value nor its dependencies affect anything.
//!
//! The analysis is the usual backward one, over the thread's instructions.
//! What is live at the end of the code is given: the registers the test
//! observes, or none for a search that ignores final registers. `BR` and
//! `BLR` may go anywhere, so every register is live before them. A `RET`
//! is taken to return to the end of the code or to the instruction after
//! a `BL` or `BLR`, the only addresses compiled code puts in X30, and a
//! search must check this with [`Liveness::ret_ok`] before each `RET`
//! (and not use the liveness if it fails). The check suffices: states
//! merged by `canon` agree on the live registers, so they follow the same
//! path, reading only live registers, until one returns elsewhere; and
//! the target is in X30, which is live, so the state kept returns there
//! too, and its check fails. SP and the SIMD&FP registers are not tracked
//! (always kept).

use crate::core::{Core, RV};
use crate::isa::{BrKind, Code, Instr};

/// Live registers: bit `i` for X`i` (0..=30), bit 31 for the flags.
pub type Mask = u32;

pub const ALL: Mask = u32::MAX;
pub const FLAGS: Mask = 1 << 31;

fn bits(regs: &[u8]) -> Mask {
    regs.iter().filter(|&&r| r < 31).fold(0, |m, &r| m | 1 << r)
}

/// The registers (and flags) that `i` reads: every one for `BR` and `BLR`,
/// which may go anywhere.
pub fn uses(i: &Instr) -> Mask {
    match i {
        Instr::BrReg { kind: BrKind::Br | BrKind::Blr, .. } => ALL,
        _ => bits(&i.reads()) | if i.reads_flags() { FLAGS } else { 0 },
    }
}

/// The registers (and flags) that `i` writes.
pub fn defs(i: &Instr) -> Mask {
    bits(&i.writes()) | if i.writes_flags() { FLAGS } else { 0 }
}

/// The registers live on entry to each instruction of a thread's code.
#[derive(Clone, Debug)]
pub struct Liveness {
    /// Indexed by instruction, and at the end of the code.
    live: Vec<Mask>,
    /// Where a `RET` may go: the end of the code, and the instruction after
    /// each `BL` and `BLR`.
    returns: Vec<usize>,
}

impl Liveness {
    /// Liveness for `code`, with `exit` live at its end.
    pub fn new(code: &Code, exit: Mask) -> Liveness {
        let n = code.instrs.len();
        let mut returns: Vec<usize> = (0..n)
            .filter(|&pc| matches!(code.instrs[pc], Instr::Bl { .. } | Instr::BrReg { kind: BrKind::Blr, .. }))
            .map(|pc| pc + 1)
            .chain([n])
            .collect();
        returns.sort();
        returns.dedup();
        let uses: Vec<Mask> = code.instrs.iter().map(uses).collect();
        let defs: Vec<Mask> = code.instrs.iter().map(defs).collect();
        let succs = |pc: usize| -> &[usize] {
            match code.instrs[pc] {
                Instr::BrReg { kind: BrKind::Ret, .. } => &returns,
                _ => &[],
            }
        };
        let next = |pc: usize| -> [Option<usize>; 2] {
            match code.instrs[pc] {
                Instr::B { target } | Instr::Bl { target } => [Some(target), None],
                Instr::BCond { target, .. } | Instr::Cbz { target, .. } | Instr::Tbz { target, .. } => {
                    [Some(pc + 1), Some(target)]
                }
                Instr::BrReg { .. } | Instr::Invalid => [None, None],
                _ => [Some(pc + 1), None],
            }
        };
        let mut live = vec![0; n + 1];
        live[n] = exit;
        loop {
            let mut changed = false;
            for pc in (0..n).rev() {
                let at = |s: usize| live.get(s).copied().unwrap_or(ALL);
                let out = next(pc).iter().flatten().chain(succs(pc)).fold(0, |m, &s| m | at(s));
                let new = uses[pc] | (out & !defs[pc]);
                if new != live[pc] {
                    live[pc] = new;
                    changed = true;
                }
            }
            if !changed {
                return Liveness { live, returns };
            }
        }
    }

    /// The registers live on entry to instruction `pc`.
    pub fn at(&self, pc: usize) -> Mask {
        self.live.get(pc).copied().unwrap_or(ALL)
    }

    /// Whether `core`'s next instruction is not a `RET` to somewhere other
    /// than the places this liveness assumes.
    pub fn ret_ok(&self, code: &Code, core: &Core) -> bool {
        match code.instrs.get(core.pc) {
            Some(&Instr::BrReg { rn, kind: BrKind::Ret }) => {
                code.index(core.get(rn).val).is_some_and(|i| self.returns.binary_search(&i).is_ok())
            }
            _ => true,
        }
    }

    /// Clear the registers of `core` that are dead at its pc.
    pub fn canon(&self, core: &mut Core) {
        let m = self.at(core.pc);
        if m == ALL {
            return;
        }
        for (i, r) in core.regs.iter_mut().enumerate() {
            if m & (1 << i) == 0 {
                *r = RV::default();
            }
        }
        if m & FLAGS == 0 {
            core.flags = RV::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm;

    fn live(src: &str) -> Vec<Mask> {
        let lines: Vec<String> = src.lines().map(String::from).collect();
        let code = Code::new(0, asm::assemble(&lines).unwrap());
        let l = Liveness::new(&code, ALL);
        (0..=code.instrs.len()).map(|pc| l.at(pc)).collect()
    }

    #[test]
    fn overwritten_and_branches() {
        // X1 is overwritten before use, X2 is read after a branch, X0 is
        // read by the store; the flags are live from CMP, which sets them,
        // to B.EQ.
        let l = live("LDR X1,[X0]\nMOV X1,#1\nCMP X1,#0\nB.EQ end\nSTR X2,[X0]\nend:\nNOP");
        assert_eq!(l[0] & 0b111, 0b101);
        assert_eq!(l[1] & 0b111, 0b101);
        assert_eq!(l[3] & FLAGS, FLAGS);
        assert_eq!(l[2] & FLAGS, 0);
        // At the end, everything is live.
        assert_eq!(l[6], ALL);
    }

    #[test]
    fn partial_writes_and_returns() {
        // MOVK and BFI keep part of the destination; RET returns to the
        // end, where everything is live.
        let l = live("MOV X0,#0\nMOVK X1,#1\nBFI X2,X3,#0,#8\nRET");
        assert_eq!(l[1] & 0b10, 0b10);
        assert_eq!(l[2] & 0b1100, 0b1100);
        assert_eq!(l[3], ALL);
        assert_eq!(l[0] & 1, 0);
    }

    #[test]
    fn returns() {
        // f's RET returns after the BL, where only X0 and X30 are read
        // (X30 by the final RET); X1 is set by f but never read after.
        let lines: Vec<String> =
            "BL f\nSTR X0,[X2]\nB end\nf:\nMOV X1,#1\nLDR X0,[X3]\nRET\nend:".lines().map(String::from).collect();
        let code = Code::new(0, asm::assemble(&lines).unwrap());
        let l = Liveness::new(&code, 0);
        assert_eq!(l.at(5), 1 << 30 | 1 << 2 | 1);
        assert_eq!(l.at(4) & 0b1110, 0b1100);
        assert_eq!(l.at(3) & 0b10, 0);
        assert_eq!(l.at(1), 0b101);
    }
}
