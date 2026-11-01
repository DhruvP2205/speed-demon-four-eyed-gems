# Speed Demon 2026 — Four-Eyed Gems

High-performance, correctness-first Rust solutions for the
[Speed Demon 2026](https://speeddemon.dev) programming competition.

The entry is organized as four independent track programs. Each program is
compiled and measured in its own container, reads only the files mounted at
`/data`, writes one required output file to `/out`, and must produce output
that is byte-for-byte identical to the official reference.

> **Current focus:** Tracks A, B, and D are actively optimized. Track C is
> intentionally retained but not part of the best-three scoring strategy.

## Highlights

- Rust with the standard library only: no runtime dependencies.
- Read-only memory mapping for large input files.
- Parallel parsing, indexing, aggregation, and query evaluation.
- Fixed-position and byte-oriented parsers on hot paths.
- Cache-conscious flat arrays, sparse workspaces, CSR graph storage, and
  specialized heaps.
- Official hash validation on seed 1 and holdout seeds before changes are kept.
- Dockerfiles pinned to allowlisted image digests and
  `target-cpu=x86-64-v3`; never `target-cpu=native`.

## Repository layout

| Path | Purpose |
|---|---|
| `track-a/` | Log ingestion, grouping, error rates, and latency percentiles |
| `track-b/` | Document indexing and BOOL/PHRASE query evaluation |
| `track-c/` | CSV join and aggregation track |
| `track-d/` | Weighted graph traversal and shortest paths |
| `ci/` | Reproducible Docker execution and benchmark scripts |
| `.github/workflows/` | Release tests, official hashes, holdouts, and benchmarks |
| `check.py` | Local Docker correctness and sandbox check |
| `LICENSE` | MIT license |

Each track contains its own `Dockerfile`, `Cargo.toml`, source tree, README,
and performance log. Track-local `PERF.md` files record experiments and
whether each candidate was kept or reverted.

## Track implementations

### Track A — ingest

`track-a/` maps `requests.log`, divides it into line-aligned ranges, and
parses those ranges concurrently. It uses fixed-position byte parsing,
per-thread aggregation, packed latency records, counting-sort scatter, and
parallel `select_nth_unstable` percentile calculation.

Output:

```text
/out/summary.txt
```

### Track B — index

`track-b/` parses queries before scanning documents, interns only terms used
by those queries, and builds presence postings for the needed vocabulary.
BOOL queries use sorted-list operations. PHRASE queries intersect candidate
presence lists and verify candidates by tokenizing their text directly from
the read-only document mapping; phrase position indexes are not stored.

Output:

```text
/out/results.txt
```

### Track C — join

`track-c/` is a complete Rust implementation of the CSV join track. It is
kept for reproducibility and correctness coverage, but current optimization
work prioritizes A, B, and D because scoring uses the best three tracks.

Output:

```text
/out/aggregates.txt
```

### Track D — traverse

`track-d/` builds a forward CSR graph from mmap-backed input, performs BFS
reordering for query locality, and answers shortest-path queries in parallel.
It uses a paged sparse workspace, early rejection for targets with no
incoming edge, and a radix heap specialized for monotonic Dijkstra keys.

Output:

```text
/out/distances.txt
```

## Runtime contract

Every submission must:

1. Read input only from `/data`.
2. Write exactly the track output file to `/out`.
3. Exit successfully with status `0`.
4. Produce the exact reference bytes.
5. Work with networking disabled and the root filesystem read-only.

The benchmark environment provides up to 12 hardware threads, 8 GB of memory,
no swap, a writable `/tmp` scratch filesystem, and a process limit. Builds
must be offline, use an allowlisted digest-pinned base image, finish within
the build limit, and keep the final image within the size limit.

## Validate locally without S3

The preferred local workflow uses generated or already-prepared template data.
No program in this repository downloads input data from S3 at runtime.

From the repository root:

```bash
# Build and run a local correctness check.
python3 check.py b --data path/to/track-b-data

# Supply the known expected digest to turn the check into a hard gate.
python3 check.py b \
  --data path/to/track-b-data \
  --expected <sha256>
```

For native Rust iteration:

```bash
cd track-b
cargo test --release
cargo build --release
./target/release/solution path/to/track-b-data path/to/output
sha256sum path/to/output/results.txt
```

The same pattern applies to tracks A, C, and D with their corresponding
output filename. On Windows PowerShell, use the equivalent executable path
and `Get-FileHash`:

```powershell
Get-FileHash .\path\to\output\results.txt -Algorithm SHA256
```

If using the supplied local template workspace, keep generated data outside
Git-tracked source directories. Large datasets are intentionally ignored by
`.gitignore`.

## Benchmark and optimization workflow

Performance changes are kept only after all of these gates:

1. Release unit tests pass.
2. Output hashes match local template fixtures.
3. Official seed-1 verification passes.
4. Holdout seeds pass without tuning against them.
5. The official benchmark shows a score-relevant improvement.
6. The change and its measured result are recorded in the track’s `PERF.md`.

The official workflow measures wall time, process execution time, and cgroup
`memory.peak`. Local Windows timings are useful for finding regressions but
are not a substitute for the Linux benchmark runner. A locally faster change
can still be rejected if it is slower under the official CPU and filesystem
constraints.

## Scoring model

Each track is scored independently. The score rewards lower time and memory,
with smaller source size and fewer direct dependencies contributing additional
terms. The overall result is the geometric mean of the best three track
scores.

Correctness is a hard requirement: a hash mismatch, timeout, sandbox
violation, or failed build produces no usable score. Consequently, measured
and reversible optimization is preferred over speculative shortcuts,
hardcoded data, network access, or benchmark-specific behavior.

## Container example

The track Dockerfiles use a two-stage offline build. A minimal Rust shape is:

```dockerfile
FROM rust:1-slim-trixie@sha256:<allowlisted-digest> AS build
WORKDIR /src
COPY . .
RUN cargo build --release --offline

FROM debian:13-slim@sha256:<allowlisted-digest>
COPY --from=build /src/target/release/solution /app/solution
ENTRYPOINT ["/app/solution"]
```

Do not use `target-cpu=native`, unpinned images, network fetches, or runtime
downloads. Check the event allowlist before changing a Dockerfile.

## Further reading

- [Speed Demon 2026](https://speeddemon.dev)
- [Official event repository](https://github.com/hackathon-raptors/speed-demon-2026)
- [Track A specification](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md)
- [Track B specification](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md)
- [Track C specification](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md)
- [Track D specification](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md)

## License

The project is released under the MIT license. See [LICENSE](LICENSE).
