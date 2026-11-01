use std::hash::Hasher;

pub struct FnvHasher(u64);

impl Default for FnvHasher {
    #[inline]
    fn default() -> Self {
        FnvHasher(0xcbf29ce484222325)
    }
}

impl Hasher for FnvHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= byte as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type FnvBuildHasher = std::hash::BuildHasherDefault<FnvHasher>;
