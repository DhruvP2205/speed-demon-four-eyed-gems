//! Speed Demon 2026, Track D (Traverse). specs/d-traverse.md is the contract.
//!
//! Reads `graph.txt` (directed, weighted, possibly with parallel
//! edges, self-loops, duplicate lines and weight-0 edges) and
//! `queries.txt` (`s t` pairs), and writes `distances.txt`: the
//! shortest directed-path distance for each query, in input order, or
//! `-1` if `t` is unreachable from `s`.
//!
//! Phase breakdown from an earlier version (temporary instrumentation,
//! not shipped) found parse (~50ms, parallel, ratio ~8.8) and queries
//! (~390ms, parallel, ratio ~11.6) both parallelizing well, but the
//! ENTIRE rest of graph loading -- CSR build (~260ms), BFS reorder
//! (~135ms), remap (~115ms), has_in_edge (~8ms) -- 100% sequential,
//! together costing more than half the run. This version replaces
//! the CSR build (by far the largest piece) with a parallel two-pass
//! design that never stores the parsed edge triples at all (see
//! `src/csr.rs`'s `build_forward_and_in_edge`); `graph.txt` is now
//! mapped via `mmap` (`src/mmap_file.rs`) instead of streamed through
//! `pread_at`, since both passes need direct, repeated, parallel
//! access to the whole file. BFS reorder (and the paged `Workspace`
//! it pairs with) is still here: a direct A/B on this exact
//! rewritten build (identity vs. BFS-ordered node ids, measured, not
//! assumed) showed the query phase going from ~390ms to ~2.7s
//! WITHOUT it -- losing cache locality costs roughly 7x on the query
//! phase, dwarfing the sequential reorder+remap cost (~250ms
//! combined) it pays for. See PERF.md for the full numbers.

mod csr;
mod heap;
mod mmap_file;
mod parse;
mod reorder;
mod workspace;

use std::io::{BufWriter, Write};

use csr::{build_forward_and_in_edge, Csr};
use mmap_file::MappedFile;
use parse::{line_aligned_ranges, parse_query_pairs, parse_uint, skip_non_digits};
use reorder::{bfs_order, remap_bitmap, remap_csr};
use workspace::Workspace;

const MAX_THREADS: usize = 12;
const GRAPH_FILE: &str = "graph.txt";
const QUERIES_FILE: &str = "queries.txt";
const OUTPUT_FILE: &str = "distances.txt";

/// Read the `n m` header line directly off the mmap, returning the
/// parsed values and the byte offset right after the header's
/// trailing `\n` (where edges start).
fn read_header(data: &[u8]) -> (usize, usize, usize) {
    let i = skip_non_digits(data, 0);
    let (n, i) = parse_uint(data, i);
    let i = skip_non_digits(data, i);
    let (m, i) = parse_uint(data, i);
    let header_end = data[i..]
        .iter()
        .position(|&b| b == b'\n')
        .expect("header has no newline")
        + i
        + 1;
    (n as usize, m as usize, header_end)
}

/// Builds the forward CSR and `has_in_edge` bitmap in the input's own
/// node numbering, then renumbers both by BFS order. Returns the
/// renumbered CSR/bitmap plus `new_id_of_old`, so the caller can
/// translate queries through the same mapping.
fn load_graph(path: &str) -> (Csr, Vec<bool>, Vec<u32>) {
    let mapped = MappedFile::open(path, false).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let data = mapped.as_slice();
    let (n, m, header_end) = read_header(data);

    let ranges = line_aligned_ranges(data, header_end, MAX_THREADS);
    let (csr, has_in_edge) = build_forward_and_in_edge(n, m, data, &ranges);

    let new_id_of_old = bfs_order(n, &csr);
    let remapped_csr = remap_csr(&csr, &new_id_of_old);
    let remapped_has_in_edge = remap_bitmap(&has_in_edge, &new_id_of_old);
    (remapped_csr, remapped_has_in_edge, new_id_of_old)
}

/// Fresh single-source Dijkstra from `s`, stopping as soon as `t` is
/// popped (an early exit: `t`'s own out-edges are never relaxed).
/// `ws` is caller-owned scratch space, reset in O(1) on entry (see
/// `Workspace::reset`), so answering many queries on one thread never
/// reallocates or re-zeroes an O(n) array.
fn shortest_path(csr: &Csr, s: u32, t: u32, ws: &mut Workspace) -> Option<u64> {
    ws.reset();
    ws.set(s, 0);
    ws.heap.push(0, s);

    while let Some((d, u)) = ws.heap.pop() {
        if d > ws.get(u).expect("heap entry always has a dist") {
            continue; // stale: u was already settled at a shorter distance
        }
        if u == t {
            return Some(d);
        }
        let (neighbors, weights) = csr.neighbors(u);
        for (&v, &w) in neighbors.iter().zip(weights.iter()) {
            let nd = d + w as u64;
            let improves = match ws.get(v) {
                Some(existing) => nd < existing,
                None => true,
            };
            if improves {
                ws.set(v, nd);
                ws.heap.push(nd, v);
            }
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(String::as_str).unwrap_or("/data");
    let out_dir = args.get(2).map(String::as_str).unwrap_or("/out");

    let (csr, has_in_edge, new_id_of_old) = load_graph(&format!("{data_dir}/{GRAPH_FILE}"));
    let n = has_in_edge.len();

    let queries_path = format!("{data_dir}/{QUERIES_FILE}");
    let qmapped = MappedFile::open(&queries_path, true)
        .unwrap_or_else(|e| panic!("open {queries_path}: {e}"));
    // Node ids in the input are in the ORIGINAL numbering; the CSR and
    // bitmap above are already in BFS-order numbering, so every query
    // must be translated through the same mapping before it touches
    // either. The answer (a distance, or unreachable) is invariant
    // under renumbering, so no mapping back is needed for output.
    let queries: Vec<(u32, u32)> = parse_query_pairs(qmapped.as_slice())
        .into_iter()
        .map(|(s, t)| (new_id_of_old[s as usize], new_id_of_old[t as usize]))
        .collect();

    let mut results: Vec<Option<u64>> = vec![None; queries.len()];
    let thread_count = MAX_THREADS.min(queries.len().max(1));
    let chunk_ranges = even_chunks(queries.len(), thread_count);

    std::thread::scope(|scope| {
        let csr_ref = &csr;
        let has_in_edge_ref = &has_in_edge;
        let queries_ref = &queries;
        let results_ref = &mut results;
        let handles: Vec<_> = chunk_ranges
            .iter()
            .scan(results_ref.as_mut_slice(), |remaining, &(start, end)| {
                let (slots, rest) = std::mem::take(remaining).split_at_mut(end - start);
                *remaining = rest;
                Some(scope.spawn(move || {
                    let mut ws = Workspace::new(n);
                    for (slot, &(s, t)) in slots.iter_mut().zip(&queries_ref[start..end]) {
                        let answer = if t != s && !has_in_edge_ref[t as usize] {
                            None // O(1): no edge points at t, so s != t can never reach it
                        } else {
                            shortest_path(csr_ref, s, t, &mut ws)
                        };
                        *slot = answer;
                    }
                }))
            })
            .collect();
        handles
            .into_iter()
            .for_each(|h| h.join().expect("query worker panicked"));
    });
    std::fs::create_dir_all(out_dir).expect("create out dir");
    let out_file =
        std::fs::File::create(format!("{out_dir}/{OUTPUT_FILE}")).expect("create output");
    let mut writer = BufWriter::new(out_file);
    for result in &results {
        match result {
            Some(d) => writeln!(writer, "{d}").expect("write output line"),
            None => writeln!(writer, "-1").expect("write output line"),
        }
    }
}

/// Splits `0..total` into up to `n` contiguous, roughly-equal ranges.
fn even_chunks(total: usize, n: usize) -> Vec<(usize, usize)> {
    if total == 0 || n == 0 {
        return Vec::new();
    }
    let n = n.min(total).max(1);
    (0..n)
        .map(|i| (total * i / n, total * (i + 1) / n))
        .filter(|&(s, e)| e > s)
        .collect()
}
