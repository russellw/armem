//! Parser for herd-style AArch64 litmus tests.
//!
//! ```text
//! AArch64 MP
//! { 0:X1=x; 0:X3=y; 1:X1=y; 1:X3=x; }
//!  P0          | P1          ;
//!  MOV W0,#1   | LDR W0,[X1] ;
//!  STR W0,[X1] | LDR W2,[X3] ;
//!  MOV W2,#1   |             ;
//!  STR W2,[X3] |             ;
//! exists (1:X0=1 /\ 1:X2=0)
//! ```

use crate::asm::{self, Result};
use crate::isa::{Instr, Size};
use crate::memory::InitMem;
use std::collections::BTreeMap;
use std::fmt;

/// Base address of the first named location; each further one is `SYM_STRIDE` higher.
pub const SYM_BASE: u64 = 0x1000;
pub const SYM_STRIDE: u64 = 0x100;

/// Something observable in a final state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Obs {
    Reg(usize, u8),
    Mem(u64),
}

/// The type of a named location, which determines how its final value
/// is read: `size` bytes, little-endian, zero- or sign-extended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocType {
    pub size: Size,
    pub signed: bool,
}

pub const DEFAULT_LOC_TYPE: LocType = LocType { size: Size::B8, signed: true };

fn parse_type(words: &[&str]) -> Result<LocType> {
    let t = words.join(" ");
    let (size, signed) = match t.as_str() {
        "int8_t" | "char" | "signed char" => (Size::B1, true),
        "uint8_t" | "unsigned char" => (Size::B1, false),
        "int16_t" | "short" => (Size::B2, true),
        "uint16_t" | "unsigned short" => (Size::B2, false),
        "int" | "int32_t" => (Size::B4, true),
        "uint32_t" | "unsigned" | "unsigned int" => (Size::B4, false),
        "int64_t" | "long" | "intptr_t" => (Size::B8, true),
        "uint64_t" | "unsigned long" | "uintptr_t" => (Size::B8, false),
        _ if t.ends_with('*') => (Size::B8, false),
        _ => return Err(format!("unsupported type '{}'", t)),
    };
    Ok(LocType { size, signed })
}

#[derive(Clone, Debug)]
pub enum Prop {
    True,
    False,
    Eq(Obs, u64),
    Not(Box<Prop>),
    And(Box<Prop>, Box<Prop>),
    Or(Box<Prop>, Box<Prop>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quant {
    Exists,
    NotExists,
    Forall,
}

#[derive(Clone, Debug)]
pub struct Condition {
    pub quant: Quant,
    pub prop: Prop,
}

#[derive(Clone, Debug)]
pub struct Test {
    pub name: String,
    pub threads: Vec<Vec<Instr>>,
    /// Source lines of each thread's code (labels included); empty for a
    /// thread that runs an ELF function.
    pub code: Vec<Vec<String>>,
    /// The ELF function each thread runs (`P0 = name`), if any.
    pub functions: Vec<Option<String>>,
    pub init_regs: Vec<[u64; 31]>,
    /// Initial memory contents (absent bytes are 0).
    pub init_mem: InitMem,
    /// Declared types of named locations (default: signed 64-bit).
    pub loc_types: BTreeMap<u64, LocType>,
    pub symbols: Vec<(String, u64)>,
    /// Arrays: base address -> (element size in bytes, element count).
    pub arrays: BTreeMap<u64, (u64, u64)>,
    pub locations: Vec<Obs>,
    pub condition: Option<Condition>,
}

impl Prop {
    pub fn eval(&self, get: &dyn Fn(Obs) -> u64) -> bool {
        match self {
            Prop::True => true,
            Prop::False => false,
            Prop::Eq(o, v) => get(*o) == *v,
            Prop::Not(p) => !p.eval(get),
            Prop::And(a, b) => a.eval(get) && b.eval(get),
            Prop::Or(a, b) => a.eval(get) || b.eval(get),
        }
    }

    pub fn observables(&self, out: &mut Vec<Obs>) {
        match self {
            Prop::True | Prop::False => {}
            Prop::Eq(o, _) => {
                if !out.contains(o) {
                    out.push(*o)
                }
            }
            Prop::Not(p) => p.observables(out),
            Prop::And(a, b) | Prop::Or(a, b) => {
                a.observables(out);
                b.observables(out)
            }
        }
    }
}

impl Test {
    pub fn loc_type(&self, addr: u64) -> LocType {
        self.loc_types.get(&addr).copied().unwrap_or(DEFAULT_LOC_TYPE)
    }

    pub fn init_byte(&self, b: u64) -> u8 {
        self.init_mem.byte(b)
    }

    /// Value of the location at `addr` (read according to its type), given
    /// a function returning the final value of each byte.
    pub fn loc_value(&self, addr: u64, byte: &dyn Fn(u64) -> u8) -> u64 {
        let ty = self.loc_type(addr);
        let mut v = 0u64;
        for i in (0..ty.size.bytes()).rev() {
            v = (v << 8) | byte(addr + i) as u64;
        }
        ty.size.extend(v, ty.signed)
    }

    pub fn symbol_name(&self, addr: u64) -> Option<&str> {
        self.symbols.iter().find(|(_, a)| *a == addr).map(|(n, _)| n.as_str())
    }

    pub fn obs_name(&self, o: Obs) -> String {
        match o {
            Obs::Reg(t, r) => format!("{}:X{}", t, r),
            Obs::Mem(a) if self.arrays.contains_key(&a) => format!("{}[0]", self.symbol_name(a).unwrap()),
            Obs::Mem(a) => match self.symbol_name(a) {
                Some(n) => n.to_string(),
                None => match self.symbols.iter().filter(|(_, sa)| *sa <= a).max_by_key(|(_, sa)| *sa) {
                    Some((n, sa)) if a - sa < SYM_STRIDE => match self.arrays.get(sa) {
                        Some(&(elem, count)) if (a - sa) % elem == 0 && (a - sa) / elem < count => {
                            format!("{}[{}]", n, (a - sa) / elem)
                        }
                        _ => format!("[{}+{}]", n, a - sa),
                    },
                    _ => format!("[{:#x}]", a),
                },
            },
        }
    }

    pub fn value_name(&self, v: u64) -> String {
        match self.symbol_name(v) {
            Some(n) => n.to_string(),
            None => format!("{}", v as i64),
        }
    }

    /// The observables reported in final states: the `locations` clause
    /// plus everything the condition mentions (or, without either, all
    /// named memory locations).
    pub fn observables(&self) -> Vec<Obs> {
        let mut out = self.locations.clone();
        if let Some(c) = &self.condition {
            c.prop.observables(&mut out);
        }
        if out.is_empty() {
            out = self.symbols.iter().map(|(_, a)| Obs::Mem(*a)).collect();
        }
        out
    }
}

impl fmt::Display for Quant {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Quant::Exists => "exists",
            Quant::NotExists => "~exists",
            Quant::Forall => "forall",
        })
    }
}

/// A named location that exists before the test is parsed, such as a
/// global variable of a loaded ELF image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreSym {
    pub name: String,
    pub addr: u64,
    /// Size in bytes (0 if unknown).
    pub size: u64,
    pub ty: LocType,
}

/// Initial memory and named locations to parse a test against
/// (see `parse_with`).
#[derive(Clone, Debug, Default)]
pub struct Preload {
    pub mem: InitMem,
    pub symbols: Vec<PreSym>,
    /// Functions a thread can run (`P0 = name`): the decoded code, or why
    /// it cannot be used.
    pub functions: BTreeMap<String, Result<Vec<Instr>>>,
}

struct Symbols {
    syms: Vec<(String, u64)>,
    /// Arrays: base address -> (element size in bytes, element count).
    arrays: BTreeMap<u64, (u64, u64)>,
    /// Locations the test names that are not preloaded get addresses
    /// `SYM_BASE + SYM_STRIDE * k`, for k = 0, 1, ...
    allocated: u64,
    preloaded: BTreeMap<String, usize>,
}

impl Symbols {
    fn get(&mut self, name: &str) -> u64 {
        if let Some((_, a)) = self.syms.iter().find(|(n, _)| n == name) {
            return *a;
        }
        let a = SYM_BASE + SYM_STRIDE * self.allocated;
        self.allocated += 1;
        self.syms.push((name.to_string(), a));
        a
    }

    fn value(&mut self, s: &str) -> Result<u64> {
        let s = s.trim();
        if s.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
            Ok(asm::parse_int(s)? as u64)
        } else if let Some((sym, off)) = s.split_once('+') {
            // symbol+offset
            let base = self.value(sym)?;
            Ok(base.wrapping_add(asm::parse_int(off)? as u64))
        } else if is_ident(s) {
            Ok(self.get(s))
        } else {
            Err(format!("bad value '{}'", s))
        }
    }
}

pub fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().unwrap().is_ascii_alphabetic()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Parse `N:Xr` / `N:Wr`.
fn parse_thread_reg(s: &str) -> Option<(usize, u8)> {
    let (t, r) = s.trim().split_once(':')?;
    let t: usize = t.trim().parse().ok()?;
    let r = asm::parse_reg(r).ok()?;
    if r.idx > 30 {
        return None;
    }
    Some((t, r.idx))
}

fn strip_comments(src: &str) -> String {
    // Remove (* ... *) comments and // line comments.
    let mut out = String::new();
    let mut rest = src;
    while let Some(i) = rest.find("(*") {
        out.push_str(&rest[..i]);
        match rest[i..].find("*)") {
            Some(j) => rest = &rest[i + j + 2..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse(src: &str) -> Result<Test> {
    parse_with(src, &Preload::default())
}

/// Parse a test whose initial memory starts as `pre.mem`, and in which the
/// names of `pre.symbols` refer to those locations. In the init block,
/// `name=value` overrides a preloaded location's initial value, and a
/// declaration without a value (`uint8_t name`) only sets its type. Other
/// names get fresh locations, which must not overlap preloaded memory.
pub fn parse_with(src: &str, pre: &Preload) -> Result<Test> {
    let src = strip_comments(src);
    let mut lines = src.lines().peekable();

    // Header: "AArch64 NAME"
    let header = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l.trim().to_string(),
            None => return Err("empty test".into()),
        }
    };
    let mut hw = header.split_whitespace();
    let arch = hw.next().unwrap_or("");
    if !arch.eq_ignore_ascii_case("AArch64") {
        return Err(format!("unsupported architecture '{}' (expected AArch64)", arch));
    }
    let name = hw.next().unwrap_or("unnamed").to_string();

    // Skip metadata until the init block.
    let mut init_text = String::new();
    let mut found = false;
    for l in lines.by_ref() {
        if let Some(i) = l.find('{') {
            let after = &l[i + 1..];
            if let Some(j) = after.find('}') {
                init_text.push_str(&after[..j]);
                found = true;
                break;
            }
            init_text.push_str(after);
            init_text.push('\n');
            for l2 in lines.by_ref() {
                if let Some(j) = l2.find('}') {
                    init_text.push_str(&l2[..j]);
                    found = true;
                    break;
                }
                init_text.push_str(l2);
                init_text.push('\n');
            }
            break;
        }
    }
    if !found {
        return Err("missing init block { ... }".into());
    }

    let mut syms = Symbols { syms: Vec::new(), arrays: BTreeMap::new(), allocated: 0, preloaded: BTreeMap::new() };
    let mut reg_inits: Vec<(usize, u8, u64)> = Vec::new();
    let mut init_mem = pre.mem.clone();
    let mut loc_types = BTreeMap::new();
    for (i, p) in pre.symbols.iter().enumerate() {
        if !is_ident(&p.name) || syms.preloaded.insert(p.name.clone(), i).is_some() {
            return Err(format!("bad or duplicate preloaded symbol name '{}'", p.name));
        }
        syms.syms.push((p.name.clone(), p.addr));
        loc_types.insert(p.addr, p.ty);
    }
    for stmt in init_text.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        let (lhs, rhs) = match stmt.split_once('=') {
            Some((l, r)) => (l.trim(), Some(r.trim())),
            None => (stmt, None),
        };
        // An optional C-like type precedes the name: "uint8_t x", "int *p".
        // herd also accepts spaces in register names: "0: X1".
        let lhs = lhs.replace('*', " * ").replace(": ", ":").replace(" :", ":");
        let words: Vec<&str> = lhs.split_whitespace().collect();
        let Some((&name, ty_words)) = words.split_last() else {
            return Err(format!("bad init statement '{}'", stmt));
        };
        if let Some((t, r)) = parse_thread_reg(name) {
            let v = match rhs {
                Some(r) => syms.value(r)?,
                None => 0,
            };
            reg_inits.push((t, r, v));
        } else {
            // "x", "[x]", or an array declaration "x[N]".
            let (lname, count) = match name.strip_suffix(']').and_then(|n| n.split_once('[')) {
                Some((n, c)) if !n.is_empty() => {
                    let c: u64 = c.trim().parse().map_err(|_| format!("bad array size in '{}'", stmt))?;
                    (n, Some(c))
                }
                _ => (name.trim_start_matches('[').trim_end_matches(']'), None),
            };
            if !is_ident(lname) {
                return Err(format!("bad init statement '{}'", stmt));
            }
            let a = syms.get(lname);
            let ty = if ty_words.is_empty() {
                loc_types.get(&a).copied().unwrap_or(DEFAULT_LOC_TYPE)
            } else {
                let mut tw: Vec<&str> = ty_words.to_vec();
                // "int *" -> "int*"
                if tw.last() == Some(&"*") {
                    tw.pop();
                    let joined = format!("{}*", tw.join(" "));
                    parse_type(&[joined.as_str()])?
                } else {
                    parse_type(&tw)?
                }
            };
            let pre_sym = syms.preloaded.get(lname).map(|&i| &pre.symbols[i]);
            if let Some(count) = count {
                let elem = ty.size.bytes();
                let room = match pre_sym {
                    Some(p) if p.size > 0 => p.size,
                    Some(_) => u64::MAX,
                    None => SYM_STRIDE,
                };
                if count == 0 || elem.saturating_mul(count) > room {
                    return Err(format!("array '{}' must have 1..={} bytes", lname, room));
                }
                if rhs.is_some() {
                    return Err(format!("array '{}' cannot be initialized", lname));
                }
                for i in 0..count {
                    loc_types.insert(a + i * elem, ty);
                }
                syms.arrays.insert(a, (elem, count));
                continue;
            }
            loc_types.insert(a, ty);
            let v = match rhs {
                Some(r) => syms.value(r)?,
                None if pre_sym.is_some() => continue, // keep the preloaded value
                None => 0,
            };
            init_mem.set(a, v, ty.size.bytes());
        }
    }

    // Thread header "P0 | P1 ;"
    let thead = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l.to_string(),
            None => return Err("missing thread header".into()),
        }
    };
    // Each column is `Pn`, or `Pn = function` for a thread that runs a
    // function of the preloaded ELF file.
    let cols: Vec<String> = thead.trim().trim_end_matches(';').split('|').map(|c| c.trim().to_string()).collect();
    let mut functions = Vec::new();
    for (i, c) in cols.iter().enumerate() {
        let (p, f) = match c.split_once('=') {
            Some((p, f)) => (p.trim(), Some(f.trim())),
            None => (c.as_str(), None),
        };
        if !p.eq_ignore_ascii_case(&format!("P{}", i)) || f.is_some_and(|f| !is_ident(f)) {
            return Err(format!("bad thread header '{}'", thead.trim()));
        }
        functions.push(f.map(String::from));
    }
    let n = cols.len();
    let mut code: Vec<Vec<String>> = vec![Vec::new(); n];

    // Code lines until the final section.
    let mut rest = String::new();
    while let Some(l) = lines.next() {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if ["exists", "~exists", "forall", "locations", "filter", "~ exists"]
            .iter()
            .any(|k| lower.starts_with(k))
        {
            rest.push_str(l);
            rest.push('\n');
            for l2 in lines.by_ref() {
                rest.push_str(l2);
                rest.push('\n');
            }
            break;
        }
        let t = t.strip_suffix(';').unwrap_or(t);
        let cells: Vec<&str> = t.split('|').collect();
        if cells.len() > n {
            return Err(format!("too many columns in line '{}'", l.trim()));
        }
        for (i, c) in cells.iter().enumerate() {
            if !c.trim().is_empty() {
                code[i].push(c.trim().to_string());
            }
        }
    }

    let mut threads = Vec::new();
    for (i, (c, f)) in code.iter().zip(&functions).enumerate() {
        threads.push(match f {
            None => asm::assemble(c).map_err(|e| format!("P{}: {}", i, e))?,
            Some(f) if !c.is_empty() => {
                return Err(format!("P{} runs function '{}', so its code column must be empty", i, f));
            }
            Some(f) => match pre.functions.get(f) {
                None if pre.functions.is_empty() => {
                    return Err(format!("P{}: function '{}' needs an ELF file (--elf)", i, f));
                }
                None => return Err(format!("P{}: no function '{}' in the ELF file", i, f)),
                Some(Err(e)) => return Err(format!("P{}: {}", i, e)),
                Some(Ok(prog)) => prog.clone(),
            },
        });
    }
    let mut init_regs = vec![[0u64; 31]; n];
    for (t, r, v) in reg_inits {
        if t >= n {
            return Err(format!("init refers to nonexistent thread {}", t));
        }
        init_regs[t][r as usize] = v;
    }

    let (locations, mut condition) = parse_final(&rest, &mut syms)?;
    for (n, a) in &syms.syms {
        if syms.preloaded.contains_key(n) {
            continue;
        }
        if let Some(r) = init_mem.overlapping(*a, SYM_STRIDE) {
            return Err(format!("location '{}' at {:#x} would overlap preloaded {} at {:#x}", n, a, r.origin, r.addr));
        }
    }
    if let Some(c) = &mut condition {
        normalize(&mut c.prop, &loc_types);
    }
    let mut mentioned = locations.clone();
    if let Some(c) = &condition {
        c.prop.observables(&mut mentioned);
    }
    for o in mentioned {
        if let Obs::Reg(t, _) = o {
            if t >= n {
                return Err(format!("final condition refers to nonexistent thread {}", t));
            }
        }
    }
    Ok(Test {
        name,
        threads,
        code,
        functions,
        init_regs,
        init_mem,
        loc_types,
        symbols: syms.syms,
        arrays: syms.arrays,
        locations,
        condition,
    })
}

/// Bring constants compared with memory locations into the location's
/// representation (so that `x=-1` and `x=255` both match an 0xff byte).
fn normalize(p: &mut Prop, types: &BTreeMap<u64, LocType>) {
    match p {
        Prop::Eq(Obs::Mem(a), v) => {
            let ty = types.get(a).copied().unwrap_or(DEFAULT_LOC_TYPE);
            *v = ty.size.extend(*v, ty.signed);
        }
        Prop::Not(q) => normalize(q, types),
        Prop::And(a, b) | Prop::Or(a, b) => {
            normalize(a, types);
            normalize(b, types)
        }
        _ => {}
    }
}

fn tokenize(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && cs.get(i + 1) == Some(&'\\') {
            toks.push("/\\".into());
            i += 2;
        } else if c == '\\' && cs.get(i + 1) == Some(&'/') {
            toks.push("\\/".into());
            i += 2;
        } else if c == '=' && cs.get(i + 1) == Some(&'=') {
            toks.push("=".into());
            i += 2;
        } else if "()[]~=;".contains(c) {
            toks.push(c.to_string());
            i += 1;
        } else {
            let st = i;
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || "_:.-+".contains(cs[i])) {
                i += 1;
                // herd allows "0: X1"; join it into one token.
                if cs[i - 1] == ':' {
                    while i < cs.len() && cs[i].is_whitespace() {
                        i += 1;
                    }
                }
            }
            if i == st {
                toks.push(c.to_string());
                i += 1;
            } else {
                toks.push(cs[st..i].iter().filter(|c| !c.is_whitespace()).collect());
            }
        }
    }
    toks
}

struct CondParser<'a> {
    toks: Vec<String>,
    pos: usize,
    syms: &'a mut Symbols,
}

impl CondParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(|s| s.as_str())
    }
    fn next(&mut self) -> Result<String> {
        let t = self.toks.get(self.pos).cloned().ok_or("unexpected end of condition")?;
        self.pos += 1;
        Ok(t)
    }
    fn expect(&mut self, t: &str) -> Result<()> {
        let n = self.next()?;
        if n != t { Err(format!("expected '{}', got '{}'", t, n)) } else { Ok(()) }
    }

    fn obs(&mut self) -> Result<Obs> {
        let t = self.next()?;
        if t == "[" {
            // [sym] or [sym+offset]
            let n = self.next()?;
            self.expect("]")?;
            return Ok(Obs::Mem(self.syms.value(&n)?));
        }
        if let Some((th, r)) = parse_thread_reg(&t) {
            return Ok(Obs::Reg(th, r));
        }
        if is_ident(&t) {
            let a = self.syms.get(&t);
            if self.peek() == Some("[") {
                // Array element x[i].
                self.pos += 1;
                let i = asm::parse_int(&self.next()?)? as u64;
                self.expect("]")?;
                let Some(&(elem, count)) = self.syms.arrays.get(&a) else {
                    return Err(format!("'{}' is not an array", t));
                };
                if i >= count {
                    return Err(format!("index {} out of bounds for '{}'", i, t));
                }
                return Ok(Obs::Mem(a + i * elem));
            }
            return Ok(Obs::Mem(a));
        }
        Err(format!("bad observable '{}'", t))
    }

    fn or(&mut self) -> Result<Prop> {
        let mut p = self.and()?;
        while self.peek() == Some("\\/") {
            self.pos += 1;
            p = Prop::Or(Box::new(p), Box::new(self.and()?));
        }
        Ok(p)
    }
    fn and(&mut self) -> Result<Prop> {
        let mut p = self.unary()?;
        while self.peek() == Some("/\\") {
            self.pos += 1;
            p = Prop::And(Box::new(p), Box::new(self.unary()?));
        }
        Ok(p)
    }
    fn unary(&mut self) -> Result<Prop> {
        match self.peek() {
            Some("~") | Some("not") => {
                self.pos += 1;
                Ok(Prop::Not(Box::new(self.unary()?)))
            }
            Some("(") => {
                self.pos += 1;
                let p = self.or()?;
                self.expect(")")?;
                Ok(p)
            }
            Some("true") => {
                self.pos += 1;
                Ok(Prop::True)
            }
            Some("false") => {
                self.pos += 1;
                Ok(Prop::False)
            }
            _ => {
                let o = self.obs()?;
                self.expect("=")?;
                let v = self.next()?;
                Ok(Prop::Eq(o, self.syms.value(&v)?))
            }
        }
    }
}

fn parse_final(s: &str, syms: &mut Symbols) -> Result<(Vec<Obs>, Option<Condition>)> {
    let mut p = CondParser { toks: tokenize(s), pos: 0, syms };
    let mut locations = Vec::new();
    let mut condition = None;
    while let Some(t) = p.peek().map(|s| s.to_string()) {
        match t.as_str() {
            "locations" => {
                p.pos += 1;
                p.expect("[")?;
                while p.peek() != Some("]") {
                    if p.peek() == Some(";") {
                        p.pos += 1;
                        continue;
                    }
                    let o = p.obs()?;
                    locations.push(o);
                }
                p.pos += 1;
            }
            "exists" | "forall" | "~" => {
                let quant = if t == "~" {
                    p.pos += 1;
                    p.expect("exists")?;
                    Quant::NotExists
                } else {
                    p.pos += 1;
                    if t == "exists" { Quant::Exists } else { Quant::Forall }
                };
                let prop = p.or()?;
                condition = Some(Condition { quant, prop });
            }
            ";" => p.pos += 1,
            "filter" => return Err("filter clauses are not supported".into()),
            _ => return Err(format!("unexpected '{}' in final section", t)),
        }
    }
    Ok((locations, condition))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herd_spacing_forms() {
        // Spaces inside "0: X1" and a ';' after the condition, as herd allows.
        let t = parse(
            "AArch64 T\n{ x = 0; 0: X1 = x; 1:X1=x; }\n P0 | P1 ;\n MOV W0,#1 | LDR W0,[X1] ;\n STR W0,[X1] | ;\nexists (1: X0 = 1);\n",
        )
        .unwrap();
        assert_eq!(t.init_regs[0][1], t.init_regs[1][1]);
        assert_eq!(t.observables(), vec![Obs::Reg(1, 0)]);
    }
}
