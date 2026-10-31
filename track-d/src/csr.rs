//! Forward-only CSR (compressed sparse row) adjacency. Only `u`'s
//! out-edges are needed: queries are answered with a single
//! unidirectional Dijkstra from `s`.
//!
//! `build_forward_and_in_edge` builds the CSR AND the `has_in_edge`
//! bitmap directly off the mmap'd edge byte ranges, in two parallel
//! passes, without ever storing the parsed `(u, v, w)` triples: phase
//! instrumentation on the previous (single-threaded, counting-sort
//! over pre-parsed triples) version found it costing ~260ms by
//! itself, more than either the parse phase or the (already well-
//! parallelized) query phase -- see PERF.md.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::parse::{parse_uint, skip_non_digits};

pub struct Csr {
    pub offsets: Vec<u32>,
    pub adj_node: Vec<u32>,
    pub adj_weight: Vec<u32>,
}

/// Builds the forward CSR and the `has_in_edge` bitmap directly off
/// the mmap'd edge byte ranges, in two parallel passes that never
/// store the parsed `(u, v, w)` triples at all.
///
/// Pass 1 (one thread per range): parse only `u` per line -- skip
/// straight to the next line without even parsing `v`/`w` -- and
/// atomically increment a shared per-node degree counter
/// (`Relaxed`: counting doesn't need ordering with anything else,
/// only the final total after every thread joins matters). A single
/// O(n) sequential prefix sum over those counts gives `offsets`.
///
/// Pass 2 (same ranges, re-parsed): parse the full triple, atomically
/// claim the next free slot within node `u`'s own `[offsets[u],
/// offsets[u+1])` range via a per-node atomic cursor (seeded from
/// `offsets`), and store `(v, w)` into exactly that slot. `adj_node`/
/// `adj_weight` are `Vec<AtomicU32>` during this phase so concurrent
/// stores from different threads are well-defined Rust, even though
/// the whole array is shared -- the cursor guarantees no two edges
/// (on any thread) ever claim the same slot, so no two threads ever
/// touch the same element; only DIFFERENT elements are ever written
/// concurrently, which plain atomic stores (not even a lock) make
/// safe. Converted to plain `Vec<u32>` once the parallel section ends
/// (`Atomic*::into_inner`, a safe O(m) pass, no unsafe anywhere in
/// this function).
pub fn build_forward_and_in_edge(
    n: usize,
    m: usize,
    data: &[u8],
    ranges: &[(usize, usize)],
) -> (Csr, Vec<bool>) {
    let degree: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    std::thread::scope(|scope| {
        for &(start, end) in ranges {
            let degree_ref = &degree;
            scope.spawn(move || {
                let mut i = start;
                while i < end {
                    i = skip_non_digits(data, i);
                    if i >= end {
                        break;
                    }
                    let (u, ni) = parse_uint(data, i);
                    degree_ref[u as usize].fetch_add(1, Ordering::Relaxed);
                    // `v`/`w` don't matter for counting -- skip straight
                    // to the next line without parsing them at all.
                    i = match data[ni.min(end)..end].iter().position(|&b| b == b'\n') {
                        Some(off) => ni + off + 1,
                        None => end,
                    };
                }
            });
        }
    });

    let mut offsets = vec![0u32; n + 1];
    for i in 0..n {
        offsets[i + 1] = offsets[i] + degree[i].load(Ordering::Relaxed);
    }
    drop(degree);

    let cursor: Vec<AtomicU32> = offsets[..n].iter().map(|&o| AtomicU32::new(o)).collect();
    let adj_node: Vec<AtomicU32> = (0..m).map(|_| AtomicU32::new(0)).collect();
    let adj_weight: Vec<AtomicU32> = (0..m).map(|_| AtomicU32::new(0)).collect();
    let has_in_edge: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();

    std::thread::scope(|scope| {
        for &(start, end) in ranges {
            let cursor_ref = &cursor;
            let adj_node_ref = &adj_node;
            let adj_weight_ref = &adj_weight;
            let has_in_edge_ref = &has_in_edge;
            scope.spawn(move || {
                let mut i = start;
                while i < end {
                    i = skip_non_digits(data, i);
                    if i >= end {
                        break;
                    }
                    let (u, ni) = parse_uint(data, i);
                    let i2 = skip_non_digits(data, ni);
                    let (v, ni2) = parse_uint(data, i2);
                    let i3 = skip_non_digits(data, ni2);
                    let (w, ni3) = parse_uint(data, i3);
                    i = ni3;

                    let pos = cursor_ref[u as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    adj_node_ref[pos].store(v, Ordering::Relaxed);
                    adj_weight_ref[pos].store(w, Ordering::Relaxed);
                    has_in_edge_ref[v as usize].store(true, Ordering::Relaxed);
                }
            });
        }
    });

    let adj_node: Vec<u32> = adj_node.into_iter().map(AtomicU32::into_inner).collect();
    let adj_weight: Vec<u32> = adj_weight.into_iter().map(AtomicU32::into_inner).collect();
    let has_in_edge: Vec<bool> = has_in_edge
        .into_iter()
        .map(AtomicBool::into_inner)
        .collect();

    (
        Csr {
            offsets,
            adj_node,
            adj_weight,
        },
        has_in_edge,
    )
}

impl Csr {
    #[inline]
    pub fn neighbors(&self, node: u32) -> (&[u32], &[u32]) {
        let begin = self.offsets[node as usize] as usize;
        let end = self.offsets[node as usize + 1] as usize;
        (&self.adj_node[begin..end], &self.adj_weight[begin..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders `edges` as `graph.txt`-style text (just the `u v w\n`
    /// lines, no header -- the header is parsed separately in
    /// `main.rs`) and builds via `build_forward_and_in_edge`, trying
    /// every thread count in `thread_counts` to exercise both the
    /// single-range (deterministic adjacency order) and multi-range
    /// (parallel, scheduling-dependent order) paths.
    fn build_with_thread_counts(
        n: usize,
        edges: &[(u32, u32, u32)],
        thread_counts: &[usize],
    ) -> Vec<(Csr, Vec<bool>)> {
        let mut text = Vec::new();
        for &(u, v, w) in edges {
            text.extend_from_slice(format!("{u} {v} {w}\n").as_bytes());
        }
        thread_counts
            .iter()
            .map(|&threads| {
                let ranges = crate::parse::line_aligned_ranges(&text, 0, threads);
                build_forward_and_in_edge(n, edges.len(), &text, &ranges)
            })
            .collect()
    }

    /// Sorted `(v, w)` pairs for one node -- the order-independent
    /// view every multi-thread-count assertion checks against, since
    /// concurrent construction only guarantees SET equality per node,
    /// not a specific insertion order (shortest-path correctness never
    /// depends on adjacency order within one node; see the module
    /// doc's multi-range tests below for the direct check that this
    /// doesn't affect hash-matched output).
    fn sorted_neighbors(csr: &Csr, node: u32) -> Vec<(u32, u32)> {
        let (neighbors, weights) = csr.neighbors(node);
        let mut pairs: Vec<(u32, u32)> = neighbors
            .iter()
            .copied()
            .zip(weights.iter().copied())
            .collect();
        pairs.sort_unstable();
        pairs
    }

    #[test]
    fn forward_csr_groups_by_u() {
        let edges = [(0, 1, 5), (0, 2, 7), (1, 2, 3)];
        for (csr, _) in build_with_thread_counts(3, &edges, &[1, 2, 3]) {
            assert_eq!(sorted_neighbors(&csr, 0), vec![(1, 5), (2, 7)]);
            assert_eq!(sorted_neighbors(&csr, 1), vec![(2, 3)]);
            assert_eq!(sorted_neighbors(&csr, 2), vec![]);
        }
    }

    #[test]
    fn handles_self_loops_and_duplicate_and_parallel_edges() {
        let edges = [(0, 1, 5), (0, 1, 9), (2, 2, 7), (0, 1, 5)];
        for (csr, _) in build_with_thread_counts(3, &edges, &[1, 2, 3, 4]) {
            assert_eq!(sorted_neighbors(&csr, 0), vec![(1, 5), (1, 5), (1, 9)]);
            assert_eq!(sorted_neighbors(&csr, 2), vec![(2, 7)]);
        }
    }

    #[test]
    fn multiple_ranges_merge_correctly() {
        let edges = [(0, 1, 5), (0, 2, 7), (1, 2, 3)];
        for (csr, _) in build_with_thread_counts(3, &edges, &[2, 3, 7, 12]) {
            assert_eq!(sorted_neighbors(&csr, 0), vec![(1, 5), (2, 7)]);
            assert_eq!(sorted_neighbors(&csr, 1), vec![(2, 3)]);
        }
    }

    #[test]
    fn has_in_edge_flags_only_nodes_with_an_incoming_edge() {
        // Node 0: two out-edges, no in-edge at all (the spec's "node with
        // no in-edges" structure). Node 3: a self-loop only -- still
        // counts as an in-edge of itself. Node 4: on no edge line at all.
        let edges = [(0, 1, 4), (0, 2, 9), (1, 2, 3), (2, 1, 6), (3, 3, 7)];
        for (_, seen) in build_with_thread_counts(5, &edges, &[1, 3, 5]) {
            assert!(
                !seen[0],
                "node 0 has two out-edges but nothing points at it"
            );
            assert!(seen[1]);
            assert!(seen[2]);
            assert!(seen[3], "the self-loop 3->3 is still an in-edge of 3");
            assert!(!seen[4], "node 4 is on no edge line at all");
        }
    }

    /// Repeated builds of the SAME input must always produce the same
    /// shortest-path answers, even though concurrent cursor claims can
    /// legitimately reorder a node's adjacency list differently run to
    /// run -- this is the property that actually matters for hash-
    /// matching (not any specific internal order).
    #[test]
    fn repeated_builds_agree_on_every_shortest_path_regardless_of_internal_order() {
        use std::cmp::Reverse;
        use std::collections::{BinaryHeap, HashMap};

        fn naive_dijkstra(csr: &Csr, s: u32, t: u32) -> Option<u64> {
            let mut dist = HashMap::new();
            let mut heap = BinaryHeap::new();
            dist.insert(s, 0u64);
            heap.push((Reverse(0u64), s));
            while let Some((Reverse(d), u)) = heap.pop() {
                if d > dist[&u] {
                    continue;
                }
                if u == t {
                    return Some(d);
                }
                let (neighbors, weights) = csr.neighbors(u);
                for (&v, &w) in neighbors.iter().zip(weights.iter()) {
                    let nd = d + w as u64;
                    if !dist.contains_key(&v) || nd < dist[&v] {
                        dist.insert(v, nd);
                        heap.push((Reverse(nd), v));
                    }
                }
            }
            None
        }

        let edges: Vec<(u32, u32, u32)> = (0..200)
            .map(|i| (i % 37, (i * 7 + 3) % 37, (i * 13 + 1) % 50 + 1))
            .collect();
        let builds = build_with_thread_counts(37, &edges, &[1, 2, 3, 5, 7, 12]);
        let (reference, _) = &builds[0];
        for (csr, _) in &builds[1..] {
            for s in 0..37u32 {
                for t in 0..37u32 {
                    assert_eq!(
                        naive_dijkstra(reference, s, t),
                        naive_dijkstra(csr, s, t),
                        "s={s} t={t}: distance changed across different thread counts"
                    );
                }
            }
        }
    }
}
