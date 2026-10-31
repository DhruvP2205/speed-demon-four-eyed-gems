//! Track D: shortest-path queries over a large directed weighted graph.
//! See src/format.rs for the (SPEC-TBD) output format, src/csr.rs for
//! the compact adjacency representation, src/dijkstra.rs for the
//! bidirectional-search algorithm and why it's correct, src/parse.rs
//! for the parallel line-based parser. See PERF.md for the
//! optimization history; every change there is verified byte-identical
//! (sha256) to reference/oracle_d before being kept (rule 7:
//! correctness first).
//!
//! Three phases, each bounded to MAX_THREADS worker threads (never
//! running two phases' threads concurrently, so the 12-thread cap
//! holds regardless of phase):
//!   1. Parse graph.txt in parallel (line-aligned byte ranges, no
//!      quoting to worry about -- Track A's simple case, not Track
//!      C's CSV one) into forward and reverse CSRs.
//!   2. Parse queries.txt (small -- 2,000 lines -- read and parsed on
//!      one thread; parallelizing a few-dozen-KB file isn't worth it).
//!   3. Run all queries, split into up to 12 contiguous chunks, one
//!      bidirectional-Dijkstra Workspace per thread, reused (via
//!      generation-stamped arrays, never reallocated or cleared) across
//!      every query that thread handles.
//!
//! Usage: solution <data_dir> <out_dir>

mod alt;
mod csr;
mod dijkstra;
mod format;
mod parse;
mod reorder;
mod signals;

use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::time::Instant;

use csr::{build_forward, build_reverse, Csr};
use dijkstra::{bidirectional_dijkstra, Workspace};
use format::write_result_line;
use parse::{line_aligned_ranges, parse_edge_triples, parse_query_pairs, pread_at};

const MAX_THREADS: usize = 12;

/// TEMPORARY profiling helper -- see PERF.md. Cumulative CPU-seconds
/// (kernel + user) consumed by the CALLING thread so far, via a direct
/// OS call (no crate -- same device as track-c's own identical
/// helper, copied here since each track is self-contained per
/// CLAUDE.md rule 8). Used only when `TRACK_D_TIMING` is set.
#[cfg(unix)]
fn thread_cpu_time_seconds() -> f64 {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
    }
    const CLOCK_THREAD_CPUTIME_ID: i32 = 3; // POSIX, stable across Linux
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

#[cfg(windows)]
fn thread_cpu_time_seconds() -> f64 {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Filetime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn GetThreadTimes(
            h: isize,
            creation: *mut Filetime,
            exit: *mut Filetime,
            kernel: *mut Filetime,
            user: *mut Filetime,
        ) -> i32;
    }
    let zero = Filetime {
        dw_low_date_time: 0,
        dw_high_date_time: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        );
    }
    let to_secs = |f: Filetime| {
        (((f.dw_high_date_time as u64) << 32) | f.dw_low_date_time as u64) as f64 * 100e-9
    };
    to_secs(kernel) + to_secs(user)
}

/// Read and parse `graph.txt`'s one-line header (`n m`), returning the
/// parsed values and the absolute byte offset where the edge data
/// starts (right after the header's own `\n`).
fn read_header(file: &File) -> (usize, usize, u64) {
    let mut probe = vec![0u8; 64]; // comfortably larger than any "n m\n" header
    let n_read = pread_at(file, &mut probe, 0).expect("read header probe");
    let nl = probe[..n_read]
        .iter()
        .position(|&b| b == b'\n')
        .expect("graph.txt missing header newline");
    let header = &probe[..nl];
    let mut tokens = header.split(|&b| b == b' ').filter(|s| !s.is_empty());
    let n: usize = std::str::from_utf8(tokens.next().expect("missing n in header"))
        .expect("header not utf8")
        .parse()
        .expect("bad n in header");
    let m: usize = std::str::from_utf8(tokens.next().expect("missing m in header"))
        .expect("header not utf8")
        .parse()
        .expect("bad m in header");
    (n, m, nl as u64 + 1)
}

/// Parse the whole graph file in parallel and build both CSRs. The
/// raw per-thread `(us, vs, ws)` triples are local to this function
/// and go out of scope (freed) once both CSRs are built -- the query
/// phase that follows never needs them again.
fn parse_graph_parallel(file: &File, file_len: u64, profiling: bool) -> (usize, usize, Csr, Csr) {
    let (n, m, header_end) = read_header(file);
    let ranges = line_aligned_ranges(file, header_end, file_len, MAX_THREADS);

    let t_parse = Instant::now();
    let chunks: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| {
                scope.spawn(move || {
                    let mut buf = vec![0u8; (end - start) as usize];
                    pread_at(file, &mut buf, start).expect("read edge range");
                    parse_edge_triples(&buf)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("edge parse worker panicked"))
            .collect()
    });
    if profiling {
        eprintln!(
            "TIMING   parse (parallel, {} chunks): {:.4}s",
            ranges.len(),
            t_parse.elapsed().as_secs_f64()
        );
    }

    let t_csr = Instant::now();
    let fwd = build_forward(n, m, &chunks);
    let rev = build_reverse(n, m, &chunks);
    if profiling {
        eprintln!(
            "TIMING   CSR build (single-threaded): {:.4}s",
            t_csr.elapsed().as_secs_f64()
        );
    }
    (n, m, fwd, rev)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let data_dir = args.get(1).map(String::as_str).unwrap_or("/data");
    let out_dir = args.get(2).map(String::as_str).unwrap_or("/out");
    let graph_path = format!("{data_dir}/graph.txt");
    let queries_path = format!("{data_dir}/queries.txt");
    let out_path = format!("{out_dir}/distances.txt");

    let profiling = env::var("TRACK_D_TIMING").is_ok();
    let t_total = Instant::now();

    let t_parse_graph = Instant::now();
    let graph_file = File::open(&graph_path).expect("open graph file");
    let graph_len = graph_file.metadata().expect("stat graph file").len();
    let (n, m, fwd, rev) = parse_graph_parallel(&graph_file, graph_len, profiling);
    if profiling {
        eprintln!(
            "TIMING parse+CSR: {:.4}s (n={n} m={m})",
            t_parse_graph.elapsed().as_secs_f64()
        );
        // CSR size is exact (computed from actual Vec lengths). Per-
        // thread WORKSPACE memory is reported separately, after the
        // query phase below -- `dijkstra::Workspace`'s touched-list
        // capacity and `alt::AstarWorkspace`'s sparse-map capacity are
        // both DATA-DEPENDENT (grown lazily to fit what each thread's
        // queries actually touch), not a fixed function of `n` the
        // way the original gen-stamp-array design was.
        let csr_bytes =
            |c: &Csr| c.offsets.len() * 4 + c.adj_node.len() * 4 + c.adj_weight.len() * 4;
        let fwd_bytes = csr_bytes(&fwd);
        let rev_bytes = csr_bytes(&rev);
        eprintln!(
            "MEMORY forward CSR: {:.1}MB  reverse CSR: {:.1}MB",
            fwd_bytes as f64 / 1e6,
            rev_bytes as f64 / 1e6
        );
    }

    // Adaptive algorithm selection (PERF.md's "adaptive algorithm
    // selection" section): reordering and ALT each help substantially
    // on road-like inputs and actively hurt on others (Step 5a/5b's
    // findings), so the decision is made HERE, once, from cheap
    // structural signals measured on the actual input -- not a fixed
    // default, and not by recognizing which generator produced the
    // file. `TRACK_D_FORCE_REORDER`/`TRACK_D_FORCE_ALT` (values "0" or
    // "1") override the automatic decision for measurement/testing;
    // unset, the signals decide. Printed to STDERR ONLY (never /out)
    // so the choice is auditable on every run, not just under
    // `TRACK_D_TIMING`.
    let t_signals = Instant::now();
    let locality = signals::locality_score(&fwd, n);
    let (reverse_pair_fraction, weight_symmetry_fraction) = signals::reverse_edge_signals(&fwd, n);
    let auto_reorder = signals::should_reorder(locality);
    let reorder_enabled = match env::var("TRACK_D_FORCE_REORDER").ok().as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => auto_reorder,
    };
    eprintln!(
        "STRATEGY locality_score={locality:.4} (reorder: {reorder_enabled}) reverse_pair_fraction={reverse_pair_fraction:.4} weight_symmetry_fraction={weight_symmetry_fraction:.4} (ALT's structural prerequisite -- combined with query-adaptive hardness sampling below, see STRATEGY ALT) signals computed in {:.4}s",
        t_signals.elapsed().as_secs_f64()
    );

    // Node reordering: purely internal, so it only needs to touch the
    // CSR (remapped to the new numbering) and the ids actually fed to
    // `bidirectional_dijkstra` -- the ORIGINAL (s, t) ids are what the
    // output format uses, untouched, via `queries` below.
    let (fwd, rev, permutation) = if reorder_enabled {
        let t_reorder = Instant::now();
        let perm = reorder::bfs_order(n, &fwd, &rev);
        let new_fwd = reorder::remap_csr(&fwd, &perm);
        let new_rev = reorder::remap_csr(&rev, &perm);
        if profiling {
            eprintln!(
                "TIMING reorder (preprocessing): {:.4}s",
                t_reorder.elapsed().as_secs_f64()
            );
        }
        (new_fwd, new_rev, Some(perm))
    } else {
        (fwd, rev, None)
    };

    let t_parse_queries = Instant::now();
    let queries_file = File::open(&queries_path).expect("open queries file");
    let queries_len = queries_file.metadata().expect("stat queries file").len();
    let mut queries_buf = vec![0u8; queries_len as usize];
    pread_at(&queries_file, &mut queries_buf, 0).expect("read queries file");
    let queries = parse_query_pairs(&queries_buf);
    if profiling {
        eprintln!(
            "TIMING parse queries: {:.4}s (count={})",
            t_parse_queries.elapsed().as_secs_f64(),
            queries.len()
        );
    }

    // Queries actually fed to the search: remapped to the new node
    // numbering if reordering is enabled, otherwise identical to
    // `queries`. Kept as a SEPARATE vector specifically so `queries`
    // (original ids) stays untouched for output formatting below.
    let search_queries: Vec<(u32, u32)> = match &permutation {
        Some(perm) => queries
            .iter()
            .map(|&(s, t)| (perm[s as usize], perm[t as usize]))
            .collect(),
        None => queries.clone(),
    };

    let num_queries = queries.len();
    let num_threads = MAX_THREADS.min(num_queries.max(1));
    let chunk_size = num_queries.div_ceil(num_threads).max(1);
    let mut results: Vec<i64> = vec![0; num_queries];
    let mut cpu_sum = 0.0f64;
    let mut workspace_bytes_sum = 0usize;
    let t_queries = Instant::now();

    // ALT's decision is QUERY-ADAPTIVE, not graph-signal-based like
    // reorder's: answer the first `sample_per_thread` queries of each
    // thread's chunk with PLAIN bidirectional Dijkstra, measure how
    // many nodes each touches (`Workspace::last_touched_count`), and
    // only pay ALT's landmark-building cost if the MEDIAN sample says
    // the ACTUAL query workload is hard enough to benefit. Graph
    // structure (the weight-symmetry signal above) says whether ALT
    // COULD help if queries are hard; it can't say whether they
    // actually ARE -- calibrated-local queries on road-like data can
    // be easy even though the graph has landmark-friendly structure.
    // `TRACK_D_FORCE_ALT` still overrides and skips sampling entirely
    // (deterministic for A/B measurement). `TRACK_D_ALT_SAMPLE_PER_
    // THREAD` overrides the sample size -- measurement-only, used to
    // calibrate `ALT_TOUCHED_THRESHOLD` (see PERF.md).
    const ALT_SAMPLE_PER_THREAD: usize = 2;
    const ALT_TOUCHED_THRESHOLD: usize = 2_000; // calibrated, see PERF.md

    let alt_force = env::var("TRACK_D_FORCE_ALT").ok();
    let (alt_enabled, samples_done_per_chunk) = match alt_force.as_deref() {
        Some("1") => {
            eprintln!("STRATEGY ALT: true (forced via TRACK_D_FORCE_ALT=1)");
            (true, 0usize)
        }
        Some("0") => {
            eprintln!("STRATEGY ALT: false (forced via TRACK_D_FORCE_ALT=0)");
            (false, 0usize)
        }
        _ => {
            let sample_per_thread: usize = env::var("TRACK_D_ALT_SAMPLE_PER_THREAD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(ALT_SAMPLE_PER_THREAD);
            let t_sample = Instant::now();
            let mut touched_samples: Vec<usize> = Vec::new();
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..num_queries)
                    .step_by(chunk_size)
                    .map(|chunk_start| {
                        let chunk_end = (chunk_start + chunk_size).min(num_queries);
                        let sample_end = (chunk_start + sample_per_thread).min(chunk_end);
                        let queries_ref = &search_queries;
                        let fwd_ref = &fwd;
                        let rev_ref = &rev;
                        let handle = scope.spawn(move || {
                            let cpu_start = if profiling {
                                thread_cpu_time_seconds()
                            } else {
                                0.0
                            };
                            let mut ws = Workspace::new(n);
                            let mut local_results = Vec::with_capacity(sample_end - chunk_start);
                            let mut local_touched = Vec::with_capacity(sample_end - chunk_start);
                            for &(s, t) in &queries_ref[chunk_start..sample_end] {
                                local_results
                                    .push(bidirectional_dijkstra(&mut ws, s, t, fwd_ref, rev_ref));
                                local_touched.push(ws.last_touched_count());
                            }
                            let cpu = if profiling {
                                thread_cpu_time_seconds() - cpu_start
                            } else {
                                0.0
                            };
                            (chunk_start, local_results, local_touched, cpu)
                        });
                        (sample_end, handle)
                    })
                    .collect();
                for (_, handle) in handles {
                    let (chunk_start, local_results, local_touched, cpu) =
                        handle.join().expect("sample worker panicked");
                    results[chunk_start..chunk_start + local_results.len()]
                        .copy_from_slice(&local_results);
                    touched_samples.extend(local_touched);
                    cpu_sum += cpu;
                }
            });
            let median = if touched_samples.is_empty() {
                0
            } else {
                touched_samples.sort_unstable();
                touched_samples[touched_samples.len() / 2]
            };
            // Query-adaptive hardness alone is NOT enough: a real
            // regression (PERF.md's step on it) showed `random` mode's
            // small-world shortcuts can make a touched-count sample
            // look "hard" even though `random`'s independent edge
            // weights make ALT's landmark bounds nearly useless --
            // ANDing with the structural weight-symmetry signal (does
            // the graph even have metric-like structure for landmark
            // bounds to exploit?) catches that case.
            let queries_are_hard = median > ALT_TOUCHED_THRESHOLD;
            let graph_can_use_alt =
                signals::graph_supports_alt(reverse_pair_fraction, weight_symmetry_fraction);
            let enabled = queries_are_hard && graph_can_use_alt;
            if profiling {
                eprintln!(
                    "TIMING ALT sampling: {:.4}s ({} samples)",
                    t_sample.elapsed().as_secs_f64(),
                    touched_samples.len()
                );
            }
            eprintln!(
                "STRATEGY ALT: {enabled} (query-adaptive: {} samples, median touched={median}, threshold={ALT_TOUCHED_THRESHOLD}, queries_are_hard={queries_are_hard}; graph structural prerequisite: reverse_pair_fraction={reverse_pair_fraction:.4} weight_symmetry_fraction={weight_symmetry_fraction:.4} graph_can_use_alt={graph_can_use_alt})",
                touched_samples.len()
            );
            (enabled, sample_per_thread)
        }
    };

    // ALT: landmark selection + the forward/backward distance tables
    // for each landmark are PREPROCESSING -- timed and reported
    // separately from the query phase. `TRACK_D_ALT_LANDMARKS`
    // overrides the landmark count (default 16); landmark 0 is always
    // node 0 of whichever numbering is active at this point (post-
    // reorder, if reordering is also enabled).
    let landmarks = if alt_enabled {
        let t_alt_prep = Instant::now();
        let landmark_count: usize = env::var("TRACK_D_ALT_LANDMARKS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16);
        let landmark_ids = alt::select_landmarks(&fwd, n, landmark_count, 0);
        let built = alt::Landmarks::build(&fwd, &rev, n, &landmark_ids);
        if profiling {
            eprintln!(
                "TIMING ALT preprocessing: {:.4}s ({} landmarks)",
                t_alt_prep.elapsed().as_secs_f64(),
                landmark_ids.len()
            );
            eprintln!(
                "MEMORY ALT landmark tables: {:.1}MB",
                built.memory_bytes() as f64 / 1e6
            );
        }
        Some(built)
    } else {
        None
    };

    // Remaining queries: whatever each chunk didn't already get
    // answered by the sampling phase above (0 of them, in the forced
    // paths -- the whole chunk is "remaining" there).
    if let Some(landmarks) = &landmarks {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..num_queries)
                .step_by(chunk_size)
                .filter_map(|chunk_start| {
                    let chunk_end = (chunk_start + chunk_size).min(num_queries);
                    let remaining_start = chunk_start + samples_done_per_chunk;
                    if remaining_start >= chunk_end {
                        return None;
                    }
                    let queries_ref = &search_queries;
                    let fwd_ref = &fwd;
                    let rev_ref = &rev;
                    let landmarks_ref = landmarks;
                    let handle = scope.spawn(move || {
                        let cpu_start = if profiling {
                            thread_cpu_time_seconds()
                        } else {
                            0.0
                        };
                        let mut ws = alt::AstarWorkspace::new(n);
                        let mut local_results = Vec::with_capacity(chunk_end - remaining_start);
                        for &(s, t) in &queries_ref[remaining_start..chunk_end] {
                            local_results.push(alt::bidirectional_astar(
                                &mut ws,
                                s,
                                t,
                                fwd_ref,
                                rev_ref,
                                landmarks_ref,
                            ));
                        }
                        let cpu = if profiling {
                            thread_cpu_time_seconds() - cpu_start
                        } else {
                            0.0
                        };
                        let workspace_bytes = if profiling { ws.memory_bytes() } else { 0 };
                        (local_results, cpu, workspace_bytes)
                    });
                    Some((remaining_start, handle))
                })
                .collect();
            for (remaining_start, handle) in handles {
                let (local_results, cpu, workspace_bytes) =
                    handle.join().expect("query worker panicked");
                results[remaining_start..remaining_start + local_results.len()]
                    .copy_from_slice(&local_results);
                cpu_sum += cpu;
                workspace_bytes_sum += workspace_bytes;
            }
        });
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..num_queries)
                .step_by(chunk_size)
                .filter_map(|chunk_start| {
                    let chunk_end = (chunk_start + chunk_size).min(num_queries);
                    let remaining_start = chunk_start + samples_done_per_chunk;
                    if remaining_start >= chunk_end {
                        return None;
                    }
                    let queries_ref = &search_queries;
                    let fwd_ref = &fwd;
                    let rev_ref = &rev;
                    let handle = scope.spawn(move || {
                        let cpu_start = if profiling {
                            thread_cpu_time_seconds()
                        } else {
                            0.0
                        };
                        let mut ws = Workspace::new(n);
                        let mut local_results = Vec::with_capacity(chunk_end - remaining_start);
                        for &(s, t) in &queries_ref[remaining_start..chunk_end] {
                            local_results
                                .push(bidirectional_dijkstra(&mut ws, s, t, fwd_ref, rev_ref));
                        }
                        let cpu = if profiling {
                            thread_cpu_time_seconds() - cpu_start
                        } else {
                            0.0
                        };
                        // Fixed arrays (18 bytes/node: dist_f+visited_f+dist_b+visited_b,
                        // no gen-stamp arrays) plus the touched-list and heap capacities
                        // actually reached -- the only DATA-DEPENDENT parts.
                        let workspace_bytes = if profiling {
                            n * (8 + 1 + 8 + 1)
                                + ws.touched_capacity_bytes()
                                + ws.heap_capacity_bytes()
                        } else {
                            0
                        };
                        (local_results, cpu, workspace_bytes)
                    });
                    Some((remaining_start, handle))
                })
                .collect();
            for (remaining_start, handle) in handles {
                let (local_results, cpu, workspace_bytes) =
                    handle.join().expect("query worker panicked");
                results[remaining_start..remaining_start + local_results.len()]
                    .copy_from_slice(&local_results);
                cpu_sum += cpu;
                workspace_bytes_sum += workspace_bytes;
            }
        });
    }
    if profiling {
        eprintln!(
            "MEMORY actual per-thread workspace total across {} threads: {:.1}MB ({})",
            num_threads,
            workspace_bytes_sum as f64 / 1e6,
            if alt_enabled {
                "sparse ALT workspaces"
            } else {
                "plain Dijkstra workspaces"
            },
        );
    }
    if profiling {
        eprintln!(
            "TIMING queries: {:.4}s cpu_sum={:.4}s",
            t_queries.elapsed().as_secs_f64(),
            cpu_sum
        );
    }

    let t_write = Instant::now();
    std::fs::create_dir_all(out_dir).expect("create output directory");
    let out_file = File::create(&out_path).expect("create output");
    let mut writer = BufWriter::new(out_file);
    for (i, &(s, t)) in queries.iter().enumerate() {
        write_result_line(&mut writer, s, t, results[i]).expect("write output line");
    }
    drop(writer);
    if profiling {
        eprintln!("TIMING write: {:.4}s", t_write.elapsed().as_secs_f64());
        eprintln!("TIMING total wall: {:.4}s", t_total.elapsed().as_secs_f64());
    }
}
