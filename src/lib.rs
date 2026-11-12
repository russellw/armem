//! armem: an emulator for finding concurrency bugs caused by the weak
//! ARMv8 hardware memory model.
//!
//! The emulator is fully deterministic: it does not use host threads, and
//! it explores *all* behaviors a program may exhibit under the selected
//! memory model (bounded by a per-thread instruction budget, the "fuel").

pub mod asm;
pub mod bytemap;
pub mod core;
pub mod decode;
pub mod elf;
pub mod hash;
pub mod image;
pub mod interleave;
pub mod isa;
pub mod litmus;
pub mod live;
pub mod memory;
pub mod outcome;
pub mod promising;
pub mod reduce;
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
    /// Prune memories that differ only in the order of writes nothing can
    /// observe the order of (see `reduce.rs`).
    pub por: bool,
    /// Perform accesses to memory that only one thread accesses like those
    /// to its stack: in the ARM model without write messages, and in the
    /// reference models as local steps (see `reduce::Reduction::private`).
    pub private: bool,
    /// Merge states that differ only in registers that are never read
    /// again (see `live`): in the ARM model's solo explorations, the
    /// reduction's pre-pass and the reference models.
    pub live: bool,
    /// Also prune memories by what the threads may do after each one
    /// (`reduce::Reduction::prunable_after`), where the partial-order
    /// reduction alone does not (with `por` only).
    pub dynamic_por: bool,
    /// In the ARM model, certify only the candidate promises that the
    /// thread's exploration shows can certify (see `Solo::promising` in
    /// `promising.rs`).
    pub filter: bool,
    /// Certify the candidates that `filter` rejects too, and panic if one
    /// certifies (a check of the filter).
    pub check_filter: bool,
    /// Instead of exploring every memory, follow this many random paths
    /// through them. The final states found are possible, but others may be
    /// missed.
    pub sample: Option<u64>,
    /// Seed for `sample`.
    pub seed: u64,
    /// For `sample`: how many times in each walk the thread whose promises
    /// come first changes for another (see `promising.rs`).
    pub pct_depth: u64,
    /// Stop the ARM model's exhaustive search after this many memories,
    /// setting `Outcomes::over_budget` (for tests that compare against a
    /// search that may be too big).
    pub max_memories: Option<u64>,
    /// In the ARM model, let each run of a thread fail a store-exclusive
    /// that could succeed at most this many times (None: without limit, as
    /// the architecture allows). A retry loop that also writes on every
    /// pass otherwise never repeats a state and runs until the fuel runs
    /// out. Behaviors that need more such failures are missed.
    pub spurious: Option<u32>,
    /// In the ARM model, let each run of a thread perform at most this many
    /// read-modify-writes to shared memory that write back the value they
    /// read (an atomic instruction, or a store-exclusive storing what its
    /// load-exclusive read), as a lock taken by exchanging a "busy" value
    /// does on each pass while another thread holds it (None: without
    /// limit). Each pass adds a write, so such a loop never repeats a state
    /// and runs until the fuel runs out. Behaviors that need more such
    /// passes are missed.
    pub stutter: Option<u32>,
}

impl Default for Options {
    fn default() -> Options {
        Options { fuel: 200, lse2: false, por: true, dynamic_por: true, private: true, live: true, filter: true, check_filter: false, sample: None, seed: 0, pct_depth: 1, max_memories: None, spurious: None, stutter: None }
    }
}

pub fn run(test: &litmus::Test, model: Model, opts: &Options) -> Outcomes {
    match model {
        Model::Arm => promising::explore(test, opts),
        Model::Tso => interleave::explore(test, interleave::Kind::Tso, opts.fuel, opts.private, opts.live),
        Model::Sc => interleave::explore(test, interleave::Kind::Sc, opts.fuel, opts.private, opts.live),
    }
}
