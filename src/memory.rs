//! Initial memory contents.

use std::collections::BTreeMap;
use std::sync::Arc;

/// A contiguous block of initial memory, such as a loaded ELF section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub addr: u64,
    pub bytes: Arc<[u8]>,
    /// For information only: the memory models do not enforce it.
    pub writable: bool,
    /// What it was loaded from, for messages.
    pub origin: String,
}

impl Region {
    pub fn end(&self) -> u64 {
        self.addr + self.bytes.len() as u64
    }
}

/// Initial memory: regions (from a loaded image) and individual bytes
/// (from a test's init block). A byte set individually takes precedence
/// over a region. Every other byte is 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitMem {
    /// Sorted by address, non-overlapping, none empty.
    regions: Arc<Vec<Region>>,
    bytes: BTreeMap<u64, u8>,
}

impl InitMem {
    /// Memory holding `regions`, which must not overlap or wrap around the
    /// address space. Empty regions are dropped.
    pub fn from_regions(mut regions: Vec<Region>) -> Result<InitMem, String> {
        regions.retain(|r| !r.bytes.is_empty());
        regions.sort_by_key(|r| r.addr);
        for r in &regions {
            if r.addr.checked_add(r.bytes.len() as u64).is_none() {
                return Err(format!("{} at {:#x} wraps around the address space", r.origin, r.addr));
            }
        }
        for w in regions.windows(2) {
            if w[0].end() > w[1].addr {
                return Err(format!("{} at {:#x} overlaps {} at {:#x}", w[0].origin, w[0].addr, w[1].origin, w[1].addr));
            }
        }
        Ok(InitMem { regions: Arc::new(regions), bytes: BTreeMap::new() })
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// The region containing `addr`, if any.
    pub fn region(&self, addr: u64) -> Option<&Region> {
        let i = self.regions.partition_point(|r| r.addr <= addr).checked_sub(1)?;
        let r = &self.regions[i];
        (addr < r.end()).then_some(r)
    }

    /// The first region overlapping `[addr, addr + len)`, if any.
    pub fn overlapping(&self, addr: u64, len: u64) -> Option<&Region> {
        let end = addr.saturating_add(len);
        self.regions.iter().find(|r| r.addr < end && addr < r.end())
    }

    pub fn byte(&self, addr: u64) -> u8 {
        if let Some(&b) = self.bytes.get(&addr) {
            return b;
        }
        match self.region(addr) {
            Some(r) => r.bytes[(addr - r.addr) as usize],
            None => 0,
        }
    }

    pub fn set_byte(&mut self, addr: u64, v: u8) {
        self.bytes.insert(addr, v);
    }

    /// Store `size` bytes of `v`, little-endian, at `addr`.
    pub fn set(&mut self, addr: u64, v: u64, size: u64) {
        for i in 0..size {
            self.set_byte(addr.wrapping_add(i), (v >> (8 * i)) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(addr: u64, bytes: &[u8]) -> Region {
        Region { addr, bytes: bytes.into(), writable: true, origin: format!("r{:#x}", addr) }
    }

    #[test]
    fn bytes_override_regions() {
        let mut m = InitMem::from_regions(vec![region(0x2000, &[5, 6]), region(0x1000, &[1, 2, 3]), region(0x3000, &[])]).unwrap();
        assert_eq!(m.regions().len(), 2);
        assert_eq!([m.byte(0xfff), m.byte(0x1000), m.byte(0x1002), m.byte(0x1003), m.byte(0x2001)], [0, 1, 3, 0, 6]);
        m.set(0x1001, 0xabcd, 2);
        assert_eq!([m.byte(0x1000), m.byte(0x1001), m.byte(0x1002)], [1, 0xcd, 0xab]);
        assert_eq!(m.region(0x1002).unwrap().addr, 0x1000);
        assert!(m.region(0x1003).is_none());
        assert_eq!(m.overlapping(0xf00, 0x101).map(|r| r.addr), Some(0x1000));
        assert!(m.overlapping(0xf00, 0x100).is_none());
    }

    #[test]
    fn rejects_overlap() {
        assert!(InitMem::from_regions(vec![region(0x1000, &[1, 2]), region(0x1001, &[3])]).unwrap_err().contains("overlaps"));
        assert!(InitMem::from_regions(vec![region(u64::MAX, &[1, 2])]).unwrap_err().contains("wraps"));
    }
}
