//! A quaternary (4-ary) min-heap of `(key, value)` pairs, specialized
//! for Dijkstra's `(distance, node)` entries -- the mission's Phase 3
//! suggestion for a queries-dominated profile (see PERF.md). A 4-ary
//! heap halves the tree height of a binary heap for the same element
//! count, trading more same-level comparisons (up to 3 children
//! instead of 1) for fewer levels to sift through on `pop`, which is
//! the hot operation in Dijkstra (one pop per settled node, plus one
//! push per relaxed edge). No `Reverse` wrapper needed: this is a
//! plain min-heap on `key` directly.

pub struct QuaternaryHeap<T> {
    data: Vec<(u64, T)>,
}

impl<T: Copy> QuaternaryHeap<T> {
    pub fn new() -> Self {
        QuaternaryHeap { data: Vec::new() }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.data.clear();
    }

    #[cfg(test)]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[inline]
    pub fn push(&mut self, key: u64, value: T) {
        self.data.push((key, value));
        let mut i = self.data.len() - 1;
        while i > 0 {
            let parent = (i - 1) / 4;
            if self.data[i].0 < self.data[parent].0 {
                self.data.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
    }

    #[inline]
    pub fn pop(&mut self) -> Option<(u64, T)> {
        if self.data.is_empty() {
            return None;
        }
        let last = self.data.len() - 1;
        self.data.swap(0, last);
        let result = self.data.pop();
        let len = self.data.len();
        let mut i = 0;
        loop {
            let first_child = 4 * i + 1;
            if first_child >= len {
                break;
            }
            let last_child = (first_child + 4).min(len);
            let mut smallest = first_child;
            let mut smallest_key = self.data[first_child].0;
            for c in (first_child + 1)..last_child {
                if self.data[c].0 < smallest_key {
                    smallest = c;
                    smallest_key = self.data[c].0;
                }
            }
            if smallest_key < self.data[i].0 {
                self.data.swap(i, smallest);
                i = smallest;
            } else {
                break;
            }
        }
        result
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
