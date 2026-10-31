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

* Init block: `N:Xr=value-or-symbol`, `sym=value`. Each named symbol gets an
  address (`0x1000`, `0x1100`, ...). Its initial value is 0 unless given.
* Final section: optional `locations [...]`, then `exists`, `~exists` or
  `forall` with `/\`, `\/`, `~` and parentheses.
* The reported state contains the registers and locations named in the
  condition and in `locations`. Without either, it contains all named
  locations.

### Supported instructions

`MOV/MOVZ`, `ADD(S)`, `SUB(S)`, `AND(S)`, `ORR`, `EOR`, `LSL`, `LSR`, `ASR`,
`MUL`, `CMP`, `CMN`, `TST`, `CSEL`, `CSINC`, `CSET`, `B`, `B.cond`,
`CBZ/CBNZ`, `LDR`, `STR`, `LDAR`, `LDAPR`, `STLR`, `LDXR`, `LDAXR`, `STXR`,
`STLXR`, LSE atomics (`LDADD/LDCLR/LDEOR/LDSET/LD{S,U}{MAX,MIN}`,
`ST<op>`, `SWP`, `CAS`, each with `A`/`L`/`AL` variants), `DMB`
(`SY/ISH/...`, `LD`, `ST` variants), `DSB` (treated as DMB), `ISB`, `NOP`.

Addressing modes: `[Xn]`, `[Xn,#imm]`, `[Xn,Xm]`, `[Xn,Xm,LSL #k]`, and
`[Xn,Wm,SXTW|UXTW #k]`.

## How it works

* **ARM model** (`src/promising.rs`): the operational *Promising-ARM* model
  (Pulte et al., PLDI 2019), which is proven equivalent to the official
  ARMv8 axiomatic model. Memory is a list of write messages. Each thread
  keeps *views* (timestamps) that limit which messages a read can still
  return. Register values carry views, so address, data and control
  dependencies are tracked exactly, false dependencies included (e.g.
  `EOR W4,W0,W0`), as the architecture requires. Out-of-order writes are
  *promises*. A thread may only make a promise if it can certify it,
  i.e. show that running alone it can fulfil it later.
  Exploration is *promise-first*:
  1. enumerate every sequence of certified promises (memory states);
  2. for each one, run every thread alone to completion, fulfilling
     exactly its promises.

  In phase 2 the threads cannot influence each other, so the final states
  are the product of the per-thread results.
* **Reference models** (`src/interleave.rs`): SC and x86-TSO. These
  enumerate interleavings, with FIFO store buffers for TSO. A full `DMB`
  acts like `MFENCE`, and atomics act as locked operations. They run the
  same ARM program under an x86-like ordering, so any outcome they cannot
  produce is an *ARM-only* behaviour.
* **Fuel** bounds the instructions per thread per execution, so loops
  terminate. A state that recurs with no more fuel used is pruned, so
  spin loops cost almost nothing. If any execution runs out of fuel, the
  report includes a warning that the results may be incomplete.

Validation: `litmus/` holds 39 tests. They include the classic shapes (MP,
SB, LB, 2+2W, IRIW, WRC, S, CoRR, PPOCA/PPOAA, with fences, dependencies
and acquire/release) and lock and atomic examples. Their expected
ARMv8/TSO verdicts are in `litmus/expected.txt`, and `cargo test` checks
them. The tests also check that every SC outcome is reachable under TSO and
under ARM, and that results are deterministic.

## Limitations (current)

* All accesses to a location must be the same size. A location holds one
  64-bit value; W-register accesses read and write its low 32 bits as
  the whole value. Mixed-size and overlapping accesses are not modelled.
* No pre/post-indexed addressing, `LDP/STP`, `LDAR` byte/halfword forms,
  or SIMD/FP.
* The status register written by `STXR` carries no dependency.
* The search is exhaustive, so it suits small snippets. The number of
  promise orders grows quickly with the number of writes.
* The TSO reference treats `LDAR/STLR` as ordinary x86 loads and stores, so
  `STLR; LDAR` (ordered on ARM) can show as a TSO-only outcome.

## Roadmap

1. More of the ISA: pair loads/stores, byte/halfword accesses with a
   mixed-size memory model, pre/post-indexing, more ALU operations.
2. A binary instruction decoder producing the same `isa::Instr`, so real
   compiled code can be loaded (ELF), starting with user-mode functions
   extracted from real programs.
3. Scalable exploration for large programs: deterministic pseudo-random
   (seeded) sampling of promise/read choices, partial-order reduction,
   and prioritising reads that disagree with what TSO would return.
4. System-level emulation: exception levels, MMU/page tables, timers and
   interrupt controller, basic devices (UART, virtio), with the memory
   model applied to normal memory. Goal: boot Linux on a multi-core
   virtual machine and search for ARM-only behaviours.
