//! Speed Demon 2026, Track A (Ingest). specs/a-ingest.md is the contract.
//!
//! Reads /data/requests.log (3.1M lines at `full` scale, not sorted by
//! time) and writes /out/summary.txt: for every (UTC hour, endpoint)
//! pair that saw traffic, the request count, p50/p95/p99 latency
//! (nearest rank) and the 5xx error count/rate. See src/format.rs for
//! the exact output arithmetic and src/parse.rs for why hour
//! truncation is a byte-slice, not a date computation.
//!
//! Two phases, each bounded to MAX_THREADS worker threads: parse+
//! aggregate the input in parallel (one HashMap per thread, endpoints
//! interned to a small integer id to avoid a `Vec<u8>` allocation per
//! line), then merge the per-thread maps and sort by (hour, endpoint).

mod chunked;
mod format;
mod parse;

use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;

use chunked::{for_each_line_in_range, line_aligned_ranges};
use format::{write_group_line, GroupLine};
use parse::{hour_bucket_bytes, parse_u32};

const MAX_THREADS: usize = 12;
const INPUT_NAME: &str = "requests.log";
const OUTPUT_NAME: &str = "summary.txt";

#[derive(Default)]
struct Bucket {
    count: u32,
    errors: u32,
    latencies: Vec<u32>,
}

/// Per-thread aggregation state. Endpoints are interned to a small
/// integer id the first time this thread sees them, so the 3.1M-line
/// hot loop only allocates on an interning MISS (bounded by this
/// thread's distinct-endpoint count, <=1,842), never per line.
struct LocalState {
    endpoint_ids: HashMap<Vec<u8>, u32>,
    endpoint_names: Vec<Vec<u8>>, // indexed by id, for recovery at merge time
    buckets: HashMap<(u32, [u8; 20]), Bucket>,
}

impl LocalState {
    fn new() -> Self {
        LocalState {
            endpoint_ids: HashMap::new(),
            endpoint_names: Vec::new(),
            buckets: HashMap::new(),
        }
    }

    #[inline]
    fn intern(&mut self, endpoint: &[u8]) -> u32 {
        if let Some(&id) = self.endpoint_ids.get(endpoint) {
            return id;
        }
        let id = self.endpoint_names.len() as u32;
        self.endpoint_names.push(endpoint.to_vec());
        self.endpoint_ids.insert(endpoint.to_vec(), id);
        id
    }
}

fn process_line(line: &[u8], state: &mut LocalState) {
    if line.is_empty() {
        return;
    }
    // ts method endpoint status latency_us, single-space separated (grammar in spec section 2).
    let mut it = line.splitn(5, |&b| b == b' ');
    let ts = it.next().expect("missing ts");
    let _method = it.next().expect("missing method");
    let endpoint = it.next().expect("missing endpoint");
    let status = it.next().expect("missing status");
    let latency = it.next().expect("missing latency");

    let hour = hour_bucket_bytes(ts);
    let status_val = parse_u32(status);
    let latency_val = parse_u32(latency);
    let endpoint_id = state.intern(endpoint);

    let bucket = state.buckets.entry((endpoint_id, hour)).or_default();
    bucket.count += 1;
    if (500..=599).contains(&status_val) {
        bucket.errors += 1;
    }
    bucket.latencies.push(latency_val);
}

type GlobalMap = HashMap<([u8; 20], Vec<u8>), Bucket>;

/// Merge happens at a much smaller scale than the hot loop (at most
/// distinct-hours x distinct-endpoints x thread-count entries, not one
/// per line), so an owned `Vec<u8>` key here is fine.
fn merge_into_global(dst: &mut GlobalMap, local: LocalState) {
    for ((endpoint_id, hour), bucket) in local.buckets {
        let name = local.endpoint_names[endpoint_id as usize].clone();
        let entry = dst.entry((hour, name)).or_default();
        entry.count += bucket.count;
        entry.errors += bucket.errors;
        entry.latencies.extend_from_slice(&bucket.latencies);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(String::as_str).unwrap_or("/data");
    let out_dir = args.get(2).map(String::as_str).unwrap_or("/out");

    let input_path = format!("{data_dir}/{INPUT_NAME}");
    let file = File::open(&input_path).unwrap_or_else(|e| panic!("open {input_path}: {e}"));
    let file_len = file.metadata().expect("stat input").len();

    let ranges = line_aligned_ranges(&file, file_len, MAX_THREADS);

    let partials: Vec<LocalState> = std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| {
                let file_ref = &file;
                scope.spawn(move || {
                    let mut state = LocalState::new();
                    for_each_line_in_range(file_ref, start, end, |line| {
                        process_line(line, &mut state)
                    });
                    state
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker thread panicked"))
            .collect()
    });

    let mut buckets: GlobalMap = HashMap::new();
    for partial in partials {
        merge_into_global(&mut buckets, partial);
    }

    // (hour, endpoint) ascending: hour's fixed width makes byte order
    // chronological, and Vec<u8>'s Ord is unsigned-byte lexicographic
    // (spec section 4's exact ordering rule). Owned keys (small: one
    // per distinct hour/endpoint group, not per request) rather than
    // borrowed ones, so the write loop below can sort each group's
    // `latencies` in place through `get_mut` instead of cloning it
    // first just to have a mutable copy to sort.
    let mut keys: Vec<([u8; 20], Vec<u8>)> = buckets.keys().cloned().collect();
    keys.sort();

    std::fs::create_dir_all(out_dir).expect("create out dir");
    let out_file = File::create(format!("{out_dir}/{OUTPUT_NAME}")).expect("create output");
    let mut writer = BufWriter::new(out_file);

    for key in &keys {
        let bucket = buckets.get_mut(key).unwrap();
        bucket.latencies.sort_unstable();
        write_group_line(
            &mut writer,
            &GroupLine {
                hour: &key.0,
                endpoint: &key.1,
                count: bucket.count,
                errors: bucket.errors,
                sorted_latencies: &bucket.latencies,
            },
        )
        .expect("write output line");
    }
}
