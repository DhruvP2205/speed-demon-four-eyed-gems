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
    #[inline]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_in_ascending_key_order() {
        let mut h: QuaternaryHeap<u32> = QuaternaryHeap::new();
        for (k, v) in [(5u64, 0u32), (1, 1), (3, 2), (9, 3), (2, 4), (7, 5)] {
            h.push(k, v);
        }
        let mut out = Vec::new();
        while let Some((k, v)) = h.pop() {
            out.push((k, v));
        }
        assert_eq!(out, vec![(1, 1), (2, 4), (3, 2), (5, 0), (7, 5), (9, 3)]);
    }

    #[test]
    fn handles_duplicate_keys() {
        let mut h: QuaternaryHeap<u32> = QuaternaryHeap::new();
        h.push(3, 0);
        h.push(3, 1);
        h.push(1, 2);
        let mut out = Vec::new();
        while let Some((k, _)) = h.pop() {
            out.push(k);
        }
        assert_eq!(out, vec![1, 3, 3]);
    }

    #[test]
    fn empty_heap_pops_none() {
        let mut h: QuaternaryHeap<u32> = QuaternaryHeap::new();
        assert_eq!(h.pop(), None);
        assert!(h.is_empty());
    }

    #[test]
    fn clear_empties_the_heap() {
        let mut h: QuaternaryHeap<u32> = QuaternaryHeap::new();
        h.push(1, 0);
        h.push(2, 1);
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.pop(), None);
    }

    #[test]
    fn interleaved_push_and_pop_stays_ordered() {
        let mut h: QuaternaryHeap<u32> = QuaternaryHeap::new();
        h.push(10, 0);
        h.push(5, 1);
        assert_eq!(h.pop(), Some((5, 1)));
        h.push(3, 2);
        h.push(20, 3);
        assert_eq!(h.pop(), Some((3, 2)));
        assert_eq!(h.pop(), Some((10, 0)));
        assert_eq!(h.pop(), Some((20, 3)));
        assert_eq!(h.pop(), None);
    }

    #[test]
    fn stress_matches_a_sorted_reference_on_random_input() {
        let mut keys: Vec<u64> = (0..2000).map(|i| (i * 2654435761u64) % 100_000).collect();
        let mut h: QuaternaryHeap<u32> = QuaternaryHeap::new();
        for (i, &k) in keys.iter().enumerate() {
            h.push(k, i as u32);
        }
        keys.sort_unstable();
        let mut out = Vec::new();
        while let Some((k, _)) = h.pop() {
            out.push(k);
        }
        assert_eq!(out, keys);
    }
}
