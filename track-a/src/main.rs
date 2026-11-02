//! Speed Demon 2026, Track A (Ingest). specs/a-ingest.md is the contract.
//!
//! Reads /data/requests.log (3.1M lines at `full` scale, not sorted by
//! time) and writes /out/summary.txt: for every (UTC hour, endpoint)
//! pair that saw traffic, the request count, p50/p95/p99 latency
//! (nearest rank) and the 5xx error count/rate. See src/format.rs for
//! the exact output arithmetic and src/parse.rs for why hour
//! truncation is a byte-slice, not a date computation.
//!
//! Phase breakdown from an earlier version (temporary instrumentation,
//! not shipped) found parse+aggregate parallelizing well (CPU/wall
//! ratio ~11.5 of 12 threads) but the merge+sort+write tail entirely
//! SEQUENTIAL and costing more wall time than the parallel phase
//! itself (~53% of the run) -- the per-group `Vec<u32>` latencies were
//! merged via per-entry `HashMap`/`Vec::extend_from_slice` calls, then
//! EACH group's latencies were fully sorted (`sort_unstable`, O(k log
//! k)) just to read off three percentile ranks. This version fixes
//! both:
//!
//! - The whole file is mapped read-only via `mmap` (`src/mmap_file.rs`)
//!   instead of streamed through `pread_at`, and each of 12 threads
//!   parses its own disjoint, line-aligned byte range (`src/chunked.rs`
//!   -- trivial here, since this format has no embedded newlines, so
//!   no quote-tracking or speculate-then-verify step is needed at all,
//!   unlike Track C).
//! - Each thread interns its own endpoints AND hour-buckets into small
//!   per-thread integer ids (`u32`), and tracks per-group count/error
//!   totals in a `HashMap` keyed by one packed `u32` (not a `(u32,
//!   [u8; 20])` string-keyed map) -- a small dense integer key instead
//!   of a 20-byte string comparison/hash per line. Every latency is
//!   pushed as one packed `u64` (`(local_group << 32) | latency`) into
//!   ONE flat per-thread `Vec`, not a separate growing `Vec` per group.
//! - After all 12 threads finish: a cheap, one-time global pass
//!   assigns GLOBAL endpoint/hour ids in SORTED byte order (so a
//!   packed `global_hour_id * num_endpoints + global_endpoint_id`
//!   "group id," iterated ascending, is already in the exact output
//!   order -- no separate sort step, same trick Track C's country
//!   packing uses), remaps every thread's local group ids and group
//!   counts/errors into one flat global array, and counting-sorts
//!   every latency (one O(total_lines) pass, no hashing, no
//!   allocation per entry) into one contiguous `Vec<u32>` where each
//!   group occupies its own slice.
//! - p50/p95/p99 are then computed per group via `select_nth_unstable`
//!   (O(k) per rank, not O(k log k) for a full sort) in PARALLEL: the
//!   one big latencies array is split into 12 disjoint mutable
//!   sub-slices along group boundaries (ordinary safe `split_at_mut`,
//!   no unsafe, no shared mutable state across threads), each thread
//!   handling its own contiguous range of groups.

mod chunked;
mod format;
mod mmap_file;
mod parse;

use std::collections::HashMap;

use chunked::line_aligned_ranges;
use format::{percentile_rank, write_group_line, GroupLine};
use mmap_file::MappedFile;
use parse::{hour_output_bytes, parse_u32};

const MAX_THREADS: usize = 12;
const MAX_ENDPOINTS: usize = 2048;
const MAX_HOURS: usize = 64;
const INPUT_NAME: &str = "requests.log";
const OUTPUT_NAME: &str = "summary.txt";

#[derive(Default, Clone, Copy)]
struct GroupAgg {
    count: u32,
    errors: u32,
}

/// Per-thread aggregation state. Endpoints and hour-buckets are each
/// interned to a small integer id the first time this thread sees
/// them -- the hot loop only allocates on an interning MISS (bounded
/// by this thread's distinct count), never per line. `groups` is
/// keyed by one packed `u32` (`local_hour_id << 16 | local_endpoint_id`),
/// not a string -- hashing/comparing a `u32` is far cheaper per line
/// than hashing/comparing a `(u32, [u8; 20])`.
struct LocalState {
    endpoint_ids: HashMap<Vec<u8>, u32>,
    endpoint_names: Vec<Vec<u8>>,
    hour_ids: HashMap<[u8; 13], u32>,
    hour_names: Vec<[u8; 13]>,
    groups: HashMap<u32, GroupAgg>,
    /// `(local_group as u64) << 32 | latency`, one flat growing `Vec`
    /// per thread instead of one small `Vec` per group.
    latencies: Vec<u64>,
}

impl LocalState {
    fn new(expected_lines: usize) -> Self {
        LocalState {
            endpoint_ids: HashMap::with_capacity(MAX_ENDPOINTS),
            endpoint_names: Vec::with_capacity(MAX_ENDPOINTS),
            hour_ids: HashMap::with_capacity(MAX_HOURS),
            hour_names: Vec::with_capacity(MAX_HOURS),
            groups: HashMap::with_capacity(MAX_HOURS * MAX_ENDPOINTS),
            latencies: Vec::with_capacity(expected_lines),
        }
    }

    #[inline]
    fn intern_endpoint(&mut self, endpoint: &[u8]) -> u32 {
        if let Some(&id) = self.endpoint_ids.get(endpoint) {
            return id;
        }
        let id = self.endpoint_names.len() as u32;
        self.endpoint_names.push(endpoint.to_vec());
        self.endpoint_ids.insert(endpoint.to_vec(), id);
        id
    }

    #[inline]
    fn intern_hour(&mut self, hour_key: [u8; 13]) -> u32 {
        if let Some(&id) = self.hour_ids.get(&hour_key) {
            return id;
        }
        let id = self.hour_names.len() as u32;
        self.hour_names.push(hour_key);
        self.hour_ids.insert(hour_key, id);
        id
    }
}

/// `ts method endpoint status latency_us`, single-space separated
/// (grammar in spec section 2), `ts` always exactly 27 bytes. Parsed
/// by fixed-position byte scanning, not a generic `split` iterator:
/// the timestamp's hour prefix is read directly by position, and
/// `method` is scanned past (its content is never used in the
/// output) rather than captured.
#[inline]
fn process_line(line: &[u8], state: &mut LocalState) {
    // Shortest possible well-formed line: 27-byte ts + ' ' + >=1-byte
    // method + ' ' + >=1-byte endpoint + ' ' + 3-digit status + ' ' +
    // >=1-digit latency. The generator never emits anything shorter,
    // but every scan below is bounds-checked (`pos < line.len()`)
    // regardless, rather than trusting that bound not to panic on
    // some future, differently-shaped input.
    if line.len() < 28 {
        return;
    }
    let hour_key: [u8; 13] = line[0..13].try_into().expect("checked above");
    let mut pos = 28usize; // "ts" (27 bytes) + ' '
    let len = line.len();

    while pos < len && line[pos] != b' ' {
        pos += 1; // method -- unused in the output, just skip past it
    }
    pos += 1;

    let endpoint_start = pos;
    while pos < len && line[pos] != b' ' {
        pos += 1;
    }
    let endpoint = &line[endpoint_start..pos.min(len)];
    pos += 1;

    let status_start = pos.min(len);
    while pos < len && line[pos] != b' ' {
        pos += 1;
    }
    let status_val = parse_u32(&line[status_start..pos.min(len)]);
    pos += 1;

    let latency_val = parse_u32(&line[pos.min(len)..]);

    let endpoint_id = state.intern_endpoint(endpoint);
    let hour_id = state.intern_hour(hour_key);
    // Packing assumes both ids fit in 16 bits: endpoints are bounded
    // to ~1,842 by spec, and hour_id only overflows past a log
    // spanning more than 65,536 hours (7+ years) -- nowhere near this
    // benchmark's scale.
    debug_assert!(endpoint_id < 1 << 16 && hour_id < 1 << 16);
    let local_group = (hour_id << 16) | endpoint_id;

    let agg = state.groups.entry(local_group).or_default();
    agg.count += 1;
    if (500..=599).contains(&status_val) {
        agg.errors += 1;
    }

    state
        .latencies
        .push(((local_group as u64) << 32) | latency_val as u64);
}

/// Computes (p50, p95, p99) for one group's latencies via
/// `select_nth_unstable` -- O(k) per rank, not O(k log k) for a full
/// sort, since only three specific ranks are ever read. Calling it
/// three times on the same mutable slice is correct regardless of
/// call order: each call re-partitions around its own target index
/// without needing the slice already sorted.
#[inline]
fn group_percentiles(latencies: &mut [u32]) -> (u32, u32, u32) {
    let n = latencies.len() as u64;
    let r50 = percentile_rank(n, 50);
    let r95 = percentile_rank(n, 95);
    let r99 = percentile_rank(n, 99);
    let p50 = *latencies.select_nth_unstable(r50).1;
    let p95 = *latencies.select_nth_unstable(r95).1;
    let p99 = *latencies.select_nth_unstable(r99).1;
    (p50, p95, p99)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(String::as_str).unwrap_or("/data");
    let out_dir = args.get(2).map(String::as_str).unwrap_or("/out");

    let input_path = format!("{data_dir}/{INPUT_NAME}");
    let mapped =
        MappedFile::open(&input_path, true).unwrap_or_else(|e| panic!("open {input_path}: {e}"));
    let data = mapped.as_slice();

    let ranges = line_aligned_ranges(data, MAX_THREADS);

    let partials: Vec<LocalState> = std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| {
                scope.spawn(move || {
                    let expected_lines = (end - start) / 80;
                    let mut state = LocalState::new(expected_lines);
                    for line in data[start..end].split(|&b| b == b'\n') {
                        if !line.is_empty() {
                            process_line(line, &mut state);
                        }
                    }
                    state
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker thread panicked"))
            .collect()
    });

    // Global endpoint/hour ids assigned in SORTED byte order, so a
    // packed group id, iterated ascending, is already the required
    // (hour, endpoint) output order -- no separate sort step.
    let mut all_endpoints: Vec<Vec<u8>> = Vec::new();
    let mut all_hours: Vec<[u8; 13]> = Vec::new();
    for p in &partials {
        all_endpoints.extend(p.endpoint_names.iter().cloned());
        all_hours.extend(p.hour_names.iter().copied());
    }
    all_endpoints.sort();
    all_endpoints.dedup();
    all_hours.sort();
    all_hours.dedup();
    let num_endpoints = all_endpoints.len();
    let num_hours = all_hours.len();
    let num_groups = num_hours * num_endpoints;

    let endpoint_global: HashMap<&[u8], u32> = all_endpoints
        .iter()
        .enumerate()
        .map(|(i, e)| (e.as_slice(), i as u32))
        .collect();
    let hour_global: HashMap<[u8; 13], u32> = all_hours
        .iter()
        .enumerate()
        .map(|(i, h)| (*h, i as u32))
        .collect();

    // Each thread's LOCAL id -> GLOBAL id lookup, built once.
    let local_to_global: Vec<(Vec<u32>, Vec<u32>)> = partials
        .iter()
        .map(|p| {
            let endpoint_map: Vec<u32> = p
                .endpoint_names
                .iter()
                .map(|e| endpoint_global[e.as_slice()])
                .collect();
            let hour_map: Vec<u32> = p.hour_names.iter().map(|h| hour_global[h]).collect();
            (endpoint_map, hour_map)
        })
        .collect();

    // Consumes `partials` by value: each thread's `groups` HashMap and
    // name/id tables (no longer needed after this merge) drop at the
    // end of their own loop iteration, instead of staying alive -- as
    // the single `Vec<LocalState>` they used to live in -- all the way
    // to the end of `main`, overlapping with `big_latencies` and the
    // percentile workers' buffers below. Only each thread's `latencies`
    // survives, moved into its own slot so pass 2 can still consume it
    // the same way. Massif confirmed this overlap: the `groups`
    // HashMap's own table growth was still a live ~9% of peak heap
    // alongside `big_latencies` and the percentile buffers (see
    // PERF.md) before this change.
    let mut global_counts = vec![0u32; num_groups];
    let mut global_errors = vec![0u32; num_groups];
    let mut latencies_per_thread: Vec<Vec<u64>> = Vec::with_capacity(partials.len());
    for (p, (endpoint_map, hour_map)) in partials.into_iter().zip(&local_to_global) {
        for (&local_group, agg) in &p.groups {
            let local_hour = (local_group >> 16) as usize;
            let local_endpoint = (local_group & 0xFFFF) as usize;
            let global_group = hour_map[local_hour] as usize * num_endpoints
                + endpoint_map[local_endpoint] as usize;
            global_counts[global_group] += agg.count;
            global_errors[global_group] += agg.errors;
        }
        latencies_per_thread.push(p.latencies);
    }

    // Counting sort: one O(total_lines) pass placing every latency
    // directly into its group's slice of one contiguous array, via
    // precomputed per-group starting offsets -- no per-entry
    // allocation, no hashing.
    let mut prefix = vec![0u32; num_groups + 1];
    for g in 0..num_groups {
        prefix[g + 1] = prefix[g] + global_counts[g];
    }
    let total = prefix[num_groups] as usize;

    let mut big_latencies = vec![0u32; total];
    let mut cursor = prefix.clone();
    for (ti, latencies) in latencies_per_thread.into_iter().enumerate() {
        let (ref endpoint_map, ref hour_map) = local_to_global[ti];
        for packed in latencies {
            let local_group = (packed >> 32) as u32;
            let latency = packed as u32;
            let local_hour = (local_group >> 16) as usize;
            let local_endpoint = (local_group & 0xFFFF) as usize;
            let global_group = hour_map[local_hour] as usize * num_endpoints
                + endpoint_map[local_endpoint] as usize;
            let pos = cursor[global_group] as usize;
            big_latencies[pos] = latency;
            cursor[global_group] += 1;
        }
    }

    // Parallel percentile computation: split the group-id range into
    // up to MAX_THREADS chunks balanced by TOTAL ENTRY COUNT (groups
    // vary hugely in size, per the Zipf endpoint distribution), then
    // give each chunk's thread a disjoint mutable slice of
    // `big_latencies` via plain `split_at_mut` -- safe, no unsafe,
    // since each thread's byte range never overlaps another's.
    let thread_count = MAX_THREADS.min(num_groups.max(1));
    let entry_targets: Vec<usize> = (1..thread_count)
        .map(|i| total * i / thread_count)
        .collect();
    let mut group_boundaries = vec![0usize];
    for &t in &entry_targets {
        let g = prefix
            .partition_point(|&p| (p as usize) < t)
            .min(num_groups);
        group_boundaries.push(g);
    }
    group_boundaries.push(num_groups);
    group_boundaries.dedup();

    let mut percentiles = vec![(0u32, 0u32, 0u32); num_groups];
    {
            let mut remaining: &mut [u32] = &mut big_latencies;
            let mut prev_group = 0usize;
            let mut prev_byte = 0usize;
            let mut thread_slices: Vec<(&mut [u32], usize, usize, usize)> = Vec::new();
            let mut percentile_slices: Vec<(&mut [(u32, u32, u32)], usize)> = Vec::new();
            let mut remaining_percentiles: &mut [(u32, u32, u32)] = &mut percentiles;
            for &g_end in &group_boundaries[1..] {
                let byte_end = prefix[g_end] as usize;
                let (this_slice, rest) = remaining.split_at_mut(byte_end - prev_byte);
                thread_slices.push((this_slice, prev_group, g_end, prev_byte));
                remaining = rest;
                let (this_percentiles, rest_percentiles) =
                    remaining_percentiles.split_at_mut(g_end - prev_group);
                percentile_slices.push((this_percentiles, prev_group));
                remaining_percentiles = rest_percentiles;
                prev_group = g_end;
                prev_byte = byte_end;
            }

            let prefix_ref = &prefix;
            let global_counts_ref = &global_counts;
            std::thread::scope(|scope| {
                let handles: Vec<_> = thread_slices
                    .into_iter()
                    .zip(percentile_slices)
                    .map(|((slice, g_start, g_end, byte_offset), (output, output_start))| {
                        scope.spawn(move || {
                            for g in g_start..g_end {
                                if global_counts_ref[g] == 0 {
                                    continue;
                                }
                                let s = prefix_ref[g] as usize - byte_offset;
                                let e = prefix_ref[g + 1] as usize - byte_offset;
                                output[g - output_start] = group_percentiles(&mut slice[s..e]);
                            }
                        })
                    })
                    .collect();
            handles
                .into_iter()
                .for_each(|h| h.join().expect("percentile worker panicked"));
            });
    }

    std::fs::create_dir_all(out_dir).expect("create out dir");
    let out_file =
        std::fs::File::create(format!("{out_dir}/{OUTPUT_NAME}")).expect("create output");
    let mut writer = std::io::BufWriter::new(out_file);

    // Ascending group id order already matches (hour, endpoint)
    // sorted output order exactly (both ids assigned in sorted byte
    // order above), so no separate sort step is needed here either.
    for g in 0..num_groups {
        let count = global_counts[g];
        if count == 0 {
            continue;
        }
        let hour_idx = g / num_endpoints;
        let endpoint_idx = g % num_endpoints;
        let hour_bytes = hour_output_bytes(all_hours[hour_idx]);
        let (p50, p95, p99) = percentiles[g];
        write_group_line(
            &mut writer,
            &GroupLine {
                hour: &hour_bytes,
                endpoint: &all_endpoints[endpoint_idx],
                count,
                errors: global_errors[g],
                p50,
                p95,
                p99,
            },
        )
        .expect("write output line");
    }
}
