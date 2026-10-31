# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, std-only. Two phases, each bounded to 12 worker threads:

1. Split `requests.log` into up to 12 line-aligned byte ranges (no line
   split across a thread boundary), then stream each range in 1 MiB
   sub-chunks with `read_at`/`seek_read` -- no mmap, no loading the
   whole 239 MB file into memory. Each thread parses its own lines
   (hand-written `u32` parsing, no `str`/`String` conversion) and
   aggregates into a local `HashMap<(endpoint_id, hour_bytes), Bucket>`,
   interning each endpoint to a small integer id on first sight so the
   hot loop never allocates on a repeat endpoint.
2. Merge the 12 threads' maps into one, sort by `(hour, endpoint)`
   ascending (the hour bucket's fixed 20-byte width makes byte order
   the same as chronological order; `Vec<u8>` ordering is already
   unsigned-byte lexicographic, matching the spec's endpoint order
   rule exactly), then write `summary.txt` with the exact nearest-rank
   percentile and integer `error_ppm` arithmetic from spec section 3.

Verified byte-identical (sha256) to the reference on seed 1 at both
`sample` and `full` scale.

## Dependencies

None (Rust standard library only).

## With another 24 hours

- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines
  it costs.
- Re-check whether per-thread `HashMap` merge is still the bottleneck
  at `full` scale, or whether the percentile/sort step dominates once
  parsing is fast enough to be noise.
