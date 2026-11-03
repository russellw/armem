//! A fast, deterministic hasher for the explorations' visited sets.
//!
//! Their keys are whole thread states, hashed as many small integers, for
//! which the standard library's SipHash (built to resist adversarial keys,
//! which these are not) spent about half the time of a large exploration.
//! This is the multiply-add hash of `rustc-hash` 2: each word is added to
//! the state, which is then multiplied by an odd constant. A multiplication
//! only carries upwards, so the low bits that index the table would depend
//! only on the low bits of the words; `finish` folds the high half into
//! the low half and multiplies once more, then rotates the best-mixed high
//! bits down.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const K: u64 = 0xf135_7aea_2e62_a9c5;

#[derive(Clone, Copy, Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = self.hash.wrapping_add(word).wrapping_mul(K);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rest.len()].copy_from_slice(rest);
            // Include the length, so that trailing zero bytes still count.
            self.add(u64::from_le_bytes(buf) ^ ((rest.len() as u64) << 59));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    #[inline]
    fn write_u128(&mut self, i: u128) {
        self.add(i as u64);
        self.add((i >> 64) as u64);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        (self.hash ^ (self.hash >> 32)).wrapping_mul(K).rotate_left(26)
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;
pub type FxHashSet<T> = HashSet<T, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn h<T: Hash>(x: T) -> u64 {
        FxBuildHasher::default().hash_one(x)
    }

    #[test]
    fn distinguishes_small_keys() {
        // Keys like those of the visited sets: short sequences of small
        // integers, and byte strings that differ only in trailing zeros.
        let mut seen = HashSet::new();
        for a in 0u64..64 {
            for b in 0u64..64 {
                assert!(seen.insert(h((a, b))), "collision at ({a}, {b})");
            }
        }
        assert_ne!(h([1u8, 0].as_slice()), h([1u8].as_slice()));
        assert_ne!(h(""), h("\0"));
    }

    #[test]
    fn spreads_into_low_bits() {
        // hashbrown indexes buckets by the low bits: keys differing only in
        // high bits must not all land in one bucket.
        let buckets: HashSet<u64> = (0u64..256).map(|i| h(i << 40) & 0xff).collect();
        assert!(buckets.len() > 128, "only {} of 256 buckets used", buckets.len());
    }
}
