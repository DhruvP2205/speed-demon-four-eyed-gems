# Track C: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

## Pre-board development measurements

Everything below predates any board run: local-machine timings and a private scratch repo's own CI runs, used to reason about and validate changes before this code ever reached the board. "Run ID" in this section refers to that scratch repo's CI, not the board -- treat every number here as directional only.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 14.44 | 367 | 0 |
| 1 | Rust port: parallel (12-thread) record-aligned CSV parse + per-thread aggregate + merge + sort + write. Reused `CsvReader` (RFC4180-style, `""`-escape, embedded newlines) and `strip_record_start_boms` from prior prep; fresh minimal `chunked::record_aligned_ranges` (single quote-nesting-aware scan, no speculative-boundary variants). | *(pending first push)* | 1.87 (local, full scale, not the board) | *(not yet measured on the board)* | |
| 2 | Added `ENV RUSTFLAGS="-C target-cpu=x86-64-v3"` to the Dockerfile build stage: explicit AVX2 baseline, never `native` -- confirmed via `board/steps.py`'s `native_cpu_flags()` regex this does not trigger the native-CPU opt-in (safe on Depot's AVX-512 builder or the scoring box either way). Re-verified hash match on `full` after rebuilding with the flag. | *(pending first push)* | 1.87 (local, not the board) | *(not yet measured on the board)* | |
| 3 | First real Linux cgroup `memory.peak` measurement (CI, full scale, seed 1): M was 646,654,635 bytes -- *worse* than Mref (367,435,776 B), an M-axis penalty rather than a gain. `valgrind --tool=massif` (CI, outside the track folder) confirmed why: at peak (443.15 MB total heap), ~43.7% (193.5 MB) was the per-thread `HashSet<Vec<u8>>` in `CountryAgg::add` -- both the hashset's own table growth (142.9 MB) and the `name.to_vec()` string copies (33.75 + 16.8 MB) -- because records are split by byte range, not by content, so the same name recurring anywhere in the file could be copied once per thread that happened to see it. | 41020047375 | 3.76 (CI, shared 2-core runner, not the board) | 646.65 MB (CI, real cgroup memory.peak) | |
| 4 | Replaced the per-thread name `HashSet<Vec<u8>>` with a global, sharded, exact arena `InternTable` (`src/intern.rs`, 64 shards, each behind its own `Mutex`): every distinct name's bytes are stored exactly once, globally, in a contiguous arena; a shard lookup is hash-then-exact-byte-comparison, never hash-only (a collision must never merge two different names, or `names_distinct` would silently corrupt and fail the hash gate). `CountryAgg.names` is now `HashSet<u64>` (compact ids) instead of `HashSet<Vec<u8>>`. Hash-matched (real Docker build on Linux CI, not just native) on `sample` and `full`. M dropped 646.65 -> 470.12 MB (-27%) and T dropped too (likely less hashing/fewer/smaller reallocations), but this is short of the <70 MB target and still worse than Mref (367 MB) -- the massif breakdown attributed 45.57% of the OLD peak to the unavoidable `fs::read` of the whole 204 MB file, so something beyond the name-HashSet (not yet profiled on this new build) still accounts for a large remainder; needs another massif pass on this exact build before the next memory round. | 41020766420 | 2.25 (CI, shared 2-core runner, not the board; down from 3.76) | 470.12 MB (CI, real cgroup memory.peak; down from 646.65, target was <70) | |
| 5 | Verified end-to-end against the real downloaded official seed-1 dataset (not a regenerated copy) and against seeds 2/3/4 (holdout, never tuned against) via `.github/workflows/verify-official.yml`: 15/15 CI jobs passed, including seed 2 at full scale through real Docker. Flagged, not yet fixed: this track is reported ~60x behind the leaderboard's T (0.037 s) -- the mutex-sharded `InternTable` added in step 4 is suspected as a hot-loop lock-contention source and is the next thing to redesign away. | CI run (private scratch repo, pre-board) | 2.25 (CI, shared 2-core runner, not the board) | 470.12 MB (CI, real cgroup memory.peak) | |

## Board runs

This repo is what the board actually builds. Every row below must carry a real board run ID -- no board run yet.

| Step | Change | Board run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| | *(awaiting first board run)* | | | | |
