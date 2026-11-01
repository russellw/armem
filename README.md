# armem

An ARM (AArch64) emulator for finding concurrency bugs that appear only
because of ARM's weak **hardware** memory model. These are bugs that stay
hidden on x86, whose TSO memory model is much stronger.

Instead of picking one value for each memory read, armem keeps track of
**every value the ARMv8 architecture allows the read to return**. It then
explores all resulting executions. For each test it reports the reachable
final states and marks those that are impossible under x86-TSO (or SC).

This is about the hardware model only, not the C/C++ language memory model.

The emulator is deterministic. It never uses host threads, and its results do
not depend on the host architecture.

## Status

Early version: it runs small AArch64 snippets written as herd-style litmus
tests. The long-term goal is to emulate a whole server running Linux (see
[Roadmap](#roadmap)).

## Usage

```
cargo build --release
./target/release/armem litmus/MP.litmus            # full report, ARM vs TSO
./target/release/armem -s litmus/*.litmus          # one line per test
./target/release/armem -m arm,sc,tso -f 500 FILE   # choose models / fuel
./target/release/armem --lse2 FILE                 # assume FEAT_LSE2 hardware
cargo test --release                               # check the litmus suite
```

Example:

```
Test MP (ARM)
States 4
  1:X0=0; 1:X2=0;
  1:X0=0; 1:X2=1;
  1:X0=1; 1:X2=0;  [condition]  <-- not possible under TSO
  1:X0=1; 1:X2=1;
...
1 final state(s) possible on ARM but not under TSO. The condition is reachable on ARM only: likely ARM-specific bug.
```

### Test format

The format is the herd7/litmus7 AArch64 format:

```
AArch64 MP-spin
"Bug: consumer spins on a flag but reads data without acquire/barrier"
{ 0:X1=data; 0:X3=flag; 1:X1=flag; 1:X3=data; }
 P0          | P1          ;
 MOV W0,#42  | L0:         ;
 STR W0,[X1] | LDR W0,[X1] ;
 DMB SY      | CBZ W0,L0   ;
 MOV W2,#1   | LDR W2,[X3] ;
 STR W2,[X3] |             ;
exists (1:X2=0)
```

* Init block: `N:Xr=value-or-symbol`, `[type] sym=value`. Each named
  symbol gets an address (`0x1000`, `0x1100`, ...). Its initial value is 0
  unless given. The optional C type (`uint8_t`, `int16_t`, `int`,
  `uint32_t`, `int64_t`, `uint64_t`, pointers, ...) sets the size and
  signedness used to read the location's final value. The default is a
  signed 64-bit integer. Arrays are declared as `uint64_t x[2]` and their
  elements named `x[i]` in conditions. Values may be written
  `sym+offset`, and conditions may also refer to `[sym+offset]` (armem
  only; herd7 does not accept this form).
* Final section: optional `locations [...]`, then `exists`, `~exists` or
  `forall` with `/\`, `\/`, `~` and parentheses.
* The reported state contains the registers and locations named in the
  condition and in `locations`. Without either, it contains all named
  locations.

### Supported instructions

`MOV/MOVZ/MOVN`, `ADD(S)`, `SUB(S)`, `NEG(S)`, `AND(S)`, `ORR`, `EOR`,
`LSL`, `LSR`, `ASR`, `MUL`, `SXTW`, `CMP`, `CMN`, `TST`, `CSEL`, `CSINC`,
`CINC`, `CSET`, `B`, `B.cond`, `CBZ/CBNZ`, `NOP` (and the hints `YIELD`,
`WFE`, `WFI`, `SEV`, `SEVL`, run as `NOP`), and these memory accesses. ALU
instructions also accept a shifted or extended register operand, as in
`ADD X7,X6,W4,SXTW` or `CMP X0,X1,LSL #2`, and `ADD`/`SUB` accept
`#imm,LSL #12`.

* loads/stores: `LDR`, `STR`, `LDUR`, `STUR`, with byte/halfword forms
  (`LDRB`, `LDRH`, `STRB`, `STRH`) and sign-extending forms (`LDRSB`,
  `LDRSH`, `LDRSW`)
* pairs: `LDP`, `STP`, `LDNP`, `STNP`, `LDPSW`
* ordered and exclusive: `LDAR`, `LDAPR`, `STLR`, `LDXR`, `LDAXR`, `STXR`,
  `STLXR`, each with `B`/`H` forms
* exclusive pairs: `LDXP`, `LDAXP`, `STXP`, `STLXP` (W or X registers)
* LSE atomics: `LDADD/LDCLR/LDEOR/LDSET/LD{S,U}{MAX,MIN}`, `ST<op>`, `SWP`,
  `CAS`, each with `A`/`L`/`AL` ordering and `B`/`H` size variants
* barriers: `DMB` (`SY/ISH/...`, `LD`, `ST` variants), `DSB` (treated as
  DMB), `ISB`

Addressing modes: `[Xn]`, `[Xn,#imm]`, `[Xn,Xm]`, `[Xn,Xm,LSL #k]`,
`[Xn,Wm,SXTW|UXTW #k]`, pre-index `[Xn,#imm]!`, and post-index
`[Xn],#imm` (writeback forms for `LDR`/`STR`/`LDP`/`STP`).

Register combinations that the architecture makes CONSTRAINED
UNPREDICTABLE are rejected: for example, a writeback base register that is
also the data register, or an `STXR` status register that is also the data
or base register.

### Machine code

`src/decode.rs` decodes A64 machine code into the same instructions the
assembly parser produces (`decode::decode_program` for a block of
little-endian code). It covers exactly the instructions listed above.
Anything else, including UNPREDICTABLE encodings, is an error. This is
the first step toward running compiled code (see [Roadmap](#roadmap)).
Instructions that use SP (register 31 as a base register, or in
`ADD`/`SUB` immediate) are not supported yet, because the emulator has
no stack pointer.

`tests/decode_llvm.rs` checks the decoder against LLVM, when `llvm-mc`
is installed:
* every thread in `litmus/` is assembled with `llvm-mc`, and decoding it
  must give the program the assembly parser produces;
* random words from each relevant encoding class (96,000 by default; set
  `ARMEM_DECODE_SAMPLES` for more per class) are disassembled by
  `llvm-mc`, and the text is parsed by armem's assembler. The decoder
  must accept a word exactly when the parser accepts its disassembly,
  with the same result, and must reject every word LLVM rejects or flags
  as potentially undefined. The one documented exception: LLVM silently
  ignores a non-ones should-be-one `Rt2` field in `STXR`, which armem
  rejects. A run of 1.2 million words (409,568 decoded) found no other
  differences.

## How it works

* **ARM model** (`src/promising.rs`): the operational *Promising-ARM* model
  (Pulte et al., PLDI 2019), which was proven equivalent to the ARMv8
  axiomatic model of the time. Memory is a list of write messages. Each
  thread keeps *views* (timestamps) that limit which messages a read can
  still return. Register values carry views, so address, data and control
  dependencies are tracked exactly, false dependencies included (e.g.
  `EOR W4,W0,W0`), as the architecture requires. Out-of-order writes are
  *promises*. A thread may only make a promise if it can certify it,
  i.e. show that running alone it can fulfill it later.

  Arm's current model (`aarch64.cat` in herdtools7) has rules added since
  that paper, and armem follows them:
  * *Pick* dependencies (through `CSEL`'s condition flags or a `CAS`
    comparison) order later writes but not later reads. Registers carry
    a second view for these.
  * A *NoRet* atomic (one whose result register is `XZR`/`WZR`) is not
    ordered by `DMB LD`, and has no acquire semantics.
  * The write of an acquire+release atomic (`SWPAL`, `CASAL`, ...)
    orders all later accesses.
  * An acquire read of the thread's own exclusive or atomic write is
    ordered after that atomic's *read*, not after its write.
  * A successful `CAS` may take the result register's dependencies
    either from the memory read or from the old register value. herd7
    explores both alternatives, and so does armem.
  Exploration is *promise-first*:
  1. enumerate every sequence of certified promises (memory states);
  2. for each one, run every thread alone to completion, fulfilling
     exactly its promises.

  In phase 2 the threads cannot influence each other, so the final states
  are the product of the per-thread results. Candidate promises come from
  a solo exploration of the thread under the same model. A
  single-threaded test is run as SC, which is exact for a lone thread.
* **Mixed sizes**: memory is byte-addressed. Each write message has a
  footprint (address and size), and coherence and forwarding are tracked
  per byte. An aligned access is single-copy atomic: an aligned read
  takes effect at one point in the memory order, and each byte reads the
  latest write covering it at that point. So it can combine bytes from
  different writes, but never sees an aligned write torn. An unaligned
  access behaves as independent byte accesses (an unaligned store is
  split into separate byte writes). `LDP`/`STP` are two independent
  accesses with no ordering between them. A W-register `LDXP` is one single-copy-atomic 8-byte
  read. An X-register `LDXP` is atomic only per 8-byte half, so it can
  return a torn 128-bit value. Only a successful `STXP` of the pair
  guarantees the halves were consistent, as on real hardware. `STXP` is
  one 8- or 16-byte write.
* **FEAT_LSE2** (`--lse2`, off by default, i.e. base ARMv8.0): an
  `LDP`/`STP` of X registers aligned to 16 bytes, or of W registers
  aligned to 8 bytes, becomes one single-copy-atomic access. Any other
  non-exclusive access that stays within an aligned 16-byte block is
  single-copy atomic even if unaligned. Running a test with and without
  `--lse2` shows whether code relies on LSE2 atomicity.
  This mixed-size and LSE2 modeling is our own extension of
  Promising-ARM. The aligned mixed-size cases are checked against herd7's
  `-variant mixed` (see below). herd7 rejects unaligned accesses and has
  no LSE2 option, so those parts are checked only by our own tests.
* **Reference models** (`src/interleave.rs`): SC and x86-TSO. These
  enumerate interleavings, with FIFO store buffers for TSO. A full `DMB`
  acts like `MFENCE`, and atomics act as locked operations. They run the
  same ARM program under an x86-like ordering, so any outcome they cannot
  produce is an *ARM-only* behavior.
* **Fuel** bounds the instructions per thread per execution, so loops
  terminate. A state that recurs with no more fuel used is pruned, so
  spin loops cost almost nothing. If any execution runs out of fuel, the
  report includes a warning that the results may be incomplete.

Validation: `litmus/` holds 65 tests. They include the classic shapes (MP,
SB, LB, 2+2W, IRIW, WRC, S, CoRR, PPOCA/PPOAA, with fences, dependencies
and acquire/release), lock and atomic examples, and mixed-size and pair
tests (tearing, byte merging, mixed-size coherence, LDP/STP ordering),
exclusive pairs, FEAT_LSE2 atomicity, and the pick, NoRet and CAS
dependency rules. Their expected ARMv8/TSO verdicts are in
`litmus/expected.txt` (one line per test and option set; an optional
`lse2` column runs the test with `--lse2`), and `cargo test` checks them.
The tests also check that every SC outcome is reachable under TSO and
under ARM, and that results are deterministic.

## Validation against herd7

Arm's official memory model is the axiomatic `aarch64.cat`, which runs in
[herd7](https://github.com/herd/herdtools7). `scripts/compare_herd.py`
runs a set of litmus tests through both herd7 and armem and compares the
complete sets of final states:

```
scripts/install_herdtools.sh                 # herd7/diy7 into ~/.local/opt
cargo build --release
scripts/compare_herd.py -q litmus/*.litmus   # -q: only show disagreements
scripts/gen_diy.sh /tmp/diy 2 6              # generate tests with diy7
scripts/gen_diy.sh /tmp/diyat 2 5 atomics    # ... with atomics and CSEL
scripts/compare_herd.py -q /tmp/diy/*.litmus
```

Tests with byte/halfword or pair accesses run in herd7 with
`-variant mixed`. A test reports `MATCH` (same states), `MISMATCH`,
`KNOWN-DIFF` (a deliberate, documented difference), or `SUPERSET`
(a loop test where armem finds extra states because herd7 unrolls loops
only a bounded number of times).

Current results (herdtools7 7.58):

| Test set | Tests | Result |
|---|---|---|
| herdtools7 catalogue (`aarch64`, `aarch64-mixed`, `aarch64-pick`, `aarch64-down-one-leg`) | 216 | 213 match; herd7 fails on 3 |
| diy7, basic edges, 2 threads | 2949 | all match |
| diy7, basic edges, 3 threads | 5428 | all match |
| diy7, atomics/exclusives/CSEL, 2 threads | 1145 | all match |
| diy7, atomics/exclusives/CSEL, 3 threads | 5813 | all match |
| `litmus/` | 65 | 57 match, 1 known difference; herd7 times out or rejects 7 |

The known difference is `LDXP-torn`. herd7 treats an X-register `LDXP` as
a 128-bit single-copy-atomic read. The Arm ARM guarantees this only when
a matching `STXP` succeeds, and GCC and LLVM rely on that rule, so armem
lets a lone `LDXP` be torn.

## Limitations (current)

* No SIMD/FP.
* The TSO and SC reference models read both halves of an `LDP` in a
  single step. They perform an `STP` as two consecutive stores, and
  ignore `--lse2`.
* The status register written by `STXR` carries no dependency (as in
  Arm's model).
* The search is exhaustive, so it suits small snippets. The number of
  promise orders grows quickly with the number of writes.
* The TSO reference treats `LDAR/STLR` as ordinary x86 loads and stores, so
  `STLR; LDAR` (ordered on ARM) can show as a TSO-only outcome.

## Roadmap

1. More of the ISA: more ALU operations, SIMD/FP loads/stores.
2. Run real compiled code: the binary decoder is done (see
   [Machine code](#machine-code)); next come an ELF loader, SP and a stack,
   calls and returns, and the instructions compilers commonly emit
   (`ADRP`, `MOVK`, `TBZ`, `UBFX`, ...), starting with user-mode
   functions extracted from real programs.
3. Scalable exploration for large programs: deterministic pseudo-random
   (seeded) sampling of promise/read choices, partial-order reduction,
   and prioritizing reads that disagree with what TSO would return.
4. System-level emulation: exception levels, MMU/page tables, timers and
   interrupt controller, basic devices (UART, virtio), with the memory
   model applied to normal memory. Goal: boot Linux on a multi-core
   virtual machine and search for ARM-only behaviors.
