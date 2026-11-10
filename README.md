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
./target/release/armem --no-live FILE              # without register liveness
./target/release/armem --no-dynamic-por FILE       # without pruning per memory
./target/release/armem --no-filter FILE            # certify every candidate promise
./target/release/armem --check-filter FILE         # ... and check the filter's verdicts
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
`SEV`, `SEVL`, run as `NOP`, and `XPACLRI`, which strips a pointer
authentication code from X30 and so does nothing to code that signs no
pointers), and:

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
`litmus::parse_with`.

A *setup function* (`{ setup(); }` in the init block, an armem
extension) is a function of the file that runs before the test, alone
and under sequential consistency, after the init block's values are
set; setup functions run in the order written. Its writes become part
of the initial memory, so a test can start from a data structure built
by the program's own code (a C++ constructor, say) without its writes
multiplying the memories the threads explore. A setup function that
faults, or does not return within 100,000 instructions, is an error. Memory permissions are not enforced: a store to
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
* `chase_lev.c`: the Chase–Lev work-stealing deque, in the C11 version of
  Lê, Pop, Cohen and Zappa Nardelli ("Correct and Efficient
  Work-Stealing for Weak Memory Models", PPoPP 2013), with a fixed-size
  buffer. The owner pushes 1, then 2, and takes once from the bottom;
  a thief steals twice from the top. The condition is a value taken
  twice, or one taken that was never pushed.

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
| chase_lev | none | no | no |
| chase_lev | push's release fence (`PUSH_FENCE`) | yes | no |
| chase_lev | take's seq_cst fence (`TAKE_FENCE`) | yes | yes |
| chase_lev | steal's seq_cst fence (`STEAL_FENCE`) | no, but yes with `-march=armv8.3-a+nolse` | no |
| chase_lev | steal's fence and its compare-and-swap's order (`STEAL_FENCE`, `CAS`) | yes, but no at `-O0` | no |
| chase_lev | steal's acquire of `bottom` (`STEAL_BOTTOM`) | yes | no |
| chase_lev | steal's acquire of `top` (`STEAL_TOP`), push's (`PUSH_TOP`), or the compare-and-swaps' order (`CAS`) | no | no |

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

In `chase_lev`, the take's fence (`TAKE_FENCE`) is the owner's half of
a store-buffering pattern: the take stores the decremented `bottom`,
then loads `top`. Without the fence, the load can be satisfied before
the store is visible, on x86 too. The thief's second steal then takes
the slot the owner takes, as it read the old `bottom`, while the owner
read the `top` from before the first steal. With `STEAL_BOTTOM` relaxed,
only a branch on `bottom` comes before the thief's read of the slot,
and ARM may read the slot early.

The steal's fence (`STEAL_FENCE`) is the thief's half of the same
pattern: the compare-and-swap of the first steal writes `top`, and the
second steal must not read `bottom` before that write is visible. A
seq_cst compare-and-swap followed by an acquire load of `top` usually
orders that already:

* With the default `-march`, clang compiles the compare-and-swap to an
  `LDAXR`/`STLXR` loop and the load to `LDAR`, and a release write is
  ordered before a later `LDAR`.
* With LSE (`-march=armv8.1-a` and later), the compare-and-swap is
  `CASAL`. Arm's model orders the write of an acquire-release LSE
  atomic before every later access.
* At `-march=armv8.3-a+nolse`, the load is the RCpc `LDAPR`, and nothing
  orders an `STLXR` before an `LDAPR`.

So the program needs the fence only with RCpc acquire loads and no LSE,
and with a relaxed compare-and-swap. herd7 agrees on both
two-instruction patterns (`CASAL; LDAPR` is ordered, `STLXR; LDAPR` is
not). At `-O0`, clang uses `LDAXR`/`STLXR` even for a relaxed
compare-and-swap, so with the default `-march` the fence and `CAS`
relaxed together are still correct there, but not with LSE
(`-march=armv8.1-a`), where the relaxed compare-and-swap is a plain `CAS`.

`STEAL_TOP`, `PUSH_TOP` and `CAS` alone are harmless in this test: the
fences order what it needs. (The test leaves out the resize, and has
only two steals and one take.) The `chase_lev` tests take at
most 1.3 s each (`PUSH_FENCE` at `-O0`).

A condition compares a location with a value, and a symbol on the right
is an address: `out_a=out_b` means "`out_a` holds the address of
`out_b`". To ask whether two locations differ, list the cases, as
`seqlock.litmus` does.

The seqlock and ring buffer tests take at most 0.3 s each. Larger rings
take longer, mostly because of the number of promise memories when
`head` is relaxed: with one slot and three items, 70 memories in 0.1 s;
with two slots and three items, 583 memories in 0.7 s (`tail` relaxed:
44 memories in 0.1 s).

The `dcl` tests take at most 0.2 s when correct, and up to 1.3 s (at
`-O0`) with the lock's release relaxed. At `-O2` that variant takes 0.4
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
0.2 s at `-O2` (315 memories) and 0.4 s at `-O0`. With `PUSH` relaxed,
nothing orders the pusher's writes, and the popper sees them in many
orders: the exhaustive search explores 7,824 memories in 5 s. For
tests too big for that, a line of `expected.txt` may give `sample=N`
(`N` random paths), but only when the condition is expected to be
reachable, since sampling may miss states. A line may also give
`fuel=N`, for a program whose threads need more than the default 1,000
instructions each.
A program may be C++ (`NAME.cpp`), and a verdict may be `fault` rather
than `yes` or `no`: some execution faults, for example on a null pointer
(see [Stacks](#stacks)).

### Concurrency Kit

`c/ck/include` holds the headers of [Concurrency
Kit](https://github.com/concurrencykit/ck) that its ring buffer
(`ck_ring.h`), spinlocks (`ck_spinlock.h`), sequence lock
(`ck_sequence.h`), epoch-based reclamation (`ck_epoch.h`) and FIFOs
(`ck_fifo.h`) need, and
`c/ck/src/ck_epoch.c`, unchanged from ck commit `8f0847c` (BSD license,
in `c/ck/LICENSE`). Its aarch64 port implements barriers and atomics in
inline assembly. `c/ck/stub` holds the `ck_md.h` that ck's configure
script generates for aarch64 Linux, and the declarations from
`<string.h>` that ck uses (there is no AArch64 libc here, so a program
that calls `memcpy` defines it). A program's first line can give clang flags
(`/* cflags: ... */`), and the ck programs use it to compile with
`-ffreestanding` and these include paths:

```
clang --target=aarch64-linux-gnu -O2 -fno-pic -ffreestanding -Ic/ck/include -Ic/ck/stub -c c/ck_spmc.c
armem -f 1000 --elf ck_spmc.o c/ck_spmc.litmus
```

* `ck_spsc.c`: one producer sends 1, 2, 3 through a ring of 2 slots
  (ck_ring keeps one free, so it holds one entry) to one consumer. The
  condition is the consumer receiving anything else.
* `ck_spmc.c`: the same ring, with two consumers taking one entry each
  with `ck_ring_dequeue_spmc`, which reads the slot and then claims it by
  a compare-and-swap of the consumer counter. The condition is the
  consumers receiving anything but 1 and 2.

In June 2026, ck commit `c7945fe` changed the fence between a consumer's
read of a slot and its update of the consumer counter from a store fence
(`DMB ISHST`) to a load-store fence (`DMB SY`). The commit message calls
the old fence "overly weak in theory ... No impact on a real modern CPU".
`-DOLD_FENCE` restores it (it turns `ck_pr_fence_load_store` back into
`ck_pr_fence_store`; the code is the same as before the commit, up to
register allocation):

| Program | Variant | ARM | TSO |
|---|---|---|---|
| ck_spsc | current | no | no |
| ck_spsc | old fence | no | no |
| ck_spmc | current | no | no |
| ck_spmc | old fence | yes, but no at `-O0` | no |

The architecture allows the bug. A store fence does not order a read
before a later write, so with `ck_spmc` at `-O1` and above, a consumer can
claim slot 0 and read it only after the other consumer has claimed slot 1
and the producer has put item 3 in slot 0. The first consumer then
receives item 3, which also stays in the ring. With one consumer
(`ck_spsc`), the old fence is enough: the producer reuses a slot only
after the dequeue that follows it, which runs `DMB ISHLD` before its own
update and so orders the earlier read first. At `-O0`, the copy of the
entry is a call to `memcpy`, which stores it to the consumer's stack. That
store is data dependent on the read, and the store fence orders it before
the update. This is the case the commit's comment warns about: the
compiler may keep the copy in a register instead, and does at `-O1`.

ck can also use the LSE atomics (`-march=armv8.1-a
-DCK_MD_LSE_ENABLE`, `CAS` instead of `LDXR`/`STXR`), with the same
verdicts. Each test takes at most 6 s (`ck_spmc` at `-O0`), and under
1 s at `-O1` and above. The harness's `memcpy` copies an aligned word
with one load, as a libc's does. Copying byte by byte made each read of a
slot eight reads that could each see a different write, and the `-O0`
tests took minutes.

`ck_spinlock.c` runs each of ck's spinlocks, chosen with `-DLOCK=N`: 1
compare-and-swap, 2 exchange, 3 ticket, 4 MCS, 5 CLH, 6 Anderson (array)
and 7 decrement. Two threads each take the lock once and increment a
plain counter, and the condition is the counter ending at anything but
2. Each lock acquires with `ck_pr_fence_lock()` and releases with
`ck_pr_fence_unlock()` (both `DMB SY` on aarch64). `-DWEAK_LOCK` or
`-DWEAK_UNLOCK` turns one of them into a compiler barrier only. Every
lock is correct as written, and either change makes it lose an increment
on ARM only, at `-O0` and `-O2`. The MCS lock's queue nodes are static
rather than on the stack, as in ck's own tests, since a thread here
cannot access another's stack. MCS is the slowest: up to 8 s at `-O2`,
and 5 s at `-O0` as written (its weakened variants take over 20 s there
and are left out of `expected.txt`). The others take under 3 s.

`ck_sequence.c` runs ck's sequence lock: a writer updates a two-word
record once, and a reader copies it with `CK_SEQUENCE_READ`. The
condition is a torn copy. It is correct as written. `-DWEAK_WRITER`
(no store fences in the writer) and `-DWEAK_READER` (no load fences in
the reader) both tear the copy on ARM only. Each test takes 0.1 s.

`ck_epoch.c` runs ck's epoch-based reclamation. A reader reads an object
through a shared pointer in an epoch section, twice. A writer replaces
the pointer, waits for a grace period with `ck_epoch_synchronize`, then
frees the old object (it overwrites the value). The condition is the
reader seeing a freed value. The reader's record is in the epoch's list
from the start, as `ck_epoch_register` would leave it.

In June 2026, ck commit `1faa673` ("add missing barrier for RMO
architectures in begin") added a `ck_pr_fence_load()` to
`ck_epoch_begin`, between the load of the global epoch and the
section's own loads. `-DOLD_BEGIN` removes it, which gives the same
reader code as before the commit, and the condition becomes reachable on
ARM only, at `-O0` and `-O2`. ARM may read the shared pointer before the
global epoch. The reader's second section can read the old pointer, then
observe the epoch the writer advanced to after replacing it. The writer
checked the record while it still showed the epoch from the first
section, which was current then, so it advanced the epoch again,
stopped waiting, and freed the object that the reader still reads. With
one section, the bug does not appear: the record's starting epoch, 0,
is never current, so the writer waits for the section to end. The tests
take up to 4 s (at `-O0`).

`ck_epoch_poll.c` reclaims through `ck_epoch_poll` instead, which never
waits. The writer replaces the pointer, defers freeing the old object
with `ck_epoch_call`, and polls twice; the reader again reads the object
in two sections. It is correct, and the tests take 3.4 s at `-O2` and
7.3 s at `-O0` (which needs `fuel=2000`). The writer's deferred lists
are private memory even though ck pops them with an atomic exchange (see
[How it works](#how-it-works)); before atomics could be private, the
tests took over 2 minutes.

ck commit `d1b348c` (June 2026) changed which deferred objects a poll
may free ("This held in production only due to x86 store / atomic
ordering guarantees but falls apart on RMO"). With the poll from before
that commit, this test is also correct, as are versions with three
sections and three polls. That fits ck's own comments: the old poll
fails only when `ck_epoch_call` files an object under an epoch that
another thread has already advanced, so it needs a second thread that
polls.

`ck_epoch_poll3.c` adds that thread: a helper with its own record and
nothing deferred, which polls once, while the writer polls once. The
writer removes the object with an atomic exchange, since with a plain
store the removal can be reordered after the reader's check, which is
a bug under TSO as well. As written the test is correct, and with
`-DOLD_POLL`, which polls with a copy of the poll from before `d1b348c`
(`c/ck_epoch_poll_old.c`), the reader sees the freed object on ARM
only, at `-O2` and `-O0`. This is the bug that commit fixed. The tests
take 5 to 7 s on ARM and 3 to 4 s under TSO at `-O2`, and up to 17 s
at `-O0`, which is left out of the test suite. With two writer polls
the verdicts are the same, in 24 to 30 s. Before the pre-pass could
handle three threads (see [How it works](#how-it-works)), and before
the reference models had private memory, neither exhaustive search nor
3,000 sampled paths finished within 200 s.

`ck_fifo_spsc.c` runs ck's single-producer, single-consumer FIFO
(`ck_fifo.h`), a linked list whose head is always a stub node. The
producer sends 1, 2, 3 with only two nodes, so it must recycle each node
(`ck_fifo_spsc_recycle`) once the consumer has moved past it. The
condition is the consumer receiving anything but 1, 2, 3. Enqueue fills
a node, runs a store fence and links the node; dequeue reads a node's
value, runs a store fence and moves the head past it. The FIFO is correct
as written. `-DWEAK_ENQ` or `-DWEAK_DEQ` drops one of the fences, and
either makes it lose or corrupt a value on ARM only, at `-O0` to `-O2`:
without the dequeue's fence, the consumer can move the head before its
read of the value, and the producer then refills the node first. The
values are sent as the pointers themselves. Had the consumer read an
item through each pointer, the read's address dependency would order the
read of the node before the next update of the head, and `-DWEAK_DEQ`
would be harmless. The tests take under 0.3 s, except those with
`-DWEAK_ENQ`: the producer's writes are then unordered, and the consumer
sees them in many orders, so they take 1.6 s at `-O2` and 4.9 s at `-O0`
(2 minutes before the dynamic reduction).

`ck_fifo_mpmc.c` runs ck's multi-producer, multi-consumer FIFO, a
Michael-Scott queue whose head, tail and links are (pointer, generation)
pairs, each updated by a 16-byte compare-and-swap (`LDXP`/`STXP` on
aarch64). As in the test of node re-use that ck added in commit `e78c421`
(June 2026), the queue starts with the entries 1 and 2, and each of two
threads dequeues once and at once enqueues the value again, in the node
the dequeue gave back. The queue is never empty, so the condition is a
dequeue that reports it empty, or the threads receiving anything but 1
and 2.

As written (ck `8f0847c`, the latest commit as of November 2026), the
condition is reachable on ARM only: both threads can receive 2, and
item 1 is lost. A dequeue reads the value of the node after the head
("Save value before commit") and then claims it by moving the head with
`ck_pr_cas_ptr_2`, which on aarch64 orders nothing. So ARM may perform
the read after the compare-and-swap. Meanwhile the other thread dequeues
the next entry, which moves the head past that node and gives it back
as garbage, and it re-enqueues its own value, 2, in it. The late read
returns 2. In Arm's model this is load buffering: the second thread's
write of the value is ordered after its read of the head (a control
dependency), but nothing orders the first thread's read of the value
before its write of the head. x86 cannot do this, since its
compare-and-swap is a full barrier. `-DFENCE_DEQ` adds
`ck_pr_fence_load_store()` (`DMB SY`) between the read and the
compare-and-swap, and then the exhaustive search shows the condition
unreachable (4,894 memories in 35 s at `-O2`, and the same at `-O1` and
`-Os`, with the dynamic reduction of
[docs/dynamic-reduction.md](docs/dynamic-reduction.md); without it, the
search did not finish in 20 minutes). As written, the exhaustive search
finds the bug in 6,752 memories and 58 s. `ck_fifo_mpmc_trydequeue`
reads the value the same way. The bug needs a node to be re-used while another
thread may still read it, which ck's own test does on purpose. It is
reported as
[concurrencykit/ck#297](https://github.com/concurrencykit/ck/issues/297).

ck changed this FIFO twice in 2026, and `ck_fifo_mpmc_variants.c` has
the code from before each change. `-DOLD_DEQ` uses the dequeue from
before `e78c421`, which could report the queue empty when its snapshot
of the head had been re-used. With `-DFENCE_DEQ -DOLD_DEQ`, a thread
receives nothing even under SC: that bug is not about the memory
model. `-DOLD_ENQ` uses the enqueue from before `ed6bace` (September
2026), which started a node's generation at 0 and had no load fence
before re-checking the tail's generation; with `-DFENCE_DEQ -DOLD_ENQ`,
the exhaustive search shows the condition unreachable too (4,934
memories in 76 s at `-O2`). The sampled tests take
5 to 7 s at `-O1` to `-O2` (and the same verdict at `-Os`), and TSO
about 2 s; at `-O0`, 20 sampled paths take over 10 minutes.

### DPDK

`c/dpdk/include` holds the ring headers of [DPDK](https://www.dpdk.org)
(`lib/ring/rte_ring*.h`, unchanged from DPDK commit `a13c105336`, 26.11-rc2;
BSD license, in `c/dpdk/LICENSE`). `c/dpdk/stub` holds just enough of
DPDK's EAL headers for them: `rte_stdatomic.h` unchanged, and small
stand-ins for the rest (each says what it covers), configured as DPDK's
build configures aarch64, with the C11 ring (`RTE_USE_C11_MEM_MODEL`).

`dpdk_ring.c` runs the multi-producer, multi-consumer ring with 4 slots,
holding the item 1; the empty slots hold 99. Thread `t0` acts first as a
producer and then as a consumer: it enqueues 2, then dequeues two items
at once. Thread `t1` dequeues one. The condition is any other outcome
than `t0` receiving 1 and 2, or `t1` receiving 1 (or nothing).

DPDK commit `a4ad0eba9d` (November 2025, by Arm) changed how a head move
orders its read of the head and of the other side's tail. It says the
old code fails "when a CPU first acts as a producer and later as a
consumer", but only "when load-acquire is implemented with RCpc
semantics (e.g. some AArch64 CPUs which support the LDAPR ... instructions)".
`-DOLD_HEAD` restores the old code (in `c/dpdk_ring_variants.h`), and
`-march=armv8.3-a` makes clang compile acquire loads to `LDAPR` instead
of `LDAR`. Before that, DPDK commit `9bc2cbb007` (November 2017) added a
fence between those two reads in the generic ring, after a crash on an
ARM server. `-DRTE_GENERIC_RING` selects the generic ring, and
`-DOLD_GENERIC` removes the fence:

| Ring | Variant | `LDAR` (armv8-a) | `LDAPR` (armv8.3-a) |
|---|---|---|---|
| C11 | current | no | no |
| C11 | before `a4ad0eba9d` | no | **ARM only** |
| generic | current | no | no |
| generic | before `9bc2cbb007` | **ARM only** | **ARM only** |

That holds at `-O0` to `-O2` and `-Os`. With either old version, `t1`
receives 99: it sees `t0`'s move of the consumer head but an older
producer tail, so its count of entries is negative and wraps around, and
it dequeues an empty slot. With the C11 ring, `t0` reads its own release
store of the producer tail with `LDAPR`, which may happen before that
store becomes visible to other threads (`LDAR` may not). So its move of
the consumer head, which depends on that read, can be seen first. The
tests take under 0.5 s, except the old C11 head move with `LDAPR`
(4.4 s at `-O1` to `-O2`, 55 s at `-O0`, which is left out of the test
suite).

The ring's other two multi-thread modes, head/tail sync (HTS, `-DHTS`)
and relaxed tail sync (RTS, `-DRTS`), had the same kind of change in DPDK
commits `66d5f96278` and `36b69b5f95`, in the same series as
`a4ad0eba9d`. Those commits describe it as establishing "a safe partial
order", not as fixing a failure. `-DOLD_HTS` and `-DOLD_RTS` restore the
old code (`c/dpdk/old`, unchanged from the commits' parents). In
`dpdk_ring.c` neither old mode goes wrong. `dpdk_ring3.c` moves the
enqueue of 2 to a third thread, `t2`, so that `t0` can take an item it
did not write:

| Ring | Variant | `LDAR` (armv8-a) | `LDAPR` (armv8.3-a) |
|---|---|---|---|
| HTS | current | no | no |
| HTS | before `66d5f96278` | **ARM only** | **ARM only** |
| RTS | current | no | no |
| RTS | before `36b69b5f95` | **ARM only** | **ARM only** |

That holds at `-O1`, `-O2` and `-Os`, and for RTS also with a maximum
distance between head and tail (`-DHTD_MAX=3`) above its default, which
is 0 here. At `-O0` the search runs out of memory. With either old mode,
`t0` dequeues both items but receives 99 for the second. The old code
reads the producer tail with a plain load and only then does the
acquire compare-and-swap of the consumer head. An acquire orders the
loads after it, not the ones before, so `t0`'s loads of the slots may be
satisfied before its load of the tail: it reads the tail that `t2`'s
release store set and slots older than that store. The old code's own
comment says the acquire compare-and-swap "serves as a hoist barrier to
prevent ... OOO reads of cons tail value", which it does not. The new
code loads the tail with acquire. In `dpdk_ring3.c` the old C11 head move
and the current rings are right, and the generic ring without the fence
of `9bc2cbb007` is wrong. The tests take under 4 s.

### liburcu

`c/urcu/include` holds the headers of
[liburcu](https://liburcu.org) (`include/urcu`, unchanged from liburcu
commit `645fdc9`, July 2026; mostly LGPL-2.1-or-later, see
`c/urcu/LICENSE.md`). `c/urcu/stub` holds the generated `urcu/config.h` as
configure sets it for aarch64 Linux, a `urcu/futex.h` that records each
`FUTEX_WAKE`, a `urcu/tls-compat.h` that makes thread-local variables
ordinary globals (each test has one reader), and small stand-ins for the
C library headers.

`urcu_qsbr.c` and `urcu_mb.c` test the QSBR and mb flavors' wakeup of
a waiting grace period. The reader, `t0`, calls the real header code: it
reports a quiescent state (`-DOFFLINE`: goes offline) or leaves its
read-side critical section. The writer, `t1`, runs one pass of
`wait_for_readers` (copied from `src/urcu-qsbr.c` and `src/urcu.c`), the
pass that arms the futex after the busy-wait attempts run out. It then
checks the reader's counter, and would sleep in `wait_gp` if the reader
has not reported and the futex is still -1. The condition is that it
would sleep and the reader never wakes it, which hangs
`synchronize_rcu`.

liburcu commits `9e6448d` (QSBR) and `96ef712` (mb) (January 2026)
changed the reader's store of its counter from `CMM_SEQ_CST` to
`CMM_SEQ_CST_FENCE`, because "Storing to rcu_reader.ctr with a
CMM_SEQ_CST store does not ensure that the following relaxed load of
the futex waiting state is ordered after that store". `-DOLD_QSBR` and
`-DOLD_MB` undo just those commits (`c/urcu/old`, through
`c/urcu_variants.h`):

| Flavor | Variant | `LDAR` (armv8-a) | `LDAPR` (armv8.3-a) | TSO |
|---|---|---|---|---|
| QSBR (quiescent state, offline) | current | no | no | no |
| QSBR (quiescent state, offline) | before `9e6448d` | **yes** | **yes** | yes |
| mb | current | no | no | no |
| mb | before `96ef712` | **yes** | **yes** | yes |

That holds at `-O0` to `-O2` and `-Os`, and each test takes about 0.1 s.
With C11 atomics, which liburcu uses whenever the compiler supports C11
(since commit `89280d0`, in 0.15.0), the old store compiles to an `STLR`
with nothing after it. The reader's next load, of its waiting flag or
the futex, is a plain `LDR`, which may be satisfied before the `STLR`
becomes visible. The writer makes the mirror-image store and load, with a
full barrier between them. That is store buffering: each side misses the
other's store. The fix adds a `DMB ISH` after the `STLR`. TSO allows
store buffering too, and armem's TSO reference runs the `STLR` as an
ordinary store, so it reports the bug under TSO as well. On real x86 the
bug cannot happen, because compilers make a seq_cst store an `XCHG`,
which is a full barrier. So it hits ARM and other weak architectures
only. Compiled without C11 atomics (`-std=gnu99`), liburcu puts a full
barrier on each side of the store, and the old code is right.

### QEMU

`c/qemu` holds QEMU's `util/event.c` and `include/qemu/atomic.h` and
`compiler.h`, unchanged from QEMU commit `f71afb4` (November 2026;
GPL-2.0-or-later, see `c/qemu/COPYING` and `c/qemu/LICENSE`).
`c/qemu/stub` holds a `qemu/thread.h` with just the `QemuEvent` type
and functions, a `qemu/osdep.h` with the little that `event.c` uses,
and a `qemu/futex.h` (Linux's, with `HAVE_FUTEX`) whose `FUTEX_WAKE`
is recorded. Its `FUTEX_WAIT` makes the full barrier the kernel makes,
then checks the futex word: if the thread would sleep, it records that
and ends the thread, by branching to the address its thread function
returns to.

`qemu_event.c` tests the call_rcu thread of `util/rcu.c` waiting for
work. The event is set, as `call_rcu1` leaves it, and no work is
pending. The thread, `t0`, runs the loop at the top of
`call_rcu_thread`: it resets the event, reads `rcu_call_count`, and if
that is 0 waits on the event. `t1` runs `call_rcu1` without the queue:
it increments `rcu_call_count` and sets the event. The condition is
that `t0` sleeps and no one wakes it, which leaves the work, and every
later `call_rcu`, waiting.

QEMU commit `9586a132` ("qemu-thread-posix: cleanup, fix, document
QemuEvent", March 2023, in QEMU 8.0) says that "QemuEvent is currently
broken on ARM due to missing memory barriers after qatomic_*()".
`-DOLD_EVENT` undoes it (`c/qemu/old/util/event.c`, the current file
with just that commit's changes undone):

| Variant | LDAXR/STLXR (armv8-a) | LSE (armv8.1-a) | TSO |
|---|---|---|---|
| current | no | no | no |
| before `9586a132` | **yes** | no | no |

That holds at `-O0` to `-O2` and `-Os`, and each test takes under 2 s.
The old `qemu_event_reset` changes `EV_SET` to `EV_FREE` with a seq_cst
`qatomic_or` and nothing after it. Without LSE, that is an
LDAXR/STLXR loop, and the caller's relaxed load of `rcu_call_count` may
be satisfied before the `STLXR`'s write is visible. `qemu_event_set`
makes the mirror-image store and load, `rcu_call_count` then the
event, with a full barrier between them. That is store buffering: `t1`
still sees `EV_SET`, so it does not wake anyone, and `t0` sees no work,
so it waits, turns the event from `EV_FREE` to `EV_BUSY` and sleeps.
On x86 the OR is a locked instruction, a full barrier. With LSE it is an
`LDSETAL`, whose write Arm's model orders before later loads (herd7
agrees), so the old code is right there. Of the commit's changes, only
the one to `qemu_event_reset` matters here: undoing it alone brings the
bug back, and the old `qemu_event_reset` with just
`smp_mb__after_rmw()` added at its end is right. The commit also adds
`smp_mb__after_rmw()` after the exchange in `qemu_event_set`, to pair
with the kernel's barrier in `FUTEX_WAIT`, and the stub futex does not
model the kernel's side of that.

### libuv

`c/libuv` holds libuv's `src/uv-common.c` and `src/unix/async.c`, with
the headers they include from libuv, unchanged from libuv 1.53.0
(September 2026; MIT, see `c/libuv/LICENSE`). On master (November 2026)
the two functions tested here are the same. `c/libuv/stub` holds stand-ins
for the C library headers they include, compiled with `-nostdlibinc` so
that none of the host's headers are used. Its `read` finds no data, and
its `write` (the eventfd write that wakes the loop) is recorded.
`externs.h` defines each function the two files call but do not define
(the rest of libuv and the C library) as a single `BRK`, which armem
reports as a fault if it runs.

libuv's documentation says that `uv_async_send` and the callback it
causes are sequentially consistent: everything the sending thread did
before the call is visible to the callback, even when libuv coalesces
the call with an earlier one that the loop has not yet handled.
`libuv_async.c` tests the coalesced case. The handle is pending, as an
earlier `uv_async_send` leaves it. `t0` runs the loop's
`uv__async_io`, which clears the pending flag with a seq_cst
`fetch_and` and runs the callback, which reads `data`. `t1` stores 1 to
`data` and calls `uv_async_send`. The condition is that `t1`'s call is
coalesced (no notification is written) and the callback reads 0. That
callback is the one that answers `t1`'s call, so `t1`'s store is lost.

libuv commit `cdc632c6` ("async: make seq_cst the default", June 2026,
in 1.53.0) made the pending flag's CAS seq_cst and removed a relaxed
load of the flag before it, which returned at once if the flag was set.
`-DOLD_SEND` undoes it (`c/libuv/old/src/uv-common.c`). `-DDATA` gives
`t1`'s store an ordering, and `-DSEE` the callback's load (both relaxed
by default):

| Variant | Store | Callback's load | LDAXR/STLXR (armv8-a) | LSE (armv8.1-a) | TSO |
|---|---|---|---|---|---|
| current | relaxed | relaxed | **yes** | **yes** | yes (no with LSE) |
| current | release or seq_cst | relaxed | **yes** | no | no |
| current | relaxed | acquire | **yes** | **yes** | yes (no with LSE) |
| current | release | acquire | no | no | no |
| before `cdc632c6` | any | any | yes | yes | yes |

The first row holds at `-O0` to `-O2` and `-Os`, and the others at `-O0`
and `-O2`. Each test takes under a second, and at `-O0` each thread
needs 3,000 instructions. So the current code still loses a coalesced
call on ARM, unless the caller stores with release and the callback
loads with acquire, or, with LSE, the caller stores with release. A coalesced `uv_async_send` is a CAS that fails, because the flag is
set. A failed CAS makes no write, so without LSE it is an `LDAXR` (then
`CLREX`), and with LSE a `CASAL` that is only an acquire load. Neither
orders `t1`'s earlier store, unless that store is a release (`STLR`
before an acquire load is ordered), and so with a relaxed store it is
store buffering against the loop. On the loop's side, without LSE the
seq_cst `fetch_and` is an `LDAXR`/`STLXR` loop, and the callback's
relaxed load may be satisfied before the `STLXR`'s write is visible, as
in [QEMU](#qemu). With LSE it is an `LDCLRAL`, which orders later loads.
herd7 agrees on both patterns: an `STLR` then a failing `LDAXR` against
an `LDAXR`/`STLXR` then `LDR` is allowed (and forbidden with an `LDAR`),
and an `STR` then a failing `CASAL` against an `LDCLRAL` then `LDR` is
allowed (and forbidden with an `STLR`). Nor does the code keep its
promise under C11: a failed seq_cst CAS is a seq_cst load, which does not order
a relaxed store before it, and a seq_cst RMW does not order a relaxed
load after it. A program that protects its data with a mutex is not
affected, as an unlock is a release and a lock an acquire. Under TSO,
armem runs the same ARM code, where the failing `LDAXR` is just a load.
x86's `LOCK CMPXCHG` is a full barrier even when it fails.

A seq_cst fence at the start of `uv_async_send` and another after the
`fetch_and` in `uv__async_io` makes every row no, with and without LSE,
at `-O0` and `-O2`. This version is not in `c/`; it was checked with
copies of the two files. This was reported as
[libuv/libuv#5416](https://github.com/libuv/libuv/issues/5416).

### liburing

`c/liburing/src` holds liburing's `src/queue.c` and the headers it
includes from liburing, unchanged from liburing commit `c63f4aa6`
(November 2026, 2.16; MIT or LGPL-2.1, see `c/liburing/LICENSE` and
`c/liburing/COPYING`), with `liburing/compat.h` and
`liburing/io_uring_version.h` as its `configure` generates them.
`c/liburing/stub` holds stand-ins for the C library and Linux headers,
compiled with `-nostdlibinc`. It also holds a stand-in for liburing's
`src/arch/aarch64/syscall.h`, whose system calls are `SVC` instructions:
here each one calls `uring_test_syscall`, which the test defines, and
liburing's `src/arch/syscall-defs.h` (unchanged) builds its wrappers
from those calls. The kernel's side of each test is a few lines of
`io_uring/` from Linux commit `d4872371` (November 2026), with arm64's
atomics and barriers written as the kernel's inline assembly. The only
exception is the `PRFM` hint before the LL/SC loop, which is left out.

With `IORING_SETUP_SQPOLL`, a kernel thread polls the submission queue,
and when it finds no work it sleeps. `io_sq_thread` first sets
`IORING_SQ_NEED_WAKEUP` in the ring's flags, then (after
`smp_mb__after_atomic`) checks the SQ tail once more. `io_uring_submit`
stores the tail with release, then reads the flags, and if the flag is
set it calls `io_uring_enter` with `IORING_ENTER_SQ_WAKEUP`.
`liburing_sqpoll.c` runs `io_uring_get_sqe`, `io_uring_prep_nop` and
`io_uring_submit` in `t0`, and the kernel thread's check in `t1`. The
condition is that the thread would sleep and `t0` does not wake it,
which leaves the new entry unsubmitted.

liburing commit `744f4156` ("Fix memory ordering in
sq_ring_needs_enter", March 2022, liburing issue 541) added a full fence
between the tail store and the load of the flags. `-DOLD_SUBMIT` undoes
it (`c/liburing/old/src/queue.c`), and `-DNO_KERNEL_MB` removes the
kernel's `smp_mb__after_atomic`:

| Variant | LDXR/STXR (armv8-a) | LSE (armv8.1-a) | TSO |
|---|---|---|---|
| current | no | no | no |
| before `744f4156` | yes | yes | yes |
| kernel without `smp_mb__after_atomic` | **yes** | **yes** | no |

That holds at `-O0` to `-O2` and `-Os`, and with `LDAPR`
(`-march=armv8.3-a`), and each test takes under 0.1 s. The old code is
store buffering: `t0`'s `STLR` of the tail and its later `LDR` of the
flags are not ordered, on ARM or on x86 (where a release store is a
plain `MOV`), so the bug was not ARM-only. The kernel's barrier is
needed only on ARM. Its `atomic_or` is relaxed on arm64 (an
`LDXR`/`STXR` loop, or `STSET`), and nothing orders it before the
`LDAR` of the tail. On x86 it is a locked `OR`, a full barrier. herd7
agrees on all three patterns, with the LSE atomic.

`liburing_cq.c` tests the completion queue, with a single entry so that
the kernel reuses its slot. `t1` posts two completions (res 1, then 2) as
`io_cqe_cache_refill` and `io_commit_cqring` do: it reads the head the
application last released (`READ_ONCE`), and only if the slot is free
writes the CQE with `WRITE_ONCE` and releases the tail. If the slot is
not free, the completion would overflow, and here `t1` stops. `t0` calls
`io_uring_peek_cqe` twice, and for each CQE it gets, checks its res and
calls `io_uring_cqe_seen`. The condition is that it reads a CQE other
than the one posted for that slot.

liburing commit `81cb9479` (July 2025) made `__io_uring_peek_cqe` load
the CQ head with acquire, on the grounds that the CQE's address depends
on the head, and C does not order address dependencies. Commit `a4d619a2`
(June 2026) reverted it: only the application writes the head, and the
acquire load of the tail already orders the CQE's contents.
`-DACQ_HEAD` restores the acquire (`c/liburing/old/include/liburing.h`).
Both versions are right at `-O0` to `-O2` and `-Os`, with and without
LSE and `LDAPR`, so the revert is safe on ARM. Each takes under 0.1 s.
The kernel's side of slot reuse is ordered by a control dependency, from
its load of the head to its stores to the CQE, which ARM respects for
stores. With `-DNO_KERNEL_RELEASE`, a `WRITE_ONCE` of the tail rather
than `smp_store_release`, `t0` may read the slot before the kernel's
writes to it. That is ARM-only (0.7 s, 1.5 s at `-O0`).

### readerwriterqueue

`c/readerwriterqueue` holds `readerwriterqueue.h` and `atomicops.h` from
moodycamel's ReaderWriterQueue, unchanged from commit `6867b564`
(September 2026; Simplified BSD, see `c/readerwriterqueue/LICENSE.md`).
It is C++, and `c/readerwriterqueue/stub` holds freestanding stand-ins
for the standard headers it includes (`<atomic>` is built on clang's
`__atomic` builtins), compiled with `-nostdlibinc`. `c/rwq.h` gives the
queue a bump allocator over zeroed memory, and its setup function (see
[Starting from an ELF file's memory](#starting-from-an-elf-files-memory))
constructs a `ReaderWriterQueue<int>` with one block, room for one element, and
enqueues 1. In both tests `t0` enqueues 2, which allocates a second
block, initializes it, links it after the first (`tailBlock->next`) and
makes it the tail block (`tailBlock`).

Pull request #172 (merged September 2026) has two commits. The first,
`0a5aee4`, fixes issue #171: the release fence sat between the two
stores, so `size_approx`, which walks the `next` chain, could reach the
new block before its fields. It moved the fence before both stores. That
left nothing between them, and the second commit, `ba8c662`, adds a
second fence there, replacing the comment that said none was needed.
`-DOLD` undoes both commits (`c/readerwriterqueue/old`), and `-DMID`
only the second (`c/readerwriterqueue/mid`):

| Variant | `rwq_size` | `rwq_dequeue` | TSO |
|---|---|---|---|
| current | no | no | no |
| after `0a5aee4` only | no | **yes** | no |
| before `0a5aee4` | **fault** | no | no |

`rwq_size.cpp` calls `size_approx` in `t1`. With the old code, on ARM, it
can read the new block's `next` before `t0` writes it, as the zero the
allocator's memory holds, and dereferences it: armem reports the null
pointer dereference. The current and intermediate versions are right.
Each run takes under 0.5 s at `-O0` to `-O2` and `-Os`, except the old
code's, which takes 5 to 12 s (330 s at `-Os`).

`rwq_dequeue.cpp` calls `try_dequeue` twice in `t1`, and the condition is
that it gets a value other than the next one enqueued. With only the
first commit, `t1` can see the new `tailBlock` while the first block's
`next` still points to itself; it then "advances" to the first block
again, which it has emptied, and dequeues a stale slot. The bug was
ARM-only, and was never in a release, since the PR was merged with both
commits. Each run takes 6 to 11 s at `-O2` and `-O1`, 1.5 to 3 minutes
at `-O0` (with 3,000 instructions of fuel), and over 3 minutes at `-Os`.
clang compiles `atomic_thread_fence(memory_order_release)` to `DMB ISH`,
a full barrier, so before the reduction used program order (see
[How it works](#how-it-works)) it took the new block's initializing
stores, which the consumer reaches only through the pointers stored
after the fence, to be observable in every order, and the search did not
finish in 15 minutes.

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
  instruction, or
* accesses memory below `0x1000` (`core::NULL_PAGE`), as a null pointer
  dereference does. No location is placed there.

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

  A `CAS` that fails writes nothing, so it orders nothing as a release
  write would, and its failure is no dependency. A thread may therefore
  need another thread's later write to make its `CAS` fail before it can
  fulfill a promise placed earlier (`litmus/CAS-fail-promise`): running
  alone against the memory so far, the comparison would succeed. So when
  certifying promises (and collecting candidates), a `CAS` may also fail
  on a later write by another thread, returning a value other than the
  expected one, with a view above every timestamp. Whatever depends on
  that value is ordered after every promise, so the value itself does not
  matter. Such a run only certifies; final states come only from runs
  against the whole memory. Before this, armem missed outcomes that Arm's
  model allows in code using LSE `CAS`. A load-exclusive loop branches on
  the value it loads, a control dependency, so it needed no such rule.
  Nor does a `CAS` with acquire (`CASA`, `CASAL`): one that read a later
  write would order every later write after all the promises, so the run
  could neither fulfill an outstanding promise nor make a write that can
  be promised. Letting it fail anyway only sent a retry loop round and
  round until it ran out of fuel (`qemu_event_wait`, in
  [QEMU](#qemu)).
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

  Most certifications still failed, though, and they took most of the
  time: 80% on `ck_fifo_mpmc`, where 12 of every 13 failed. Each of these
  candidates is a write that the thread could make, and could even
  place right after the memory, but not on any run that also fulfills
  the thread's older promises. The solo exploration that collects the
  candidates already shows this, so armem certifies only a candidate
  that some run of it writes with every view the write must exceed
  (its write views, the coherence of its bytes, and an exclusive's
  atomicity) within the memory, and that some run appends and then goes
  on to fulfill every outstanding promise. A run certifying the promise
  gives such a run when the same steps append the write instead: it
  differs only in where that one write sits among the thread's own new
  writes, and no earlier one of them shares its bytes (the promise could
  not be placed before it otherwise), so the order on each byte is the
  same, and every check against an older promise compares with a
  timestamp within the memory. `--no-filter` certifies every candidate,
  and `--check-filter` certifies them all and stops with an error if
  one the filter rejects certifies; the test suites run with that
  check. On `ck_fifo_mpmc` the filter rejected every candidate that
  failed and none that certified. It takes the relaxed Treiber stack
  from 4.1 s to 1.5 s, the exhaustive `ck_fifo_spsc` with `-DWEAK_ENQ`
  from 120 s to 36 s, and `ck_epoch_poll3` with `-DOLD_POLL` from 6.4 s to
  3.0 s, on the same memories.

  The visited sets of the solo explorations, the reduction's pre-pass
  and the reference models use a fast hasher of their own (`src/hash.rs`,
  after `rustc-hash`) rather than the standard library's SipHash, and
  look each state up once. Hashing whole thread states had taken half
  the time; the relaxed Treiber stack went from 15 s to 9 s. A thread's
  per-byte coherence and forwarding views are kept as ranges of bytes
  with one value (`src/bytemap.rs`), since an access sets the same value
  for every byte it covers: a state then has a few ranges to copy and
  hash rather than dozens of bytes, which took that test to 5 s.
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
  after changing the model or the reduction. The pre-pass widens a byte
  whose set of values grows in three rounds (a counter, such as a sequence
  number) to any value, rather than adding values round after round. The
  rounds need not be consecutive, as long as no two in a row pass without
  growth: in `ck_epoch`, the global epoch grows in one round and each
  thread's copy of it in the next, so neither grew in consecutive rounds,
  and the analysis ran out of states. An AND with an immediate, or an
  unsigned bitfield extract, that keeps at most four bits of a value the
  analysis does not know gives each value those bits may have, so an array
  index computed that way (`epoch & 3` in `ck_epoch`) keeps the address
  known. A byte that only one thread writes is read by that thread exactly
  as it last wrote it (by coherence, no other write to it exists), so the
  indices and counters a thread owns stay precise; this is what lets the
  analysis bound `buf[head % N]` in a ring buffer. (It keeps such values
  only for bytes the thread reads; when a run reads one it dropped, the
  thread's run is repeated.) The pre-pass also keeps the whole values
  written with each footprint. An aligned read of bytes that every write
  covers with the read's own footprint reads them all from one write (or
  initial memory), since an aligned access is single-copy atomic, so it
  returns only those values. Byte by byte, a pointer that may be null or
  point to one of two nodes would combine into many addresses no write
  stored. Reading through those found yet more values until the bytes were
  widened, and the pre-pass gave up on the [Treiber stack](#c-programs).
  Being more precise, it also cut the correct double-checked
  initialization at `-O2` from 178 memories to 47, all of them now from
  private memory rather than the reduction. Besides barriers, a thread
  orders reads by dependencies: by an address dependency, by reading back
  a write whose address or data depends on a read, and after an acquire so
  ordered. Where a thread may do so, the pre-pass runs it again once for
  each instruction that may read another thread's writes, tracking which
  later reads depend on that one, so a dependency that orders only a ring
  buffer's index reads does not count as ordering its data reads. If the
  pre-pass gets too big, or a thread accesses memory at an address it
  cannot bound, it gives up and nothing is pruned.
  Two rules keep the pre-pass precise on `ck_fifo_mpmc`, whose head, tail
  and links are each a pointer and a generation counter written together
  by a 16-byte store-exclusive and read as a pair or as separate 8-byte
  loads. The pre-pass keeps the values of each half of a 16-byte write as
  well as those of the whole, and reads an exclusive pair as two halves,
  so a widened generation leaves its pointer known; an aligned read inside
  a write's footprint reads a slice of that write's values. And a byte is
  widened only once it holds more than four values: a pointer that comes
  to point to one more of three nodes in each of three rounds, as values
  spread between the threads, is not a counter. Before these rules, the
  pre-pass gave up on `ck_fifo_mpmc` and nothing was pruned.
  Two more rules use where accesses lie in program order, which a
  thread-wide flag such as "has a full barrier" loses. First, the
  pre-pass runs each thread once more counting the ordering points it
  passes (full and store barriers, and acquire and release accesses), and
  two writes of one thread can be ordered only if some run makes the first
  with fewer behind it than the second. The writes that initialize a
  block before the one `DMB ISH` that publishes it are then unordered.
  Second, a reader that finds those writes only through the pointer that
  publishes them cannot observe their order. Suppose only thread A writes
  location `P`, every write of value `v` to `P` follows a full or store
  barrier after A's writes to some footprints (or is a release after
  them), and `P` does not initially hold `v`. If every read by thread C
  of such a footprint is ordered after a read that returns `v` from `P`
  (by a dependency, by that read being an acquire, or by a `DMB LD` or
  full barrier after it), then none of those reads can return a write
  older than A's. So C cannot reveal the order of A's writes there.
  The pre-pass finds such reads by running C again with the reads of
  published values as sources, counting only orderings that hold in that
  run. It does not count the dependency rule's over-approximation of
  writes that may be read back, which is safe for ordering reads but
  not here. Together these rules took moodycamel's
  [ReaderWriterQueue](#readerwriterqueue) dequeue test from over 15
  minutes to 5.6 s, and in `tests/por.rs` a writer that stores four
  locations and publishes a pointer to a reader with `DMB LD` between its
  reads, from 89 memories to 17. (An acquire or release access counts on
  both sides of itself for the first rule. Counting it only before itself
  lost final states in a random test, where the write of a `SWPAL` must
  come before the thread's later store.)
* **Dynamic reduction** (on by default; `--no-dynamic-por` turns it off;
  see [docs/dynamic-reduction.md](docs/dynamic-reduction.md)). Where the
  pre-pass cannot rule out that some thread orders the last two writes
  of a memory, armem asks the memory itself. For a memory ending in
  write `x`, it runs each thread alone against it. A read may return a
  later write by another thread, with any value the pre-pass allows and a
  timestamp above every message. Runs with a promise they can no longer
  fulfill are dropped. In Promising-ARM an access ordered after `x` has a
  pre-view of at least `ts(x)`, and one that could order `x` before the
  next write `y` has exactly `ts(x)`, so the runs record the reads and
  writes they make with that pre-view. If no thread reads `y`'s location
  that way, and `y`'s writer does not write it so, `x` is ordered before
  `y` in no execution that extends the memory, and the memory with them
  out of priority order is skipped. Deep in the search each thread's
  promises pin down which path it took, so this prunes far more than
  the thread-wide footprints. A run is cut off once it is past `x` (every
  later access has a larger pre-view); until then, a value read from a
  later write that the pre-pass does not know (a counter it widened) is
  poisoned: computing with it poisons the result, a branch on it goes
  both ways, and storing it makes a poisoned write. A run that needs a
  poisoned value for anything else (an address, say) makes the memory
  fall back to running the threads in rounds, in which reads of later
  writes return the values the other threads wrote in the round before,
  until no round adds one. A memory whose runs grow too large is pruned
  by the static reduction alone. `tests/por.rs` checks this reduction
  too. It took `ck_fifo_mpmc` with `-DFENCE_DEQ` from not finishing in 20
  minutes to 35 s, and the C programs from 320 s to 91 s.
* **Register liveness** (on by default; `--no-live` turns it off,
  `src/live.rs`). Compiled code leaves values in registers it no longer
  needs: the comparison results of a retry loop, a value loaded only to be
  thrown away (a volatile read), the status of a store-exclusive. States
  that differ only in such registers behave alike, so the solo
  explorations, the pre-pass and the reference models clear the registers
  that are dead at a state's next instruction before they look the state
  up. Liveness is found by the usual backward analysis. At the end of the
  code, the registers the test observes are live (for the pre-pass, none),
  and before `BR` and `BLR` all are. A `RET` is taken to return to the end
  of the code or after a `BL` or `BLR`, the only addresses compiled code
  puts in X30; each search checks this before a `RET`, and if it fails it
  starts again without liveness (the pre-pass gives up). That check is
  enough: merged states agree on the live registers, X30 included, so
  they take the same path until the first unexpected `RET`, which the
  state kept also takes. In `ck_fifo_mpmc`, liveness took the pre-pass's
  second round from 25,419 states to 1,276, and with it the pre-pass
  finishes instead of running out of states: on two threads that each
  enqueue once, it then finds their private memory, and the search takes
  1,221 memories in 4.4 s rather than 39,421 in 232 s. `tests/por.rs` compares every search with one without liveness,
  including a test whose `RET` goes elsewhere: without the check, TSO and
  SC miss one of its final states.
* **Private memory** (on by default; `--no-private` turns it off). Every
  write another thread might see is a promise, and memories multiply with
  each one, its value included. But a C program's results, and variables
  only one thread uses, are often in memory that no other thread
  accesses. The reduction's pre-pass finds each thread's private memory:
  bytes that no other thread may read or write, and that the thread
  accesses with no acquire or release instruction. A byte is private only if
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
  Exclusive and atomic instructions that neither acquire nor release may
  access private memory too. The model performs them (a store-exclusive
  may always fail, and a successful `CAS` may leave `Rs` with the value
  from either memory or the register), but on the thread's private bytes,
  as a load and then a store, rather than with write messages. No other
  thread writes those bytes, so the access is trivially atomic, and the
  ordering that sets atomics apart only matters to acquire reads, which
  are never private. This is what makes `ck_epoch_poll` tractable (see
  [Concurrency Kit](#concurrency-kit)): ck pops a thread's lists of
  deferred objects with an atomic exchange. While those were shared
  writes, one poll against one reader's section took over 60 s; it now
  takes 0.1 s. The random tests give each thread a location of its own,
  which it sometimes accesses with atomics and exclusive pairs, to check
  this.
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
  milliseconds on it. Memory private to one thread (see Private memory
  above) is treated like its stack, so its accesses are local steps too.
  Its exclusive and atomic accesses wait for the thread's store buffer
  to drain, as locked operations do, but are performed on the thread's
  own bytes. Each state is stored as the ids of its threads' states,
  interned so that a thread state shared by many global states is stored
  once, plus memory. Successors share the threads they leave unchanged.
  On `ck_epoch_poll3` with two writer polls, these took TSO from 100 s
  and 20 GB to 14 s and 0.5 GB. A full `DMB`
  acts like `MFENCE`, and atomics act as locked operations. An `LDAR`
  (or `LDAXR`, `LDAXP`) waits until every earlier `STLR` of its thread
  has left the store buffer, as ARM orders them (`LDAPR` does not), so
  TSO keeps every ordering ARM guarantees within a thread. They run the
  same ARM program under an x86-like ordering, so any outcome they cannot
  produce is an *ARM-only* behavior.
* **Fuel** bounds the instructions per thread per execution, so loops
  terminate. A state that recurs with no more fuel used is pruned, so
  spin loops cost almost nothing. The reference models count fuel per
  thread, so they prune a state only if some earlier visit used no more
  fuel on every thread. They explore states in order of total fuel used,
  so a state is usually first reached by the path that spins least. With
  three threads spinning on each other, this took a ring buffer's TSO
  run from over 100 s to 0.2 s. If any execution runs out of fuel, the
  report includes a warning that the results may be incomplete.

Validation: `litmus/` holds 87 tests. They include the classic shapes (MP,
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
scripts/gen_diy.sh /tmp/diyat3 3 6 atomics   # ... and with 3 threads
scripts/compare_herd.py -q /tmp/diy/*.litmus
```

Tests with byte/halfword or pair accesses run in herd7 with
`-variant mixed`. A test reports `MATCH` (same states), `MISMATCH`,
`KNOWN-DIFF` (a deliberate, documented difference), or `SUPERSET`
(a loop test where armem finds extra states because herd7 unrolls loops
only a bounded number of times).

Current results (herdtools7 7.58; the diy7 sets and `litmus/` rerun in
November 2026 with the dynamic reduction, the default since then):

| Test set | Tests | Result |
|---|---|---|
| herdtools7 catalogue (`aarch64`, `aarch64-mixed`, `aarch64-pick`, `aarch64-down-one-leg`) | 216 | 213 match; herd7 fails on 3 |
| diy7, basic edges, 2 threads | 2949 | all match |
| diy7, basic edges, 3 threads | 5428 | all match |
| diy7, atomics/exclusives/CSEL, 2 threads | 1145 | all match |
| diy7, atomics/exclusives/CSEL, 3 threads | 13507 | 13461 match; herd7 times out (120 s) on 46 |
| `litmus/` | 91 | 71 match, 1 known difference; herd7 times out or rejects 19 |

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
  atomics (other private memory can, if they neither acquire nor
  release).
* The exhaustive search suits tests with tens of shared writes, not
  thousands. The reduction removes orders nothing can observe, and
  private memory removes writes no other thread can see, but writes that
  other threads read in an order they can observe still multiply the
  memories, and so does each prefix of every memory. So do a shared
  counter that several threads increment, and a spinlock that spins on
  an exchange, which writes on every turn (see [C programs](#c-programs)).
  Beyond that, `--sample` gives up completeness. ck's multi-producer
  FIFO (`ck_fifo_mpmc`), with two threads that each dequeue and enqueue
  once, does not finish in 20 minutes: it explores about 300 memories a
  second, and is still finding new ones after 361,000. Nearly all of them
  differ only in the order of writes to different locations: the first
  60,001 hold just 182 sets of writes, each with one order of the writes
  to each location, and one set comes in 4,974 orders. The reduction
  prunes none of them. Nodes are re-used, so both threads read and write
  every field of every node, and ck's load fences order any read before
  any later one, so for any two writes the other thread may read both in
  order, as far as the thread-wide footprints of the pre-pass can tell.
  The orders are real ARM behaviors (an enqueue's stores to the fields
  of the node it re-uses come in every order, some before the dequeue's
  compare-and-swap that claimed the node), and observable in principle,
  since the bug is the other thread still reading that node. Telling
  which orders some execution observes needs a reduction that knows
  which writes the threads read, not only where they may read; see
  [docs/dynamic-reduction.md](docs/dynamic-reduction.md). That reduction
  is the default (`--no-dynamic-por` turns it off). It runs each thread
  against each memory, with later writes by other threads read as any
  value the pre-pass allows, and records which accesses some run orders
  after the memory's last write. Runs that get past that write are cut
  off, and a widened counter's value is tracked as unknown until then. It
  takes this test to 6,752 memories (58 s), and with `-DFENCE_DEQ` to
  4,894 (35 s); across the C programs, from 422,677 memories in 320 s to
  119,710 in 91 s, with the same final states. Where its runs grow too
  large (as in ck's spinlocks) it gives up on the memory, which is then
  pruned by the static reduction alone.
* The TSO reference cannot tell a seq_cst store from a release store,
  since both compile to `STLR` on ARM, so it runs every `STLR` as an
  ordinary x86 store. x86 compilers make a seq_cst store an `XCHG`, a
  full barrier. A seq_cst store followed by a plain load (`STLR; LDR`)
  can therefore show store buffering under TSO that real x86 code does
  not (see [liburcu](#liburcu)).

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
   ARM-only bugs in their weakened versions (see [C programs](#c-programs)).
   So does Concurrency Kit's ring buffer, and armem finds the ARM-only
   bugs that ck fixed in June 2026 in it and in `ck_epoch`, and ck's
   seven spinlocks and its sequence lock run too (see [Concurrency
   Kit](#concurrency-kit)), and with a second poller armem finds the
   ARM-only bug ck fixed in `ck_epoch_poll`, and its FIFOs run too: in
   the multi-producer one, armem finds an ARM-only bug that ck has not
   fixed. On DPDK's ring, armem finds both of the ARM-only bugs DPDK fixed
   in it, including one that needs RCpc acquire loads, and ARM-only bugs
   in the old HTS and RTS modes that DPDK's commits changed without
   describing them as bugs (see [DPDK](#dpdk)). In liburcu's QSBR and mb
   flavors, armem finds the lost grace-period wakeups liburcu fixed in
   January 2026 (see [liburcu](#liburcu)), and in QEMU's QemuEvent the
   lost wakeup QEMU fixed in March 2023 (see [QEMU](#qemu)), and in
   libuv's async handle a lost coalesced `uv_async_send` that libuv's
   June 2026 fix does not prevent on ARM (see [libuv](#libuv)), and in
   liburing the lost SQPOLL wakeup liburing fixed in March 2022, and that
   its June 2026 change to the CQ head load is safe (see
   [liburing](#liburing)), and in moodycamel's ReaderWriterQueue both
   ARM-only bugs that its pull request #172 fixed, one of them added by
   the PR's own first commit (see [readerwriterqueue](#readerwriterqueue)). In the Chase–Lev
   work-stealing deque, armem finds which of its fences each compiler
   mapping needs (see [C programs](#c-programs)). Next come other
   libraries.
3. Scalable exploration for large programs: seeded sampling and a
   partial-order reduction of promise orders are done (see
   [How it works](#how-it-works)), and so are taking local steps at
   once and in order of steps used, private memory, and finding which
   reads each dependency orders (the last took the ring buffer with
   `head` relaxed, 2 slots and 3 items, from 2,745 memories in 3 minutes
   43 s to 583 in 20 s), and dropping states with a promise that can no
   longer be fulfilled (the Treiber stack with its push's release
   relaxed, from 6 minutes 43 s to 15 s, then with a faster hasher and
   coherence kept as ranges to 5 s), and private memory for atomics
   that neither acquire nor release (`ck_epoch_poll` from over 2
   minutes to 3.4 s), and private memory and interned thread states in
   the reference models, and register liveness (two enqueuers on ck's
   multi-producer FIFO from 39,421 memories in 232 s to 1,221 in 4.4 s),
   and certifying only the promises the thread's own exploration shows
   can certify (that FIFO's full test from 30 memories a second to 300);
   and a reduction that uses what the threads may do after each memory,
   which that test needed (`ck_fifo_mpmc` with `-DFENCE_DEQ` from not
   finishing in 20 minutes to 35 s, and the C programs from 320 s to
   91 s; see [docs/dynamic-reduction.md](docs/dynamic-reduction.md));
   next comes guiding the sampling towards reads that disagree with
   what TSO would return.
4. System-level emulation: exception levels, MMU/page tables, timers and
   interrupt controller, basic devices (UART, virtio), with the memory
   model applied to normal memory. Goal: boot Linux on a multi-core
   virtual machine and search for ARM-only behaviors.
