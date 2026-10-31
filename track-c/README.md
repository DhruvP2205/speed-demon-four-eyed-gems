# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, std-only. Both input files are mapped read-only via `mmap`
(`src/mmap_file.rs`, a hand-declared `extern "C"` call into libc --
already linked into every Rust binary on Linux, so this is not a new
dependency) instead of read into an owned buffer: for the 204 MB
`records.csv` at `full`, `fs::read` is a real kernel-to-heap memcpy
(confirmed by `valgrind --massif` as a literal 200+ MB entry in the
peak-heap breakdown); `mmap` maps the page cache directly into the
process's address space, no copy.

`records.csv` is split into up to 12 record-aligned byte ranges via
`src/chunked.rs`'s `record_aligned_ranges`, which does NOT scan the
whole file sequentially to find them (an earlier version did, and
phase instrumentation showed that single-threaded scan alone costing
more than phases 1 and 2 below combined -- see PERF.md). Instead: each
of the 11 target split points is independently snapped forward to the
nearest RAW `\n` (parallel, not quote-aware -- a cheap guess, correct
for the overwhelming majority of targets since most lines aren't
inside a multi-line quoted field); the resulting chunks are scanned in
parallel, each assuming it starts at a field boundary; then one O(n)
sequential pass (not O(file size)) verifies each chunk's assumed entry
state against the previous chunk's REAL exit state, re-scanning only
the rare chunk whose guess was wrong. `CsvReader` (`src/csv_parser.rs`)
is an RFC4180-style reader
matching Python's own lenient `csv` module: `""`-escaped quotes,
embedded raw LF/CRLF inside quoted fields, mixed CRLF/LF terminators
between records, "trailing content after a closing quote is appended
literally, not re-quoted," and a BOM stripped only where it marks a
record start (byte 0, or right after a `\n`) -- skipped in place as
each record is read, with no separate whole-buffer pre-pass, so fields
borrow directly from the mapped bytes with no copy except where
`""`-unescaping forces one.

Of the nine output fields, eight (`count`, `id_sum`,
`amount_sum`/`min`/`max`, `name_bytes`, `note_bytes`) need no retained
per-record data -- just an O(1) update into a flat array, direct-
indexed by a dense country id. A `[A-Z]{3}` country code packs
injectively into `0..17,576` (`pack_country`), and the packing is also
order-preserving, so iterating that range ascending already produces
the fully sorted output order with no separate sort step.
`region_to_country` is a flat `Vec<u16>` built once, single-threaded,
from the small `regions.csv`, and read with direct, bounds-checked
indexing by `region_id` -- no hashing anywhere in the region join.

`names_distinct` is the one field that needs real deduplication, and
is the only reason there's a second phase:

1. **Phase 1** (12 threads, disjoint byte ranges, zero shared state):
   each thread parses its range straight off the mmap, updates its own
   flat aggregate array by direct index, and for each record's name
   computes `shard = hash(name) % 12` and pushes `(country, name)`
   into its own per-shard buffer (144 buffers total: 12 threads x 12
   shards). No hashmap, no lock, no shared mutable state anywhere in
   this phase.
2. **Phase 2** (12 threads, one shard each): a name's shard depends
   only on its own bytes, so every occurrence of a given name across
   all 12 phase-1 threads lands in the same shard index -- each
   phase-2 thread owns its shard exclusively and needs no lock either.
   No hashing at all: collects its shard's `(country, name)` pairs
   into one flat `Vec`, sorts it (still an exact byte comparison,
   never hash-only -- a collision can never silently merge two
   different names), and counts a new distinct name wherever
   consecutive sorted entries differ. An earlier version used a
   `HashMap<u16, HashSet<&[u8]>>` instead (first with std's default
   SipHash, then a hand-rolled FNV-1a hasher once phase 2 became the
   single largest remaining cost in the program) -- an interleaved,
   5-round A/B measured the sort consistently faster, non-overlapping
   timing ranges (see PERF.md): the two nested hash structures' own
   overhead (table growth, probing) cost more than the hashing itself
   did. Summing the 12 shards' counts per country gives the exact
   global `names_distinct`.

This replaces an earlier version that funnelled every name through a
single global, 64-way mutex-sharded `InternTable` -- correct, but a
lock-contention hot-loop cost across all 12 threads, which the first
board run's T pointed at directly.

`aggregates.txt` is written with cents formatted as `-`(if negative) +
whole + `.` + exactly 2 fractional digits -- no binary floating point
anywhere.

Verified byte-identical (sha256) to the reference on seed 1 at both
`sample` and `full` scale, and against holdout seeds 2/3/4, after
every change above.

## Dependencies

None (Rust standard library only).

## With another 24 hours

- `CsvReader::next()` still allocates a fresh `Vec` per record for its
  field list -- the next candidate hot-loop allocation to remove, per
  phase instrumentation on this exact build showing phase 1 (parsing)
  as the second-largest remaining phase after phase 2's dedup.
- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines it
  costs.
- `MADV_SEQUENTIAL` is currently applied to the whole mapping before
  any thread starts; worth checking whether per-thread range hints
  (each thread's own slice, not the whole file) help now that access
  is genuinely 12-way parallel rather than one sequential pass.
