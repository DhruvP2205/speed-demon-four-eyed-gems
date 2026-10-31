//! Compact CSR (compressed sparse row) adjacency, built via counting
//! sort -- same idiom as reference/oracle_d's own CSR build (not
//! reinvented independently; this track's oracle already established
//! the pattern is correct and fast enough for this scale).
//!
//! Two CSRs are built from the same edge list: `forward` (grouped by
//! `u`, giving each node's out-edges -- used by the forward half of
//! bidirectional Dijkstra) and `reverse` (grouped by `v`, storing each
//! edge's `u` endpoint -- used by the backward half, which conceptually
//! walks the graph with every edge direction flipped).

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
/// `us`/`vs`/`ws` may be split across several same-length slices (one
/// per parsing thread, in any order) -- passed as a slice of triples
/// of slices rather than one concatenated `Vec` to avoid an extra O(m)
/// copy merging them first.
pub fn build_forward(n: usize, m: usize, chunks: &[(Vec<u32>, Vec<u32>, Vec<u32>)]) -> Csr {
    build_csr(n, m, chunks, true)
}

/// Build the reverse CSR (grouped by `v`): `adj_node`/`adj_weight` at
/// `offsets[v]..offsets[v+1]` give the predecessors `(u, w)` of edges
/// `u -> v` -- i.e. node `v`'s in-edges, stored as if the graph's
/// edges were all flipped. This is exactly what the backward half of
/// bidirectional Dijkstra needs to walk "against" the original edge
/// direction.
pub fn build_reverse(n: usize, m: usize, chunks: &[(Vec<u32>, Vec<u32>, Vec<u32>)]) -> Csr {
    build_csr(n, m, chunks, false)
}

/// `forward`: group by `u`, storing `v` as the neighbor (out-edges).
/// `!forward`: group by `v`, storing `u` as the neighbor (in-edges, as
/// if every edge were flipped). Tracking which endpoint is the
/// grouping key via this explicit flag -- rather than trying to infer
/// it afterward by comparing values -- matters specifically because a
/// self-loop (`u == v`, a real case the edge-case fixture exercises)
/// makes `u == v` true regardless of which endpoint was actually used
/// as the key, so any attempt to recover "which one was the key" from
/// the values alone silently breaks exactly on that input.
fn build_csr(n: usize, m: usize, chunks: &[(Vec<u32>, Vec<u32>, Vec<u32>)], forward: bool) -> Csr {
    // Degree counting is embarrassingly parallel across chunks (each
    // chunk already corresponds to one parse thread's share of the
    // edges) -- give each a private `n+1`-sized local count array (no
    // synchronization needed, since nothing is shared during the
    // count), then sum the per-chunk arrays into the final `offsets`
    // sequentially. The sum is O(n * chunks.len()), comparable to or
    // cheaper than the O(m) counting work it replaces at this
    // project's n/chunk-count ratio, and avoids any unsafe/atomic
    // code for a step that was ~half of CSR-build's total cost
    // (Step 8's finding: calibrated-local queries make parse+CSR a
    // much bigger fraction of total wall time than it used to be,
    // once query time itself drops to near nothing).
    let local_counts: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|(us, vs, _)| {
                scope.spawn(move || {
                    let mut counts = vec![0u32; n + 1];
                    for i in 0..us.len() {
                        let key = (if forward { us[i] } else { vs[i] }) as usize;
                        counts[key + 1] += 1;
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

    let mut cursor = offsets.clone();
    let mut adj_node = vec![0u32; m];
    let mut adj_weight = vec![0u32; m];
    for (us, vs, ws) in chunks {
        for i in 0..us.len() {
            let (u, v, w) = (us[i], vs[i], ws[i]);
            let (key, other) = if forward { (u, v) } else { (v, u) };
            let pos = cursor[key as usize] as usize;
            adj_node[pos] = other;
            adj_weight[pos] = w;
            cursor[key as usize] += 1;
        }
    }

    Csr {
        offsets,
        adj_node,
        adj_weight,
    }
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
        let csr = build_forward(3, 3, &chunk(&edges));
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
    fn reverse_csr_groups_by_v_storing_u() {
        let edges = [(0, 1, 5), (0, 2, 7), (1, 2, 3)];
        let csr = build_reverse(3, 3, &chunk(&edges));
        let (preds, _) = csr.neighbors(0);
        assert!(preds.is_empty());
        let (preds, weights) = csr.neighbors(1);
        assert_eq!(preds, &[0]);
        assert_eq!(weights, &[5]);
        let (preds, weights) = csr.neighbors(2);
        assert_eq!(preds, &[0, 1]);
        assert_eq!(weights, &[7, 3]);
    }

    #[test]
    fn handles_self_loops_and_duplicate_edges() {
        let edges = [(0, 1, 5), (0, 1, 9), (2, 2, 7)];
        let fwd = build_forward(3, 3, &chunk(&edges));
        let (neighbors, weights) = fwd.neighbors(0);
        assert_eq!(neighbors, &[1, 1]);
        assert_eq!(weights, &[5, 9]);
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
        let fwd = build_forward(3, 3, &chunks);
        let (neighbors, _) = fwd.neighbors(0);
        assert_eq!(neighbors, &[1, 2]);
        let (neighbors, _) = fwd.neighbors(1);
        assert_eq!(neighbors, &[2]);
    }
}
