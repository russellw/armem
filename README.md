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
./target/release/armem --elf data.o FILE           # start from an ELF file's memory
./target/release/armem --sample 100 FILE           # 100 random paths, for big tests
./target/release/armem --no-por FILE               # without partial-order reduction
./target/release/armem --no-private FILE           # without private memory
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

`MOV/MOVZ/MOVN`, `MOVK`, `ADD(S)`, `SUB(S)`, `NEG(S)`, `CMP`, `CMN`, and
`B`, `B.cond`, `CBZ/CBNZ`, `TBZ/TBNZ`, calls and returns (`BL`, `BLR`, `BR`,
`RET`; see [Calls](#calls)), `NOP` (and the hints `YIELD`, `WFE`, `WFI`,
`SEV`, `SEVL`, run as `NOP`), and:

* logical: `AND(S)`, `ORR`, `EOR`, `TST`, and with the second operand
  inverted `BIC(S)`, `ORN`, `EON`, `MVN`
* shifts and rotates: `LSL`, `LSR`, `ASR`, `ROR` (immediate or register),
  and `EXTR`
* bit and byte manipulation: `RBIT`, `REV16`, `REV32`, `REV` (`REV64`),
  `CLZ`, `CLS`
* add and subtract with carry: `ADC(S)`, `SBC(S)`, `NGC(S)`
* multiply and divide: `MUL`, `MADD`, `MSUB`, `MNEG`; the long forms
  `SMADDL`, `SMSUBL`, `UMADDL`, `UMSUBL`, `SMULL`, `UMULL`, `SMNEGL`,
  `UMNEGL`; `SMULH`, `UMULH`; and `UDIV`, `SDIV` (division by zero gives 0)
* bitfield moves: `UBFM`, `SBFM`, `BFM` and their aliases `UBFX`, `SBFX`,
  `UBFIZ`, `SBFIZ`, `BFI`, `BFXIL`, `BFC`, `UXTB`, `UXTH`, `SXTB`, `SXTH`,
  `SXTW`
* conditional: `CSEL`, `CSINC`, `CSINV`, `CSNEG` and their aliases `CINC`,
  `CINV`, `CNEG`, `CSET`, `CSETM`; and the conditional compares `CCMP`,
  `CCMN` (register or 5-bit immediate)
* memory accesses, below.

ALU instructions also accept a shifted or extended register operand, as
in `ADD X7,X6,W4,SXTW` or `CMP X0,X1,LSR #2` (`LSL`, `LSR`, `ASR`, and for
logical instructions `ROR`), and `ADD`/`SUB` accept `#imm,LSL #12`.

herd7 (7.58) does not accept the bitfield moves, `UDIV`/`SDIV`, `SMULH`/
`UMULH`, `CCMP`/`CCMN`, `CNEG`, `CLZ`/`CLS`, add/subtract with carry or
most of the SIMD&FP instructions, so tests that use them run only in
armem. It also computes some results
differently from the architecture: `UMULL`/`UMSUBL` as if signed, `EON`
as `EOR`, some W-register results sign-extended rather than
zero-extended, and X-register `REV`, `RBIT` and `EXTR` results as if
only 32 bits wide. The tests it checks, `ALU-mul-logic` and
`LB+rev-extr-datas`, avoid these.

Like `CSEL`, a `CCMP` gives the flags it sets a data dependency on its
operands (when its condition holds) and only a pick dependency on the
flags that it tested. `ADC` and `SBC` compute with the carry flag, so
their result has a data dependency on the flags.

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
* `CLREX`, which clears the exclusive monitor
* SIMD&FP registers (`V0`-`V31`, also named `Bn`, `Hn`, `Sn`, `Dn`, `Qn`
  for their low 1 to 16 bytes); see [SIMD&FP registers](#simdfp-registers)

Addressing modes: `[Xn]`, `[Xn,#imm]`, `[Xn,Xm]`, `[Xn,Xm,LSL #k]`,
`[Xn,Wm,SXTW|UXTW #k]`, pre-index `[Xn,#imm]!`, and post-index
`[Xn],#imm` (writeback forms for `LDR`/`STR`/`LDP`/`STP`).

The stack pointer `SP` (`WSP`) can be used where the architecture allows
it: as a base register, as the first source of `ADD`/`SUB`/`ADDS`/`SUBS`
and `CMP`/`CMN`, as the destination of `ADD`/`SUB` and of
`AND`/`ORR`/`EOR` with an immediate, and in `MOV` to or from SP (an alias of
`ADD #0`). See [Stacks](#stacks).

### SIMD&FP registers

Compilers use SIMD&FP registers even in integer code: for `memset`,
`memcpy` and struct copies, for popcount, and at `-O0` to spill
`__int128` values. armem supports:

* loads and stores: `LDR`, `STR`, `LDUR`, `STUR` of `B`, `H`, `S`, `D`
  and `Q` registers, and `LDP`, `STP`, `LDNP`, `STNP` of `S`, `D` and `Q`,
  with the same addressing modes as general-purpose registers (a register
  offset is shifted by 0 or by the log2 of the access size);
* moves: `FMOV` between general-purpose and `S`, `D` or `Vn.D[1]`
  registers, and between `S` or `D` registers; `MOVI` and `MVNI` (every
  form); `DUP` from a general-purpose register or an element; `INS`
  (`MOV Vd.T[i], ...`) from a general-purpose register or an element;
  `UMOV` and `SMOV` (`MOV Wd, Vn.S[i]`, `MOV Xd, Vn.D[i]`); `MOV` of a
  scalar from an element; and `MOV Vd.T, Vn.T`;
* vector logic: `AND`, `BIC`, `ORR`, `ORN`, `EOR` (`8B` or `16B`);
* `CNT`, `ADDV` and `UADDLV` on bytes (`8B` or `16B`), which compilers use
  for popcount.

There is no floating-point or other vector arithmetic yet. Dependencies
are tracked per register, as for general-purpose registers: inserting an
element makes the whole register depend on the inserted value and on the
register's old value.

A load or store of a `B`, `H`, `S` or `D` register is single-copy atomic
when aligned, like a general-purpose one. A `Q` load or store is two
single-copy-atomic 64-bit accesses (when 8-byte aligned), so another
thread can see half of it (`LDR-Q-torn`). That is the Arm ARM's rule.
`--lse2` makes each half atomic even if unaligned within a 16-byte
block, but never the whole `Q` access, and an `LDP`/`STP` of SIMD&FP
registers is never one atomic access. This is the conservative reading:
it may report tearing that some LSE2 hardware rules out, but not miss
any.

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
Register 31 is decoded as SP or the zero register, depending on the
operand. `ADR` and `ADRP` are decoded when the code's load
address is known (`decode::decode_program_at`), as a `MOV` of the
address they compute.

### ELF files

`src/elf.rs` reads 64-bit little-endian AArch64 ELF files: relocatable
objects (`.o`), executables and shared objects. It has no dependencies.
It exposes the sections, loadable segments, symbol table and
relocations, and `Elf::function(name)` / `Elf::decode_function(name)`
extract a function's code by symbol name (using the symbol's size) and
decode it. Branches, including calls (`BL`), must stay inside the function. Every offset and size
in the file is checked, so a malformed file gives an error, not a panic.

A function is rejected, rather than decoded wrongly, when:
* read directly from an object file, an instruction in it still needs a
  relocation (a call, or the address of a global, is a placeholder until
  link time); load the file as an image instead (below);
* it contains data, such as a literal pool, marked by a `$d` mapping
  symbol.

`src/image.rs` loads a file into memory (`Image::load`). An object file
is linked on its own, like a static link. Allocated sections are placed
in order from a base address, or at addresses given by name. Common
symbols get space, and a GOT is built if the code uses one. The caller
gives addresses for the symbols the file uses but does not define. The
relocations are then applied. `Image::function` / `decode_function` give
a function's relocated code at its final address, and `Image::code`
decodes all of its executable memory for threads to run (see below). These relocations are
supported:
* data: `ABS64/32/16`, `PREL64/32/16`, `PLT32`;
* instructions: `ADR_PREL_PG_HI21(_NC)`, `ADR_PREL_LO21`,
  `ADD_ABS_LO12_NC`, `LDST{8,16,32,64,128}_ABS_LO12_NC`, `CALL26`,
  `JUMP26`, `CONDBR19`, `TSTBR14`, `LD_PREL_LO19`, `MOVW_UABS_G0..G3`;
* GOT: `ADR_GOT_PAGE`, `LD64_GOT_LO12_NC`.

As in lld, values are checked for range and alignment, and an undefined
weak symbol is 0 for absolute relocations, the place itself for
PC-relative ones, and the next instruction for branches. Any other
relocation (TLS, for example) is an error. A linked executable or
shared object is loaded from its `PT_LOAD` segments. Its dynamic
relocations are not applied yet.

### Starting from an ELF file's memory

`--elf FILE` loads an ELF file (relocating an object file, placed at
`--elf-base`, default `0x10000`) and uses its memory as every test's
initial memory. The file's data symbols (objects and common symbols)
become named locations, so a test can use them like its own:

```
AArch64 MP-elf
{ 0:X1=x; 0:X3=y; 1:X1=y; 1:X3=x; }
...
```

Values come from the file. A pointer initialized to `&y` holds y's
address and is reported as `y`. A symbol of 1, 2, 4 or 8 bytes is read
as a signed integer of that size. In the init block, `x=5` overrides the
file's value, and a declaration without a value (`uint8_t x`) only
changes the type. Other names get fresh locations, which must not
overlap the file's memory. In the library this is `Image::preload` and
`litmus::parse_with`. Memory permissions are not enforced: a store to
read-only data or code succeeds.

A thread can run a function of the file: write `P0 = name` in the
thread header and leave that thread's column empty (an armem extension;
herd7 does not accept it). Arguments go in registers as usual
(`0:X0=...`), and the return value can be checked in `X0`. For example,
with this C compiled by `clang --target=aarch64-linux-gnu -O2 -fno-pic
-c mp.c`:

```c
int data, flag;
void writer(void) { data = 42; __atomic_store_n(&flag, 1, __ATOMIC_RELAXED); }
int reader(void) { while (!__atomic_load_n(&flag, __ATOMIC_RELAXED)) {} return data; }
```

```
AArch64 MP-c
{ }
 P0 = writer | P1 = reader ;
exists (1:X0=0)
```

`armem --elf mp.o MP-c.litmus` reports that the reader can return 0 on
ARM only. With `__ATOMIC_RELEASE` / `__ATOMIC_ACQUIRE` it cannot.

Such a thread runs the image's code: all of its executable memory, from
the lowest executable address to the highest, so it can call other
functions directly (`BL`) or through pointers (`BLR`). The code is decoded
once, when the file is loaded, and a word that cannot be decoded (an
unsupported instruction, data marked by a `$d` mapping symbol, or a gap
between sections) is a fault only if a thread runs it. Code is not reread
from memory, so self-modifying code is not supported. A thread returns
from its function to an exit address just past the code (see
[Calls](#calls)), which ends it. A thread that branches to an address
outside the code, for example through a bad return address, faults.
`tests/preload.rs` runs the example above at `-O0` and `-O2`, and a
spinlock whose functions call each other at `-O0`, `-O2` and
`-O2 -mcmodel=large`. It also runs integer arithmetic (bitfields,
division by variables and constants, conditional selects and compares,
byte and bit reversal, leading-bit counts, rotates, 128-bit
arithmetic and popcount) compiled at `-O0` to `-Os`, and checks the
result against the same C compiled for the host and run natively. A
last test publishes a 16-byte struct, which clang copies through `Q`
registers at `-O0` and `-O2`: with a relaxed flag the reader can copy a
stale or torn struct on ARM only.

`tests/elf_llvm.rs` checks the loader against LLVM, when `llvm-mc` and
`llvm-readobj` are installed: every litmus thread is assembled into an
object file as a function, and extracting and decoding it must give the
parsed program. The sections, symbols and relocations armem reads must
match `llvm-readobj` for assembled objects, for `clang` output (`-O2`,
and `-O0 -g` with per-function sections), and for an object with more
than 0xff00 sections (extended section numbering).

`tests/image_lld.rs` checks relocation against lld, when `llvm-mc` and
lld are installed (`ld.lld`, `$ARMEM_LLD`, or the `rust-lld` that comes
with Rust toolchains). Each object is loaded by armem, then linked by lld
with a linker script that uses the same section, GOT and external symbol
addresses. Every section's bytes and every symbol's address must agree,
and each function must decode the same from both. The objects are an
assembly file using every supported relocation type (in a compact layout
and with data more than 1 GiB from the code) and `clang` output (`-O0`,
`-O2`, per-function sections, `-mcmodel=large`, `-fPIC`), each at two
base addresses.

`tests/decode_llvm.rs` checks the decoder against LLVM, when `llvm-mc`
is installed:
* every thread in `litmus/` is assembled with `llvm-mc`, and decoding it
  must give the program the assembly parser produces;
* random words from each relevant encoding class (204,000 by default; set
  `ARMEM_DECODE_SAMPLES` for more per class) are disassembled by
  `llvm-mc`, and the text is parsed by armem's assembler. The decoder
  must accept a word exactly when the parser accepts its disassembly,
  with the same result, and must reject every word LLVM rejects or flags
  as potentially undefined. The one documented exception: LLVM silently
  ignores a non-ones should-be-one `Rt2` field in `STXR`, which armem
  rejects. A run of 2 million words (750,468 decoded) found no other
  differences.

### C programs

`c/` holds small lock-free C programs, each with a litmus test
(`c/NAME.litmus`) whose condition is the bug the program must avoid:

* `seqlock.c`: a sequence lock. A writer updates a two-word record and a
  reader copies it, retrying until it sees the same even sequence number
  before and after. The condition is a torn copy.
* `spsc.c`: a single-producer, single-consumer ring buffer. The condition
  is the consumer receiving anything but 1, then 2.
* `dcl.c`: double-checked initialization. Two threads each get a shared
  object. The fast path checks without a lock whether it is published;
  otherwise a thread takes a compare-and-swap spinlock, checks again, and
  constructs and publishes the object if no one has. The object is
  published through a pointer, or with `-DFLAG` through a flag. The
  condition is a thread reading anything but the constructed value, or
  both threads constructing the object.
* `refcount.c`: reference counting. Two threads share an object, each
  holding a reference. Each uses the object, then drops its reference
  with a release decrement, and the one that drops the last reference
  runs an acquire fence and destroys the object: it checks that both
  uses happened, then frees it. The condition is a use after the object
  is freed, or the destructor missing a use.
* `treiber.c`: a Treiber stack, a linked list whose top changes only by
  compare-and-swap. One thread pushes 1, then 2, and another pops once.
  The condition is the pop returning a value never pushed, or the stack
  left with anything but the nodes not popped (a lost node).

Each program is correct as written. Macros name the orderings it relies
on, and defining one as `__ATOMIC_RELAXED` removes it:

```
clang --target=aarch64-linux-gnu -O2 -fno-pic -c -DW_FENCE=__ATOMIC_RELAXED c/seqlock.c
armem --elf seqlock.o c/seqlock.litmus
```

`tests/c_programs.rs` compiles them with clang and checks the verdicts in
`c/expected.txt`, when clang is installed. All the variants give the same
verdicts at `-O0`, `-O1`, `-Os` and `-O2`, except where noted:

| Program | Ordering removed | ARM | TSO |
|---|---|---|---|
| seqlock | none | no | no |
| seqlock | writer's fence after the odd sequence number (`W_FENCE`) | yes | no |
| seqlock | writer's release of the even sequence number (`W_SEQ`) | yes | no |
| seqlock | reader's acquire of the first sequence number (`R_SEQ`) | yes | no |
| seqlock | reader's fence before the second sequence number (`R_FENCE`) | yes | no |
| spsc | none | no | no |
| spsc | producer's release of `head` (`PUB`) | yes | no |
| spsc | consumer's release of `tail` (`FREE`) | yes | no |
| spsc | consumer's acquire of `head` (`SEE`) | yes | yes at `-O2`, else no |
| spsc | producer's acquire of `tail` (`ROOM`) | no | no |
| dcl | none | no | no |
| dcl | release of the object's pointer or flag (`PUB`) | yes | no |
| dcl | fast path's acquire of the pointer (`SEE`) | no | no |
| dcl | fast path's acquire of the flag (`SEE`, `-DFLAG`) | yes | no |
| dcl | lock's acquire (`LOCK`) | yes, but no at `-O0` | no |
| dcl | lock's release (`UNLOCK`) | yes | no |
| refcount | none | no | no |
| refcount | release of the decrement (`DEC`) | yes, but no at `-O0` | no |
| refcount | acquire fence before destruction (`ACQ`) | yes, but no at `-O0` | no |
| treiber | push's release (`PUSH`) | yes, but no at `-O0` | no |
| treiber | pop's acquire (`POP`) | no | no |

Two results are worth a note. With `SEE` relaxed, clang `-O2` reuses its
first read of `buf[0]` for the second item, which C allows, so the
compiled code is wrong on every machine. armem runs the binary and so
finds compiler-introduced bugs too. With `ROOM` relaxed, the producer's
store to the slot is control dependent on its load of `tail`, and ARM never
makes a store visible before a load it depends on, so the hardware cannot
show the bug. The program still has a data race in C.

In `dcl`, which dependency the read of the value has decides `SEE`.
Published through a pointer, the object's value is read through the
pointer, and that address dependency keeps the read in order on ARM even
without the acquire. Published through a flag, only a control dependency
(the branch on the flag) comes before the read, and ARM may perform the
read early.

In `refcount` with `ACQ` relaxed, only the branch on the count orders the
destructor after the other thread's use. ARM may perform the destructor's
reads early, so it can miss the use, but it never makes the destructor's
write of the freed value visible early, since a store waits for the
branches before it.

clang `-O0` compiles every atomic read-modify-write (compare-and-swap and
`__atomic_fetch_sub` included) with `LDAXR`/`STLXR`, whatever the order
asked for. So at `-O0`, `dcl`'s lock acquires even with `LOCK` relaxed,
and `refcount`'s decrement acquires and releases, which makes all its
variants correct, as does `treiber`'s push.

In `treiber`, `POP` is harmless on ARM for the same reason as `dcl`'s
`SEE` with a pointer: the pop reads the node through the pointer it read
from `top`. The node's link is a relaxed atomic, as in most C11
implementations. As a plain field, clang `-O1` and above moves the
push's store of it out of the retry loop, after the compare-and-swap
that publishes the node, and with `PUSH` relaxed that loses nodes on
every machine.

A condition compares a location with a value, and a symbol on the right
is an address: `out_a=out_b` means "`out_a` holds the address of
`out_b`". To ask whether two locations differ, list the cases, as
`seqlock.litmus` does.

The seqlock and ring buffer tests take at most 0.3 s each. Larger rings
take longer, mostly because of the number of promise memories when
`head` is relaxed: with one slot and three items, 70 memories in 0.1 s;
with two slots and three items, 583 memories in 0.9 s (`tail` relaxed:
44 memories in 0.1 s).

The `dcl` tests take at most 0.2 s when correct, and up to 1.4 s (at
`-O0`) with the lock's release relaxed. At `-O2` that variant takes 0.6
s; before the reduction's pre-pass kept whole values (see [How it
works](#how-it-works)), it took 21 s. Two choices keep them that fast.
Each thread records whether it constructed the object in its own
variable, which is private memory: counting constructions in one shared
counter instead took the correct program from 47 memories to 623, and
the `UNLOCK` variant beyond 5 minutes, because the two threads' read-
modify-writes of the counter multiply the memories. And the lock uses
compare-and-swap rather than exchange: a failed exchange still writes,
so each turn of a spinning thread adds a write (with exchange, the
correct program took over 10 minutes).

The `refcount` tests take under 1 s each. The correct `treiber` takes
0.3 s at `-O2` (315 memories) and 0.6 s at `-O0`. With `PUSH` relaxed,
nothing orders the pusher's writes, and the popper sees them in many
orders: the exhaustive search explores 7,824 memories in 9 s. For
tests too big for that, a line of `expected.txt` may give `sample=N`
(`N` random paths), but only when the condition is expected to be
reachable, since sampling may miss states.

### Calls

`BL` and `BLR` put the address of the next instruction in X30, and `BR`,
`BLR` and `RET` branch to the address in a register. Every instruction
therefore has an address. An ELF function's code is at its loaded address.
Thread `i`'s assembly code is at `0x7e0000000000 + i * 0x1000000`. These
addresses appear only in registers, since a test's code is not in memory.
A branch to an address in a register is a control dependency on that
register, like `CBZ`.

Each thread starts with X30 holding its code's exit address, just past
the last instruction, unless the init block sets X30. So a `RET` from the
code a thread starts in ends the thread, and so does running off the end
of the code. A function that calls others saves X30 first, usually on the
stack:

```
 P0             ;
 BL f           ;
 r0:            ;
 B end          ;
 f:             ;
 LDR W0,[X1]    ;
 RET            ;
 end:           ;
```

herd7 accepts `BL` and `RET`, given a label after each `BL` (`r0:` above),
as long as return addresses stay in registers: it cannot follow one that
is saved to memory.

### Stacks

Each thread has its own 1 MiB stack, and SP starts at its top. Thread
`i`'s stack is `[0x7f0000000000 + (i+1) * 0x100000, ... + 0x100000)`. The
stack area also has an unused guard slot below the first stack and above
the last. A preloaded image or a test location that overlaps the stack
area is an error. A stack is initially zero.

A stack is private to its thread, so plain loads and stores within it
(including those of SIMD&FP registers) are not given to the memory
model. The core performs them directly: a
load reads the thread's latest store to each byte. The value keeps the
address and data dependencies of the store, as a load that is forwarded
a store's value does in Arm's model (`(addr | data); rfi` orders it). The
address of a stack access still orders later writes (Arm's
`addr; po; [W]`). A store still counts as a write for barriers: after
`DMB ST`, later writes are ordered after it, and so after the reads its
address and data depend on. The ARM model treats it as a write placed as
early in memory as it may be, right after everything it must follow. No
other thread can see these accesses, so this changes no outcome. It keeps
compiled code's spills and stack frames from multiplying the states the
models explore. (Before this rule, a stack store between a load and
`DMB ST` lost that ordering, and armem could report an ARM-only outcome
that the architecture forbids.) `tests/stack.rs` checks
this: each test runs again with its stacks replaced by ordinary memory
locations that only one thread uses, and every model must give the same
final states.

An execution *faults* when a thread:
* accesses the stack area outside its own stack (a stack overflow, a
  guard slot, or another thread's stack, for example through a pointer
  it was given), or
* accesses its stack with an exclusive, atomic, acquire or release
  instruction.

A faulting execution has no final state. The report lists each fault,
with the thread (`FAULT: P1: 8-byte access at 0x7f00001ffff8 is outside
its stack ...`), and the one-line summary marks the model with `(fault)`.

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
  a solo exploration of the thread under the same model, the same one
  that finds its final states: those of its runs that fulfill all its
  promises without new writes. A
  single-threaded test is run as SC, which is exact for a lone thread.

  A solo exploration takes a thread's local steps (register operations,
  branches and private stack accesses) at once, from one memory
  operation to the next, rather than making each one a state of the
  search, since each has exactly one successor. It expands states in
  order of steps used, so a state is first expanded with the fewest
  steps it can be reached in and never needs to be expanded again. On
  compiled code at `-O0` most instructions are local steps. With the
  widening in the reduction's pre-pass (below), this cut the `-O0`
  seqlock of [C programs](#c-programs) from 20-63 s to 1-10 s, and
  private memory (below) to 0.1-0.3 s.

  A solo exploration drops any state with a promise it can no longer
  fulfill. Fulfilling promise `t` needs the thread's write views and the
  coherence view of each of its bytes to be below `t`, and these never
  decrease, so once one reaches `t` no run from that state certifies or
  completes. Without this, a failed certification explored every
  reachable state before giving up, and most certifications fail: on
  the Treiber stack with its push's release relaxed, about 5 in 6. That
  test now takes 15 s instead of 6 minutes 43 s (the same 7,824
  memories), and the ring buffer with `head` relaxed, 2 slots and 3
  items, 1.7 s instead of 17 s.

  The visited sets of the solo explorations, the reduction's pre-pass
  and the reference models use a fast hasher of their own (`src/hash.rs`,
  after `rustc-hash`) rather than the standard library's SipHash, and
  look each state up once. Hashing whole thread states had taken half
  the time; the relaxed Treiber stack went from 15 s to 9 s.
* **Partial-order reduction** (`src/reduce.rs`, on by default; `--no-por`
  turns it off). Every write another thread can see is a promise, so
  `n` writes that nothing orders give `n!` memories, although the order of
  writes to different locations usually cannot be observed. In Arm's
  axiomatic model the memory order only has to extend the
  *ordered-before* relation, so for each execution armem explores only
  the least such order under a fixed priority on writes: a memory whose
  last two writes are out of priority order is skipped when nothing can
  order the first before the second. Whether something can is decided
  from a sound pre-pass. It runs each thread alone with every load
  returning any value some write may have stored, and finds what each
  thread may read and where it uses acquires, releases, atomics,
  barriers and dependencies. For example, two threads storing to four
  locations each explore 2,242 memories in 2 s instead of 109,601 in 5
  minutes. A compiled writer publishing a 32-byte struct to a reader
  with release/acquire takes 0.6 s instead of 50 s. With relaxed
  atomics, which previously did not finish in 10 minutes, it takes 11 s.
  The argument (that any order extending ordered-before will do, also
  with armem's mixed-size extension) is our own and not machine-checked,
  so `tests/por.rs` checks that the reduction never changes the final
  states: on every litmus test, with and without `--lse2`, and on random
  tests mixing sizes, barriers, dependencies, acquire/release, exclusives
  and atomics. `cargo test` runs 200 small random tests; larger ones,
  which can take hours, run with
  `cargo test --release --test por -- --ignored` and are worth running
  after changing the model or the reduction. The pre-pass widens a
  byte whose set of values grows in three rounds in a row (a counter,
  such as a sequence number) to any value, rather than adding values
  round after round. A byte that only one thread writes is read by that
  thread exactly as it last wrote it (by coherence, no other write to it
  exists), so the indices and counters a thread owns stay precise; this
  is what lets the analysis bound `buf[head % N]` in a ring buffer.
  (It keeps such values only for bytes the thread reads; when a run
  reads one it dropped, the thread's run is repeated.) The pre-pass also
  keeps the whole values written with each footprint. An aligned read
  of bytes that every write covers with the read's own footprint reads
  them all from one write (or initial memory), since an aligned access
  is single-copy atomic, so it returns only those values. Byte by byte,
  a pointer that may be null or point to one of two nodes would combine
  into many addresses no write stored. Reading through those found yet
  more values until the bytes were widened, and the pre-pass gave up on
  the [Treiber stack](#c-programs). Being more precise, it also cut the
  correct double-checked initialization at `-O2` from 178 memories to
  47, all of them now from private memory rather than the reduction.
  Besides barriers,
  a thread orders reads by dependencies: by an address dependency, by
  reading back a write whose address or data depends on a read, and
  after an acquire so ordered. Where a thread may do so, the pre-pass
  runs it again once for each instruction that may read another
  thread's writes, tracking which later reads depend on that one, so a
  dependency that orders only a ring buffer's index reads does not count
  as ordering its data reads. If
  the pre-pass gets too big, or a thread accesses memory at an address it
  cannot bound, it gives up and nothing is pruned.
* **Private memory** (on by default; `--no-private` turns it off). Every
  write another thread might see is a promise, and memories multiply with
  each one, its value included. But a C program's results, and variables
  only one thread uses, are often in memory that no other thread
  accesses. The reduction's pre-pass finds each thread's private memory:
  bytes that no other thread may read or write, and that the thread
  accesses only with plain loads and stores. A byte is private only if
  every access to it, and every location the test observes, lies wholly
  in private memory or wholly outside it. The core performs accesses to
  it like those to the stack (see [Stacks](#stacks)), so they make no
  write messages, and a private location's final value comes from its
  thread. A private store is placed as early as it may be, which allows
  every behavior a later place would, since views only bound what a
  thread may do. No other thread can see it, so its place matters to
  nothing else. On the ring buffer with one slot and three items, with
  `head` relaxed, this cut 538 memories to 197 (the consumer's `got`
  results no longer make memories), and with four items and `tail`
  relaxed, 301 memories to 13. `tests/por.rs` checks it too: on every
  litmus test and random test, the final states with private memory
  alone, and with private memory and the reduction, must equal those
  with neither.
* **Sampling** (`--sample N`, with `--seed`): for tests too big to explore
  exhaustively, armem follows `N` random paths through the promise
  memories instead of all of them. Each path adds random certified
  promises until none is left, and backtracks from dead ends. Every final
  state reported is reachable, but others may be missed, so a condition
  reported `Never` only means the walks did not find it. The report says
  the results are sampled.
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
  enumerate interleavings, with FIFO store buffers for TSO. Only
  shared-memory operations are interleaving points: a thread's local
  steps (register operations, branches and private stack accesses)
  commute with everything the other threads do, so after each step a
  thread runs on through them at once. Compiled code at `-O0` is mostly
  local steps, and this takes the reference models from minutes to
  milliseconds on it. A full `DMB`
  acts like `MFENCE`, and atomics act as locked operations. They run the
  same ARM program under an x86-like ordering, so any outcome they cannot
  produce is an *ARM-only* behavior.
* **Fuel** bounds the instructions per thread per execution, so loops
  terminate. A state that recurs with no more fuel used is pruned, so
  spin loops cost almost nothing. If any execution runs out of fuel, the
  report includes a warning that the results may be incomplete.

Validation: `litmus/` holds 85 tests. They include the classic shapes (MP,
SB, LB, 2+2W, IRIW, WRC, S, CoRR, PPOCA/PPOAA, with fences, dependencies
and acquire/release), lock and atomic examples, and mixed-size and pair
tests (tearing, byte merging, mixed-size coherence, LDP/STP ordering),
exclusive pairs, FEAT_LSE2 atomicity, the pick, NoRet and CAS
dependency rules, and SIMD&FP values, `Q` tearing and dependencies
through SIMD&FP registers. Their expected ARMv8/TSO verdicts are in
`litmus/expected.txt` (one line per test and option set; an optional
`lse2` column runs the test with `--lse2`), and `cargo test` checks them.
The tests also check that every SC outcome is reachable under TSO and
under ARM, that results are deterministic, and that the partial-order
reduction and sampling never change or invent final states.

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
| `litmus/` | 85 | 65 match, 1 known difference; herd7 times out or rejects 19 |

The known difference is `LDXP-torn`. herd7 treats an X-register `LDXP` as
a 128-bit single-copy-atomic read. The Arm ARM guarantees this only when
a matching `STXP` succeeds, and GCC and LLVM rely on that rule, so armem
lets a lone `LDXP` be torn.

## Limitations (current)

* No floating-point arithmetic, and of the other SIMD instructions only
  those listed in [SIMD&FP registers](#simdfp-registers).
* The TSO and SC reference models read both halves of an `LDP`, and all
  of a `Q` load, in a single step. They perform an `STP` as two
  consecutive stores and a `Q` store as one, and ignore `--lse2`.
* The status register written by `STXR` carries no dependency (as in
  Arm's model).
* Stacks are private: a thread cannot access another thread's stack
  (that is reported as a fault), and stack memory cannot be used for
  atomics.
* The exhaustive search suits tests with tens of shared writes, not
  thousands. The reduction removes orders nothing can observe, and
  private memory removes writes no other thread can see, but writes that
  other threads read in an order they can observe still multiply the
  memories, and so does each prefix of every memory. So do a shared
  counter that several threads increment, and a spinlock that spins on
  an exchange, which writes on every turn (see [C programs](#c-programs)).
  Beyond that, `--sample` gives up completeness. Each memory
  also costs a solo exploration of every thread, which grows with
  the threads' own branching.
* The TSO reference treats `LDAR/STLR` as ordinary x86 loads and stores, so
  `STLR; LDAR` (ordered on ARM) can show as a TSO-only outcome.

## Roadmap

1. More of the ISA: floating-point and vector arithmetic.
2. Run real compiled code: the binary decoder, an ELF loader and
   static relocation are done, and a loaded image can be a test's initial
   memory (see [Machine code](#machine-code)); tests can run ELF functions as
   threads (`P0 = name`), with SP and a private stack per thread, and
   they can make calls (`BL`, `BLR`, `RET`) and use the integer
   instructions compilers commonly emit (bitfield moves, multiply-add,
   division, conditional compare, byte reversal, add with carry, ...),
   and the SIMD&FP loads, stores and moves that integer code uses (see
   [SIMD&FP registers](#simdfp-registers)). Lock-free C programs (a
   seqlock, a ring buffer, double-checked initialization, reference
   counting and a Treiber stack) run unchanged, and armem finds the
   ARM-only bugs in their weakened versions (see [C programs](#c-programs));
   next come functions extracted from real code.
3. Scalable exploration for large programs: seeded sampling and a
   partial-order reduction of promise orders are done (see
   [How it works](#how-it-works)), and so are taking local steps at
   once and in order of steps used, private memory, and finding which
   reads each dependency orders (the last took the ring buffer with
   `head` relaxed, 2 slots and 3 items, from 2,745 memories in 3 minutes
   43 s to 583 in 20 s), and dropping states with a promise that can no
   longer be fulfilled (the Treiber stack with its push's release
   relaxed, from 6 minutes 43 s to 15 s, and with a faster hasher to
   9 s); next come reducing the cost of each solo exploration's states,
   now mostly copying the per-byte coherence and forwarding maps (which
   could be kept as ranges), and guiding the sampling towards reads that disagree
   with what TSO would return.
4. System-level emulation: exception levels, MMU/page tables, timers and
   interrupt controller, basic devices (UART, virtio), with the memory
   model applied to normal memory. Goal: boot Linux on a multi-core
   virtual machine and search for ARM-only behaviors.
