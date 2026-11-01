//! Loading an ELF file into memory: section layout and relocation.
//!
//! An object file is linked on its own, as a static linker would do. Each
//! allocated section gets an address, common symbols get space, and the
//! relocations are applied to the section contents. Symbols the file uses
//! but does not define must be given addresses (`LoadOptions::externs`).
//! The exception is an undefined weak symbol, which resolves to 0.
//!
//! The supported relocations are the static data and instruction
//! relocations that non-PIC code uses, and the basic GOT relocations (see
//! `apply`). They follow lld: values are checked for range and alignment,
//! an undefined weak symbol resolves as `undefined_weak` describes, and a
//! GOT is built with one slot per symbol. TLS and other relocations are
//! errors, so a loaded image is never silently wrong.
//!
//! A linked executable or shared object is loaded from its `PT_LOAD`
//! segments at the addresses in the file. Its dynamic relocations are not
//! applied yet.

use crate::elf::*;
use crate::isa::Size;
use crate::litmus::{DEFAULT_LOC_TYPE, LocType, PreSym, Preload, is_ident};
use crate::memory::{InitMem, Region};
use std::collections::BTreeMap;

pub type Result<T> = std::result::Result<T, String>;

/// Largest total image size, so a corrupt size cannot exhaust memory.
const MAX_IMAGE: u64 = 1 << 30;

#[derive(Clone, Debug)]
pub struct LoadOptions {
    /// Where an object file's allocated sections are placed, in section
    /// order and each aligned, skipping the sections in `section_addrs`.
    /// Common symbols follow.
    pub base: u64,
    /// Explicit addresses for an object file's sections, by name.
    pub section_addrs: BTreeMap<String, u64>,
    /// Addresses of symbols an object file uses but does not define.
    pub externs: BTreeMap<String, u64>,
}

impl Default for LoadOptions {
    fn default() -> LoadOptions {
        LoadOptions { base: 0x1_0000, section_addrs: BTreeMap::new(), externs: BTreeMap::new() }
    }
}

/// A contiguous piece of the loaded image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub addr: u64,
    pub bytes: Vec<u8>,
    pub writable: bool,
    pub exec: bool,
    /// What it was loaded from, for messages: a section or symbol name, or
    /// a segment number.
    pub origin: String,
}

#[derive(Clone, Debug)]
pub struct Image<'e, 'a> {
    pub elf: &'e Elf<'a>,
    /// Sorted by address, non-overlapping, none empty.
    pub chunks: Vec<Chunk>,
    /// The address of each section, if it was loaded.
    pub section_addrs: Vec<Option<u64>>,
    /// The address of each symbol of `elf.symbols`, if it has one.
    pub symbol_addrs: Vec<Option<u64>>,
    /// The address of the GOT built for an object file, if it needs one.
    pub got: Option<u64>,
}

fn align_up(v: u64, align: u64) -> Option<u64> {
    v.checked_add(align - 1).map(|x| x & !(align - 1))
}

/// Free address ranges, for placing sections around explicit ones.
struct Placer {
    cursor: u64,
    taken: Vec<(u64, u64)>,
}

impl Placer {
    /// The first address at or after the cursor, aligned to `align`, where
    /// `size` bytes do not overlap a taken range.
    fn place(&mut self, size: u64, align: u64) -> Result<u64> {
        let overflow = || "image layout overflows the address space".to_string();
        let mut a = align_up(self.cursor, align).ok_or_else(overflow)?;
        loop {
            let end = a.checked_add(size).ok_or_else(overflow)?;
            match self.taken.iter().find(|&&(s, e)| a < e && s < end.max(a.saturating_add(1))) {
                Some(&(_, e)) => a = align_up(e, align).ok_or_else(overflow)?,
                None => {
                    self.cursor = end;
                    return Ok(a);
                }
            }
        }
    }
}

fn section_align(s: &Section) -> Result<u64> {
    match s.align {
        0 | 1 => Ok(1),
        a if a.is_power_of_two() => Ok(a),
        a => Err(format!("section '{}' has alignment {}, which is not a power of two", s.name, a)),
    }
}

/// Check that `v` is a signed `n`-bit value.
fn check_int(v: u64, n: u32) -> Result<()> {
    let x = v as i64;
    if x >= -(1i64 << (n - 1)) && x < 1i64 << (n - 1) {
        Ok(())
    } else {
        Err(format!("value {} is out of range for a signed {}-bit field", x, n))
    }
}

/// Check that `v` is an unsigned `n`-bit value.
fn check_uint(v: u64, n: u32) -> Result<()> {
    if v >> n == 0 { Ok(()) } else { Err(format!("value {:#x} is out of range for an unsigned {}-bit field", v, n)) }
}

/// Check that `v` is a signed or unsigned `n`-bit value.
fn check_int_uint(v: u64, n: u32) -> Result<()> {
    let x = v as i64;
    if x >= -(1i64 << (n - 1)) && x < 1i64 << n {
        Ok(())
    } else {
        Err(format!("value {:#x} is out of range for a {}-bit field", v, n))
    }
}

fn check_align(v: u64, n: u64) -> Result<()> {
    if v % n == 0 { Ok(()) } else { Err(format!("value {:#x} is not a multiple of {}", v, n)) }
}

fn place_bytes<'p>(place: &'p mut [u8], offset: u64, width: u64) -> Result<&'p mut [u8]> {
    match offset.checked_add(width) {
        Some(end) if end <= place.len() as u64 => Ok(&mut place[offset as usize..end as usize]),
        _ => Err("the place is outside its section".into()),
    }
}

/// Replace the bits of `mask` in the instruction at `offset`.
fn patch(bytes: &mut [u8], offset: u64, mask: u32, value: u32) -> Result<()> {
    let p = place_bytes(bytes, offset, 4)?;
    let w = u32::from_le_bytes(p[..4].try_into().unwrap());
    p.copy_from_slice(&((w & !mask) | (value & mask)).to_le_bytes());
    Ok(())
}

/// Set the 21-bit immediate of an ADR/ADRP instruction.
fn patch_adr(bytes: &mut [u8], offset: u64, imm: u64) -> Result<()> {
    let imm = imm as u32;
    patch(bytes, offset, 3 << 29 | 0x7ffff << 5, (imm & 3) << 29 | (imm >> 2 & 0x7ffff) << 5)
}

fn write(bytes: &mut [u8], offset: u64, value: u64, width: u64) -> Result<()> {
    place_bytes(bytes, offset, width)?.copy_from_slice(&value.to_le_bytes()[..width as usize]);
    Ok(())
}

/// R_AARCH64_ADR_GOT_PAGE, R_AARCH64_LD64_GOT_LO12_NC
fn is_got(kind: u32) -> bool {
    kind == 311 || kind == 312
}

/// The value used for an undefined weak symbol, as in lld and the AArch64
/// ELF ABI: 0 for absolute relocations, the place itself for PC-relative
/// ones (so the result is the addend), and the next instruction for
/// branches (which then do nothing).
fn undefined_weak(kind: u32, p: u64) -> u64 {
    match kind {
        279 | 280 | 282 | 283 => p.wrapping_add(4),
        260..=262 | 314 | 273..=276 => p,
        _ => 0,
    }
}

/// Apply relocation `kind` at `offset` in `bytes`, for symbol address `s`,
/// addend `a` and place address `p`. Range and alignment checks follow lld.
fn apply(kind: u32, bytes: &mut [u8], offset: u64, s: u64, a: i64, p: u64) -> Result<()> {
    let sa = s.wrapping_add(a as u64);
    let prel = sa.wrapping_sub(p);
    let page = |x: u64| x & !0xfff;
    match kind {
        0 => Ok(()), // R_AARCH64_NONE
        257 => write(bytes, offset, sa, 8),
        258 => check_int_uint(sa, 32).and_then(|_| write(bytes, offset, sa, 4)),
        259 => check_int_uint(sa, 16).and_then(|_| write(bytes, offset, sa, 2)),
        260 => write(bytes, offset, prel, 8),
        261 => check_int_uint(prel, 32).and_then(|_| write(bytes, offset, prel, 4)),
        262 => check_int_uint(prel, 16).and_then(|_| write(bytes, offset, prel, 2)),
        314 => check_int(prel, 32).and_then(|_| write(bytes, offset, prel, 4)), // PLT32
        // MOVW_UABS_G0, G0_NC, G1, G1_NC, G2, G2_NC, G3
        263..=269 => {
            let group = (kind - 263) / 2;
            if (kind - 263) % 2 == 0 && group < 3 {
                check_uint(sa, 16 * (group + 1))?;
            }
            patch(bytes, offset, 0xffff << 5, ((sa >> (16 * group)) as u32 & 0xffff) << 5)
        }
        // LD_PREL_LO19, CONDBR19
        273 | 280 => {
            check_int(prel, 21)?;
            check_align(prel, 4)?;
            patch(bytes, offset, 0x7ffff << 5, ((prel >> 2) as u32) << 5)
        }
        274 => check_int(prel, 21).and_then(|_| patch_adr(bytes, offset, prel)),
        275 | 276 => {
            let v = page(sa).wrapping_sub(page(p));
            if kind == 275 {
                check_int(v, 33)?;
            }
            patch_adr(bytes, offset, ((v as i64) >> 12) as u64)
        }
        277 | 278 => patch(bytes, offset, 0xfff << 10, (sa as u32 & 0xfff) << 10),
        // LDST16/32/64/128_ABS_LO12_NC
        284 | 285 | 286 | 299 => {
            let scale = match kind {
                284 => 1,
                285 => 2,
                286 => 3,
                _ => 4,
            };
            check_align(sa, 1 << scale)?;
            patch(bytes, offset, 0xfff << 10, ((sa as u32 & 0xfff) >> scale) << 10)
        }
        279 => {
            check_int(prel, 16)?;
            check_align(prel, 4)?;
            patch(bytes, offset, 0x3fff << 5, ((prel >> 2) as u32) << 5)
        }
        // JUMP26, CALL26
        282 | 283 => {
            check_int(prel, 28)?;
            check_align(prel, 4)?;
            patch(bytes, offset, 0x3ff_ffff, (prel >> 2) as u32)
        }
        // ADR_GOT_PAGE, LD64_GOT_LO12_NC: `s` is the GOT slot's address.
        311 => {
            let v = page(sa).wrapping_sub(page(p));
            check_int(v, 33)?;
            patch_adr(bytes, offset, ((v as i64) >> 12) as u64)
        }
        312 => {
            check_align(sa, 8)?;
            patch(bytes, offset, 0xfff << 10, ((sa as u32 & 0xfff) >> 3) << 10)
        }
        _ => Err("this relocation type is not supported yet".into()),
    }
}

impl<'e, 'a> Image<'e, 'a> {
    pub fn load(elf: &'e Elf<'a>, opts: &LoadOptions) -> Result<Image<'e, 'a>> {
        match elf.kind {
            FileKind::Relocatable => Image::link(elf, opts),
            _ => Image::load_linked(elf, opts),
        }
    }

    fn load_linked(elf: &'e Elf<'a>, opts: &LoadOptions) -> Result<Image<'e, 'a>> {
        if !opts.section_addrs.is_empty() || !opts.externs.is_empty() {
            return Err("section addresses and externs apply only to object files".into());
        }
        let mut chunks = Vec::new();
        let mut total = 0u64;
        for (i, p) in elf.segments.iter().enumerate() {
            if p.kind != PT_LOAD || p.memsz == 0 {
                continue;
            }
            total = total.saturating_add(p.memsz);
            if total > MAX_IMAGE {
                return Err("the image is too large".into());
            }
            let mut bytes = elf.segment_data(p)?.to_vec();
            bytes.resize(p.memsz as usize, 0);
            chunks.push(Chunk {
                addr: p.vaddr,
                bytes,
                writable: p.flags & PF_W != 0,
                exec: p.flags & PF_X != 0,
                origin: format!("segment {}", i),
            });
        }
        let section_addrs = elf.sections.iter().map(|s| (s.flags & SHF_ALLOC != 0).then_some(s.addr)).collect();
        let symbol_addrs = elf
            .symbols
            .iter()
            .map(|s| match s.section {
                SymSection::Index(_) | SymSection::Absolute => Some(s.value),
                _ => None,
            })
            .collect();
        Image::finish(elf, chunks, section_addrs, symbol_addrs, None)
    }

    fn link(elf: &'e Elf<'a>, opts: &LoadOptions) -> Result<Image<'e, 'a>> {
        let alloc: Vec<usize> = (0..elf.sections.len()).filter(|&i| elf.sections[i].flags & SHF_ALLOC != 0).collect();
        let total = alloc.iter().fold(0u64, |t, &i| t.saturating_add(elf.sections[i].size));
        if total > MAX_IMAGE {
            return Err("the image is too large".into());
        }

        // Layout: explicit addresses first, then the rest in order.
        let mut addrs: Vec<Option<u64>> = vec![None; elf.sections.len()];
        let mut placer = Placer { cursor: opts.base, taken: Vec::new() };
        for (name, &a) in &opts.section_addrs {
            let mut matches = alloc.iter().filter(|&&i| elf.sections[i].name == *name);
            let (Some(&i), None) = (matches.next(), matches.next()) else {
                return Err(format!("section_addrs: no unique allocated section named '{}'", name));
            };
            let s = &elf.sections[i];
            if a % section_align(s)? != 0 {
                return Err(format!("section '{}' at {:#x} is not aligned to {}", name, a, s.align));
            }
            let end = a.checked_add(s.size).ok_or_else(|| format!("section '{}' at {:#x} overflows", name, a))?;
            if let Some(j) = (0..elf.sections.len()).find(|&j| addrs[j].is_some_and(|b| b < end.max(a + 1) && a < b + elf.sections[j].size.max(1))) {
                return Err(format!("section '{}' at {:#x} overlaps section '{}' at {:#x}", name, a, elf.sections[j].name, addrs[j].unwrap()));
            }
            placer.taken.push((a, end));
            addrs[i] = Some(a);
        }
        for &i in &alloc {
            if addrs[i].is_none() {
                let s = &elf.sections[i];
                addrs[i] = Some(placer.place(s.size, section_align(s)?)?);
            }
        }

        let mut chunks = Vec::new();
        let mut bytes: Vec<Vec<u8>> = vec![Vec::new(); elf.sections.len()];
        for &i in &alloc {
            let s = &elf.sections[i];
            bytes[i] = if s.kind == SHT_NOBITS { vec![0; s.size as usize] } else { elf.section_data(i)?.to_vec() };
        }

        // Symbols. Common symbols get space after the sections.
        let mut symbol_addrs = Vec::with_capacity(elf.symbols.len());
        let mut common_total = total;
        for s in &elf.symbols {
            symbol_addrs.push(match s.section {
                SymSection::Index(x) => addrs[x].map(|a| a.wrapping_add(s.value)),
                SymSection::Absolute => Some(s.value),
                SymSection::Common => {
                    if !s.value.is_power_of_two() {
                        return Err(format!("common symbol '{}' has alignment {}, which is not a power of two", s.name, s.value));
                    }
                    common_total = common_total.saturating_add(s.size);
                    if common_total > MAX_IMAGE {
                        return Err("the image is too large".into());
                    }
                    let a = placer.place(s.size, s.value)?;
                    chunks.push(Chunk { addr: a, bytes: vec![0; s.size as usize], writable: true, exec: false, origin: s.name.clone() });
                    Some(a)
                }
                SymSection::Undefined => opts.externs.get(&s.name).copied(),
                SymSection::Reserved(_) => None,
            });
        }

        // The relocations to apply: those for loaded sections (not, for
        // example, debug information), with their symbols resolved. An
        // undefined weak symbol resolves to None.
        let mut relocs = Vec::new();
        for r in &elf.relocs {
            let Some(t) = r.target else { continue };
            let Some(base) = addrs[t] else { continue };
            let ctx = move |e: String| {
                format!("{} at {}+{:#x} against {}: {}", reloc_display(r.kind), elf.sections[t].name, r.offset, elf.reloc_symbol(r), e)
            };
            if Some(elf.sections[r.from].link as usize) != elf.symtab {
                return Err(ctx("relocation section does not use the symbol table".into()));
            }
            let Some(a) = r.addend else {
                return Err(ctx("SHT_REL relocations are not supported".into()));
            };
            let s = if r.symbol == 0 {
                Some(0)
            } else {
                let i = r.symbol as usize;
                let sym = elf.symbols.get(i).ok_or_else(|| ctx("symbol index out of range".into()))?;
                match (symbol_addrs[i], sym.section, sym.bind) {
                    (Some(v), ..) => Some(v),
                    (None, SymSection::Undefined, Binding::Weak) => None,
                    (None, SymSection::Undefined, _) => {
                        return Err(ctx("undefined symbol (give its address in LoadOptions::externs)".into()));
                    }
                    _ => return Err(ctx("the symbol is not in loaded memory".into())),
                }
            };
            relocs.push((r, t, base.wrapping_add(r.offset), s, a, ctx));
        }

        // The GOT: one 8-byte slot per symbol, in order of first use, holding
        // the symbol's address (0 for an undefined weak symbol).
        let mut got_slots: BTreeMap<u32, u64> = BTreeMap::new();
        let mut got_bytes = Vec::new();
        for (r, _, _, s, a, ctx) in &relocs {
            if is_got(r.kind) && !got_slots.contains_key(&r.symbol) {
                if *a != 0 {
                    return Err(ctx("GOT relocations with an addend are not supported".into()));
                }
                got_slots.insert(r.symbol, got_bytes.len() as u64);
                got_bytes.extend_from_slice(&s.unwrap_or(0).to_le_bytes());
            }
        }
        let got = if got_bytes.is_empty() { None } else { Some(placer.place(got_bytes.len() as u64, 8)?) };
        if let Some(g) = got {
            chunks.push(Chunk { addr: g, bytes: got_bytes, writable: true, exec: false, origin: ".got".into() });
        }

        for (r, t, p, s, a, ctx) in relocs {
            let s = if is_got(r.kind) {
                got.unwrap() + got_slots[&r.symbol]
            } else {
                s.unwrap_or_else(|| undefined_weak(r.kind, p))
            };
            apply(r.kind, &mut bytes[t], r.offset, s, a, p).map_err(ctx)?;
        }

        for &i in &alloc {
            let s = &elf.sections[i];
            chunks.push(Chunk {
                addr: addrs[i].unwrap(),
                bytes: std::mem::take(&mut bytes[i]),
                writable: s.flags & SHF_WRITE != 0,
                exec: s.flags & SHF_EXECINSTR != 0,
                origin: s.name.clone(),
            });
        }
        Image::finish(elf, chunks, addrs, symbol_addrs, got)
    }

    fn finish(
        elf: &'e Elf<'a>,
        mut chunks: Vec<Chunk>,
        section_addrs: Vec<Option<u64>>,
        symbol_addrs: Vec<Option<u64>>,
        got: Option<u64>,
    ) -> Result<Image<'e, 'a>> {
        chunks.retain(|c| !c.bytes.is_empty());
        chunks.sort_by_key(|c| c.addr);
        for c in &chunks {
            if c.addr.checked_add(c.bytes.len() as u64).is_none() {
                return Err(format!("{} at {:#x} overflows the address space", c.origin, c.addr));
            }
        }
        for w in chunks.windows(2) {
            if w[0].addr + w[0].bytes.len() as u64 > w[1].addr {
                return Err(format!("{} at {:#x} overlaps {} at {:#x}", w[0].origin, w[0].addr, w[1].origin, w[1].addr));
            }
        }
        Ok(Image { elf, chunks, section_addrs, symbol_addrs, got })
    }

    /// `len` bytes at `addr`, if they lie within one chunk.
    pub fn read(&self, addr: u64, len: u64) -> Option<&[u8]> {
        let i = self.chunks.partition_point(|c| c.addr <= addr).checked_sub(1)?;
        let c = &self.chunks[i];
        let off = addr - c.addr;
        let end = off.checked_add(len)?;
        c.bytes.get(off as usize..usize::try_from(end).ok()?)
    }

    /// The address of the symbol called `name`. A unique global or weak
    /// definition takes precedence over local ones.
    pub fn symbol(&self, name: &str) -> Result<u64> {
        let found: Vec<(&Symbol, u64)> = self
            .elf
            .symbols
            .iter()
            .zip(&self.symbol_addrs)
            .filter(|(s, a)| s.name == name && a.is_some() && !matches!(s.kind, SymKind::Section | SymKind::File))
            .map(|(s, a)| (s, a.unwrap()))
            .collect();
        let global: Vec<_> = found.iter().filter(|(s, _)| s.bind != Binding::Local).collect();
        match (found.len(), global.len()) {
            (0, _) => Err(format!("no defined symbol '{}'", name)),
            (1, _) => Ok(found[0].1),
            (_, 1) => Ok(global[0].1),
            (n, _) => Err(format!("symbol name '{}' is ambiguous ({} definitions)", name, n)),
        }
    }

    /// The loaded (relocated) code of the function called `name`.
    pub fn function(&self, name: &str) -> Result<Function<'_>> {
        let r = self.elf.function_range(name)?;
        let base = self.section_addrs[r.section].ok_or_else(|| format!("function '{}' is not in loaded memory", name))?;
        let addr = base + r.start;
        let code = self.read(addr, r.end - r.start).ok_or_else(|| format!("function '{}' is not in loaded memory", name))?;
        Ok(Function { name: name.to_string(), section: r.section, load_addr: Some(addr), code })
    }

    /// The image as initial memory for a test, with its functions (decoded
    /// to run as threads; see `Function::decode_thread`) and its data symbols
    /// (objects and common symbols) as named locations. A symbol's type is
    /// a signed integer of its size if that is 1, 2, 4 or 8 bytes, else the
    /// default (signed 64-bit). Names that are not identifiers, or that
    /// more than one symbol has, are left out.
    pub fn preload(&self) -> Result<Preload> {
        let regions = self
            .chunks
            .iter()
            .map(|c| Region { addr: c.addr, bytes: c.bytes.as_slice().into(), writable: c.writable, origin: c.origin.clone() })
            .collect();
        let mem = InitMem::from_regions(regions)?;
        let mut count: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &self.elf.symbols {
            *count.entry(&s.name).or_default() += 1;
        }
        let mut symbols: Vec<PreSym> = self
            .elf
            .symbols
            .iter()
            .zip(&self.symbol_addrs)
            .filter(|(s, a)| a.is_some() && matches!(s.kind, SymKind::Object | SymKind::Common))
            .filter(|(s, _)| count[s.name.as_str()] == 1 && is_ident(&s.name))
            .map(|(s, a)| {
                let ty = match Size::from_bytes(s.size) {
                    Some(size) if size <= Size::B8 => LocType { size, signed: true },
                    _ => DEFAULT_LOC_TYPE,
                };
                PreSym { name: s.name.clone(), addr: a.unwrap(), size: s.size, ty }
            })
            .collect();
        symbols.sort_by_key(|p| p.addr);
        let functions = self
            .elf
            .symbols
            .iter()
            .filter(|s| s.kind == SymKind::Func && s.section != SymSection::Undefined && is_ident(&s.name))
            .map(|s| (s.name.clone(), self.function(&s.name).and_then(|f| f.decode_thread())))
            .collect();
        Ok(Preload { mem, symbols, functions })
    }

    /// Decode the loaded code of the function called `name`.
    pub fn decode_function(&self, name: &str) -> Result<Vec<crate::isa::Instr>> {
        self.function(name)?.decode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insn(w: u32) -> Vec<u8> {
        w.to_le_bytes().to_vec()
    }
    fn word(b: &[u8]) -> u32 {
        u32::from_le_bytes(b[..4].try_into().unwrap())
    }

    #[test]
    fn instruction_fields() {
        // ADRP X0 at 0x1_0ffc to 0x1_3010: 3 pages up.
        let mut b = insn(0x9000_0000);
        apply(275, &mut b, 0, 0x1_3000, 0x10, 0x1_0ffc).unwrap();
        assert_eq!(word(&b), 0xf000_0000);
        // ADD X0,X0,#:lo12: of 0x1_3010.
        let mut b = insn(0x9100_0000);
        apply(277, &mut b, 0, 0x1_3010, 0, 0).unwrap();
        assert_eq!(word(&b), 0x9100_4000);
        // LDR X0,[X0,#:lo12:] of 0x1_3010: 0x10 / 8 = 2.
        let mut b = insn(0xf940_0000);
        apply(286, &mut b, 0, 0x1_3010, 0, 0).unwrap();
        assert_eq!(word(&b), 0xf940_0800);
        assert!(apply(286, &mut b, 0, 0x1_3014, 0, 0).unwrap_err().contains("multiple of 8"));
        // BL back by 8.
        let mut b = insn(0x9400_0000);
        apply(283, &mut b, 0, 0x1000, 0, 0x1008).unwrap();
        assert_eq!(word(&b), 0x97ff_fffe);
        assert!(apply(283, &mut b, 0, 0x1000_0000, 0, 0).unwrap_err().contains("out of range"));
        // MOVZ X0,#:abs_g1: of 0x1234_5678.
        let mut b = insn(0xd2a0_0000);
        apply(265, &mut b, 0, 0x1234_5678, 0, 0).unwrap();
        assert_eq!(word(&b), 0xd2a2_4680);
        assert!(apply(265, &mut b, 0, 1 << 32, 0, 0).is_err());
        // ABS32 accepts signed and unsigned 32-bit values only.
        let mut b = vec![0; 4];
        apply(258, &mut b, 0, 0xffff_ffff, 0, 0).unwrap();
        apply(258, &mut b, 0, 0, -0x8000_0000, 0).unwrap();
        assert!(apply(258, &mut b, 0, 0x1_0000_0000, 0, 0).is_err());
        assert!(apply(257, &mut b, 0, 0, 0, 0).unwrap_err().contains("outside"));
        assert!(apply(313, &mut b, 0, 0, 0, 0).unwrap_err().contains("not supported"));
    }

    #[test]
    fn placer_skips_taken_ranges() {
        let mut p = Placer { cursor: 0x1000, taken: vec![(0x1000, 0x1100), (0x1180, 0x1200)] };
        assert_eq!(p.place(0x10, 0x10).unwrap(), 0x1100);
        assert_eq!(p.place(0x80, 1).unwrap(), 0x1200);
        assert_eq!(p.place(0, 0x1000).unwrap(), 0x2000);
        let mut p = Placer { cursor: u64::MAX - 4, taken: vec![] };
        assert!(p.place(8, 1).is_err());
    }
}
