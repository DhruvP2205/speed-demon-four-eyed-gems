# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, std-only. Whole `records.csv` (204 MB at `full`) is read into
memory once -- well inside the 8 GB budget -- then split into up to
12 record-aligned byte ranges. The split (`src/chunked.rs`) tracks the
same quote-nesting state as the parser itself, so a target split point
that would land inside a quoted field (which may contain embedded
newlines) is snapped forward to the next out-of-quote `\n`; no record
is ever cut across a thread boundary.

Each of the 12 threads:
1. Strips a BOM only where it marks a record start (`strip_record_start_boms`
   -- byte 0 or right after a `\n`; a BOM elsewhere is left as literal
   field content, since the spec only places it at record boundaries).
2. Parses records with `CsvReader` (`src/csv_parser.rs`), an
   RFC4180-style reader matching Python's own lenient `csv` module:
   `""`-escaped quotes, embedded raw LF/CRLF inside quoted fields,
   mixed CRLF/LF terminators between records, and "trailing content
   after a closing quote is appended literally, not re-quoted."
3. Looks up `region_id` in a small in-memory table built from
   `regions.csv`, and aggregates into a per-thread
   `HashMap<country_or_UNMATCHED, CountryAgg>` (count, id sum, amount
   sum/min/max in integer cents, a `HashSet` of distinct names, and
   summed name/note byte lengths). Each record's `name` is interned
   through a shared `InternTable` (`src/intern.rs`) before the
   distinct-name set ever sees it: a sharded (64-way), mutex-
   protected arena stores each distinct name's bytes exactly once,
   globally, and `CountryAgg.names` stores the compact 8-byte id
   instead of a copy of the name -- a real-world cgroup measurement
   found the naive per-thread `HashSet<Vec<u8>>` version costing
   646 MB (worse than the reference's 367 MB), since records are
   split by byte range rather than by content, so the same name
   recurring anywhere in the file got copied once per thread that
   happened to see it. Lookup is hash-then-exact-byte-comparison,
   never hash-only, so a hash collision can never silently merge two
   different names (which would corrupt `names_distinct` and fail
   the hash gate).

The 12 per-thread maps are then merged (sum the sums, min/max
properly, union the name-id sets), the country keys sorted as raw
bytes ascending (`_UNMATCHED`'s leading `_` sorts after any 3-letter
uppercase code, so no special-casing is needed), and `aggregates.txt`
is written with cents formatted as `-`(if negative) + whole + `.` +
exactly 2 fractional digits -- no binary floating point anywhere.

Verified byte-identical (sha256) to the reference on seed 1 at both
`sample` and `full` scale, after every change above.

## Dependencies

None (Rust standard library only).

## With another 24 hours

- Time profiling: CI showed this track furthest from its reference
  ratio of the three (relative to Tref), worth a closer look at
  where the time actually goes before assuming it's near-optimal.
- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines
  it costs.
- Re-check whether the single up-front `fs::read` of the whole file is
  still the right call at `full` scale, or whether streaming the
  record-aligned ranges directly off disk (as Track A does) would
  shave peak memory without costing wall time.
