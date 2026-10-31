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

`records.csv` is split into up to 12 record-aligned byte ranges.
The split (`src/chunked.rs`) tracks the same quote-nesting state as
the parser itself, so a target split point that would land inside a
quoted field (which may contain embedded newlines) is snapped forward
to the next out-of-quote `\n`; no record is ever cut across a thread
boundary. `CsvReader` (`src/csv_parser.rs`) is an RFC4180-style reader
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
   It does the real, exact dedup (hash then full byte comparison via
   `HashSet<&[u8]>`, never hash-only, so a collision can never
   silently merge two different names) and emits a per-country
   distinct count for its shard. Summing the 12 shards' counts per
   country gives the exact global `names_distinct`.

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

- Time profiling on the real board/CI numbers for this exact design,
  to find the next-largest cost (`CsvReader::next()` still allocates a
  fresh `Vec` per record for its field list, which this round left
  untouched since the dominant cost was the mutex, not that).
- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines it
  costs.
- `MADV_SEQUENTIAL` is currently applied to the whole mapping before
  any thread starts; worth checking whether per-thread range hints
  (each thread's own slice, not the whole file) help now that access
  is genuinely 12-way parallel rather than one sequential pass.
