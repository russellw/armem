//! armem: an emulator for finding concurrency bugs caused by the weak
//! ARMv8 hardware memory model.
//!
//! The emulator is fully deterministic: it does not use host threads, and
//! it explores *all* behaviours a program may exhibit under the selected
//! memory model (bounded by a per-thread instruction budget, the "fuel").

pub mod asm;
pub mod core;
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

pub fn run(test: &litmus::Test, model: Model, fuel: u32) -> Outcomes {
    match model {
        Model::Arm => promising::explore(test, &promising::Config { fuel }),
        Model::Tso => interleave::explore(test, interleave::Kind::Tso, fuel),
        Model::Sc => interleave::explore(test, interleave::Kind::Sc, fuel),
    }
}
