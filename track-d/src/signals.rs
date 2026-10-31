//! Cheap structural signals, computed once during preprocessing from
//! the already-built forward CSR, that AUTOMATICALLY decide whether
//! node reordering (`reorder.rs`) and ALT (`alt.rs`) are worth their
//! preprocessing cost on THIS input -- not by recognizing which
//! generator produced it, but by measuring two general graph
//! properties PERF.md's Step 5a/5b found to predict the win/loss:
//!
//!   - id-locality: does the input already correlate node ids with
//!     graph structure (nothing for reordering to fix), or not
//!     (reordering has real room to help)?
//!   - reverse-edge weight symmetry: does the graph have the
//!     metric-like structure (mostly-bidirectional edges with closely
//!     matched forward/backward weights) ALT's landmark bounds need
//!     to be tight, or is it structurally closer to independent
//!     random edge weights (where ALT's preprocessing buys nothing
//!     and actively regresses query time)?
//!
//! Both signals are a single extra O(m * avg_degree) pass over the
//! forward CSR (avg_degree is small -- ~2.9 on the event's own
//! road-like signature, at most a handful even on `grid` mode's dense
//! case), timed and reported like every other preprocessing step in
//! this file. See PERF.md's "adaptive algorithm selection" section
//! for the calibration data (measured on `grid`, `random`, synthetic
//! `road`, real DIMACS FLA, DIMACS FLA with shuffled ids, and `random`
//! pre-reordered) behind the threshold constants below.

use crate::csr::Csr;

/// Below this, id-locality is "good enough" that reordering has
/// nothing worth fixing (real DIMACS FLA measured ~0.004; `grid` mode
/// measured ~0.0003). Above it, ids carry no usable relationship to
/// graph structure (synthetic `road` and `random` both measured
/// ~0.29-0.33, close to the ~1/3 expected value for two independent
/// uniform ids). The gap between these two clusters is roughly two
/// orders of magnitude -- 0.05 sits with wide margin on both sides.
pub const LOCALITY_POOR_THRESHOLD: f64 = 0.05;

/// A directed edge needs a reverse counterpart to even be assessable
/// for weight symmetry. Below this fraction of edges having one at
/// all, the symmetry signal below would be computed from too few
/// samples to trust either way.
pub const REVERSE_PAIR_GATE: f64 = 0.3;

/// Among edges that DO have a reverse counterpart, the fraction whose
/// two weights fall within +/-20% of each other. Road-like data
/// (synthetic `road`'s explicit +/-10% asymmetry, real DIMACS FLA's
/// symmetric distances) measured ~0.95-1.0. Independent-random-weight
/// data (`grid`, `random`) measured ~0.00-0.20 (draws from the full
/// u32 range, so two independent draws landing within 20% of each
/// other is rare). 0.7 sits with wide margin in the gap between
/// those two clusters.
///
/// Used as a NECESSARY (not sufficient) condition for ALT alongside
/// the query-adaptive touched-count sampling in `main.rs` -- query-
/// adaptive sampling alone is NOT enough, confirmed by a real
/// regression: `random` mode's small-world shortcuts gave a touched-
/// count sample well above the threshold (ALT "looked" worth it from
/// query hardness alone), but `random`'s independent-random edge
/// weights make ALT's landmark bounds nearly useless -- building them
/// anyway cost 156s of wasted preprocessing on top of query time that
/// was ALREADY fast without it. Query-adaptive sampling answers "are
/// the queries hard"; this signal answers "would landmark bounds even
/// help if they are" -- both must hold.
pub const WEIGHT_SYMMETRY_THRESHOLD: f64 = 0.7;

/// Median `|u - v| / n` over every directed edge `u -> v` in `fwd`.
/// Near 0: neighbor ids are already close together (reordering would
/// just add overhead for no benefit -- Step 5a's real-DIMACS-FLA
/// finding). Near 1/3 (the expected value for two independent uniform
/// ids in `[0, n)`) or higher: ids carry no relationship to graph
/// structure (reordering has real locality to create).
pub fn locality_score(fwd: &Csr, n: usize) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let mut gaps: Vec<u32> = Vec::with_capacity(fwd.adj_node.len());
    for u in 0..n as u32 {
        let (neighbors, _) = fwd.neighbors(u);
        for &v in neighbors {
            gaps.push(u.abs_diff(v));
        }
    }
    if gaps.is_empty() {
        return 0.0;
    }
    let mid = gaps.len() / 2;
    let (_, &mut median, _) = gaps.select_nth_unstable(mid);
    median as f64 / n as f64
}

/// `(reverse_pair_fraction, weight_symmetry_fraction)`. Used as a
/// NECESSARY, not sufficient, condition for ALT alongside `main.rs`'s
/// query-adaptive touched-count sampling (see `WEIGHT_SYMMETRY_
/// THRESHOLD`'s doc comment for the real regression that showed
/// query-adaptive sampling ALONE isn't enough).
///
/// `reverse_pair_fraction`: among every directed edge `u -> v`, the
/// fraction that also have a `v -> u` edge present (too few of these
/// and the symmetry fraction below would be noise).
///
/// `weight_symmetry_fraction`: among JUST the edges with a reverse
/// counterpart, the fraction whose two weights are within +/-20% of
/// each other. Zero-weight pairs (`0` and `0`) count as matched.
pub fn reverse_edge_signals(fwd: &Csr, n: usize) -> (f64, f64) {
    let mut total: u64 = 0;
    let mut has_reverse: u64 = 0;
    let mut within_tolerance: u64 = 0;
    for u in 0..n as u32 {
        let (neighbors, weights) = fwd.neighbors(u);
        for (&v, &w) in neighbors.iter().zip(weights) {
            total += 1;
            let (rev_neighbors, rev_weights) = fwd.neighbors(v);
            if let Some(&w2) = rev_neighbors
                .iter()
                .zip(rev_weights)
                .find(|&(&ru, _)| ru == u)
                .map(|(_, rw)| rw)
            {
                has_reverse += 1;
                let (lo, hi) = (w.min(w2), w.max(w2));
                let matched = hi == 0 || (hi - lo) as f64 / hi as f64 <= 0.2;
                if matched {
                    within_tolerance += 1;
                }
            }
        }
    }
    if total == 0 {
        return (0.0, 0.0);
    }
    let reverse_pair_fraction = has_reverse as f64 / total as f64;
    let weight_symmetry_fraction = if has_reverse == 0 {
        0.0
    } else {
        within_tolerance as f64 / has_reverse as f64
    };
    (reverse_pair_fraction, weight_symmetry_fraction)
}

/// Reorder's decision stays fully graph-signal-based -- split out
/// from `locality_score` so the policy itself is one small, easily-
/// reviewed place.
pub fn should_reorder(locality: f64) -> bool {
    locality > LOCALITY_POOR_THRESHOLD
}

/// Structural prerequisite for ALT: does the graph even have the
/// metric-like structure landmark bounds need? This is NECESSARY but
/// not SUFFICIENT -- `main.rs` ANDs this with the query-adaptive
/// touched-count sample before actually building landmarks (see
/// `WEIGHT_SYMMETRY_THRESHOLD`'s doc comment for why both checks are
/// needed, not either alone).
pub fn graph_supports_alt(reverse_pair_fraction: f64, weight_symmetry_fraction: f64) -> bool {
    reverse_pair_fraction >= REVERSE_PAIR_GATE
        && weight_symmetry_fraction >= WEIGHT_SYMMETRY_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csr::build_forward;

    fn fwd_of(n: usize, edges: &[(u32, u32, u32)]) -> Csr {
        let us = edges.iter().map(|&(u, _, _)| u).collect();
        let vs = edges.iter().map(|&(_, v, _)| v).collect();
        let ws = edges.iter().map(|&(_, _, w)| w).collect();
        let chunks = vec![(us, vs, ws)];
        build_forward(n, edges.len(), &chunks)
    }

    #[test]
    fn locality_score_is_near_zero_for_a_grid_like_graph() {
        // id = row*cols+col, every edge connects adjacent cells --
        // |u - v| is always 1 (same row) or `cols` (next row), tiny
        // compared to n for any graph bigger than a handful of nodes.
        let cols = 100u32;
        let mut edges = Vec::new();
        for r in 0..20u32 {
            for c in 0..cols {
                let u = r * cols + c;
                if c + 1 < cols {
                    edges.push((u, u + 1, 5));
                }
                if r + 1 < 20 {
                    edges.push((u, u + cols, 5));
                }
            }
        }
        let n = 20 * cols as usize;
        let fwd = fwd_of(n, &edges);
        let score = locality_score(&fwd, n);
        assert!(
            score < LOCALITY_POOR_THRESHOLD,
            "grid-like locality score {score} should be well under the poor threshold"
        );
    }

    #[test]
    fn locality_score_is_high_for_a_fully_shuffled_chain() {
        // A permuted chain: edge endpoints are effectively uniform
        // random ids in [0, n) -- the median gap should land near the
        // ~1/3 expected value for two independent uniform draws, well
        // above the poor threshold.
        let n = 2000u32;
        let mut perm: Vec<u32> = (0..n).collect();
        let mut seed = 12345u64;
        let mut next_rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for i in (1..n as usize).rev() {
            let j = (next_rand() % (i as u64 + 1)) as usize;
            perm.swap(i, j);
        }
        let edges: Vec<(u32, u32, u32)> = (0..n as usize - 1)
            .map(|i| (perm[i], perm[i + 1], 5))
            .collect();
        let fwd = fwd_of(n as usize, &edges);
        let score = locality_score(&fwd, n as usize);
        assert!(
            score > LOCALITY_POOR_THRESHOLD,
            "shuffled-chain locality score {score} should clear the poor threshold"
        );
    }

    #[test]
    fn weight_symmetry_is_high_when_reverse_weights_closely_match() {
        let edges = [
            (0u32, 1u32, 100u32),
            (1, 0, 95),
            (1, 2, 200),
            (2, 1, 210),
            (2, 3, 50),
            (3, 2, 48),
        ];
        let fwd = fwd_of(4, &edges);
        let (reverse_pair_fraction, weight_symmetry_fraction) = reverse_edge_signals(&fwd, 4);
        assert_eq!(reverse_pair_fraction, 1.0);
        assert_eq!(weight_symmetry_fraction, 1.0);
        assert!(graph_supports_alt(
            reverse_pair_fraction,
            weight_symmetry_fraction
        ));
    }

    #[test]
    fn weight_symmetry_is_low_when_reverse_weights_are_independent() {
        let edges = [
            (0u32, 1u32, 100u32),
            (1, 0, 9000u32),
            (1, 2, 50u32),
            (2, 1, 1u32),
        ];
        let fwd = fwd_of(3, &edges);
        let (reverse_pair_fraction, weight_symmetry_fraction) = reverse_edge_signals(&fwd, 3);
        assert_eq!(reverse_pair_fraction, 1.0);
        assert_eq!(weight_symmetry_fraction, 0.0);
        assert!(!graph_supports_alt(
            reverse_pair_fraction,
            weight_symmetry_fraction
        ));
    }

    #[test]
    fn zero_weight_reverse_pairs_count_as_matched() {
        let edges = [(0u32, 1u32, 0u32), (1, 0, 0u32)];
        let fwd = fwd_of(2, &edges);
        let (_, weight_symmetry_fraction) = reverse_edge_signals(&fwd, 2);
        assert_eq!(weight_symmetry_fraction, 1.0);
    }

    #[test]
    fn few_reverse_pairs_fails_the_gate_regardless_of_symmetry() {
        // Only 2 of 8 directed edges have a reverse -- below
        // REVERSE_PAIR_GATE, so ALT's structural prerequisite should
        // be rejected even though that one matched pair is exact.
        let edges = [
            (0u32, 1u32, 10u32),
            (1, 0, 10u32),
            (1, 2, 20u32),
            (2, 3, 30u32),
            (3, 4, 40u32),
            (4, 5, 50u32),
            (5, 6, 60u32),
            (6, 7, 70u32),
        ];
        let fwd = fwd_of(8, &edges);
        let (reverse_pair_fraction, weight_symmetry_fraction) = reverse_edge_signals(&fwd, 8);
        assert_eq!(reverse_pair_fraction, 0.25);
        assert_eq!(weight_symmetry_fraction, 1.0);
        assert!(!graph_supports_alt(
            reverse_pair_fraction,
            weight_symmetry_fraction
        ));
    }

    #[test]
    fn empty_graph_signals_are_well_defined() {
        let fwd = fwd_of(5, &[]);
        assert_eq!(locality_score(&fwd, 5), 0.0);
        assert_eq!(reverse_edge_signals(&fwd, 5), (0.0, 0.0));
    }
}
