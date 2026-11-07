//! A map from bytes of memory to values, stored as ranges.
//!
//! A thread's coherence and forwarding state is per byte, but an access
//! sets the same value for every byte it covers. Kept byte by byte in a
//! `BTreeMap`, a state had dozens of entries to copy, hash and free each
//! time it was expanded; as ranges it has a few.

/// Bytes `lo..hi`, all mapped to `val`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Run<V> {
    lo: u64,
    hi: u64,
    val: V,
}

/// The runs are sorted, disjoint and non-empty, and two that touch have
/// different values. So each map has exactly one representation, and
/// derived equality and hashing compare maps by content.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ByteMap<V> {
    runs: Vec<Run<V>>,
}

impl<V> Default for ByteMap<V> {
    fn default() -> Self {
        ByteMap { runs: Vec::new() }
    }
}

impl<V: Copy + Eq> ByteMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, b: u64) -> Option<V> {
        let i = self.runs.partition_point(|r| r.hi <= b);
        self.runs.get(i).filter(|r| r.lo <= b).map(|r| r.val)
    }

    /// The runs overlapping `lo..hi`, as an index range.
    fn overlapping(&self, lo: u64, hi: u64) -> std::ops::Range<usize> {
        let start = self.runs.partition_point(|r| r.hi <= lo);
        if lo >= hi {
            return start..start;
        }
        start..self.runs.partition_point(|r| r.lo < hi)
    }

    /// The values of the mapped bytes in `lo..hi`.
    pub fn values(&self, lo: u64, hi: u64) -> impl Iterator<Item = V> + '_ {
        self.runs[self.overlapping(lo, hi)].iter().map(|r| r.val)
    }

    /// Map every byte `b` in `lo..hi` to `f(self.get(b))` (`None` unmaps it).
    pub fn update(&mut self, lo: u64, hi: u64, f: impl Fn(Option<V>) -> Option<V>) {
        if lo >= hi {
            return;
        }
        let mut range = self.overlapping(lo, hi);
        let mut new: Vec<Run<V>> = Vec::with_capacity(range.len() + 4);
        let mut push = |r: Run<V>| match new.last_mut() {
            Some(last) if last.hi == r.lo && last.val == r.val => last.hi = r.hi,
            _ => new.push(r),
        };
        // A run touching the range on either side may merge with it.
        if range.start > 0 && self.runs[range.start - 1].hi == lo {
            range.start -= 1;
            push(self.runs[range.start]);
        }
        let mut p = lo;
        for r in &self.runs[range.start..range.end] {
            if r.hi <= lo {
                continue; // the neighbor on the left, already pushed
            }
            if r.lo < lo {
                push(Run { lo: r.lo, hi: lo, val: r.val });
            }
            if p < r.lo {
                if let Some(val) = f(None) {
                    push(Run { lo: p, hi: r.lo, val });
                }
            }
            let (from, to) = (r.lo.max(lo), r.hi.min(hi));
            if let Some(val) = f(Some(r.val)) {
                push(Run { lo: from, hi: to, val });
            }
            if r.hi > hi {
                push(Run { lo: hi, hi: r.hi, val: r.val });
            }
            p = to;
        }
        if p < hi {
            if let Some(val) = f(None) {
                push(Run { lo: p, hi, val });
            }
        }
        if range.end < self.runs.len() && self.runs[range.end].lo == hi {
            push(self.runs[range.end]);
            range.end += 1;
        }
        self.runs.splice(range, new);
    }

    /// Map the value of every mapped byte `b` to `f(self.get(b))` (`None`
    /// unmaps it).
    pub fn map_values(&mut self, f: impl Fn(V) -> Option<V>) {
        let mut new: Vec<Run<V>> = Vec::with_capacity(self.runs.len());
        for r in &self.runs {
            let Some(val) = f(r.val) else { continue };
            match new.last_mut() {
                Some(last) if last.hi == r.lo && last.val == val => last.hi = r.hi,
                _ => new.push(Run { val, ..*r }),
            }
        }
        self.runs = new;
    }

    /// Map every byte in `lo..hi` to `val`.
    pub fn set(&mut self, lo: u64, hi: u64, val: V) {
        self.update(lo, hi, |_| Some(val));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Check `m` against the byte-by-byte model `model`, including the
    /// canonical form.
    fn check(m: &ByteMap<u8>, model: &BTreeMap<u64, u8>) {
        for w in m.runs.windows(2) {
            assert!(w[0].hi <= w[1].lo, "runs out of order: {:?}", m.runs);
            assert!(w[0].hi < w[1].lo || w[0].val != w[1].val, "runs not merged: {:?}", m.runs);
        }
        assert!(m.runs.iter().all(|r| r.lo < r.hi), "empty run: {:?}", m.runs);
        for b in 0..40 {
            assert_eq!(m.get(b), model.get(&b).copied(), "byte {b} of {:?}", m.runs);
        }
        for lo in 0..40 {
            for hi in lo..40 {
                let mut got: Vec<u8> = m.values(lo, hi).collect();
                let mut want: Vec<u8> = (lo..hi).filter_map(|b| model.get(&b).copied()).collect();
                got.sort();
                got.dedup();
                want.sort();
                want.dedup();
                assert_eq!(got, want, "values in {lo}..{hi} of {:?}", m.runs);
            }
        }
    }

    #[test]
    fn matches_byte_by_byte_map() {
        let mut rng: u64 = 1;
        let mut next = |n: u64| {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng >> 33) % n
        };
        for _ in 0..200 {
            let mut m = ByteMap::new();
            let mut model = BTreeMap::new();
            for _ in 0..30 {
                let lo = next(32);
                let hi = lo + next(9);
                let v = next(3) as u8;
                match next(4) {
                    3 => {
                        // Merge values 1 and 2, and unmap value `v`.
                        let f = |x: u8| (x != v).then_some(x.min(1));
                        m.map_values(f);
                        model = model.into_iter().filter_map(|(b, x)| f(x).map(|y| (b, y))).collect();
                    }
                    0 => {
                        m.set(lo, hi, v);
                        for b in lo..hi {
                            model.insert(b, v);
                        }
                    }
                    1 => {
                        m.update(lo, hi, |o| Some(o.unwrap_or(0).max(v)));
                        for b in lo..hi {
                            let o = model.get(&b).copied().unwrap_or(0);
                            model.insert(b, o.max(v));
                        }
                    }
                    _ => {
                        m.update(lo, hi, |o| o.filter(|&x| x != v));
                        for b in lo..hi {
                            if model.get(&b) == Some(&v) {
                                model.remove(&b);
                            }
                        }
                    }
                }
                check(&m, &model);
            }
            // Equal contents give equal maps, however they were built.
            let mut rebuilt = ByteMap::new();
            for (&b, &v) in &model {
                rebuilt.set(b, b + 1, v);
            }
            assert_eq!(rebuilt, m);
        }
    }
}
