//! Per-thread Dijkstra scratch space, reused across every query that
//! thread answers.
//!
//! The open-addressing table this replaces scatters accesses (a hash,
//! not an index) and needs ~2 probes/edge at its load factor -- real
//! cost on top of the cache misses BFS reordering (`src/reorder.rs`)
//! was already fighting. This is a PAGED array instead: node ids are
//! split into fixed-size pages (`PAGE_SIZE`), with a small per-thread
//! `page_table` of pointers (one per page, mostly `None`) and the
//! actual page memory drawn from a per-thread pool. `get`/`set` are
//! direct index arithmetic, no hashing, no probing. A page is
//! allocated (from the pool, or freshly if the pool is empty) the
//! first time any node in its range is touched; at the end of a
//! query, every page that query touched is zeroed and returned to
//! the pool, so the pool's size tracks the busiest SINGLE query's
//! distinct-page count, not the total touched across the whole run.
//! `u64::MAX` marks an unset slot -- safe as a sentinel since every
//! real distance fits in `(n - 1) * (2^32 - 1) < 2^63` (spec section
//! 3), far below it.

use crate::heap::QuaternaryHeap;

const PAGE_SIZE: usize = 1024;
const UNSET: u64 = u64::MAX;

type Page = Box<[u64; PAGE_SIZE]>;

pub struct Workspace {
    page_table: Vec<Option<Page>>,
    pool: Vec<Page>,
    touched_pages: Vec<usize>,
    pub heap: QuaternaryHeap<u32>,
}

impl Workspace {
    pub fn new(n: usize) -> Self {
        let num_pages = n.div_ceil(PAGE_SIZE);
        let mut page_table = Vec::with_capacity(num_pages);
        page_table.resize_with(num_pages, || None);
        Workspace {
            page_table,
            pool: Vec::new(),
            touched_pages: Vec::new(),
            heap: QuaternaryHeap::new(),
        }
    }

    /// Start a fresh query: every page touched by the PREVIOUS query is
    /// zeroed (to `UNSET`) and returned to the pool, so a later page
    /// allocation never sees stale data from a different node range.
    /// O(pages touched last query), never O(n) or O(num_pages).
    #[inline]
    pub fn reset(&mut self) {
        self.heap.clear();
        for &page_idx in &self.touched_pages {
            if let Some(mut page) = self.page_table[page_idx].take() {
                page.fill(UNSET);
                self.pool.push(page);
            }
        }
        self.touched_pages.clear();
    }

    #[inline]
    pub fn get(&self, node: u32) -> Option<u64> {
        let node = node as usize;
        let page_idx = node / PAGE_SIZE;
        let offset = node % PAGE_SIZE;
        match &self.page_table[page_idx] {
            Some(page) if page[offset] != UNSET => Some(page[offset]),
            _ => None,
        }
    }

    #[inline]
    pub fn set(&mut self, node: u32, value: u64) {
        let node = node as usize;
        let page_idx = node / PAGE_SIZE;
        let offset = node % PAGE_SIZE;
        if self.page_table[page_idx].is_none() {
            let page = self
                .pool
                .pop()
                .unwrap_or_else(|| Box::new([UNSET; PAGE_SIZE]));
            self.page_table[page_idx] = Some(page);
            self.touched_pages.push(page_idx);
        }
        self.page_table[page_idx].as_mut().unwrap()[offset] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_set_before_reset_is_not_visible_after() {
        let mut ws = Workspace::new(4096);
        ws.set(2, 99);
        assert_eq!(ws.get(2), Some(99));
        ws.reset();
        assert_eq!(
            ws.get(2),
            None,
            "stale value from the previous query must not leak through"
        );
        ws.set(1, 7);
        assert_eq!(ws.get(1), Some(7));
        assert_eq!(ws.get(2), None);
    }

    #[test]
    fn reset_drops_leftover_heap_entries_from_an_early_exit() {
        let mut ws = Workspace::new(4096);
        ws.set(0, 0);
        ws.heap.push(0, 0);
        ws.heap.push(5, 3); // never popped: the query returned early
        ws.reset();
        assert!(ws.heap.is_empty());
    }

    #[test]
    fn updating_an_existing_key_does_not_move_its_page() {
        let mut ws = Workspace::new(4096);
        ws.set(5, 100);
        ws.set(5, 42); // a relax: same node, better distance
        assert_eq!(ws.get(5), Some(42));
        assert_eq!(
            ws.touched_pages.len(),
            1,
            "no duplicate page allocation for the same node"
        );
    }

    #[test]
    fn a_reused_pooled_page_never_leaks_a_different_nodes_old_value() {
        // Force two different queries to claim the SAME page (same PAGE_SIZE
        // range) so the pool-reuse path is actually exercised.
        let mut ws = Workspace::new(4096);
        ws.set(10, 111); // page 0
        ws.reset(); // page 0 zeroed and pooled
        ws.set(20, 222); // same page 0, reused from the pool
        assert_eq!(ws.get(20), Some(222));
        assert_eq!(
            ws.get(10),
            None,
            "a value from a different query must not survive a page's reuse"
        );
    }

    #[test]
    fn works_with_node_ids_spread_across_many_pages() {
        let n = 1 << 20;
        let mut ws = Workspace::new(n);
        let ids: [u32; 5] = [
            0,
            (PAGE_SIZE as u32) - 1,
            PAGE_SIZE as u32,
            3 * PAGE_SIZE as u32 + 7,
            (n - 1) as u32,
        ];
        for (i, &id) in ids.iter().enumerate() {
            ws.set(id, i as u64);
        }
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(ws.get(id), Some(i as u64));
        }
    }
}
