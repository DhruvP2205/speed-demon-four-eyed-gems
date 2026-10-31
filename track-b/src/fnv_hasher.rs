//! FNV-1a, for the term-lookup `HashMap` in `index.rs`'s hot loop
//! (looked up once per token -- 36M times at `full` scale). Std's
//! default `HashMap` hasher (SipHash) is DoS-resistant, a property
//! this lookup table never needs (it's built once, from the query
//! file's own terms, not from untrusted network input); FNV-1a is
//! visibly cheaper per short byte slice and is exactly as sound for
//! equality-based lookup correctness, since hash collisions are
//! still resolved by `HashMap`'s own full-key comparison, never
//! trusted alone.

use std::hash::Hasher;

pub struct FnvHasher(u64);

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

impl Default for FnvHasher {
    #[inline]
    fn default() -> Self {
        FnvHasher(FNV_OFFSET_BASIS)
    }
}

impl Hasher for FnvHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        self.0 = hash;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type FnvBuildHasher = std::hash::BuildHasherDefault<FnvHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{BuildHasher, Hash};

    fn hash_of<T: Hash>(value: &T) -> u64 {
        let build: FnvBuildHasher = Default::default();
        let mut hasher = build.build_hasher();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn equal_byte_slices_hash_equal() {
        assert_eq!(hash_of(&b"bail".as_slice()), hash_of(&b"bail".as_slice()));
    }

    #[test]
    fn different_byte_slices_almost_always_hash_different() {
        assert_ne!(hash_of(&b"bail".as_slice()), hash_of(&b"bond".as_slice()));
        assert_ne!(hash_of(&b"".as_slice()), hash_of(&b"a".as_slice()));
    }

    #[test]
    fn works_as_a_hashmap_hasher() {
        use std::collections::HashMap;
        let mut map: HashMap<&[u8], u32, FnvBuildHasher> = HashMap::default();
        map.insert(b"bail", 0);
        map.insert(b"bond", 1);
        assert_eq!(map.get(b"bail".as_slice()), Some(&0));
        assert_eq!(map.get(b"bond".as_slice()), Some(&1));
        assert_eq!(map.get(b"nope".as_slice()), None);
    }
}
