//! Speed Demon 2026, Track B (Index). specs/b-index.md is the contract.
//!
//! Reads `docs.txt` (250,000 documents, 254 MB at `full`) and
//! `queries.txt` (50,000 BOOL/PHRASE queries) and writes
//! `results.txt`: one line per query, in order, with the match count,
//! the sum of matching ids, and the ten smallest.
//!
//! The reference builds a full inverted index over the corpus's whole
//! 600,000-word vocabulary before looking at a single query. This
//! does the opposite, deliberately: `query_parse.rs` reads
//! `queries.txt` FIRST and interns only the terms these 50,000
//! queries actually mention (spec section 3 and section 8 point 4
//! both call this out as a legitimate strategy, not a shortcut) --
//! every document still has to be tokenized once (there's no way to
//! know a token matches a needed term without looking at it), but
//! `index.rs` only ever STORES postings for that much smaller term
//! set, and only tracks per-document token POSITIONS (needed for
//! PHRASE matching) for terms actually used in a phrase, never for
//! BOOL-only terms.
//!
//! Both input files are mapped read-only via `mmap`
//! (`src/mmap_file.rs`); `docs.txt` is split into up to 12
//! line-aligned byte ranges and scanned in parallel (`index.rs`).
//! BOOL queries are answered by sorted-list set operations (AND =
//! intersect, OR = union, NOT = difference, all O(n+m)); PHRASE
//! queries are answered directly against position lists (`eval.rs`),
//! never by re-tokenizing a candidate document the way the reference
//! does. The 50,000 queries are independent given the shared,
//! read-only index, so they're answered in parallel too, each thread
//! writing its answers into its own slice of a result buffer indexed
//! by original query position (so output order doesn't depend on
//! which thread finishes which query first).

mod chunked;
mod eval;
mod fnv_hasher;
mod format;
mod index;
mod mmap_file;
mod query_parse;
mod tokenize;

use chunked::line_aligned_ranges;
use index::build_index;
use mmap_file::MappedFile;
use query_parse::parse_queries;

const MAX_THREADS: usize = 12;
const DOCS_FILE: &str = "docs.txt";
const QUERIES_FILE: &str = "queries.txt";
const OUTPUT_FILE: &str = "results.txt";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(String::as_str).unwrap_or("/data");
    let out_dir = args.get(2).map(String::as_str).unwrap_or("/out");

    let queries_path = format!("{data_dir}/{QUERIES_FILE}");
    let qmapped = MappedFile::open(&queries_path, true)
        .unwrap_or_else(|e| panic!("open {queries_path}: {e}"));
    let query_index = parse_queries(qmapped.as_slice());

    let docs_path = format!("{data_dir}/{DOCS_FILE}");
    let dmapped =
        MappedFile::open(&docs_path, true).unwrap_or_else(|e| panic!("open {docs_path}: {e}"));
    let docs_data = dmapped.as_slice();
    let ranges = line_aligned_ranges(docs_data, MAX_THREADS);
    let index = build_index(
        docs_data,
        &ranges,
        &query_index.term_bytes,
        &query_index.needs_position,
    );

    let num_queries = query_index.queries.len();
    let thread_count = MAX_THREADS.min(num_queries.max(1));
    let chunk_ranges = even_chunks(num_queries, thread_count);

    let mut results: Vec<Vec<u32>> = Vec::with_capacity(num_queries);
    results.resize_with(num_queries, Vec::new);

    let index_ref = &index;
    let queries_ref = &query_index.queries;
    let per_thread: Vec<(usize, Vec<Vec<u32>>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunk_ranges
            .iter()
            .map(|&(start, end)| {
                scope.spawn(move || {
                    let answers: Vec<Vec<u32>> = queries_ref[start..end]
                        .iter()
                        .map(|q| eval::eval(index_ref, q))
                        .collect();
                    (start, answers)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("query worker panicked"))
            .collect()
    });
    for (start, answers) in per_thread {
        for (i, answer) in answers.into_iter().enumerate() {
            results[start + i] = answer;
        }
    }

    std::fs::create_dir_all(out_dir).expect("create out dir");
    let out_file =
        std::fs::File::create(format!("{out_dir}/{OUTPUT_FILE}")).expect("create output");
    let mut writer = std::io::BufWriter::new(out_file);
    for result in &results {
        format::write_result_line(&mut writer, result).expect("write output line");
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
