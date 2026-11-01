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
