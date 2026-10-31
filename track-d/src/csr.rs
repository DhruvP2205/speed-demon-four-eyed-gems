//! Forward-only CSR (compressed sparse row) adjacency, built via
//! counting sort. Only `u`'s out-edges are needed: queries are
//! answered with a single unidirectional Dijkstra from `s`.

pub struct Csr {
    pub offsets: Vec<u32>,
    pub adj_node: Vec<u32>,
    pub adj_weight: Vec<u32>,
}

impl Csr {
    #[inline]
    pub fn neighbors(&self, node: u32) -> (&[u32], &[u32]) {
        let begin = self.offsets[node as usize] as usize;
        let end = self.offsets[node as usize + 1] as usize;
        (&self.adj_node[begin..end], &self.adj_weight[begin..end])
    }
}

/// Build the forward CSR (grouped by `u`): `adj_node`/`adj_weight` at
/// `offsets[u]..offsets[u+1]` give node `u`'s out-edges `(v, w)`.
///
/// `chunks` (one `(us, vs, ws)` triple per parsing thread) is taken by
/// value and consumed in the fill loop below, not borrowed: `for
/// (us, vs, ws) in chunks` drops each chunk's three `Vec`s as soon as
/// its edges are copied into `adj_node`/`adj_weight`, instead of
/// holding every chunk (the whole parsed edge list, ~60 MB at `full`)
/// alive for the entire call alongside the CSR it's being copied
/// into. `local_counts` (~84 MB at `full`: one `n`-sized array per
/// chunk) is dropped the moment `offsets` is built from it, rather
/// than lingering unused to the end of the function.
pub fn build_forward(n: usize, m: usize, chunks: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)>) -> Csr {
    let local_counts: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|(us, _, _)| {
                scope.spawn(move || {
                    let mut counts = vec![0u32; n + 1];
                    for &u in us {
                        counts[u as usize + 1] += 1;
                    }
                    counts
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("degree-count worker panicked"))
            .collect()
    });

    let mut offsets = vec![0u32; n + 1];
    for counts in &local_counts {
        for i in 0..=n {
            offsets[i] += counts[i];
        }
    }
    for i in 0..n {
        offsets[i + 1] += offsets[i];
    }
    drop(local_counts);

    let mut cursor = offsets.clone();
    let mut adj_node = vec![0u32; m];
    let mut adj_weight = vec![0u32; m];
    for (us, vs, ws) in chunks {
        for i in 0..us.len() {
            let (u, v, w) = (us[i], vs[i], ws[i]);
            let pos = cursor[u as usize] as usize;
            adj_node[pos] = v;
            adj_weight[pos] = w;
            cursor[u as usize] += 1;
        }
    }

    Csr {
        offsets,
        adj_node,
        adj_weight,
    }
}

/// `has_in_edge[v]` is true iff some edge `u -> v` exists. A single
/// linear scan over the already-in-memory parsed triples (no new I/O,
/// no reverse CSR) -- cheap enough at `m` = 5,000,000 that threading
/// it would only add complexity for no measurable gain.
///
/// A node with no in-edges can never be reached from any other node
/// (the last edge of any path into it would be one of its in-edges),
/// so `d(s, t) = -1` for every `s != t` once this is false -- an O(1)
/// answer for exactly the "no-in-target" queries the reference's own
/// notes flag as its worst case (settling the whole main body, about
/// 3 s each in Python).
pub fn has_in_edge_bitmap(n: usize, chunks: &[(Vec<u32>, Vec<u32>, Vec<u32>)]) -> Vec<bool> {
    let mut seen = vec![false; n];
    for (_, vs, _) in chunks {
        for &v in vs {
            seen[v as usize] = true;
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(edges: &[(u32, u32, u32)]) -> Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> {
        let us = edges.iter().map(|&(u, _, _)| u).collect();
        let vs = edges.iter().map(|&(_, v, _)| v).collect();
        let ws = edges.iter().map(|&(_, _, w)| w).collect();
        vec![(us, vs, ws)]
    }

    #[test]
    fn forward_csr_groups_by_u() {
        let edges = [(0, 1, 5), (0, 2, 7), (1, 2, 3)];
        let csr = build_forward(3, 3, chunk(&edges));
        let (neighbors, weights) = csr.neighbors(0);
        assert_eq!(neighbors, &[1, 2]);
        assert_eq!(weights, &[5, 7]);
        let (neighbors, weights) = csr.neighbors(1);
        assert_eq!(neighbors, &[2]);
        assert_eq!(weights, &[3]);
        let (neighbors, _) = csr.neighbors(2);
        assert!(neighbors.is_empty());
    }

    #[test]
    fn handles_self_loops_and_duplicate_and_parallel_edges() {
        let edges = [(0, 1, 5), (0, 1, 9), (2, 2, 7), (0, 1, 5)];
        let fwd = build_forward(3, 4, chunk(&edges));
        let (neighbors, weights) = fwd.neighbors(0);
        assert_eq!(neighbors, &[1, 1, 1]);
        assert_eq!(weights, &[5, 9, 5]);
        let (neighbors, weights) = fwd.neighbors(2);
        assert_eq!(neighbors, &[2]);
        assert_eq!(weights, &[7]);
    }

    #[test]
    fn multiple_chunks_merge_correctly() {
        let chunks = vec![
            (vec![0u32], vec![1u32], vec![5u32]),
            (vec![0u32, 1u32], vec![2u32, 2u32], vec![7u32, 3u32]),
        ];
        let fwd = build_forward(3, 3, chunks);
        let (neighbors, _) = fwd.neighbors(0);
        assert_eq!(neighbors, &[1, 2]);
        let (neighbors, _) = fwd.neighbors(1);
        assert_eq!(neighbors, &[2]);
    }

    #[test]
    fn has_in_edge_bitmap_flags_only_nodes_with_an_incoming_edge() {
        // Node 0: two out-edges, no in-edge at all (the spec's "node with
        // no in-edges" structure). Node 3: a self-loop only -- still
        // counts as an in-edge of itself. Node 4: on no edge line at all.
        let edges = [(0, 1, 4), (0, 2, 9), (1, 2, 3), (2, 1, 6), (3, 3, 7)];
        let seen = has_in_edge_bitmap(5, &chunk(&edges));
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
