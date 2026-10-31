# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Rust, std-only. Both input files are mapped read-only via `mmap`
(`src/mmap_file.rs`). The reference builds a full inverted index over
the corpus's whole 600,000-word vocabulary before looking at a single
query. This does the opposite, deliberately -- spec section 3 and
section 8 point 4 both call "indexing only the terms the queries use"
a legitimate strategy, not a shortcut:

1. **Parse `queries.txt` first** (`src/query_parse.rs`), before
   `docs.txt` is ever touched. Every distinct term across all 50,000
   BOOL/PHRASE queries is interned to a small integer id. At `full`
   scale this is ~75,700 distinct terms out of the corpus's 600,000-
   word vocabulary.
2. **Scan `docs.txt` once, in parallel** (`src/index.rs`): up to 12
   threads, each over a disjoint line-aligned byte range (one
   document per line, spec section 2.1 guarantees no embedded TAB/LF/
   CR, so this needs no quote-tracking, unlike Track C). Every
   document still has to be tokenized (`src/tokenize.rs`, byte-based,
   case-folding only ASCII, spec section 2.2's exact rule, which never
   needs to decode UTF-8 at all) -- there's no way to know a token
   matches a needed term without looking at it -- but only tokens that
   DO match a needed term are ever stored, via a lookup table keyed
   with a hand-rolled FNV-1a hasher (`src/fnv_hasher.rs`) rather than
   std's default SipHash, since this table is built from the query
   file's own terms, not untrusted input, and gets looked up once per
   token (36M times at `full`). Only PRESENCE is stored (which
   documents contain a term at least once) -- never per-document
   token POSITIONS: an earlier version kept a position index for
   PHRASE queries too, and massif showed it was 62% of peak heap
   (see PERF.md) for a structure that only ever gets consulted for
   the small subset of documents a PHRASE query's PRESENCE
   intersection narrows down to. Also built in this pass, at no extra
   scanning cost: `doc_offsets`, each document's absolute byte range
   in the mmap'd corpus, an 8-byte-per-document table (~2 MB at
   `full`) that step 4 uses to re-tokenize a PHRASE candidate
   directly off the mapping, never from a stored copy.
3. **Merge with a counting-sort scatter, not a comparison sort.** Each
   thread already pushes its own presence entries in ascending
   `doc_id` order (documents are scanned in file order within one
   thread's range, and the ranges themselves are in ascending doc-id
   order), so concatenating thread 0's, then thread 1's, ... entries
   for a given term is ALREADY fully sorted. An earlier version
   concatenated everything into one `Vec` and called `sort_unstable`
   on it instead -- measured costing 1.81s of a 2.67s total index
   build at `full` (~47M entries), by far the dominant cost. Replacing
   it with summing each thread's per-term counts into one histogram,
   prefix-summing it into offsets, then copying each thread's entries
   into their term's slot turns that into a single O(n) linear pass,
   cutting the index-build phase to well under a third of its former
   cost. `doc_offsets` is placed the same way, by direct `doc_id`
   index rather than a per-term histogram.
4. **Answer queries, in parallel.** BOOL queries are plain
   sorted-list set operations (`src/eval.rs`): AND = intersect, OR =
   union, NOT = difference, each O(n+m), applied left to right exactly
   as spec section 3 defines (no precedence). PHRASE queries find
   candidates by intersecting every term's PRESENCE list (smallest
   first, to shrink the working set fastest), then verify each
   candidate -- a tiny fraction of all documents, never the whole
   corpus -- by re-tokenizing its text sliced directly from the mmap
   via `doc_offsets`, checking for `k` consecutive token positions
   matching the query's terms in order (the same exact-match semantics
   the reference uses; a token skipped for exceeding `MAX_TOKEN_LEN`
   still consumes a position, so it correctly breaks adjacency even
   though it emits no token). The 50,000 queries are independent given
   the shared, read-only index, so they're answered across 12 threads
   too, each writing into its own slice of a result buffer indexed by
   original query position, so output order never depends on which
   thread finishes which query first.

Verified byte-identical (sha256) to the reference on seed 1 at `tiny`,
`sample` and `full` scale, against the spec's own worked example, and
against holdout seeds 2/3 at `sample` scale.

## Dependencies

None (Rust standard library only). No banned search engine, full-text
index or SQL engine is used (spec section 6) -- just `HashMap`,
sorting and plain sorted-list set operations.

## With another 24 hours

- `scan_docs` (tokenizing all 36M tokens at `full`, checking each
  against the needed-term table) is now the single largest remaining
  phase (~470ms of ~800ms total index build). The FNV hasher only
  shaved a little off it, which suggests the per-byte tokenization
  loop itself -- not the hashmap lookup -- is the real floor; worth a
  closer look at whether it can be SIMD-accelerated (bulk-classify
  `[a-z0-9]` runs the way Track C's bulk quote/comma/newline search
  does) rather than one branch per byte.
- The counting-sort scatter (`build_index`'s final pass) is currently
  single-threaded (~190ms at `full`); parallelizing it would need each
  thread to know its own precise per-term starting offset ahead of
  time (cumulative per-thread-per-term counts), the same idea Track
  A's parallel percentile split uses for safe disjoint `split_at_mut`
  slices.
- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines it
  costs.
