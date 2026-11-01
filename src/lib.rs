//! armem: an emulator for finding concurrency bugs caused by the weak
//! ARMv8 hardware memory model.
//!
//! The emulator is fully deterministic: it does not use host threads, and
//! it explores *all* behaviors a program may exhibit under the selected
//! memory model (bounded by a per-thread instruction budget, the "fuel").

pub mod asm;
pub mod core;
pub mod decode;
pub mod elf;
pub mod image;
pub mod interleave;
pub mod isa;
pub mod litmus;
pub mod outcome;
pub mod promising;
pub mod report;

use outcome::Outcomes;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Model {
    Arm,
    Tso,
    Sc,
}

impl Model {
    pub fn name(self) -> &'static str {
        match self {
            Model::Arm => "ARM",
            Model::Tso => "TSO",
            Model::Sc => "SC",
        }
    }
}

/// Exploration options.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Maximum instructions per thread per execution.
    pub fuel: u32,
    /// Model FEAT_LSE2: 16-byte-aligned LDP/STP of X registers (8-byte-aligned
    /// for W registers) are single-copy atomic, and so is any other
    /// non-exclusive access that stays within an aligned 16-byte block.
    pub lse2: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options { fuel: 200, lse2: false }
    }
}

pub fn run(test: &litmus::Test, model: Model, opts: &Options) -> Outcomes {
    match model {
        Model::Arm => promising::explore(test, opts),
        Model::Tso => interleave::explore(test, interleave::Kind::Tso, opts.fuel),
        Model::Sc => interleave::explore(test, interleave::Kind::Sc, opts.fuel),
    }
}
