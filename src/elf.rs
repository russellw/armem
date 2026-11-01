//! Loader for 64-bit little-endian AArch64 ELF files.
//!
//! Reads relocatable objects (`.o`), executables and shared objects with no
//! external dependencies. It exposes the sections, loadable segments,
//! symbol table and relocations, and extracts a function's machine code by
//! symbol name, ready for the decoder.
//!
//! Every offset and size read from the file is checked: a malformed file is
//! an error, never a panic or a guess.
//!
//! Extracting a function fails if the result could silently differ from
//! the real code: when an instruction in it still needs a relocation (in
//! an object file, a call or a global's address is a placeholder until
//! link time), or when the function contains data, such as a literal pool,
//! marked by a `$d` mapping symbol.

use crate::decode;
use crate::isa::Instr;

pub type Result<T> = std::result::Result<T, String>;

const EM_AARCH64: u16 = 183;

pub const SHT_PROGBITS: u32 = 1;
pub const SHT_SYMTAB: u32 = 2;
pub const SHT_STRTAB: u32 = 3;
pub const SHT_RELA: u32 = 4;
pub const SHT_NOBITS: u32 = 8;
pub const SHT_REL: u32 = 9;
pub const SHT_DYNSYM: u32 = 11;
pub const SHT_SYMTAB_SHNDX: u32 = 18;

pub const SHF_WRITE: u64 = 0x1;
pub const SHF_ALLOC: u64 = 0x2;
pub const SHF_EXECINSTR: u64 = 0x4;

pub const PT_LOAD: u32 = 1;

pub const PF_X: u32 = 0x1;
pub const PF_W: u32 = 0x2;
pub const PF_R: u32 = 0x4;

const SHN_UNDEF: u32 = 0;
const SHN_LORESERVE: u32 = 0xff00;
const SHN_ABS: u32 = 0xfff1;
const SHN_COMMON: u32 = 0xfff2;
const SHN_XINDEX: u32 = 0xffff;
const PN_XNUM: u16 = 0xffff;

const EHDR_SIZE: usize = 64;
const SHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;
const SYM_SIZE: u64 = 24;
const RELA_SIZE: u64 = 24;
const REL_SIZE: u64 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// ET_REL: an object file.
    Relocatable,
    /// ET_EXEC
    Executable,
    /// ET_DYN: a shared object or position-independent executable.
    Shared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    /// `SHT_*`
    pub kind: u32,
    /// `SHF_*`
    pub flags: u64,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
    pub info: u32,
    pub align: u64,
    pub entsize: u64,
}

impl Section {
    pub fn is_code(&self) -> bool {
        self.kind == SHT_PROGBITS && self.flags & SHF_EXECINSTR != 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// `PT_*`
    pub kind: u32,
    /// `PF_*`
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymKind {
    NoType,
    Object,
    Func,
    Section,
    File,
    Common,
    Tls,
    Other(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    Local,
    Global,
    Weak,
    Other(u8),
}

/// Where a symbol is defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymSection {
    Undefined,
    Absolute,
    Common,
    /// A section index (already resolved through `SHT_SYMTAB_SHNDX`).
    Index(usize),
    /// Another reserved index.
    Reserved(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// In an object file, the offset within its section; otherwise a
    /// virtual address.
    pub value: u64,
    pub size: u64,
    pub kind: SymKind,
    pub bind: Binding,
    /// The `st_other` byte (visibility).
    pub other: u8,
    pub section: SymSection,
}

impl Symbol {
    /// An AArch64 mapping symbol: `Some(true)` for `$x` (code starts here),
    /// `Some(false)` for `$d` (data starts here).
    pub fn mapping(&self) -> Option<bool> {
        let tag = |t: &str| self.name == t || self.name.strip_prefix(t).is_some_and(|r| r.starts_with('.'));
        if tag("$x") {
            Some(true)
        } else if tag("$d") {
            Some(false)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reloc {
    /// The relocation section this entry came from.
    pub from: usize,
    /// The section it applies to (`sh_info`), if any.
    pub target: Option<usize>,
    /// In an object file, the offset within the target section; otherwise
    /// a virtual address.
    pub offset: u64,
    /// `R_AARCH64_*`
    pub kind: u32,
    /// Index into the symbol table named by the relocation section's
    /// `sh_link` (0: no symbol).
    pub symbol: u32,
    /// None for an `SHT_REL` entry, whose addend is stored in the place.
    pub addend: Option<i64>,
}

/// A function's machine code.
#[derive(Clone, Debug)]
pub struct Function<'a> {
    pub name: String,
    pub section: usize,
    /// The symbol value: section offset in an object file, else address.
    pub addr: u64,
    pub code: &'a [u8],
}

impl Function<'_> {
    /// Decode the code. Branches must stay within the function.
    pub fn decode(&self) -> Result<Vec<Instr>> {
        decode::decode_program(self.code).map_err(|e| format!("function '{}': {}", self.name, e))
    }
}

#[derive(Clone, Debug)]
pub struct Elf<'a> {
    data: &'a [u8],
    pub kind: FileKind,
    pub entry: u64,
    pub sections: Vec<Section>,
    pub segments: Vec<Segment>,
    /// The static symbol table (`.symtab`), or the dynamic one if the file
    /// has no static table. Index 0 is the null symbol.
    pub symbols: Vec<Symbol>,
    /// The section holding `symbols`, if any.
    pub symtab: Option<usize>,
    pub relocs: Vec<Reloc>,
}

fn rd<const N: usize>(b: &[u8], off: usize) -> [u8; N] {
    b[off..off + N].try_into().unwrap()
}
fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(rd(b, off))
}
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(rd(b, off))
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(rd(b, off))
}

/// `len` bytes at `off`, or an error naming `what`.
fn slice<'a>(data: &'a [u8], off: u64, len: u64, what: &str) -> Result<&'a [u8]> {
    let end = off.checked_add(len);
    match end {
        Some(end) if end <= data.len() as u64 => Ok(&data[off as usize..end as usize]),
        _ => Err(format!("{} (offset {:#x}, size {:#x}) extends past the end of the file", what, off, len)),
    }
}

/// The NUL-terminated string at `off` in a string table.
fn cstr(table: &[u8], off: u32, what: &str) -> Result<String> {
    let rest = table.get(off as usize..).ok_or_else(|| format!("{} name offset {:#x} is outside the string table", what, off))?;
    let len = rest.iter().position(|&c| c == 0).ok_or_else(|| format!("{} name at {:#x} is not terminated", what, off))?;
    Ok(String::from_utf8_lossy(&rest[..len]).into_owned())
}

/// The name of an AArch64 relocation type, for messages.
pub fn reloc_name(kind: u32) -> Option<&'static str> {
    Some(match kind {
        0 => "R_AARCH64_NONE",
        257 => "R_AARCH64_ABS64",
        258 => "R_AARCH64_ABS32",
        259 => "R_AARCH64_ABS16",
        260 => "R_AARCH64_PREL64",
        261 => "R_AARCH64_PREL32",
        262 => "R_AARCH64_PREL16",
        263 => "R_AARCH64_MOVW_UABS_G0",
        264 => "R_AARCH64_MOVW_UABS_G0_NC",
        265 => "R_AARCH64_MOVW_UABS_G1",
        266 => "R_AARCH64_MOVW_UABS_G1_NC",
        267 => "R_AARCH64_MOVW_UABS_G2",
        268 => "R_AARCH64_MOVW_UABS_G2_NC",
        269 => "R_AARCH64_MOVW_UABS_G3",
        273 => "R_AARCH64_LD_PREL_LO19",
        274 => "R_AARCH64_ADR_PREL_LO21",
        275 => "R_AARCH64_ADR_PREL_PG_HI21",
        276 => "R_AARCH64_ADR_PREL_PG_HI21_NC",
        277 => "R_AARCH64_ADD_ABS_LO12_NC",
        278 => "R_AARCH64_LDST8_ABS_LO12_NC",
        279 => "R_AARCH64_TSTBR14",
        280 => "R_AARCH64_CONDBR19",
        282 => "R_AARCH64_JUMP26",
        283 => "R_AARCH64_CALL26",
        284 => "R_AARCH64_LDST16_ABS_LO12_NC",
        285 => "R_AARCH64_LDST32_ABS_LO12_NC",
        286 => "R_AARCH64_LDST64_ABS_LO12_NC",
        299 => "R_AARCH64_LDST128_ABS_LO12_NC",
        311 => "R_AARCH64_ADR_GOT_PAGE",
        312 => "R_AARCH64_LD64_GOT_LO12_NC",
        1024 => "R_AARCH64_COPY",
        1025 => "R_AARCH64_GLOB_DAT",
        1026 => "R_AARCH64_JUMP_SLOT",
        1027 => "R_AARCH64_RELATIVE",
        1032 => "R_AARCH64_IRELATIVE",
        _ => return None,
    })
}

impl<'a> Elf<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Elf<'a>> {
        if data.len() < EHDR_SIZE || data[..4] != *b"\x7fELF" {
            return Err("not an ELF file".into());
        }
        if data[4] != 2 {
            return Err("not a 64-bit ELF file".into());
        }
        if data[5] != 1 {
            return Err("not a little-endian ELF file".into());
        }
        if data[6] != 1 {
            return Err(format!("unknown ELF version {}", data[6]));
        }
        let kind = match u16_at(data, 16) {
            1 => FileKind::Relocatable,
            2 => FileKind::Executable,
            3 => FileKind::Shared,
            t => return Err(format!("unsupported ELF file type {}", t)),
        };
        let machine = u16_at(data, 18);
        if machine != EM_AARCH64 {
            return Err(format!("not an AArch64 file (machine {})", machine));
        }
        let entry = u64_at(data, 24);
        let (phoff, shoff) = (u64_at(data, 32), u64_at(data, 40));
        let (phentsize, phnum) = (u16_at(data, 54), u16_at(data, 56));
        let (shentsize, shnum, shstrndx) = (u16_at(data, 58), u16_at(data, 60), u16_at(data, 62));

        // Section headers. Section 0 holds the real count and string table
        // index when they do not fit in the ELF header.
        let mut sections = Vec::new();
        let mut names = Vec::new();
        if shoff != 0 {
            if shentsize as u64 != SHDR_SIZE {
                return Err(format!("unexpected section header size {}", shentsize));
            }
            let s0 = slice(data, shoff, SHDR_SIZE, "section header 0")?;
            let count = if shnum == 0 { u64_at(s0, 32) } else { shnum as u64 };
            let len = count.checked_mul(SHDR_SIZE).ok_or("too many sections")?;
            let table = slice(data, shoff, len, "section header table")?;
            for h in table.chunks_exact(SHDR_SIZE as usize) {
                let s = Section {
                    name: String::new(),
                    kind: u32_at(h, 4),
                    flags: u64_at(h, 8),
                    addr: u64_at(h, 16),
                    offset: u64_at(h, 24),
                    size: u64_at(h, 32),
                    link: u32_at(h, 40),
                    info: u32_at(h, 44),
                    align: u64_at(h, 48),
                    entsize: u64_at(h, 56),
                };
                if s.kind != SHT_NOBITS && !sections.is_empty() {
                    slice(data, s.offset, s.size, &format!("section {}", sections.len()))?;
                }
                names.push(u32_at(h, 0));
                sections.push(s);
            }
            let strndx = if shstrndx as u32 == SHN_XINDEX { u32_at(s0, 40) } else { shstrndx as u32 };
            if strndx != SHN_UNDEF {
                let st = sections.get(strndx as usize).ok_or("section name table index out of range")?;
                if st.kind != SHT_STRTAB {
                    return Err("section name table is not a string table".into());
                }
                let table = slice(data, st.offset, st.size, "section name table")?;
                for (s, &n) in sections.iter_mut().zip(&names) {
                    s.name = cstr(table, n, "section")?;
                }
            }
        }

        // Program headers.
        let mut segments = Vec::new();
        if phoff != 0 {
            if phentsize as u64 != PHDR_SIZE {
                return Err(format!("unexpected program header size {}", phentsize));
            }
            let count = if phnum == PN_XNUM {
                sections.first().ok_or("PN_XNUM program header count without section 0")?.info as u64
            } else {
                phnum as u64
            };
            let table = slice(data, phoff, count * PHDR_SIZE, "program header table")?;
            for (i, h) in table.chunks_exact(PHDR_SIZE as usize).enumerate() {
                let p = Segment {
                    kind: u32_at(h, 0),
                    flags: u32_at(h, 4),
                    offset: u64_at(h, 8),
                    vaddr: u64_at(h, 16),
                    filesz: u64_at(h, 32),
                    memsz: u64_at(h, 40),
                    align: u64_at(h, 48),
                };
                slice(data, p.offset, p.filesz, &format!("segment {}", i))?;
                if p.kind == PT_LOAD && p.filesz > p.memsz {
                    return Err(format!("segment {}: file size exceeds memory size", i));
                }
                if p.vaddr.checked_add(p.memsz).is_none() {
                    return Err(format!("segment {}: address range overflows", i));
                }
                segments.push(p);
            }
        }

        let mut elf = Elf { data, kind, entry, sections, segments, symbols: Vec::new(), symtab: None, relocs: Vec::new() };
        let find = |k| elf.sections.iter().position(|s| s.kind == k);
        elf.symtab = find(SHT_SYMTAB).or_else(|| find(SHT_DYNSYM));
        if let Some(i) = elf.symtab {
            elf.symbols = elf.parse_symbols(i)?;
        }
        elf.relocs = elf.parse_relocs()?;
        Ok(elf)
    }

    /// The file contents of section `i` (empty for `SHT_NOBITS`).
    pub fn section_data(&self, i: usize) -> Result<&'a [u8]> {
        let s = self.sections.get(i).ok_or_else(|| format!("no section {}", i))?;
        if s.kind == SHT_NOBITS {
            return Ok(&[]);
        }
        slice(self.data, s.offset, s.size, &format!("section {}", i))
    }

    /// The file contents of a segment (`filesz` bytes; the rest of
    /// `memsz` is zero-filled when loaded).
    pub fn segment_data(&self, p: &Segment) -> Result<&'a [u8]> {
        slice(self.data, p.offset, p.filesz, "segment")
    }

    pub fn section_by_name(&self, name: &str) -> Option<usize> {
        self.sections.iter().position(|s| s.name == name)
    }

    /// The table of fixed-size entries in section `i`.
    fn table(&self, i: usize, entsize: u64, what: &str) -> Result<&'a [u8]> {
        let s = &self.sections[i];
        if s.entsize != entsize || s.size % entsize != 0 {
            return Err(format!("{} section {} has entry size {} and size {}", what, i, s.entsize, s.size));
        }
        self.section_data(i)
    }

    fn section_index(&self, i: u32, sym: usize) -> Result<usize> {
        if (i as usize) < self.sections.len() {
            Ok(i as usize)
        } else {
            Err(format!("symbol {} has section index {} out of range", sym, i))
        }
    }

    fn parse_symbols(&self, i: usize) -> Result<Vec<Symbol>> {
        let table = self.table(i, SYM_SIZE, "symbol table")?;
        let link = self.sections[i].link as usize;
        if self.sections.get(link).is_none_or(|s| s.kind != SHT_STRTAB) {
            return Err(format!("symbol table section {} does not link to a string table", i));
        }
        let strings = self.section_data(link)?;
        // Extended section indexes, for symbols with st_shndx == SHN_XINDEX.
        let xindex = match self.sections.iter().position(|s| s.kind == SHT_SYMTAB_SHNDX && s.link as usize == i) {
            Some(x) => Some(self.table(x, 4, "extended section index")?),
            None => None,
        };
        let mut syms = Vec::new();
        for (n, e) in table.chunks_exact(SYM_SIZE as usize).enumerate() {
            let info = e[4];
            let shndx = u16_at(e, 6) as u32;
            let section = match shndx {
                SHN_UNDEF => SymSection::Undefined,
                SHN_ABS => SymSection::Absolute,
                SHN_COMMON => SymSection::Common,
                SHN_XINDEX => {
                    let x = xindex.and_then(|x| x.get(4 * n..4 * n + 4));
                    let x = u32_at(x.ok_or_else(|| format!("symbol {} has no extended section index", n))?, 0);
                    SymSection::Index(self.section_index(x, n)?)
                }
                _ if shndx >= SHN_LORESERVE => SymSection::Reserved(shndx),
                _ => SymSection::Index(self.section_index(shndx, n)?),
            };
            syms.push(Symbol {
                name: cstr(strings, u32_at(e, 0), "symbol")?,
                value: u64_at(e, 8),
                size: u64_at(e, 16),
                kind: match info & 0xf {
                    0 => SymKind::NoType,
                    1 => SymKind::Object,
                    2 => SymKind::Func,
                    3 => SymKind::Section,
                    4 => SymKind::File,
                    5 => SymKind::Common,
                    6 => SymKind::Tls,
                    t => SymKind::Other(t),
                },
                bind: match info >> 4 {
                    0 => Binding::Local,
                    1 => Binding::Global,
                    2 => Binding::Weak,
                    b => Binding::Other(b),
                },
                other: e[5],
                section,
            });
        }
        Ok(syms)
    }

    fn parse_relocs(&self) -> Result<Vec<Reloc>> {
        let mut out = Vec::new();
        for (i, s) in self.sections.iter().enumerate() {
            let rela = match s.kind {
                SHT_RELA => true,
                SHT_REL => false,
                _ => continue,
            };
            let table = self.table(i, if rela { RELA_SIZE } else { REL_SIZE }, "relocation")?;
            let target = match s.info as usize {
                0 => None,
                t if t < self.sections.len() => Some(t),
                t => return Err(format!("relocation section {} applies to section {}, which does not exist", i, t)),
            };
            for e in table.chunks_exact(s.entsize as usize) {
                let info = u64_at(e, 8);
                out.push(Reloc {
                    from: i,
                    target,
                    offset: u64_at(e, 0),
                    kind: info as u32,
                    symbol: (info >> 32) as u32,
                    addend: rela.then(|| u64_at(e, 16) as i64),
                });
            }
        }
        Ok(out)
    }

    /// The name of a relocation's symbol, for messages.
    fn reloc_symbol(&self, r: &Reloc) -> String {
        let link = self.sections[r.from].link as usize;
        if Some(link) != self.symtab || r.symbol == 0 {
            return format!("symbol {}", r.symbol);
        }
        match self.symbols.get(r.symbol as usize) {
            Some(s) if s.kind == SymKind::Section => match s.section {
                SymSection::Index(i) => format!("section '{}'", self.sections[i].name),
                _ => format!("symbol {}", r.symbol),
            },
            Some(s) => format!("'{}'", s.name),
            None => format!("symbol {}", r.symbol),
        }
    }

    /// The function symbol called `name`. A unique global or weak
    /// definition takes precedence over local ones.
    pub fn function_symbol(&self, name: &str) -> Result<&Symbol> {
        let all: Vec<&Symbol> = self.symbols.iter().filter(|s| s.name == name).collect();
        let funcs: Vec<&Symbol> = all.iter().copied().filter(|s| s.kind == SymKind::Func).collect();
        let defined: Vec<&Symbol> = funcs.iter().copied().filter(|s| s.section != SymSection::Undefined).collect();
        let global: Vec<&Symbol> = defined.iter().copied().filter(|s| s.bind != Binding::Local).collect();
        match (all.len(), funcs.len(), defined.len(), global.len()) {
            (0, ..) => Err(format!("no symbol '{}'", name)),
            (_, 0, ..) => Err(format!("symbol '{}' is not a function", name)),
            (_, _, 0, _) => Err(format!("function '{}' is not defined in this file", name)),
            (_, _, 1, _) => Ok(defined[0]),
            (_, _, _, 1) => Ok(global[0]),
            _ => Err(format!("function name '{}' is ambiguous ({} definitions)", name, defined.len())),
        }
    }

    /// The machine code of the function called `name`.
    pub fn function(&self, name: &str) -> Result<Function<'a>> {
        let sym = self.function_symbol(name)?;
        let SymSection::Index(si) = sym.section else {
            return Err(format!("function '{}' is not in a section", name));
        };
        let sec = &self.sections[si];
        if !sec.is_code() {
            return Err(format!("function '{}' is in section '{}', which is not code", name, sec.name));
        }
        if sym.size == 0 {
            return Err(format!("function '{}' has no size (missing .size directive?)", name));
        }
        let start = sym.value.checked_sub(sec.addr).filter(|&o| o <= sec.size);
        let end = start.and_then(|s| s.checked_add(sym.size)).filter(|&e| e <= sec.size);
        let (Some(start), Some(end)) = (start, end) else {
            return Err(format!("function '{}' extends outside section '{}'", name, sec.name));
        };
        if start % 4 != 0 {
            return Err(format!("function '{}' is not 4-byte aligned", name));
        }
        // In an object file, relocations patch the code at link time.
        if self.kind == FileKind::Relocatable {
            if let Some(r) = self.relocs.iter().filter(|r| r.target == Some(si) && (start..end).contains(&r.offset)).min_by_key(|r| r.offset) {
                let kind = reloc_name(r.kind).map_or_else(|| format!("relocation type {}", r.kind), String::from);
                return Err(format!(
                    "function '{}': instruction at offset {:#x} needs relocation {} against {} (relocations are not supported yet)",
                    name,
                    r.offset - start,
                    kind,
                    self.reloc_symbol(r)
                ));
            }
        }
        // Mapping symbols mark where code and data start within a section.
        let mut maps: Vec<(u64, bool)> = self
            .symbols
            .iter()
            .filter(|s| s.section == SymSection::Index(si))
            .filter_map(|s| Some((s.value.wrapping_sub(sec.addr), s.mapping()?)))
            .collect();
        maps.sort();
        let at_start = maps.iter().rev().find(|m| m.0 <= start).is_none_or(|m| m.1);
        let data = if at_start { maps.iter().find(|m| (start..end).contains(&m.0) && !m.1).map(|m| m.0) } else { Some(start) };
        if let Some(d) = data {
            return Err(format!("function '{}' contains data (e.g. a literal pool) at offset {:#x}", name, d - start));
        }
        let code = &self.section_data(si)?[start as usize..end as usize];
        Ok(Function { name: name.to_string(), section: si, addr: sym.value, code })
    }

    /// Decode the function called `name`.
    pub fn decode_function(&self, name: &str) -> Result<Vec<Instr>> {
        self.function(name)?.decode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isa::*;

    /// Build a minimal executable: one PT_LOAD segment holding `.text`
    /// at 0x400000 + offset, plus `.symtab`, `.strtab` and `.shstrtab`.
    /// `syms` are (name, value, size, st_info).
    fn build(text: &[u8], syms: &[(&str, u64, u64, u8)]) -> Vec<u8> {
        const BASE: u64 = 0x40_0000;
        let mut f = vec![0u8; 64 + 56];
        let text_off = f.len() as u64;
        f.extend_from_slice(text);
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; 24];
        for &(name, value, size, info) in syms {
            let off = strtab.len() as u32;
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            let mut e = [0u8; 24];
            e[0..4].copy_from_slice(&off.to_le_bytes());
            e[4] = info;
            e[6..8].copy_from_slice(&1u16.to_le_bytes());
            e[8..16].copy_from_slice(&value.to_le_bytes());
            e[16..24].copy_from_slice(&size.to_le_bytes());
            symtab.extend_from_slice(&e);
        }
        let shstrtab = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
        let pad = |f: &mut Vec<u8>| f.resize(f.len().next_multiple_of(8), 0);
        pad(&mut f);
        let symtab_off = f.len() as u64;
        f.extend_from_slice(&symtab);
        let strtab_off = f.len() as u64;
        f.extend_from_slice(&strtab);
        let shstrtab_off = f.len() as u64;
        f.extend_from_slice(shstrtab);
        pad(&mut f);
        let shoff = f.len() as u64;
        // (name, type, flags, addr, offset, size, link, info, align, entsize)
        let shdrs: [(u32, u32, u64, u64, u64, u64, u32, u32, u64, u64); 5] = [
            (0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
            (1, SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, BASE + text_off, text_off, text.len() as u64, 0, 0, 4, 0),
            (7, SHT_SYMTAB, 0, 0, symtab_off, symtab.len() as u64, 3, 1, 8, 24),
            (15, SHT_STRTAB, 0, 0, strtab_off, strtab.len() as u64, 0, 0, 1, 0),
            (23, SHT_STRTAB, 0, 0, shstrtab_off, shstrtab.len() as u64, 0, 0, 1, 0),
        ];
        for (name, kind, flags, addr, offset, size, link, info, align, entsize) in shdrs {
            f.extend_from_slice(&name.to_le_bytes());
            f.extend_from_slice(&kind.to_le_bytes());
            for v in [flags, addr, offset, size] {
                f.extend_from_slice(&v.to_le_bytes());
            }
            f.extend_from_slice(&link.to_le_bytes());
            f.extend_from_slice(&info.to_le_bytes());
            f.extend_from_slice(&align.to_le_bytes());
            f.extend_from_slice(&entsize.to_le_bytes());
        }
        // ELF header.
        f[0..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x00");
        let put16 = |f: &mut Vec<u8>, o: usize, v: u16| f[o..o + 2].copy_from_slice(&v.to_le_bytes());
        let put64 = |f: &mut Vec<u8>, o: usize, v: u64| f[o..o + 8].copy_from_slice(&v.to_le_bytes());
        put16(&mut f, 16, 2);
        put16(&mut f, 18, EM_AARCH64);
        f[20] = 1;
        put64(&mut f, 24, BASE + text_off);
        put64(&mut f, 32, 64);
        put64(&mut f, 40, shoff);
        put16(&mut f, 52, 64);
        put16(&mut f, 54, 56);
        put16(&mut f, 56, 1);
        put16(&mut f, 58, 64);
        put16(&mut f, 60, 5);
        put16(&mut f, 62, 4);
        // Program header: the file from offset 0 through the text.
        let ph = 64;
        f[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        f[ph + 4..ph + 8].copy_from_slice(&(PF_R | PF_X).to_le_bytes());
        put64(&mut f, ph + 16, BASE);
        put64(&mut f, ph + 24, BASE);
        put64(&mut f, ph + 32, text_off + text.len() as u64);
        put64(&mut f, ph + 40, text_off + text.len() as u64);
        put64(&mut f, ph + 48, 0x1000);
        f
    }

    const FUNC: u8 = 0x12; // global function
    const NOP: u32 = 0xd503201f;
    const MOV_X0_1: u32 = 0xd2800020; // MOVZ X0,#1
    const B_BACK: u32 = 0x17ffffff; // B .-4

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn sample() -> Vec<u8> {
        build(
            &words(&[NOP, MOV_X0_1, B_BACK, 0xdeadbeef, NOP]),
            &[("f", 0x400078, 12, FUNC), ("g", 0x400078 + 16, 4, FUNC), ("$d", 0x400078 + 12, 0, 0), ("$x", 0x400078 + 16, 0, 0), ("obj", 0x400078, 4, 0x11)],
        )
    }

    #[test]
    fn executable() {
        let data = sample();
        let elf = Elf::parse(&data).unwrap();
        assert_eq!(elf.kind, FileKind::Executable);
        assert_eq!(elf.entry, 0x400078);
        assert_eq!(elf.sections.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["", ".text", ".symtab", ".strtab", ".shstrtab"]);
        assert_eq!(elf.segments.len(), 1);
        let seg = &elf.segments[0];
        assert_eq!((seg.kind, seg.vaddr, seg.filesz), (PT_LOAD, 0x400000, 0x78 + 20));
        assert_eq!(&elf.segment_data(seg).unwrap()[..4], b"\x7fELF");
        let x = |i| RegOp { idx: i, wide: true };
        assert_eq!(elf.decode_function("f").unwrap(), [Instr::Nop, Instr::MovImm { rd: x(0), imm: 1 }, Instr::B { target: 1 }]);
        assert_eq!(elf.decode_function("g").unwrap(), [Instr::Nop]);
        assert_eq!(elf.function("f").unwrap().addr, 0x400078);
    }

    #[test]
    fn function_errors() {
        let err = |f: Vec<u8>, name: &str| Elf::parse(&f).unwrap().function(name).unwrap_err();
        assert!(err(sample(), "nope").contains("no symbol"));
        assert!(err(sample(), "obj").contains("not a function"));
        let pool = build(&words(&[NOP, 0, NOP]), &[("f", 0x400078, 12, FUNC), ("$x", 0x400078, 0, 0), ("$d.1", 0x40007c, 0, 0), ("$x.2", 0x400080, 0, 0)]);
        assert!(err(pool.clone(), "f").contains("data (e.g. a literal pool) at offset 0x4"), "{}", err(pool, "f"));
        let in_data = build(&words(&[0, NOP]), &[("f", 0x40007c, 4, FUNC), ("$d", 0x400078, 0, 0)]);
        assert!(err(in_data, "f").contains("at offset 0x0"));
        assert!(err(build(&words(&[NOP]), &[("f", 0x400078, 8, FUNC)]), "f").contains("outside section"));
        assert!(err(build(&words(&[NOP]), &[("f", 0x400078, 0, FUNC)]), "f").contains("no size"));
        assert!(err(build(&words(&[NOP]), &[("f", 0x400000, 4, FUNC)]), "f").contains("outside section"));
        let dup = build(&words(&[NOP, NOP]), &[("f", 0x400078, 4, 0x02), ("f", 0x40007c, 4, 0x02)]);
        assert!(err(dup, "f").contains("ambiguous"));
        // A unique global wins over a local of the same name.
        let shadow = build(&words(&[NOP, MOV_X0_1]), &[("f", 0x400078, 4, 0x02), ("f", 0x40007c, 4, FUNC)]);
        assert_eq!(Elf::parse(&shadow).unwrap().function("f").unwrap().addr, 0x40007c);
        // A branch out of the function is a decoding error.
        let out = build(&words(&[NOP, B_BACK]), &[("f", 0x40007c, 4, FUNC)]);
        assert!(Elf::parse(&out).unwrap().decode_function("f").unwrap_err().contains("before the start"));
        let out = build(&words(&[0x14000002, NOP, NOP]), &[("f", 0x400078, 4, FUNC)]); // B .+8
        assert!(Elf::parse(&out).unwrap().decode_function("f").unwrap_err().contains("outside the code"));
    }

    #[test]
    fn rejects_bad_headers() {
        let data = sample();
        let bad = |o: usize, v: u8| {
            let mut d = data.clone();
            d[o] = v;
            Elf::parse(&d).unwrap_err()
        };
        assert!(bad(0, 0).contains("not an ELF"));
        assert!(bad(4, 1).contains("64-bit"));
        assert!(bad(5, 2).contains("little-endian"));
        assert!(bad(18, 62).contains("AArch64"));
        assert!(bad(16, 4).contains("file type"));
        assert!(Elf::parse(&data[..40]).unwrap_err().contains("not an ELF"));
        assert!(Elf::parse(&data[..data.len() - 1]).unwrap_err().contains("past the end"));
    }

    /// Malformed input must give an error, never a panic.
    #[test]
    fn corrupt_files_do_not_panic() {
        let data = sample();
        for len in 0..data.len() {
            let _ = Elf::parse(&data[..len]).map(|e| e.function("f"));
        }
        // Deterministic pseudo-random byte and field corruption.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            let mut d = data.clone();
            for _ in 0..1 + next() % 4 {
                let o = (next() % d.len() as u64) as usize;
                d[o] = match next() % 3 {
                    0 => next() as u8,
                    1 => 0xff,
                    _ => 0,
                };
            }
            if let Ok(e) = Elf::parse(&d) {
                for name in ["f", "g", "$d"] {
                    let _ = e.decode_function(name);
                }
                for i in 0..e.sections.len() {
                    let _ = e.section_data(i);
                }
            }
        }
    }

    #[test]
    fn mapping_symbols() {
        let s = |n: &str| Symbol {
            name: n.into(),
            value: 0,
            size: 0,
            kind: SymKind::NoType,
            bind: Binding::Local,
            other: 0,
            section: SymSection::Undefined,
        };
        assert_eq!(s("$x").mapping(), Some(true));
        assert_eq!(s("$d.42").mapping(), Some(false));
        assert_eq!(s("$xyz").mapping(), None);
        assert_eq!(s("x").mapping(), None);
    }
}
