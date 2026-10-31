//! Node reordering: renumber nodes by BFS visitation order instead of
//! whatever order the input happened to use, then rebuild the CSR
//! under the new numbering. Purely a cache-locality optimization --
//! it changes nothing about which distances get computed, only how
//! close in memory the nodes a single search actually touches end up.
//!
//! Forward edges only (no reverse CSR): the spec's main body is
//! strongly connected, so a forward-only BFS from any main-body node
//! already reaches the whole main body -- no need to build a second,
//! reverse adjacency just for this ordering pass.

use crate::csr::Csr;

/// BFS over `csr`'s forward edges, starting a new tree from any node
/// not yet visited (so isolated nodes, no-out nodes and islands all
/// still get a new id). Returns `new_id_of_old[old_id] = new_id`.
pub fn bfs_order(n: usize, csr: &Csr) -> Vec<u32> {
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
            let (neighbors, _) = csr.neighbors(u);
            for &v in neighbors {
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

/// Rebuild a CSR under a new node numbering. Degree is preserved per
/// node (just relocated to its new id's slot); every neighbor id
/// stored in the adjacency arrays is relabeled too, since neighbors
/// are nodes themselves.
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

/// Reindex a per-node bitmap (e.g. `has_in_edge`) from old ids to new.
pub fn remap_bitmap(old: &[bool], new_id_of_old: &[u32]) -> Vec<bool> {
    let mut remapped = vec![false; old.len()];
    for (old_id, &value) in old.iter().enumerate() {
        remapped[new_id_of_old[old_id] as usize] = value;
    }
    remapped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plain single-threaded counting-sort CSR build, test-only --
    /// deliberately independent of `csr::build_forward_and_in_edge`
    /// (which this module's tests should not need to know anything
    /// about), so these tests only ever depend on the `Csr` type
    /// itself, not on how one happened to get built.
    fn graph(n: usize, edges: &[(u32, u32, u32)]) -> Csr {
        let mut offsets = vec![0u32; n + 1];
        for &(u, _, _) in edges {
            offsets[u as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let mut cursor = offsets.clone();
        let mut adj_node = vec![0u32; edges.len()];
        let mut adj_weight = vec![0u32; edges.len()];
        for &(u, v, w) in edges {
            let pos = cursor[u as usize] as usize;
            adj_node[pos] = v;
            adj_weight[pos] = w;
            cursor[u as usize] += 1;
        }
        Csr {
            offsets,
            adj_node,
            adj_weight,
        }
    }

    #[test]
    fn bfs_order_visits_every_node_exactly_once() {
        let csr = graph(6, &[(0, 1, 1), (1, 2, 1), (2, 3, 1), (4, 5, 1)]);
        let order = bfs_order(6, &csr);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 1, 2, 3, 4, 5],
            "every old id must map to a distinct new id covering 0..n"
        );
    }

    #[test]
    fn bfs_order_covers_disconnected_components_and_isolated_nodes() {
        // Two separate components reachable via forward edges, plus an
        // isolated node (5) and a no-out node (reached but never explored
        // from, since it has no out-edges of its own).
        let csr = graph(6, &[(0, 1, 1), (1, 2, 1), (3, 4, 1)]);
        let order = bfs_order(6, &csr);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 1, 2, 3, 4, 5],
            "disconnected/isolated nodes must still get a unique new id"
        );
    }

    #[test]
    fn bfs_order_groups_a_connected_component_into_contiguous_new_ids() {
        let csr = graph(5, &[(0, 2, 1), (2, 4, 1)]); // component {0,2,4}; nodes 1,3 isolated
        let order = bfs_order(5, &csr);
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
    fn remap_csr_preserves_every_edge_weight_and_shortest_path_distance() {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;

        fn naive_dijkstra(csr: &Csr, s: u32, t: u32) -> Option<u64> {
            let mut dist = std::collections::HashMap::new();
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

        let edges = [(0u32, 3u32, 10u32), (3, 1, 20), (1, 0, 30), (2, 2, 40)]; // includes a self-loop
        let csr = graph(4, &edges);
        let new_id_of_old = bfs_order(4, &csr);
        let remapped = remap_csr(&csr, &new_id_of_old);

        for u in 0..4u32 {
            for v in 0..4u32 {
                let before = naive_dijkstra(&csr, u, v);
                let after = naive_dijkstra(
                    &remapped,
                    new_id_of_old[u as usize],
                    new_id_of_old[v as usize],
                );
                assert_eq!(
                    before, after,
                    "u={u} v={v}: distance changed after reordering"
                );
            }
        }
    }

    #[test]
    fn remap_bitmap_moves_each_flag_to_its_new_id() {
        let has_in_edge = vec![true, false, true, false];
        let new_id_of_old = vec![2u32, 0, 3, 1]; // old 0->new 2, old 1->new 0, ...
        let remapped = remap_bitmap(&has_in_edge, &new_id_of_old);
        assert_eq!(remapped, vec![false, false, true, true]);
    }
}
