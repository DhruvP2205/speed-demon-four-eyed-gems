//! Bidirectional Dijkstra, one query at a time, with stamped
//! (generation-counter) workspace arrays reused across every query a
//! thread handles -- never cleared/reallocated per query.
//!
//! Why bidirectional: a plain single-source Dijkstra (reference/
//! oracle_d's own approach, and this track's naive reference) explores
//! roughly a full ball of radius `dist(s,t)` around `s`. Searching
//! simultaneously from `s` forward and from `t` backward (over the
//! REVERSE graph) and stopping once the two frontiers' combined reach
//! can no longer beat the best meeting point found so far needs only
//! two balls of roughly half that radius each -- for a small-world
//! random graph this is a dramatic reduction; for a grid-like graph
//! (see gen_d.py's second mode) the benefit is smaller (no shortcuts
//! to exploit) but still real, since it still roughly halves the
//! search radius needed on each side.
//!
//! Touched-list + undo: the classic "reset is O(touched), not O(n)"
//! trick -- instead of a generation-stamp array per direction (the
//! earlier design here, same device as reference/oracle_d's own
//! `Workspace`), each direction keeps a plain `dist`/`visited` array
//! PLUS a list of the indices touched so far this query. Finishing a
//! query walks that list and resets exactly those entries back to
//! their pristine state (`u64::MAX` / not-visited), then clears the
//! list -- still O(1) amortized per touch, and removes the two
//! generation-counter arrays entirely (4 bytes/node each) that added
//! nothing beyond bookkeeping. `set_dist_*` only pushes an index onto
//! the touched list the FIRST time it's touched this query (checked
//! via `dist == u64::MAX`, the pristine sentinel) -- a node can never
//! be marked visited before its distance is set at least once (the
//! search always relaxes/seeds a node before popping it), so a single
//! touched list per direction correctly undoes both arrays.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::csr::Csr;

pub struct Workspace {
    dist_f: Vec<u64>,
    visited_f: Vec<bool>,
    touched_f: Vec<u32>,
    dist_b: Vec<u64>,
    visited_b: Vec<bool>,
    touched_b: Vec<u32>,
    heap_f: BinaryHeap<Reverse<(u64, u32)>>,
    heap_b: BinaryHeap<Reverse<(u64, u32)>>,
}

impl Workspace {
    pub fn new(n: usize) -> Self {
        Workspace {
            dist_f: vec![u64::MAX; n],
            visited_f: vec![false; n],
            touched_f: Vec::new(),
            dist_b: vec![u64::MAX; n],
            visited_b: vec![false; n],
            touched_b: Vec::new(),
            heap_f: BinaryHeap::new(),
            heap_b: BinaryHeap::new(),
        }
    }

    /// Current backing capacity (not length -- capacity never shrinks
    /// once grown, so this reflects the LARGEST either heap has ever
    /// needed to be across every query this Workspace has handled).
    /// Diagnostic only (PERF.md's memory-breakdown step), not used by
    /// the search itself.
    pub fn heap_capacity_bytes(&self) -> usize {
        (self.heap_f.capacity() + self.heap_b.capacity())
            * std::mem::size_of::<Reverse<(u64, u32)>>()
    }

    /// Largest number of nodes EITHER direction has ever had touched
    /// at once (i.e. the touched-list's peak length, reflecting
    /// capacity, not the current/empty-between-queries length).
    /// Diagnostic only, same spirit as `heap_capacity_bytes`.
    pub fn touched_capacity_bytes(&self) -> usize {
        (self.touched_f.capacity() + self.touched_b.capacity()) * std::mem::size_of::<u32>()
    }

    /// Number of nodes touched (either direction) by the MOST RECENT
    /// `bidirectional_dijkstra` call -- valid to read any time after
    /// that call returns and before the next one starts (`reset()`
    /// runs at the START of the next call, not the end, specifically
    /// so this stays readable in between). Used by the ALT query-
    /// adaptive sampling step (PERF.md's step on it) to measure how
    /// hard the actual query workload is, not just the graph's
    /// structure.
    pub fn last_touched_count(&self) -> usize {
        self.touched_f.len() + self.touched_b.len()
    }

    #[inline]
    fn get_dist_f(&self, i: usize) -> u64 {
        self.dist_f[i]
    }
    #[inline]
    fn set_dist_f(&mut self, i: usize, d: u64) {
        if self.dist_f[i] == u64::MAX {
            self.touched_f.push(i as u32);
        }
        self.dist_f[i] = d;
    }
    #[inline]
    fn is_visited_f(&self, i: usize) -> bool {
        self.visited_f[i]
    }
    #[inline]
    fn mark_visited_f(&mut self, i: usize) {
        self.visited_f[i] = true;
    }

    #[inline]
    fn get_dist_b(&self, i: usize) -> u64 {
        self.dist_b[i]
    }
    #[inline]
    fn set_dist_b(&mut self, i: usize, d: u64) {
        if self.dist_b[i] == u64::MAX {
            self.touched_b.push(i as u32);
        }
        self.dist_b[i] = d;
    }
    #[inline]
    fn is_visited_b(&self, i: usize) -> bool {
        self.visited_b[i]
    }
    #[inline]
    fn mark_visited_b(&mut self, i: usize) {
        self.visited_b[i] = true;
    }

    /// Undo every touch from the PREVIOUS query (O(touched), not
    /// O(n)), readying both directions' arrays for a fresh query.
    /// Called at the start of `bidirectional_dijkstra` rather than
    /// the end, so a workspace that's never been used (empty touched
    /// lists) costs nothing on its first call.
    fn reset(&mut self) {
        for &i in &self.touched_f {
            let i = i as usize;
            self.dist_f[i] = u64::MAX;
            self.visited_f[i] = false;
        }
        self.touched_f.clear();
        for &i in &self.touched_b {
            let i = i as usize;
            self.dist_b[i] = u64::MAX;
            self.visited_b[i] = false;
        }
        self.touched_b.clear();
    }
}

/// Shortest path distance from `start` to `target`, or `-1` if
/// unreachable. `fwd`/`rev` must be CSRs over the SAME graph (`rev`
/// grouped by destination, storing predecessors -- see csr.rs).
///
/// Termination: standard bidirectional-Dijkstra stopping rule. Once
/// `top_f + top_b >= best` (the two frontiers' smallest remaining
/// tentative distances can no longer combine to beat the best meeting
/// point already found), no further expansion can improve on `best`:
/// any node popped later has distance >= its own side's current top,
/// so any NEW meeting point through it is already >= top_f + top_b.
/// `best` itself is only ever updated when a node is settled
/// (popped+marked visited) on one side while ALREADY settled on the
/// other -- using the other side's FINAL (visited) distance, not a
/// possibly-still-improvable tentative one, is what keeps this simple
/// check correct without needing to also check at relax time.
pub fn bidirectional_dijkstra(
    ws: &mut Workspace,
    start: u32,
    target: u32,
    fwd: &Csr,
    rev: &Csr,
) -> i64 {
    if start == target {
        return 0;
    }
    ws.reset();
    ws.heap_f.clear();
    ws.heap_b.clear();
    ws.set_dist_f(start as usize, 0);
    ws.heap_f.push(Reverse((0u64, start)));
    ws.set_dist_b(target as usize, 0);
    ws.heap_b.push(Reverse((0u64, target)));

    let mut best = u64::MAX;

    loop {
        let top_f = ws
            .heap_f
            .peek()
            .map(|&Reverse((d, _))| d)
            .unwrap_or(u64::MAX);
        let top_b = ws
            .heap_b
            .peek()
            .map(|&Reverse((d, _))| d)
            .unwrap_or(u64::MAX);
        if top_f == u64::MAX && top_b == u64::MAX {
            break;
        }
        if best != u64::MAX && top_f.saturating_add(top_b) >= best {
            break;
        }

        if top_f <= top_b {
            let Some(Reverse((d, u))) = ws.heap_f.pop() else {
                break;
            };
            let ui = u as usize;
            if ws.is_visited_f(ui) {
                continue;
            }
            ws.mark_visited_f(ui);
            if ws.is_visited_b(ui) {
                let cand = d.saturating_add(ws.get_dist_b(ui));
                if cand < best {
                    best = cand;
                }
            }
            let (neighbors, weights) = fwd.neighbors(u);
            for k in 0..neighbors.len() {
                let v = neighbors[k];
                let nd = d + weights[k] as u64;
                let vi = v as usize;
                if nd < ws.get_dist_f(vi) {
                    ws.set_dist_f(vi, nd);
                    ws.heap_f.push(Reverse((nd, v)));
                    // The TRUE meeting point can be a node whose
                    // tentative forward distance is now correct (or
                    // at least informative) WITHOUT that node ever
                    // being popped/settled by the forward side before
                    // the stopping condition fires below -- checking
                    // only at pop time (as an earlier, buggy version
                    // of this function did) misses exactly that case
                    // and reports a too-large distance. A relaxed
                    // edge into an ALREADY backward-settled node is
                    // enough to form a valid s-t path of length
                    // nd + dist_b(v), regardless of whether the
                    // forward side later settles v itself.
                    if ws.is_visited_b(vi) {
                        let cand = nd.saturating_add(ws.get_dist_b(vi));
                        if cand < best {
                            best = cand;
                        }
                    }
                }
            }
        } else {
            let Some(Reverse((d, u))) = ws.heap_b.pop() else {
                break;
            };
            let ui = u as usize;
            if ws.is_visited_b(ui) {
                continue;
            }
            ws.mark_visited_b(ui);
            if ws.is_visited_f(ui) {
                let cand = d.saturating_add(ws.get_dist_f(ui));
                if cand < best {
                    best = cand;
                }
            }
            let (neighbors, weights) = rev.neighbors(u);
            for k in 0..neighbors.len() {
                let v = neighbors[k];
                let nd = d + weights[k] as u64;
                let vi = v as usize;
                if nd < ws.get_dist_b(vi) {
                    ws.set_dist_b(vi, nd);
                    ws.heap_b.push(Reverse((nd, v)));
                    if ws.is_visited_f(vi) {
                        let cand = nd.saturating_add(ws.get_dist_f(vi));
                        if cand < best {
                            best = cand;
                        }
                    }
                }
            }
        }
    }

    if best == u64::MAX {
        -1
    } else {
        best as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csr::{build_forward, build_reverse};

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
    fn same_start_and_target_is_zero() {
        let (fwd, rev) = graphs(3, &[(0, 1, 5)]);
        let mut ws = Workspace::new(3);
        assert_eq!(bidirectional_dijkstra(&mut ws, 1, 1, &fwd, &rev), 0);
    }

    #[test]
    fn simple_chain_shortest_path() {
        let (fwd, rev) = graphs(5, &[(0, 1, 2), (1, 2, 3), (2, 3, 4), (3, 4, 1)]);
        let mut ws = Workspace::new(5);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 4, &fwd, &rev), 10);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 2, &fwd, &rev), 5);
    }

    #[test]
    fn picks_the_cheaper_of_two_paths() {
        // 0 -> 1 -> 3 costs 1+1=2; 0 -> 2 -> 3 costs 10+10=20.
        let (fwd, rev) = graphs(4, &[(0, 1, 1), (1, 3, 1), (0, 2, 10), (2, 3, 10)]);
        let mut ws = Workspace::new(4);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 3, &fwd, &rev), 2);
    }

    #[test]
    fn unreachable_target_returns_negative_one() {
        let (fwd, rev) = graphs(4, &[(0, 1, 5)]); // 2, 3 are isolated
        let mut ws = Workspace::new(4);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 2, &fwd, &rev), -1);
        assert_eq!(bidirectional_dijkstra(&mut ws, 3, 0, &fwd, &rev), -1);
    }

    #[test]
    fn self_loop_and_zero_weight_edge_do_not_confuse_the_search() {
        // Matches gen_d.py's edge-case fixture shape: a self-loop and a
        // zero-weight edge sitting right on the shortest path.
        let (fwd, rev) = graphs(4, &[(0, 1, 5), (1, 1, 999), (1, 2, 0), (2, 3, 2)]);
        let mut ws = Workspace::new(4);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 3, &fwd, &rev), 7);
    }

    #[test]
    fn duplicate_edges_with_different_weights_the_cheaper_one_wins() {
        let (fwd, rev) = graphs(2, &[(0, 1, 9), (0, 1, 5)]);
        let mut ws = Workspace::new(2);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 1, &fwd, &rev), 5);
    }

    #[test]
    fn workspace_is_correctly_reset_across_repeated_queries() {
        let (fwd, rev) = graphs(5, &[(0, 1, 2), (1, 2, 3), (2, 3, 4), (3, 4, 1)]);
        let mut ws = Workspace::new(5);
        // Run the SAME workspace across several different queries, out
        // of order, repeating some -- the whole point of the
        // generation-stamp design is that none of this needs a fresh
        // Workspace or an explicit clear.
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 4, &fwd, &rev), 10);
        assert_eq!(bidirectional_dijkstra(&mut ws, 1, 3, &fwd, &rev), 7);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 4, &fwd, &rev), 10);
        assert_eq!(bidirectional_dijkstra(&mut ws, 4, 0, &fwd, &rev), -1); // wrong direction, directed graph
        assert_eq!(bidirectional_dijkstra(&mut ws, 2, 2, &fwd, &rev), 0);
    }

    #[test]
    fn large_weight_sums_do_not_overflow_u32() {
        // Two near-u32::MAX weight edges in a row: the sum exceeds
        // u32::MAX but fits comfortably in u64 (project convention,
        // matching reference/oracle_d's own comment on why path sums
        // use u64 -- a u32 accumulator would silently wrap here).
        let big = 4_000_000_000u32;
        let (fwd, rev) = graphs(3, &[(0, 1, big), (1, 2, big)]);
        let mut ws = Workspace::new(3);
        assert_eq!(
            bidirectional_dijkstra(&mut ws, 0, 2, &fwd, &rev),
            big as i64 * 2
        );
    }

    // --- Stopping-criterion confirmation: `top_f + top_b >= best`.
    // Zero-weight edges and ties are exactly the cases where that sum
    // can land EXACTLY on `best` (not strictly greater), which is the
    // boundary the `>=` (not `>`) in the real code has to get right --
    // stopping one query-state too early OR one too late both read as
    // "it still returned the right answer" on an ordinary test, so
    // these are specifically built to make the boundary condition
    // itself load-bearing, not just exercised incidentally. ---

    #[test]
    fn zero_weight_edge_on_the_only_path_is_still_found() {
        // s -(0)-> a -(0)-> t: both edges free, so the true distance
        // is 0 even though s != t -- `best` starts at u64::MAX and
        // the ONLY way to reach the correct answer is a path made
        // entirely of zero-weight relaxations.
        let (fwd, rev) = graphs(3, &[(0, 1, 0), (1, 2, 0)]);
        let mut ws = Workspace::new(3);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 2, &fwd, &rev), 0);
    }

    #[test]
    fn zero_weight_edge_not_on_the_shortest_path_does_not_create_a_shortcut() {
        // A zero-weight edge exists (1 -> 2), but it doesn't connect
        // s to t at all -- the real shortest path is the longer one
        // through node 3. Confirms a zero-weight relaxation elsewhere
        // in the graph can't accidentally lower `best` via a bogus
        // meeting point.
        let (fwd, rev) = graphs(5, &[(0, 1, 10), (1, 2, 0), (0, 3, 3), (3, 4, 4)]);
        let mut ws = Workspace::new(5);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 4, &fwd, &rev), 7);
    }

    #[test]
    fn exact_tie_between_two_disjoint_equal_cost_paths() {
        // Diamond: s -> a -> t (cost 1+1=2) and s -> b -> t (cost
        // 1+1=2), completely disjoint apart from the shared
        // endpoints. Both the forward and backward searches discover
        // TWO candidate meeting points (a and b) at the exact same
        // tentative distance -- if the stopping check used `>` instead
        // of `>=`, or updated `best` inconsistently between the two
        // ties, this is the shape most likely to expose it.
        let (fwd, rev) = graphs(4, &[(0, 1, 1), (1, 3, 1), (0, 2, 1), (2, 3, 1)]);
        let mut ws = Workspace::new(4);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 3, &fwd, &rev), 2);
    }

    #[test]
    fn tie_with_a_zero_weight_edge_on_one_of_the_tied_paths() {
        // Same diamond shape, but one of the two equal-cost paths
        // achieves its cost via a zero-weight edge (0 + 2 = 2) instead
        // of two unit edges (1 + 1 = 2) -- combines both boundary
        // conditions (tie AND zero-weight) in one graph.
        let (fwd, rev) = graphs(4, &[(0, 1, 0), (1, 3, 2), (0, 2, 1), (2, 3, 1)]);
        let mut ws = Workspace::new(4);
        assert_eq!(bidirectional_dijkstra(&mut ws, 0, 3, &fwd, &rev), 2);
    }

    /// Correct, independently-implemented (NOT a copy of
    /// `bidirectional_dijkstra`'s own logic) all-pairs shortest path
    /// via Floyd-Warshall, used ONLY as a reference oracle for the
    /// brute-force cross-check below -- O(n^3), computed ONCE for the
    /// whole graph (the caller then just reads `result[s][t]` for
    /// each pair). Earlier version recomputed this from scratch
    /// inside the s/t loop below -- O(n^3) work repeated O(n^2) times
    /// is O(n^5), which is why the test APPEARED to hang (not an
    /// infinite loop, just ~2.4*10^12 operations at n=300 -- caught by
    /// running it under an explicit `timeout` wrapper after a plain
    /// `cargo test` sat for several minutes with no other explanation).
    fn floyd_warshall_all_pairs(n: usize, edges: &[(u32, u32, u32)]) -> Vec<Vec<i64>> {
        const INF: i64 = i64::MAX / 2;
        let mut dist = vec![vec![INF; n]; n];
        for i in 0..n {
            dist[i][i] = 0;
        }
        for &(u, v, w) in edges {
            let d = &mut dist[u as usize][v as usize];
            *d = (*d).min(w as i64);
        }
        for k in 0..n {
            for i in 0..n {
                if dist[i][k] >= INF {
                    continue;
                }
                for j in 0..n {
                    let via_k = dist[i][k] + dist[k][j];
                    if via_k < dist[i][j] {
                        dist[i][j] = via_k;
                    }
                }
            }
        }
        dist
    }

    #[test]
    fn brute_force_cross_check_against_floyd_warshall_with_zero_weights_and_ties() {
        // Small enough for O(n^3) Floyd-Warshall to be trivially fast
        // and trivially correct. Deliberately SPARSE (a random chain
        // for connectivity plus a handful of shortcut edges), NOT a
        // dense complete graph: a dense graph's diameter is tiny (1-2
        // hops always suffice), which settles almost the WHOLE graph
        // before the stopping criterion ever gets a chance to fire
        // early -- exactly the shape that let the original pop-time-
        // only bug hide (it was only caught on a 50,000-node SPARSE
        // fixture, not any small hand-crafted graph). A sparse graph
        // with a real spanning structure is what actually gives the
        // stopping criterion -- and the relax-time candidate check it
        // depends on -- room to matter. Still folds in zero-weight
        // edges and ties (both by explicit construction), but via a
        // topology that can actually exercise the early-stop path.
        let n = 300u32;
        let mut edges = Vec::new();
        let mut seed = 987654321u64;
        let mut next_rand = move || {
            // Tiny xorshift PRNG -- deterministic, no external crate.
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        // Random chain (both directions) for guaranteed connectivity,
        // mostly a WIDE weight range (0..1000) so the graph has real
        // "distance" to it (diameter many hops, not 1-2) -- but every
        // 7th link is forced to weight 0, and every 11th link's
        // reverse direction is forced to match the forward one
        // exactly (a deliberate tie between two directions of the
        // same edge).
        let mut perm: Vec<u32> = (0..n).collect();
        for i in (1..n as usize).rev() {
            let j = (next_rand() % (i as u64 + 1)) as usize;
            perm.swap(i, j);
        }
        for i in 0..perm.len() - 1 {
            let (a, b) = (perm[i], perm[i + 1]);
            let w_fwd = if i % 7 == 0 {
                0
            } else {
                (next_rand() % 1000) as u32
            };
            let w_bwd = if i % 11 == 0 {
                w_fwd
            } else {
                (next_rand() % 1000) as u32
            };
            edges.push((a, b, w_fwd));
            edges.push((b, a, w_bwd));
        }
        // A modest number of extra random shortcut edges on top --
        // enough to create alternative routes (so the shortest path
        // isn't always just "walk the chain"), not enough to turn
        // this back into a dense, tiny-diameter graph.
        for _ in 0..(n / 2) {
            let a = (next_rand() % n as u64) as u32;
            let b = (next_rand() % n as u64) as u32;
            if a != b {
                edges.push((a, b, (next_rand() % 1000) as u32));
            }
        }

        let us: Vec<u32> = edges.iter().map(|&(u, _, _)| u).collect();
        let vs: Vec<u32> = edges.iter().map(|&(_, v, _)| v).collect();
        let ws: Vec<u32> = edges.iter().map(|&(_, _, w)| w).collect();
        let chunks = vec![(us, vs, ws)];
        let fwd = build_forward(n as usize, edges.len(), &chunks);
        let rev = build_reverse(n as usize, edges.len(), &chunks);
        let mut workspace = Workspace::new(n as usize);
        let all_pairs = floyd_warshall_all_pairs(n as usize, &edges);
        const INF: i64 = i64::MAX / 2;

        for s in 0..n {
            for t in 0..n {
                let got = bidirectional_dijkstra(&mut workspace, s, t, &fwd, &rev);
                let raw = all_pairs[s as usize][t as usize];
                let want = if raw >= INF { -1 } else { raw };
                assert_eq!(
                    got, want,
                    "s={s} t={t}: bidirectional Dijkstra diverged from Floyd-Warshall"
                );
            }
        }
    }
}
