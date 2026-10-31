# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, std-only. `requests.log` is mapped read-only via `mmap`
(`src/mmap_file.rs`, a hand-declared `extern "C"` call into libc --
already linked into every Rust binary on Linux, so not a new
dependency) instead of streamed through `read_at`/`seek_read`. It's
split into up to 12 line-aligned byte ranges (`src/chunked.rs`):
unlike Track C's CSV, this format has no embedded newlines inside a
field, so finding each range's boundary is a plain, independent,
parallel search for the nearest `\n` -- no quote-tracking, no
speculate-then-verify step needed at all.

Each of the 12 threads parses its own range by fixed-position byte
scanning (not a generic `split` iterator): the timestamp's 13-byte
hour prefix is read directly by position, `method` is scanned past
(never used in the output) rather than captured, and endpoint/status/
latency are extracted by straightforward forward scans for the next
space. Endpoints AND hour-buckets are each interned to small
per-thread integer ids; a `HashMap<u32, GroupAgg>` keyed by one packed
`u32` (`hour_id << 16 | endpoint_id`) tracks each group's count and
5xx error count -- a dense integer key, not a 20-byte string, so
there's no per-line string hash/compare. Every latency is pushed as
one packed `u64` (`local_group << 32 | latency`) into ONE flat
per-thread `Vec`, not a separate small `Vec` per group.

After all 12 threads finish, three cheap, one-time steps replace what
was previously a sequential merge-then-sort tail that cost more wall
time than the parallel parsing phase itself. Each step consumes the
previous one's per-thread state BY VALUE, not by reference: a thread's
`groups` HashMap and endpoint/hour name tables drop as soon as its
counts/errors are merged (step 1), and its own `latencies` drop as
soon as they're scattered into the shared array (step 2) -- `massif`
showed these otherwise staying alive, unreleased, all the way to the
end of `main`, overlapping with the very array they'd just been
copied into and with the percentile workers' own buffers (see
PERF.md).

1. Global endpoint and hour-bucket ids are assigned in SORTED byte
   order, so a packed `hour_global_id * num_endpoints +
   endpoint_global_id` "group id," iterated ascending, is already the
   exact required `(hour, endpoint)` output order -- no separate sort
   step (the same trick Track C's country-code packing uses).
2. A single O(total_lines) counting sort places every latency
   directly into its group's own slice of one contiguous `Vec<u32>`,
   via precomputed per-group starting offsets -- no hashing, no
   per-entry allocation, unlike extending thousands of small per-group
   `Vec`s.
3. p50/p95/p99 (exact nearest-rank, integer arithmetic, spec section
   3.4) are computed per group via `select_nth_unstable` -- O(k) per
   rank rather than O(k log k) for a full sort, since only three ranks
   are ever read -- in PARALLEL: the one big latencies array is split
   into up to 12 disjoint mutable sub-slices along group boundaries
   (plain safe `split_at_mut`, no unsafe, balanced by total entry
   count rather than group count since groups vary hugely in size
   under the spec's Zipf endpoint distribution), each thread handling
   its own contiguous range of groups.

`summary.txt` is then written in one pass, iterating group ids
ascending (already sorted), with the exact integer `error_ppm`
arithmetic from spec section 3.6.

Verified byte-identical (sha256) to the reference on seed 1 at both
`sample` and `full` scale, and against official holdout seeds 2/3/4.

## Dependencies

None (Rust standard library only).

## With another 24 hours

- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines
  it costs.
- The counting sort's scatter pass (step 2 above) is single-threaded;
  worth profiling on the real board/CI numbers for this exact design
  to see whether it's worth parallelizing too (it would need each
  thread to know its own precise per-group write offset ahead of
  time, not just a disjoint byte range like the percentile step gets
  for free).
