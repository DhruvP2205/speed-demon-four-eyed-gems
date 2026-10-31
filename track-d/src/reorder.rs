//! Node reordering: renumber nodes internally by BFS visitation order
//! (multi-source, so disconnected components are all covered) instead
//! of whatever order they arrived in, then rebuild the CSR on the new
//! numbering. Purely a CACHE-LOCALITY optimization -- it changes
//! nothing about which distances get computed, only how the `dist`/
//! `visited` arrays `Workspace` indexes into are laid out in memory
//! relative to the nodes a single search actually touches.
//!
//! Why this matters at all: a Dijkstra-style search explores a
//! GRAPH-LOCAL region (nodes close in graph distance to the source),
//! but `Workspace`'s arrays are indexed by NODE ID -- if node ids
//! aren't correlated with graph locality, that region is scattered
//! across the whole `dist_f`/`dist_b` arrays, hitting a different
//! cache line almost every access. BFS order puts nodes visited close
//! together in time (hence close in graph distance) at numerically
//! close ids, so a search's working set becomes a tighter, more
//! cache-friendly range.
//!
//! Why this is the SAME optimization `gen_d.py --mode road`'s random
//! id permutation was specifically designed to make non-trivial (see
//! that function's own doc comment): `grid` mode's id = `row*cols+col`
//! is ALREADY locality-correlated by construction, so reordering it
//! would do almost nothing measurable. Real road data (and this
//! project's `road` mode, once permuted) has no such correlation --
//! exactly the case this preprocessing step earns its keep on.

use crate::csr::Csr;

/// Multi-source BFS over the UNDIRECTED union of `fwd` and `rev`
/// (traversing either direction is valid for an ORDERING heuristic,
/// even though the graph itself is directed and distances are NOT
/// treated as symmetric anywhere else) -- maximizes how much of the
/// graph a single connected traversal can cover, since `road`/`grid`
/// mode's edges are MOSTLY (not always) bidirectional. Returns
/// `new_id_of_old[old_id] = new_id`.
pub fn bfs_order(n: usize, fwd: &Csr, rev: &Csr) -> Vec<u32> {
    let mut new_id_of_old = vec![u32::MAX; n];
    let mut next_new_id = 0u32;
    let mut queue = std::collections::VecDeque::with_capacity(n.min(1 << 16));

    for start in 0..n {
        if new_id_of_old[start] != u32::MAX {
            continue; // already assigned by an earlier component's BFS
        }
        new_id_of_old[start] = next_new_id;
        next_new_id += 1;
        queue.push_back(start as u32);
        while let Some(u) = queue.pop_front() {
            let (fwd_neighbors, _) = fwd.neighbors(u);
            let (rev_neighbors, _) = rev.neighbors(u);
            for &v in fwd_neighbors.iter().chain(rev_neighbors.iter()) {
                let vi = v as usize;
                if new_id_of_old[vi] == u32::MAX {
                    new_id_of_old[vi] = next_new_id;
                    next_new_id += 1;
                    queue.push_back(v);
                }
            }
        }
    }
    debug_assert_eq!(
        next_new_id as usize, n,
        "every node must get exactly one new id"
    );
    new_id_of_old
}

/// Rebuild a CSR under a new node numbering, given the OLD CSR and
/// `new_id_of_old`. Degree is preserved per node (just relocated to
/// its new id's slot); every neighbor id stored in the adjacency
/// arrays is relabeled too, since neighbors are nodes themselves.
pub fn remap_csr(old: &Csr, new_id_of_old: &[u32]) -> Csr {
    let n = new_id_of_old.len();
    let m = old.adj_node.len();

    let mut offsets = vec![0u32; n + 1];
    for old_id in 0..n {
        let degree = old.offsets[old_id + 1] - old.offsets[old_id];
        let new_id = new_id_of_old[old_id] as usize;
        offsets[new_id + 1] = degree;
    }
    for i in 0..n {
        offsets[i + 1] += offsets[i];
    }

    let mut cursor = offsets.clone();
    let mut adj_node = vec![0u32; m];
    let mut adj_weight = vec![0u32; m];
    for old_id in 0..n {
        let new_id = new_id_of_old[old_id] as usize;
        let begin = old.offsets[old_id] as usize;
        let end = old.offsets[old_id + 1] as usize;
        let pos = cursor[new_id] as usize;
        for (k, i) in (begin..end).enumerate() {
            adj_node[pos + k] = new_id_of_old[old.adj_node[i] as usize];
            adj_weight[pos + k] = old.adj_weight[i];
        }
        cursor[new_id] += end as u32 - begin as u32;
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
    use crate::csr::{build_forward, build_reverse};
    use crate::dijkstra::{bidirectional_dijkstra, Workspace};

    fn graphs(n: usize, edges: &[(u32, u32, u32)]) -> (Csr, Csr) {
        let us = edges.iter().map(|&(u, _, _)| u).collect();
        let vs = edges.iter().map(|&(_, v, _)| v).collect();
        let ws = edges.iter().map(|&(_, _, w)| w).collect();
        let chunks = vec![(us, vs, ws)];
        (
            build_forward(n, edges.len(), &chunks),
            build_reverse(n, edges.len(), &chunks),
        )
    }

    #[test]
    fn bfs_order_visits_every_node_exactly_once() {
        let (fwd, rev) = graphs(6, &[(0, 1, 1), (1, 2, 1), (2, 3, 1), (4, 5, 1)]);
        let order = bfs_order(6, &fwd, &rev);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 1, 2, 3, 4, 5],
            "every old id must map to a distinct new id covering 0..n"
        );
    }

    #[test]
    fn bfs_order_covers_disconnected_components() {
        // Two separate components: {0,1,2} and {3,4}; node 5 isolated.
        let (fwd, rev) = graphs(6, &[(0, 1, 1), (1, 2, 1), (3, 4, 1)]);
        let order = bfs_order(6, &fwd, &rev);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 1, 2, 3, 4, 5],
            "disconnected nodes must still get a (unique) new id"
        );
    }

    #[test]
    fn bfs_order_groups_a_connected_component_into_contiguous_new_ids() {
        // 0's whole component {0,1,2} should all land in the new-id
        // range [0,3) (the FIRST component BFS starts from node 0),
        // regardless of the original ids' own ordering/gaps.
        let (fwd, rev) = graphs(5, &[(0, 2, 1), (2, 4, 1)]); // component {0,2,4}; nodes 1,3 isolated
        let order = bfs_order(5, &fwd, &rev);
        let mut component_new_ids: Vec<u32> =
            [0usize, 2, 4].iter().map(|&old| order[old]).collect();
        component_new_ids.sort_unstable();
        assert_eq!(
            component_new_ids,
            vec![0, 1, 2],
            "0's component should occupy a contiguous block starting at 0"
        );
    }

    #[test]
    fn remap_csr_preserves_every_edge_and_weight() {
        let edges = [(0u32, 3u32, 10u32), (3, 1, 20), (1, 0, 30), (2, 2, 40)]; // includes a self-loop
        let (fwd, rev) = graphs(4, &edges);
        let new_id_of_old = bfs_order(4, &fwd, &rev);

        let remapped_fwd = remap_csr(&fwd, &new_id_of_old);
        let remapped_rev = remap_csr(&rev, &new_id_of_old);

        // Distances must be IDENTICAL regardless of numbering -- the
        // whole point of this step is that it's purely cosmetic for
        // correctness purposes.
        let mut ws_before = Workspace::new(4);
        let mut ws_after = Workspace::new(4);
        for &(u, v, _) in &edges {
            let before = bidirectional_dijkstra(&mut ws_before, u, v, &fwd, &rev);
            let new_u = new_id_of_old[u as usize];
            let new_v = new_id_of_old[v as usize];
            let after =
                bidirectional_dijkstra(&mut ws_after, new_u, new_v, &remapped_fwd, &remapped_rev);
            assert_eq!(
                before, after,
                "u={u} v={v}: distance changed after reordering"
            );
        }
    }

    #[test]
    fn remap_csr_on_a_larger_random_graph_matches_original_distances_for_every_pair() {
        let n = 40u32;
        let mut edges = Vec::new();
        let mut seed = 42u64;
        let mut next_rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut perm: Vec<u32> = (0..n).collect();
        for i in (1..n as usize).rev() {
            let j = (next_rand() % (i as u64 + 1)) as usize;
            perm.swap(i, j);
        }
        for i in 0..perm.len() - 1 {
            let (a, b) = (perm[i], perm[i + 1]);
            edges.push((a, b, (next_rand() % 100) as u32));
            edges.push((b, a, (next_rand() % 100) as u32));
        }

        let (fwd, rev) = graphs(n as usize, &edges);
        let new_id_of_old = bfs_order(n as usize, &fwd, &rev);
        let remapped_fwd = remap_csr(&fwd, &new_id_of_old);
        let remapped_rev = remap_csr(&rev, &new_id_of_old);

        let mut ws_before = Workspace::new(n as usize);
        let mut ws_after = Workspace::new(n as usize);
        for s in 0..n {
            for t in 0..n {
                let before = bidirectional_dijkstra(&mut ws_before, s, t, &fwd, &rev);
                let after = bidirectional_dijkstra(
                    &mut ws_after,
                    new_id_of_old[s as usize],
                    new_id_of_old[t as usize],
                    &remapped_fwd,
                    &remapped_rev,
                );
                assert_eq!(
                    before, after,
                    "s={s} t={t}: distance changed after reordering"
                );
            }
        }
    }
}
