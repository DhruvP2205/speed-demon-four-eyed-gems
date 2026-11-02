//! A radix min-heap of `(key, value)` pairs for Dijkstra's
//! monotonic nonnegative distance keys. Redistributing a bucket after
//! each new minimum avoids tree comparisons on the hot pop path.

pub struct QuaternaryHeap<T> {
    buckets: Vec<Vec<(u64, T)>>,
    last: u64,
    len: usize,
}

impl<T: Copy> QuaternaryHeap<T> {
    pub fn new() -> Self {
        QuaternaryHeap {
            buckets: (0..65).map(|_| Vec::new()).collect(),
            last: 0,
            len: 0,
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        self.last = 0;
        self.len = 0;
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn push(&mut self, key: u64, value: T) {
        debug_assert!(key >= self.last);
        let bucket = Self::bucket_index(self.last, key);
        self.buckets[bucket].push((key, value));
        self.len += 1;
    }

    #[inline]
    pub fn pop(&mut self) -> Option<(u64, T)> {
        if self.len == 0 {
            return None;
        }
        if self.buckets[0].is_empty() {
            let bucket_index = (1..65)
                .find(|&i| !self.buckets[i].is_empty())
                .expect("non-empty radix heap has a populated bucket");
            let new_last = self.buckets[bucket_index]
                .iter()
                .map(|&(key, _)| key)
                .min()
                .expect("bucket is non-empty");
            self.last = new_last;
            let bucket = std::mem::take(&mut self.buckets[bucket_index]);
            for entry in bucket {
                let i = Self::bucket_index(self.last, entry.0);
                self.buckets[i].push(entry);
            }
        }
        self.len -= 1;
        self.buckets[0].pop()
    }

    #[inline]
    fn bucket_index(last: u64, key: u64) -> usize {
        if key == last {
            0
        } else {
            (64 - (key ^ last).leading_zeros()) as usize
        }
    }
}
