//! Speed Demon 2026, Track D (Traverse). specs/d-traverse.md is the contract.
//!
//! Reads `graph.txt` (directed, weighted, possibly with parallel
//! edges, self-loops, duplicate lines and weight-0 edges) and
//! `queries.txt` (`s t` pairs), and writes `distances.txt`: the
//! shortest directed-path distance for each query, in input order, or
//! `-1` if `t` is unreachable from `s`.
//!
//! Graph build: `graph.txt` is split into up to MAX_THREADS
//! line-aligned byte ranges (no edge line split across a thread
//! boundary) and parsed in parallel into per-thread `u32` triples,
//! then counting-sorted into one forward CSR (`src/csr.rs`); the same
//! triples also give an O(n) `has_in_edge` bitmap, so a query whose
//! target has no in-edge (and isn't its own source) is answered `-1`
//! without running Dijkstra at all. Nodes are then renumbered by BFS
//! visitation order (`src/reorder.rs`) -- a pure cache-locality
//! optimization, since the input's node ids are a random relabelling
//! with no positional structure -- and every query is translated
//! through the same mapping before it touches the CSR or bitmap.
//! Queries are independent, so threads claim them one at a time from
//! a shared atomic counter (not a static split, since query cost is
//! very skewed by Dijkstra rank), each running a plain single-source
//! Dijkstra over a `Workspace` (`src/workspace.rs`): a paged sparse
//! array (direct indexing, no hashing) whose pages are drawn from a
//! per-thread pool and returned (zeroed) at the end of each query, so
//! memory tracks the busiest single query's distinct-page count, not
//! `n`. It stops as soon as `t` is popped.

mod csr;
mod parse;
mod reorder;
mod workspace;

use std::cmp::Reverse;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use csr::{build_forward, has_in_edge_bitmap, Csr};
use parse::{
    line_aligned_ranges, parse_edge_triples, parse_query_pairs, parse_uint, pread_at,
    skip_non_digits,
};
use reorder::{bfs_order, remap_bitmap, remap_csr};
use workspace::Workspace;

const MAX_THREADS: usize = 12;
const GRAPH_FILE: &str = "graph.txt";
const QUERIES_FILE: &str = "queries.txt";
const OUTPUT_FILE: &str = "distances.txt";

/// Read the `n m` header line, returning the parsed values and the
/// byte offset right after the header's trailing `\n` (where edges
/// start).
fn read_header(file: &File) -> (usize, usize, u64) {
    let mut buf = [0u8; 64];
    let read = pread_at(file, &mut buf, 0).expect("read graph.txt header");
    let buf = &buf[..read];
    let i = skip_non_digits(buf, 0);
    let (n, i) = parse_uint(buf, i);
    let i = skip_non_digits(buf, i);
    let (m, i) = parse_uint(buf, i);
    let header_end = buf[i..]
        .iter()
        .position(|&b| b == b'\n')
        .expect("header has no newline")
        + i
        + 1;
    (n as usize, m as usize, header_end as u64)
}

/// Builds the forward CSR and `has_in_edge` bitmap in the input's own
/// node numbering, then renumbers both by BFS order. Returns the
/// renumbered CSR/bitmap plus `new_id_of_old`, so the caller can
/// translate queries through the same mapping.
fn load_graph(path: &str) -> (Csr, Vec<bool>, Vec<u32>) {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let file_len = file.metadata().expect("stat graph.txt").len();
    let (n, m, header_end) = read_header(&file);

    let ranges = line_aligned_ranges(&file, header_end, file_len, MAX_THREADS);
    let chunks: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = std::thread::scope(|scope| {
        let file_ref = &file;
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| {
                scope.spawn(move || {
                    let mut buf = vec![0u8; (end - start) as usize];
                    pread_at(file_ref, &mut buf, start).expect("read edge range");
                    parse_edge_triples(&buf)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("parse worker panicked"))
            .collect()
    });

    // Computed from `&chunks` before `build_forward` consumes it by value,
    // so the ~60 MB parsed edge list is only ever borrowed here, never
    // held alongside the CSR it gets folded into.
    let has_in_edge = has_in_edge_bitmap(n, &chunks);
    let csr = build_forward(n, m, chunks);

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
    ws.heap.push((Reverse(0), s));

    while let Some((Reverse(d), u)) = ws.heap.pop() {
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
                ws.heap.push((Reverse(nd), v));
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
    let qdata = fs::read(&queries_path).unwrap_or_else(|e| panic!("open {queries_path}: {e}"));
    // Node ids in the input are in the ORIGINAL numbering; the CSR and
    // bitmap above are already in BFS-order numbering, so every query
    // must be translated through the same mapping before it touches
    // either. The answer (a distance, or unreachable) is invariant
    // under renumbering, so no mapping back is needed for output.
    let queries: Vec<(u32, u32)> = parse_query_pairs(&qdata)
        .into_iter()
        .map(|(s, t)| (new_id_of_old[s as usize], new_id_of_old[t as usize]))
        .collect();

    let mut results: Vec<Option<u64>> = vec![None; queries.len()];
    let thread_count = MAX_THREADS.min(queries.len().max(1));
    let next_query = AtomicUsize::new(0);

    let per_thread: Vec<Vec<(usize, Option<u64>)>> = std::thread::scope(|scope| {
        let csr_ref = &csr;
        let has_in_edge_ref = &has_in_edge;
        let next_query_ref = &next_query;
        let queries_ref = &queries;
        let total_queries = queries.len();
        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                scope.spawn(move || {
                    let mut ws = Workspace::new(n);
                    let mut local: Vec<(usize, Option<u64>)> = Vec::new();
                    loop {
                        let i = next_query_ref.fetch_add(1, Ordering::Relaxed);
                        if i >= total_queries {
                            break;
                        }
                        let (s, t) = queries_ref[i];
                        let answer = if t != s && !has_in_edge_ref[t as usize] {
                            None // O(1): no edge points at t, so s != t can never reach it
                        } else {
                            shortest_path(csr_ref, s, t, &mut ws)
                        };
                        local.push((i, answer));
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker thread panicked"))
            .collect()
    });
    for local in per_thread {
        for (i, answer) in local {
            results[i] = answer;
        }
    }

    std::fs::create_dir_all(out_dir).expect("create out dir");
    let out_file = File::create(format!("{out_dir}/{OUTPUT_FILE}")).expect("create output");
    let mut writer = BufWriter::new(out_file);
    for result in &results {
        match result {
            Some(d) => writeln!(writer, "{d}").expect("write output line"),
            None => writeln!(writer, "-1").expect("write output line"),
        }
    }
}
