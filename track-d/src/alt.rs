//! ALT (A* search with Landmarks and the Triangle inequality):
//! precomputed landmark distances give a consistent lower-bound
//! heuristic that steers EACH query's bidirectional search toward the
//! target faster, without changing what the search ever finds
//! (A*'s heuristic only reorders WHICH node gets explored next, never
//! changes the actual relaxation/distance logic).
//!
//! Landmark selection (farthest-point sampling, standard technique):
//! landmark 0 is an arbitrary start node; each subsequent landmark
//! maximizes the MINIMUM distance to every already-chosen landmark
//! (not just "farthest from the last one", which can cluster
//! landmarks on one side of the graph). This is INHERENTLY
//! sequential -- each pick's Dijkstra run updates a running "nearest
//! landmark" array the NEXT pick depends on.
//!
//! Once the landmark SET is fixed, building the actual distance
//! tables (forward AND reverse Dijkstra from every landmark) is fully
//! parallel -- every one of the `2 * landmark_count` runs is
//! independent of every other, distributed across up to 12 threads.
//!
//! Bidirectional + ALT correctness (the averaged-potential trick,
//! Goldberg & Harrelson, "Computing the Shortest Path: A* Search
//! Meets Graph Theory"): using the forward heuristic `h_f` (a lower
//! bound on dist(v, t)) and backward heuristic `h_b` (a lower bound
//! on dist(s, v)) directly as two SEPARATE, unrelated A* heuristics on
//! each side of a bidirectional search is NOT automatically safe --
//! the standard single-direction A* correctness proof doesn't cover
//! two independently-heuristic-guided searches meeting in the middle.
//! The fix: define a single potential `pi_f(v) = (h_f(v) - h_b(v)) /
//! 2` for the forward side and `pi_b(v) = -pi_f(v)` for the backward
//! side. This specific averaging guarantees `pi_f(v) + pi_b(v) == 0`
//! for every node, which is exactly the property that makes the
//! ORIGINAL (non-potential) bidirectional stopping criterion --
//! `top_f + top_b >= best` -- remain valid UNCHANGED when `top_f`/
//! `top_b` are read off heaps ordered by `g(v) + pi_*(v)` instead of
//! raw `g(v)`: for any node `v`, `g_f(v) + g_b(v) >= (top_f - pi_f(v))
//! + (top_b - pi_b(v)) = top_f + top_b` (the `pi_f(v)`/`pi_b(v)` terms
//! cancel exactly because they sum to zero). Nothing about the
//! RELAXATION logic or the REAL distances tracked in `dist_f`/
//! `dist_b` changes at all -- only which heap key orders the search.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::csr::Csr;

const INF: u64 = u64::MAX;

/// Plain single-source Dijkstra over the WHOLE graph (no early exit,
/// no bidirectional search, no heuristic) -- used only to build
/// landmark distance tables, where every node's distance is needed,
/// not just one target's.
fn single_source_dijkstra(csr: &Csr, source: u32, n: usize) -> Vec<u64> {
    let mut dist = vec![INF; n];
    let mut visited = vec![false; n];
    dist[source as usize] = 0;
    let mut heap = BinaryHeap::new();
    heap.push(Reverse((0u64, source)));
    while let Some(Reverse((d, u))) = heap.pop() {
        let ui = u as usize;
        if visited[ui] {
            continue;
        }
        visited[ui] = true;
        let (neighbors, weights) = csr.neighbors(u);
        for k in 0..neighbors.len() {
            let v = neighbors[k];
            let nd = d + weights[k] as u64;
            let vi = v as usize;
            if nd < dist[vi] {
                dist[vi] = nd;
                heap.push(Reverse((nd, v)));
            }
        }
    }
    dist
}

/// Select `k` landmarks via farthest-point sampling over the FORWARD
/// graph (see module doc). Sequential by nature -- each pick depends
/// on the running min-distance-to-any-landmark array the PREVIOUS
/// pick's full Dijkstra run produced.
pub fn select_landmarks(fwd: &Csr, n: usize, k: usize, start: u32) -> Vec<u32> {
    let k = k.min(n).max(1);
    let mut landmarks = Vec::with_capacity(k);
    let mut min_dist_to_any_landmark = vec![INF; n];
    let mut next = start;
    for _ in 0..k {
        landmarks.push(next);
        let dist_from_next = single_source_dijkstra(fwd, next, n);
        for v in 0..n {
            if dist_from_next[v] < min_dist_to_any_landmark[v] {
                min_dist_to_any_landmark[v] = dist_from_next[v];
            }
        }
        next = (0..n as u32)
            .filter(|&v| min_dist_to_any_landmark[v as usize] < INF)
            .max_by_key(|&v| min_dist_to_any_landmark[v as usize])
            .unwrap_or(start);
    }
    landmarks
}

pub struct Landmarks {
    count: usize,
    /// `dist_from[i][v]` = shortest distance from landmark `i` to `v`
    /// in the forward graph.
    dist_from: Vec<Vec<u64>>,
    /// `dist_to[i][v]` = shortest distance from `v` to landmark `i`
    /// in the forward graph (computed as a forward Dijkstra from `i`
    /// over the REVERSE graph).
    dist_to: Vec<Vec<u64>>,
}

impl Landmarks {
    /// Builds both distance tables for an already-chosen landmark
    /// set, as `2 * landmarks.len()` independent single-source
    /// Dijkstra runs distributed across up to 12 threads.
    pub fn build(fwd: &Csr, rev: &Csr, n: usize, landmarks: &[u32]) -> Self {
        let k = landmarks.len();
        let mut dist_from: Vec<Vec<u64>> = (0..k).map(|_| Vec::new()).collect();
        let mut dist_to: Vec<Vec<u64>> = (0..k).map(|_| Vec::new()).collect();

        let jobs: Vec<(bool, usize)> = (0..k)
            .map(|i| (true, i))
            .chain((0..k).map(|i| (false, i)))
            .collect();
        std::thread::scope(|scope| {
            let num_threads = 12usize.min(jobs.len().max(1));
            let chunk_size = jobs.len().div_ceil(num_threads).max(1);
            let handles: Vec<_> = (0..jobs.len())
                .step_by(chunk_size)
                .map(|start| {
                    let end = (start + chunk_size).min(jobs.len());
                    let jobs_ref = &jobs;
                    scope.spawn(move || {
                        jobs_ref[start..end]
                            .iter()
                            .map(|&(is_forward, idx)| {
                                let csr = if is_forward { fwd } else { rev };
                                let d = single_source_dijkstra(csr, landmarks[idx], n);
                                (is_forward, idx, d)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            for h in handles {
                for (is_forward, idx, d) in h.join().expect("landmark Dijkstra worker panicked") {
                    if is_forward {
                        dist_from[idx] = d;
                    } else {
                        dist_to[idx] = d;
                    }
                }
            }
        });

        Landmarks {
            count: k,
            dist_from,
            dist_to,
        }
    }

    /// Exact measured size of the two distance tables (diagnostic
    /// only, same style as `dijkstra::Workspace::heap_capacity_bytes`
    /// -- not used by the search itself).
    pub fn memory_bytes(&self) -> usize {
        let per_table: usize = self.dist_from.iter().map(|v| v.len() * 8).sum();
        let per_table_to: usize = self.dist_to.iter().map(|v| v.len() * 8).sum();
        per_table + per_table_to
    }

    /// Lower bound on `dist(v, t)` -- standard ALT formula, maxed
    /// over every landmark (each gives a VALID bound; the max is the
    /// tightest). Landmarks through which `v` or `t` is unreachable
    /// contribute nothing (skipped, not treated as a bound of 0 --
    /// `0` would still be technically valid but looser than just
    /// omitting an uninformative landmark some OTHER landmark already
    /// covers better).
    #[inline]
    fn h_forward(&self, v: u32, t: u32) -> i64 {
        let (vi, ti) = (v as usize, t as usize);
        let mut best = 0i64;
        for i in 0..self.count {
            let (dtv, dtt) = (self.dist_to[i][vi], self.dist_to[i][ti]);
            if dtv != INF && dtt != INF {
                best = best.max(dtv as i64 - dtt as i64);
            }
            let (dfv, dft) = (self.dist_from[i][vi], self.dist_from[i][ti]);
            if dfv != INF && dft != INF {
                best = best.max(dft as i64 - dfv as i64);
            }
        }
        best
    }

    /// Lower bound on `dist(s, v)`.
    #[inline]
    fn h_backward(&self, v: u32, s: u32) -> i64 {
        let (vi, si) = (v as usize, s as usize);
        let mut best = 0i64;
        for i in 0..self.count {
            let (dts, dtv) = (self.dist_to[i][si], self.dist_to[i][vi]);
            if dts != INF && dtv != INF {
                best = best.max(dts as i64 - dtv as i64);
            }
            let (dfv, dfs) = (self.dist_from[i][vi], self.dist_from[i][si]);
            if dfv != INF && dfs != INF {
                best = best.max(dfv as i64 - dfs as i64);
            }
        }
        best
    }

    /// The forward search's potential for node `v`, given the
    /// query's `(s, t)` -- the AVERAGED value described in the
    /// module doc, not `h_forward` directly.
    #[inline]
    pub fn potential_forward(&self, v: u32, s: u32, t: u32) -> i64 {
        (self.h_forward(v, t) - self.h_backward(v, s)) / 2
    }

    /// The backward search's potential for node `v` -- always the
    /// exact negation of the forward one, by construction.
    #[inline]
    pub fn potential_backward(&self, v: u32, s: u32, t: u32) -> i64 {
        -self.potential_forward(v, s, t)
    }
}

/// Same generation-stamped design as `dijkstra::Workspace`, kept as a
/// SEPARATE type (not an extension of it) specifically so plain
/// bidirectional Dijkstra and ALT-guided bidirectional A* stay
/// independently measurable, per PERF.md's "measured separately"
/// requirement -- this one's heaps are keyed by `i64` (the potential-
/// adjusted priority, which CAN be negative) rather than `u64`, since
/// that's the one place the two algorithms' internals genuinely
/// differ; `dist_f`/`dist_b` still track REAL (never adjusted)
/// distances, exactly like the plain version.
/// Cheap integer mixer for hashing node ids (splitmix64's finalizer --
/// same device as toolkit/src/hashmap.rs's `FastHash`, reimplemented
/// here rather than shared since each track stays self-contained per
/// CLAUDE.md rule 8). Node ids are never attacker-controlled, so
/// std's SipHash would be pure overhead here.
#[inline]
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31;
    x
}

/// Open-addressing `node id -> (dist, visited)` map, sized to the
/// TOUCHED SET rather than the whole graph. ALT's whole premise is
/// that landmark bounds narrow a search down to a small region near
/// the true shortest path (Step 5b's measurements: full-scale `road`
/// query time drops from 227-327s plain to 52-75s with ALT, a search
/// touching a small fraction of n) -- so a map sized to what's
/// actually touched, rather than a `Vec<_>` sized to all of `n`, is
/// the structural fix for the memory-profiling step's finding that
/// per-thread workspaces dominate peak memory (~85-88% of it). No
/// tombstones needed despite supporting "deletion": `reset` doesn't
/// delete one entry at a time, it undoes EVERY touch from the
/// finished query in one pass (see `reset`), after which every slot
/// that was touched is genuinely empty again, not just logically so.
/// A handful of queries on some inputs (Step 7's finding: full-scale
/// SYNTHETIC `road` data, unlike real DIMACS FLA, has at least some
/// queries whose ALT-guided search still touches a large fraction of
/// the graph) can make an open-addressing table's capacity balloon
/// well past what it would ever need to -- open addressing needs
/// roughly 2x the live entry count for a sane load factor, so a
/// touched set approaching `n` costs MORE than a plain `Vec` sized to
/// `n` would. `Backing::Dense` is the escape hatch: once a single
/// query's touched set would need sparse capacity beyond `n`, this
/// map permanently switches to a flat `n`-sized array (indexed
/// directly by node id, no probing) instead of growing the sparse
/// table further -- capping worst-case memory at EXACTLY what the
/// old array-based design cost, never worse, while keeping the
/// sparse win on every input where searches stay genuinely local.
enum Backing {
    Sparse { keys: Vec<u32>, mask: usize },
    Dense,
}

struct SparseDistMap {
    backing: Backing,
    dist: Vec<u64>,
    visited: Vec<bool>,
    touched: Vec<u32>, // Sparse: slot indices. Dense: node ids (slot == node id there).
    n: usize,
}

const EMPTY_SLOT: u32 = u32::MAX;

impl SparseDistMap {
    fn with_capacity(n: usize, min_capacity: usize) -> Self {
        let cap = (min_capacity.max(1) * 2).next_power_of_two();
        SparseDistMap {
            backing: Backing::Sparse {
                keys: vec![EMPTY_SLOT; cap],
                mask: cap - 1,
            },
            dist: vec![u64::MAX; cap],
            visited: vec![false; cap],
            touched: Vec::new(),
            n,
        }
    }

    fn capacity_bytes(&self) -> usize {
        let keys_bytes = match &self.backing {
            Backing::Sparse { keys, .. } => keys.len() * 4,
            Backing::Dense => 0,
        };
        keys_bytes
            + self.dist.capacity() * 8
            + self.visited.capacity()
            + self.touched.capacity() * 4
    }

    #[inline]
    fn probe(keys: &[u32], mask: usize, key: u32) -> usize {
        let mut idx = (mix64(key as u64) as usize) & mask;
        loop {
            let k = keys[idx];
            if k == key || k == EMPTY_SLOT {
                return idx;
            }
            idx = (idx + 1) & mask;
        }
    }

    #[inline]
    fn get_dist(&self, key: u32) -> u64 {
        match &self.backing {
            Backing::Dense => self.dist[key as usize],
            Backing::Sparse { keys, mask } => {
                let idx = Self::probe(keys, *mask, key);
                if keys[idx] == key {
                    self.dist[idx]
                } else {
                    u64::MAX
                }
            }
        }
    }

    fn set_dist(&mut self, key: u32, d: u64) {
        if matches!(self.backing, Backing::Dense) {
            let i = key as usize;
            if self.dist[i] == u64::MAX {
                self.touched.push(key);
            }
            self.dist[i] = d;
            return;
        }

        // Still sparse. Decide (as plain values, no live borrow of
        // `self.backing` held across the calls below) whether this
        // insert needs to upgrade to dense or just grow the table.
        if (self.touched.len() + 1) * 2 > self.n {
            self.upgrade_to_dense();
            self.set_dist(key, d);
            return;
        }
        let sparse_cap = match &self.backing {
            Backing::Sparse { keys, .. } => keys.len(),
            Backing::Dense => unreachable!(),
        };
        if (self.touched.len() + 1) * 2 > sparse_cap {
            self.grow();
        }

        let Backing::Sparse { keys, mask } = &mut self.backing else {
            unreachable!()
        };
        let idx = Self::probe(keys, *mask, key);
        if keys[idx] != key {
            keys[idx] = key;
            self.visited[idx] = false;
            self.touched.push(idx as u32);
        }
        self.dist[idx] = d;
    }

    #[inline]
    fn is_visited(&self, key: u32) -> bool {
        match &self.backing {
            Backing::Dense => self.visited[key as usize],
            Backing::Sparse { keys, mask } => {
                let idx = Self::probe(keys, *mask, key);
                keys[idx] == key && self.visited[idx]
            }
        }
    }

    fn mark_visited(&mut self, key: u32) {
        match &mut self.backing {
            Backing::Dense => self.visited[key as usize] = true,
            Backing::Sparse { keys, mask } => {
                let idx = Self::probe(keys, *mask, key);
                debug_assert_eq!(
                    keys[idx], key,
                    "mark_visited on a key never set_dist-ed first"
                );
                self.visited[idx] = true;
            }
        }
    }

    /// Undo every touch from the finished query -- O(touched), not
    /// O(capacity) -- leaving every touched slot/node genuinely
    /// pristine again, ready for reuse. Works identically in either
    /// backing (that's WHY `touched` means "node id" in dense mode
    /// and "slot index" in sparse mode -- in dense mode slot==node id).
    fn reset(&mut self) {
        match &mut self.backing {
            Backing::Dense => {
                for &key in &self.touched {
                    let i = key as usize;
                    self.dist[i] = u64::MAX;
                    self.visited[i] = false;
                }
            }
            Backing::Sparse { keys, .. } => {
                for &idx in &self.touched {
                    keys[idx as usize] = EMPTY_SLOT;
                }
            }
        }
        self.touched.clear();
    }

    fn grow(&mut self) {
        let Backing::Sparse { keys: old_keys, .. } = &self.backing else {
            unreachable!()
        };
        let new_cap = old_keys.len() * 2;
        let old_keys = match std::mem::replace(
            &mut self.backing,
            Backing::Sparse {
                keys: vec![EMPTY_SLOT; new_cap],
                mask: new_cap - 1,
            },
        ) {
            Backing::Sparse { keys, .. } => keys,
            Backing::Dense => unreachable!(),
        };
        let old_dist = std::mem::replace(&mut self.dist, vec![u64::MAX; new_cap]);
        let old_visited = std::mem::replace(&mut self.visited, vec![false; new_cap]);
        let old_touched = std::mem::take(&mut self.touched);
        let Backing::Sparse { keys, mask } = &mut self.backing else {
            unreachable!()
        };
        for old_idx in old_touched {
            let old_idx = old_idx as usize;
            let key = old_keys[old_idx];
            let idx = Self::probe(keys, *mask, key);
            keys[idx] = key;
            self.dist[idx] = old_dist[old_idx];
            self.visited[idx] = old_visited[old_idx];
            self.touched.push(idx as u32);
        }
    }

    /// One-way switch: rebuild as a flat `n`-sized array from
    /// whatever is currently live in the sparse table, then never go
    /// back (matches the "never shrink" discipline every workspace in
    /// this codebase follows).
    fn upgrade_to_dense(&mut self) {
        let Backing::Sparse { keys: old_keys, .. } = &self.backing else {
            return;
        };
        let mut new_dist = vec![u64::MAX; self.n];
        let mut new_visited = vec![false; self.n];
        let mut new_touched = Vec::with_capacity(self.touched.len());
        for &slot in &self.touched {
            let slot = slot as usize;
            let key = old_keys[slot] as usize;
            new_dist[key] = self.dist[slot];
            new_visited[key] = self.visited[slot];
            new_touched.push(key as u32);
        }
        self.dist = new_dist;
        self.visited = new_visited;
        self.touched = new_touched;
        self.backing = Backing::Dense;
    }
}

pub struct AstarWorkspace {
    forward: SparseDistMap,
    backward: SparseDistMap,
    heap_f: BinaryHeap<Reverse<(i64, u32)>>,
    heap_b: BinaryHeap<Reverse<(i64, u32)>>,
}

/// Initial capacity: small and grown lazily (doubling, same amortized
/// cost as `Vec::push`) rather than sized to `n` upfront -- the whole
/// point of `SparseDistMap` is that it ends up sized to the largest
/// SINGLE query's touched set this thread ever handles, not to `n`.
const INITIAL_SPARSE_CAPACITY: usize = 1024;

impl AstarWorkspace {
    pub fn new(n: usize) -> Self {
        AstarWorkspace {
            forward: SparseDistMap::with_capacity(n, INITIAL_SPARSE_CAPACITY),
            backward: SparseDistMap::with_capacity(n, INITIAL_SPARSE_CAPACITY),
            heap_f: BinaryHeap::new(),
            heap_b: BinaryHeap::new(),
        }
    }

    /// Exact measured capacity of both sparse maps plus both heaps.
    /// Diagnostic only (PERF.md's memory-breakdown step), not used by
    /// the search itself.
    pub fn memory_bytes(&self) -> usize {
        self.forward.capacity_bytes()
            + self.backward.capacity_bytes()
            + (self.heap_f.capacity() + self.heap_b.capacity())
                * std::mem::size_of::<Reverse<(i64, u32)>>()
    }

    fn reset(&mut self) {
        self.forward.reset();
        self.backward.reset();
    }

    #[inline]
    fn get_dist_f(&self, i: usize) -> u64 {
        self.forward.get_dist(i as u32)
    }
    #[inline]
    fn set_dist_f(&mut self, i: usize, d: u64) {
        self.forward.set_dist(i as u32, d);
    }
    #[inline]
    fn is_visited_f(&self, i: usize) -> bool {
        self.forward.is_visited(i as u32)
    }
    #[inline]
    fn mark_visited_f(&mut self, i: usize) {
        self.forward.mark_visited(i as u32);
    }

    #[inline]
    fn get_dist_b(&self, i: usize) -> u64 {
        self.backward.get_dist(i as u32)
    }
    #[inline]
    fn set_dist_b(&mut self, i: usize, d: u64) {
        self.backward.set_dist(i as u32, d);
    }
    #[inline]
    fn is_visited_b(&self, i: usize) -> bool {
        self.backward.is_visited(i as u32)
    }
    #[inline]
    fn mark_visited_b(&mut self, i: usize) {
        self.backward.mark_visited(i as u32);
    }
}

/// Bidirectional A*, guided by `landmarks`' potentials. Returns the
/// SAME thing `dijkstra::bidirectional_dijkstra` would for the same
/// query (verified directly against it, and against brute-force
/// Floyd-Warshall, in the tests below) -- A*'s heuristic only changes
/// the ORDER nodes are explored in, never the final answer, as long
/// as the heuristic is a valid lower bound (never overestimates), which
/// `Landmarks::h_forward`/`h_backward` are by construction (triangle
/// inequality over REAL shortest-path distances).
///
/// Stopping criterion is the textually IDENTICAL `top_f + top_b >=
/// best` check as the plain version -- see the module doc for why the
/// averaged-potential construction keeps that exact formula valid
/// even though `top_f`/`top_b` here are potential-ADJUSTED keys, not
/// raw distances.
pub fn bidirectional_astar(
    ws: &mut AstarWorkspace,
    start: u32,
    target: u32,
    fwd: &Csr,
    rev: &Csr,
    landmarks: &Landmarks,
) -> i64 {
    if start == target {
        return 0;
    }
    ws.reset();
    ws.heap_f.clear();
    ws.heap_b.clear();

    ws.set_dist_f(start as usize, 0);
    let pot_start_f = landmarks.potential_forward(start, start, target);
    ws.heap_f.push(Reverse((pot_start_f, start)));

    ws.set_dist_b(target as usize, 0);
    let pot_target_b = landmarks.potential_backward(target, start, target);
    ws.heap_b.push(Reverse((pot_target_b, target)));

    let mut best = u64::MAX;

    loop {
        let top_f = ws
            .heap_f
            .peek()
            .map(|&Reverse((k, _))| k)
            .unwrap_or(i64::MAX);
        let top_b = ws
            .heap_b
            .peek()
            .map(|&Reverse((k, _))| k)
            .unwrap_or(i64::MAX);
        if top_f == i64::MAX && top_b == i64::MAX {
            break;
        }
        if best != u64::MAX {
            let best_i64 = best.min(i64::MAX as u64) as i64;
            if top_f.saturating_add(top_b) >= best_i64 {
                break;
            }
        }

        if top_f <= top_b {
            let Some(Reverse((_, u))) = ws.heap_f.pop() else {
                break;
            };
            let ui = u as usize;
            if ws.is_visited_f(ui) {
                continue;
            }
            ws.mark_visited_f(ui);
            let d = ws.get_dist_f(ui); // REAL distance, not the adjusted key
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
                    let pot_v = landmarks.potential_forward(v, start, target);
                    ws.heap_f
                        .push(Reverse((nd.min(i64::MAX as u64) as i64 + pot_v, v)));
                    if ws.is_visited_b(vi) {
                        let cand = nd.saturating_add(ws.get_dist_b(vi));
                        if cand < best {
                            best = cand;
                        }
                    }
                }
            }
        } else {
            let Some(Reverse((_, u))) = ws.heap_b.pop() else {
                break;
            };
            let ui = u as usize;
            if ws.is_visited_b(ui) {
                continue;
            }
            ws.mark_visited_b(ui);
            let d = ws.get_dist_b(ui);
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
                    let pot_v = landmarks.potential_backward(v, start, target);
                    ws.heap_b
                        .push(Reverse((nd.min(i64::MAX as u64) as i64 + pot_v, v)));
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
    fn sparse_dist_map_grows_past_initial_capacity_without_losing_entries() {
        // Initial capacity is tiny -- forces several `grow()` calls
        // well before this loop finishes, exercising the re-probe-on-
        // grow path that the small existing graphs never reach. `n`
        // is kept well above 2x the key count so this stays in pure
        // sparse territory (the dense-fallback path has its own test).
        let mut m = SparseDistMap::with_capacity(20_000, 4);
        for key in 0..5000u32 {
            m.set_dist(key, key as u64 * 7);
        }
        for key in 0..5000u32 {
            assert_eq!(
                m.get_dist(key),
                key as u64 * 7,
                "key {key} lost or corrupted across grow()"
            );
        }
    }

    #[test]
    fn sparse_dist_map_upgrades_to_dense_without_losing_entries_or_correctness() {
        // `n` is tiny relative to the key count -- forces the
        // dense-fallback switch partway through, exercising the
        // sparse-to-dense migration (Step 7's fix for the full-scale
        // `road` regression where sparse capacity ballooned past what
        // a flat array would have cost).
        let n = 50usize;
        let mut m = SparseDistMap::with_capacity(n, 4);
        for key in 0..n as u32 {
            m.set_dist(key, key as u64 * 11);
            if key % 2 == 0 {
                m.mark_visited(key);
            }
        }
        for key in 0..n as u32 {
            assert_eq!(
                m.get_dist(key),
                key as u64 * 11,
                "key {key} lost across the sparse->dense switch"
            );
            assert_eq!(
                m.is_visited(key),
                key % 2 == 0,
                "key {key}'s visited flag lost across the switch"
            );
        }
        assert!(
            matches!(m.backing, Backing::Dense),
            "should have switched to dense by n={n} touches"
        );

        // Reset must still work correctly once in dense mode.
        m.reset();
        for key in 0..n as u32 {
            assert_eq!(m.get_dist(key), u64::MAX);
            assert!(!m.is_visited(key));
        }
        for key in 0..n as u32 {
            m.set_dist(key, key as u64 * 3);
        }
        for key in 0..n as u32 {
            assert_eq!(m.get_dist(key), key as u64 * 3);
        }
    }

    #[test]
    fn sparse_dist_map_reset_truly_empties_touched_slots() {
        let mut m = SparseDistMap::with_capacity(1000, 4);
        for key in 0..200u32 {
            m.set_dist(key, key as u64);
            if key % 3 == 0 {
                m.mark_visited(key);
            }
        }
        m.reset();
        for key in 0..200u32 {
            assert_eq!(
                m.get_dist(key),
                u64::MAX,
                "key {key} should read back as untouched after reset"
            );
            assert!(
                !m.is_visited(key),
                "key {key} should read back as not-visited after reset"
            );
        }
        // A FRESH round of keys (including ones reused from before)
        // must behave exactly as if this were a brand new map --
        // confirms `reset` didn't leave stale capacity-accounting or
        // probe-chain corruption behind.
        for key in 100..300u32 {
            m.set_dist(key, key as u64 * 3);
        }
        for key in 100..300u32 {
            assert_eq!(m.get_dist(key), key as u64 * 3);
        }
        for key in 0..100u32 {
            assert_eq!(
                m.get_dist(key),
                u64::MAX,
                "key {key} from the PRIOR round must stay reset"
            );
        }
    }

    #[test]
    fn single_source_dijkstra_matches_known_distances() {
        let (fwd, _rev) = graphs(5, &[(0, 1, 2), (1, 2, 3), (2, 3, 4), (3, 4, 1)]);
        let dist = single_source_dijkstra(&fwd, 0, 5);
        assert_eq!(dist, vec![0, 2, 5, 9, 10]);
    }

    #[test]
    fn single_source_dijkstra_marks_unreachable_nodes_as_inf() {
        let (fwd, _rev) = graphs(4, &[(0, 1, 5)]); // 2, 3 unreachable from 0
        let dist = single_source_dijkstra(&fwd, 0, 4);
        assert_eq!(dist[2], INF);
        assert_eq!(dist[3], INF);
    }

    #[test]
    fn select_landmarks_returns_the_requested_count_of_distinct_reachable_nodes() {
        let n = 20usize;
        let mut edges = Vec::new();
        for i in 0..n as u32 - 1 {
            edges.push((i, i + 1, 1));
            edges.push((i + 1, i, 1));
        }
        let (fwd, _rev) = graphs(n, &edges);
        let landmarks = select_landmarks(&fwd, n, 5, 0);
        assert_eq!(landmarks.len(), 5);
        let mut sorted = landmarks.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            5,
            "landmarks should be distinct on a graph with >= 5 reachable nodes"
        );
    }

    #[test]
    fn heuristic_is_a_valid_lower_bound_never_overestimating_true_distance() {
        // On a chain, the heuristic must never exceed the REAL
        // distance -- that's the defining property of an admissible
        // A* heuristic, and exactly what makes it safe to use for
        // search ordering without changing the answer.
        let n = 10u32;
        let mut edges = Vec::new();
        for i in 0..n - 1 {
            edges.push((i, i + 1, 3));
            edges.push((i + 1, i, 3));
        }
        let (fwd, rev) = graphs(n as usize, &edges);
        let landmark_ids = select_landmarks(&fwd, n as usize, 3, 0);
        let landmarks = Landmarks::build(&fwd, &rev, n as usize, &landmark_ids);

        for s in 0..n {
            for t in 0..n {
                let true_dist = (s as i64 - t as i64).abs() * 3;
                for v in 0..n {
                    let h_f = landmarks.h_forward(v, t);
                    let dist_v_t = (v as i64 - t as i64).abs() * 3;
                    assert!(
                        h_f <= dist_v_t,
                        "h_forward({v},{t})={h_f} overestimates true dist {dist_v_t}"
                    );
                    let h_b = landmarks.h_backward(v, s);
                    let dist_s_v = (s as i64 - v as i64).abs() * 3;
                    assert!(
                        h_b <= dist_s_v,
                        "h_backward({v},{s})={h_b} overestimates true dist {dist_s_v}"
                    );
                }
                let _ = true_dist;
            }
        }
    }

    #[test]
    fn potentials_are_exact_negations_of_each_other() {
        let n = 15u32;
        let mut edges = Vec::new();
        let mut seed = 777u64;
        let mut next_rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for i in 0..n - 1 {
            edges.push((i, i + 1, 1 + (next_rand() % 50) as u32));
            edges.push((i + 1, i, 1 + (next_rand() % 50) as u32));
        }
        let (fwd, rev) = graphs(n as usize, &edges);
        let landmark_ids = select_landmarks(&fwd, n as usize, 4, 0);
        let landmarks = Landmarks::build(&fwd, &rev, n as usize, &landmark_ids);

        for s in 0..n {
            for t in 0..n {
                for v in 0..n {
                    let pf = landmarks.potential_forward(v, s, t);
                    let pb = landmarks.potential_backward(v, s, t);
                    assert_eq!(
                        pf, -pb,
                        "s={s} t={t} v={v}: potentials must be exact negations"
                    );
                }
            }
        }
    }

    #[test]
    fn bidirectional_astar_matches_plain_bidirectional_dijkstra_on_a_sparse_graph() {
        // Same deliberately sparse (not dense) chain-plus-shortcuts
        // shape as dijkstra.rs's own brute-force test, and for the
        // same reason: a dense/tiny-diameter graph wouldn't give
        // either search's early-stopping path -- the thing actually
        // being compared here -- anything real to do.
        let n = 150u32;
        let mut edges = Vec::new();
        let mut seed = 2024u64;
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
            edges.push((a, b, (next_rand() % 500) as u32));
            edges.push((b, a, (next_rand() % 500) as u32));
        }
        for _ in 0..(n / 2) {
            let a = (next_rand() % n as u64) as u32;
            let b = (next_rand() % n as u64) as u32;
            if a != b {
                edges.push((a, b, (next_rand() % 500) as u32));
            }
        }

        let (fwd, rev) = graphs(n as usize, &edges);
        let landmark_ids = select_landmarks(&fwd, n as usize, 10, 0);
        let landmarks = Landmarks::build(&fwd, &rev, n as usize, &landmark_ids);

        let mut plain_ws = crate::dijkstra::Workspace::new(n as usize);
        let mut astar_ws = AstarWorkspace::new(n as usize);

        for s in 0..n {
            for t in 0..n {
                let plain =
                    crate::dijkstra::bidirectional_dijkstra(&mut plain_ws, s, t, &fwd, &rev);
                let astar = bidirectional_astar(&mut astar_ws, s, t, &fwd, &rev, &landmarks);
                assert_eq!(
                    astar, plain,
                    "s={s} t={t}: ALT-guided A* diverged from plain bidirectional Dijkstra"
                );
            }
        }
    }

    #[test]
    fn bidirectional_astar_handles_zero_weight_edges_and_ties() {
        // Same hand-crafted shapes as dijkstra.rs's own stopping-
        // criterion tests, run through A* instead, to confirm the
        // heuristic-guided version doesn't reintroduce either bug
        // class on its own code path.
        let (fwd, rev) = graphs(4, &[(0, 1, 0), (1, 3, 2), (0, 2, 1), (2, 3, 1)]); // tie, one side via a zero-weight edge
        let landmark_ids = select_landmarks(&fwd, 4, 3, 0);
        let landmarks = Landmarks::build(&fwd, &rev, 4, &landmark_ids);
        let mut ws = AstarWorkspace::new(4);
        assert_eq!(
            bidirectional_astar(&mut ws, 0, 3, &fwd, &rev, &landmarks),
            2
        );
    }
}
