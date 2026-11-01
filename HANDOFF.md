# HANDOFF

Repo root, outside every `track-*/` folder on purpose -- this file does not
count toward any track's S. Written for an agent with zero prior context on
this session. Keep it accurate; update it at the end of every phase.

Last updated: 2026-11-01, after Track A percentile-output validation,
by the agent executing the MISSION in the section
below.

## 0. The mission in force right now

The user gave a phased mission to maximize overall score before the team's
own freeze (2026-11-02 10:00 UTC) and the official freeze (2026-11-02 15:00
UTC). Work through phases 1-6 **in order**, autonomously, pushing to
`speed-demon` after every kept change, reporting briefly at the end of each
phase, and stopping only for a failed gate or a decision only the user can
make. See section 4 for the phase list and section 3 for current progress.

## 1. Scoring (official, as given by the user -- this supersedes anything
older)

```
K = (Tref/T)^0.50 x min(Mref/M, 50)^0.20 x min(Sref/S, 1)^0.15 x min(1/D, 1)^0.15
points = 50 x log10(K), clamped to [0, 100]
overall = geometric mean of the best THREE of the four tracks' points
```

- **T** (wall time, mean of middle 3 of 5 runs): UNCAPPED. Dominates (weight
  0.50). Each 2x speedup is worth about +7.5 points on its own.
- **M** (cgroup `memory.peak`): capped at 50x (so once you're 50x under
  Mref, more M improvement stops mattering to K, though it's still worth
  having headroom). Weight 0.20.
- **S** (source lines, via `scc`'s Code column, i.e. excluding comments and
  blanks): capped at factor 1.0 -- beating Sref gives NO bonus, only matching
  or exceeding it can cost you. Weight 0.15. This means **lines are a pure
  tax**: every line added must be justified by what it does for T or M.
- **D** = 1 + direct non-stdlib dependencies. All four tracks currently
  carry zero dependencies (empty `[dependencies]` in every `Cargo.toml`), so
  D = 1 everywhere and this term is always at its best possible value
  (`min(1/1,1)^0.15 = 1`) -- do not add a dependency without recomputing
  whether it's worth the D hit, which is almost never only possible to
  justify this late.

Seed-1 calibration (Tref/Mref/Sref), per track:

| Track | Tref (s) | Mref | M cap (Mref/50) | Sref (lines) |
|---|---|---|---|---|
| A | 32.05 | 228 MB | ~4.6 MB | 56 |
| B | 56.35 | 2.2 GB | ~44 MB | 91 |
| C | 14.44 | 367 MB | ~7.3 MB | 108 |
| D | 72.69 | 893 MB | ~17.9 MB | 43 |

### Break-even rule for any change that adds lines

Solving for where S's line-cost cancels T's gain (weight 0.15 vs 0.50):
an optimization that grows the code by x% (S -> S*(1+x)) needs at least a
`(1+x)^0.3` wall-time speedup (T_old/T_new) to be worth keeping. Anything
slower than that is a net score loss even if it is genuinely faster in
absolute terms. Dead code, an unused tuning knob, an experimental branch
nobody measures as better -- these cost S with zero T contribution and are
pure losses under this rule; see Phase 6.

## 2. Repo layout, workflow, and how to run things

```
.
|- check.py            starter correctness/sandbox check (see below)
|- LICENSE              MIT -- already present, already correct
|- ci/
|  |- docker-run.sh     exact official `docker run` invocation (cpuset 0-11,
|  |                    8g mem/no swap, no network, read-only root, 1g tmpfs)
|  |- bench.sh          5 runs, drop min/max, mean of middle 3; sha256 gate;
|  |                    auto-reruns once if wall-time spread > 5%
|  |- Dockerfile.bench  CI-only: same build as the submission Dockerfile,
|  |                    entrypoint wrapped by mem_wrapper.sh so bench.sh can
|  |                    read real cgroup memory.peak before teardown
|  |- mem_wrapper.sh    the wrapper itself (CI-only, never shipped)
|- track-a/ track-b/ track-c/ track-d/
|  |- Dockerfile        multi-stage: rust:1-slim-trixie (pinned by digest)
|  |                    build, debian:13-slim final stage, binary named
|  |                    "solution", RUSTFLAGS="-C target-cpu=x86-64-v3"
|  |- Cargo.toml        opt-level=3, lto="fat", codegen-units=1,
|  |                    panic="abort"; NO [dependencies] section (D=1)
|  |- src/              all code; `src/format.rs`-equivalent output-writing
|  |                    module per track for spec-dependent formatting
|  |- README.md         approach, design decisions, "with another 24 hours"
|  |- PERF.md           every step, with board run IDs, kept or reverted
|- .github/workflows/
   |- track-a.yml track-b.yml track-c.yml track-d.yml
   |     cargo test --release, then: clone the official event repo, generate
   |     full-scale seed-1 data with the OFFICIAL generator, run the OFFICIAL
   |     python reference for the expected hash, build the submission image
   |     exactly, run it with docker-run.sh's real sandbox flags, hash-check,
   |     then valgrind massif (heap profile) and ci/bench.sh (wall/exec T +
   |     real Linux cgroup M) -- all on a shared 2-core GitHub runner, NOT
   |     comparable 1:1 to the 12-core scoring board (see pitfalls below).
   |     Triggered on push to files under that track/, ci/, or the workflow
   |     file itself.
   |- verify-official.yml
         official-seed1: downloads the REAL seed-1 input files from S3,
         verifies them against the published SHA256SUMS.json, runs check.py
         with --expected the official output hash, then bench.sh for M.
         holdout-robustness: seeds 2/3/4 (never tuned against) at sample
         scale plus seed 2 at full scale, generated fresh from the official
         generators with a freshly-run official reference for ground truth
         -- the overfitting check. Both jobs run on every push to any
         track-*/, ci/, or this workflow file.
```

**The official event repo** (generators + Python reference + specs) is
`https://github.com/hackathon-raptors/speed-demon-2026` -- cloned fresh by
CI every run, never vendored locally. Generators: `generators/<module>.py
--seed N --scale {sample,full} --out <dir>`; module names are
`a_ingest`, `b_index`, `c_join`, `d_traverse`. Reference:
`reference/<module>.py --data <dir> --out <dir>`. Official seed-1 data for
the live scoring S3 bucket is at
`https://s3.amazonaws.com/raptors.dev/speeddemon.dev/2026/data/track-<x>/full-seed1/`,
with a master `SHA256SUMS.json` at the bucket root giving both input and
expected-output hashes per track.

**The board** is external and not this repo -- "push to speed-demon" is the
only lever here; the board itself triggers on that push and is the final
judge of K (rule 4 below). We cannot query the board directly; the user
relays board run IDs/numbers back into the conversation.

### Running hash checks locally

```sh
# Starter-provided check: builds the track's own Dockerfile exactly
# (--network=none), runs it with the board's sandbox flags, hashes /out.
python3 check.py <a|b|c|d> --data <dir-with-that-track's-input-files> [--expected <sha256>]

# Full local dev loop, native (no Docker), fast iteration:
cd track-<x> && cargo test --release && cargo build --release
./target/release/solution <data-dir> <out-dir>
sha256sum <out-dir>/<output-file>   # compare by hand against a reference run

# Faithful dry run of the EXACT official docker run invocation (no M):
bash ci/docker-run.sh <image> <data-dir> <out-dir> [TRACK]

# Full T + M measurement via the wrapped bench image (needs Dockerfile.bench
# assembled into a build context with mem_wrapper.sh copied alongside, see
# any track-*.yml's "Assemble the bench-only build context" step):
bash ci/bench.sh <image> <data-dir> <out-base-dir> <expected-sha256-file> <track> [output-filename]
```

There is no official data checked into this repo (no `data/` directory) --
either download the real seed-1 files from the S3 paths above, or clone the
official event repo and run its generators/reference yourself.

### dockerfile_check.py / allowlist / x86-64-v3

`check.py` at the repo root is the correctness+sandbox check; its FROM-line
allowlist logic (`from_problems`, `allowlisted_digests`) is the
"dockerfile_check.py" the mission rules refer to: every `FROM` not an
earlier build stage must be pinned by `@sha256:` digest AND that digest
must appear in the board's own `https://speeddemon.dev/allowlist.json`.
Currently every Dockerfile uses:
```
FROM rust:1-slim-trixie@sha256:... AS build
...
FROM debian:13-slim@sha256:...
```
both pinned, both expected to be allowlisted (confirm against the live
allowlist URL before freeze -- `check.py` fetches it automatically and
fails loud if a digest isn't on it, but only when it can reach the network;
offline it just skips the check with a note).

RUSTFLAGS is `-C target-cpu=x86-64-v3` in every Dockerfile, NOT `native`
-- see pitfall below for why. PGO is not yet implemented anywhere (Phase 5);
when it is, the plan (per every track's README "with another 24 hours"
section already written) is: train on a seed other than 1, using the
organiser's allowlisted `llvm-tools` image, commit the resulting
`.profdata`, build with `-Cprofile-use`, keep only if the board improves.

## 3. Current state per track

Numbers below are the MISSION's fresh real board numbers (given directly by
the user, current as of 2026-10-31) unless marked otherwise. "Lines" is the
mission's reported board S; where it differs from a fresh local `scc`
count, both are given -- `scc` run on `track-<x>/src` reports its `Code`
column (excludes comments/blanks), which is what the board's own S count
should match modulo which commit each was measured on.

**Overall board score at mission start: 47.98** (best three of A/B/D, since
C was behind and dropped). Leaders' numbers (same board, other teams) are
in the targets table.

### Track A -- Ingest (`a_ingest`, `requests.log` -> `summary.txt`)

- **Design**: mmap `requests.log`; up to 12 line-aligned byte-range workers;
  per-thread endpoint+hour interning to small u32 ids; one packed `u32` key
  (`hour_id<<16 | endpoint_id`) indexes a flat `GroupAgg` array (no string-
  keyed HashMap); latencies pushed as packed `u64` (`local_group<<32 |
  latency`) into one flat per-thread Vec; global ids assigned in sorted
  byte order so output order falls out for free; one O(n) counting-sort
  scatter places every latency into its group's slice of one contiguous
  array; p50/p95/p99 via `select_nth_unstable` (O(k) not O(k log k)),
  computed in parallel across 12 threads via safe `split_at_mut` on
  disjoint group ranges.
- **Board (pre-Phase-2)**: T 0.142 s, M 69 MB, lines 659 (`scc`: 651 Code
  lines), 56.0 pts (commit `798624b`, logged in `track-a/PERF.md` step 3).
- **Leader**: T 0.023 s, M 22 MB.
- **Target** (mission, Phase 2): T <= 0.10 s, M <= 25 MB.
- **Phase 2 memory fix shipped (commits `7f0b249`, `6da99c1`, pushed)**:
  massif on the pre-fix build (CI run `41180038148`) showed its peak
  (53.86 MB useful-heap) had FOUR things alive simultaneously: each
  thread's `latencies: Vec<u64>` (46.11%/25.17MB), the final counting-sort
  array `big_latencies` (22.72%/12.40MB), the per-thread `groups` HashMap's
  table growth (9.02%/4.93MB), and the percentile workers' own result
  buffers (8.65%/4.72MB) -- `partials` (every thread's latencies AND
  groups AND endpoint/hour tables) was staying alive, unreleased, all the
  way to the end of `main`, overlapping with structures it didn't need to
  overlap with. Same pattern as Track B's step-4 fix. Restructured both
  merge passes to consume per-thread state BY VALUE: `groups`/name tables
  drop right after their counts/errors are merged; each thread's
  `latencies` Vec drops right after its own contribution is scattered.
  Verified byte-identical output (sha256) on a synthetic 500K-line dataset
  between pre- and post-fix binaries; all 14 unit tests pass unchanged.
  CI confirmed (`verify-official` run `41241108818` PASS; `track-a` run
  `41241108725`): massif peak 53.86 -> 40.66 MB (-24.5%), CI cgroup
  `memory.peak` 67.45 -> 39.22 MB (-41.8%). CI's wall/exec T moved
  slightly worse (0.636/0.411s -> 0.697/0.531s) but this is a memory-only
  change and local T was unchanged within noise -- almost certainly CI
  noise, same caveat logged everywhere else in this repo for memory-only
  changes measured on the 2-core runner.
  - **Remaining floor, NOT pursued further this round**: the NEW peak is
    61.85% (25.17MB) just the per-thread `latencies: Vec<u64>` growth
    DURING THE PARSE PHASE ITSELF (before any merge) -- a structural
    floor, not a leak: 3.1M entries x 8 packed bytes (32-bit group id +
    32-bit latency, spec's own type) is close to the practical minimum
    for holding every raw value before exact nearest-rank percentiles can
    be computed. Two riskier ideas documented in `track-a/README.md`'s
    "with another 24 hours" (tighter group-id bit-packing if real
    cardinality is confirmed well under 32 bits; an exact latency
    histogram instead of raw storage, only safe if the real value range
    is confirmed tight) -- NOT attempted, since both require assumptions
    about the data not explicitly guaranteed by spec, and this round's
    budget favored the safer, already-substantial win.
  - **Still short of the <=25MB target** (~39-40MB vs 25MB) but a huge
    step down from 69MB. The remaining ~15MB gap is mostly
    `big_latencies` (12.4MB, also structurally required for the parallel
    percentile step) -- likely not worth further risk this round.
  - **Mission's other Phase 2 bullets NOT done** (de-prioritized in favor
    of the bigger, already-confirmed memory win above, given Phase 2's
    ~5h budget): per-thread open-addressing endpoint interning (currently
    `HashMap<Vec<u8>,u32>` -- cardinality is tiny, ~1,842 endpoints, so
    expected ROI is low relative to its rewrite risk); parallelizing the
    counting-sort scatter.
- **Phase 2 considered done for this round** (task #21) -- a decisive,
  CI-confirmed memory win shipped; moving to Phase 3 (Track D) per the
  mission's "keep going phase to phase" instruction. Still awaiting a
  board run to confirm the real M/points; relay the next board number and
  update the numbers above once in hand.
- **Post-phase memory/lifetime optimization (commit `e041d79`, pushed)**:
  percentile workers now write directly into their disjoint final output
  slices instead of allocating per-thread result vectors and serially
  merging them. Release tests passed; official seed-1 and all holdout
  correctness jobs passed in verification run `41264778829`. The shared
  2-core CI measurement was wall 0.7076 s, exec 0.5381 s, and 39.22 MB.
  Keep pending the board's Track A score; Track C remains intentionally
  skipped.

### Track B -- Index (`b_index`, `docs.txt`+`queries.txt` -> `results.txt`)

- **Design**: parse `queries.txt` first (interns only the ~75,700 terms the
  50,000 queries actually use, out of 600k vocabulary -- spec-sanctioned);
  mmap `docs.txt`, scan in parallel building PRESENCE postings only
  (u32 doc-id lists per term) via a counting-sort scatter (no comparison
  sort); BOOL = sorted-list AND/OR/NOT, left-to-right, no precedence;
  PHRASE = rarest-first PRESENCE intersection for candidates, then
  **as of commit `0465a16`, re-tokenize each candidate directly off the
  mmap** via a new `doc_offsets: Vec<(u32,u32)>` table (doc_id -> absolute
  byte range), never a stored per-document token-position index.
- **Board (pre-redesign)**: T 1.084 s, M 835 MB, lines 1084, 39.0 pts --
  this was the LOWEST counted track and the mission's top-leverage target.
  Logged in `track-b/PERF.md`'s Board runs table, step 5.
- **Leader**: T 0.284 s, M 43 MB.
- **Target** (mission, Phase 1): T <= 0.30 s, M <= 50 MB (exit Phase 1
  early if B reaches >= 60 pts on the board).
- **Phase 1a shipped (commit `0465a16`, pushed)**: removed the
  per-document token-POSITION index entirely. `valgrind massif` on the
  PRE-redesign build (CI run `41183747430`) showed its true peak (814.2 MB
  heap) was 62% position-related: 37.09% (302.0 MB) the per-thread
  `positions: Vec<(u32,u32,u32)>` in `scan_docs`, plus 24.54% (199.8 MB)
  the final `position_pairs` array in `build_index`. Replaced with the
  `doc_offsets` table (~2 MB) plus re-tokenize-on-candidate verification
  (mirrors the reference's exact adjacency semantics, including an
  oversized/skipped token correctly breaking adjacency -- covered by new
  tests). `needs_position` removed end-to-end (dead once nothing stores
  positions). All 43 unit tests pass (37 prior + 6 new `eval_phrase` edge
  cases). `scc` reports 1058 Code lines (down from the pre-redesign 1084).
  CI confirmed (`verify-official` run `41239004719` PASS; `track-b` run
  `41239004748`): massif heap peak 814.2 -> 482.9 MB (-40.7%), CI cgroup
  `memory.peak` 699.01 -> 499.59 MB (-28.5%), CI wall 4.671 -> 3.938 s
  (-15.7%, a free bonus -- removing position tracking also cut
  `scan_docs`'s per-token work).
- **Phase 1b shipped (commit `9cacecd`, pushed)**: diagnosed the
  post-1a build with temporary phase instrumentation (reverted before
  commit) on a SYNTHETIC full-scale-sized dataset -- a sandbox policy
  blocked running the officially cloned event repo's generator script
  without explicit authorization, so this was a same-format proxy for
  relative timing only, not hash-matched or absolute. Breakdown:
  `build_index` 70-75% of total, `scan_docs` 70-80% of `build_index`'s own
  time; `eval_queries` (what Phase 1b's own bullet list focuses on --
  parallel eval, atomic work counter, galloping intersection) is already
  only ~2-3% of total, nothing to gain there, matching the mission's
  "exit when the budget is used" clause more than its bullet list. Found
  and fixed one real win in `scan_docs`'s dominant cost: `tokenize.rs`'s
  `for_each_token` classified a token's bytes once (via `is_token_byte`,
  itself calling `to_lower`) then copied+lowered them AGAIN in a second
  loop -- `to_lower` computed twice per byte. Fused into one pass, net
  -5 lines (an S improvement, not a cost). Interleaved 5-round local
  12-thread A/B isolating `scan_docs`: fused beat baseline in EVERY
  round, ~15% faster on that sub-phase. All 43 tests still pass
  (behavior is identical, just computed once). CI's 2-core runner barely
  moved (3.938->3.842s wall, 499.59->499.73MB M) -- expected and NOT a
  sign the fix is wrong: this is the same CI-2-core-vs-12-thread-board
  divergence Track D hit (see pitfalls section); kept on the strength of
  the paired local signal, awaiting the board's own T.
  - **Remaining known bottleneck, NOT pursued this round**: `scan_docs`'s
    per-byte branch-per-byte classification itself (not the hashing, not
    the (now-fixed) double-lowercasing) is likely the real remaining
    floor. SIMD bulk-classification of `[a-z0-9]` runs (the way Track C's
    `find_next_quote`/`find_next_special` do for CSV) is the documented
    next idea in `track-b/README.md`'s "with another 24 hours" section --
    a bigger rewrite than fit in this round's time budget; revisit in
    Phase 5 if time remains, or leave for "with another 24 hours."
- **Phase 1 (1a+1b) considered done for this round** -- both the explicit
  Phase 1b bullet-list items were already satisfied by the existing
  design (nothing to change there), and the one real remaining lever
  (SIMD tokenization) is out of scope for the budget. Moving to Phase 2
  per the mission's "keep going phase to phase" instruction. Still
  **awaiting a board run** to confirm whether B actually reached its
  <=0.30s/<=50MB targets or >=60pts -- get the user to relay the next
  board number and update both PERF.md and this file's numbers above
  once it's in hand.
- **Tasks #19 (Phase 1a) and #20 (Phase 1b): both should be marked
  completed** (task-tracker IDs are session-local; re-derive from this
  file plus PERF.md if the tracker is gone).

### Track C -- Join (`c_join`, `records.csv`+`regions.csv` -> `aggregates.txt`)

- **Design**: mmap both files; `records.csv` split via snap-to-newline
  speculation (parallel, not quote-aware) + one sequential verify pass that
  only re-scans a chunk whose guess was wrong; RFC4180-style `CsvReader`
  (`""`-escapes, embedded LF/CRLF, mixed terminators, BOM stripped in place
  at record start); region join via a direct-indexed flat `Vec<u16>`
  (`pack_country`, an injective AND order-preserving `[A-Z]{3}` -> `0..
  17576` packing, so iterating ascending already yields sorted output, no
  sort step); 8 of 9 output fields are O(1) flat-array updates; the 9th
  (`names_distinct`) uses a two-phase, lock-free design: phase 1 shards
  `(country, name)` by `hash(name) % 12` into per-thread-per-shard buffers
  (zero shared state); phase 2 (12 threads, one shard each, no lock needed
  since a name's shard depends only on its own bytes) **sorts** each
  shard's pairs and counts distinct-by-byte-comparison runs (NOT a
  HashMap/HashSet -- see "tried and reverted" below).
- **Board**: T 0.345 s, M 253 MB, lines 1203, 34.3 pts -- currently the
  DROPPED track (not in the best-3). `scc` now reports 1241 Code lines
  (close to the board's 1203; small drift, re-measure before freeze).
- **Leader**: T 0.037 s, M 12 MB.
- **No explicit target given** -- Phase 4 is conditional: "only if C can
  plausibly beat our lowest counted track" (currently that's whichever of
  A/B/D ends up lowest after Phases 1-3; re-evaluate then). Phase 4
  targets if pursued: T <= 0.12 s, M <= 40 MB.
- **Tried and reverted / superseded** (see `track-c/PERF.md` steps 3-5):
  per-thread `HashSet<Vec<u8>>` for dedup -> replaced with a global
  mutex-sharded `InternTable` (worse M than Mref, lock contention
  suspected) -> replaced with the lock-free two-phase shard design above
  -> that phase 2's `HashMap<u16,HashSet<&[u8]>>` tried with std SipHash,
  then a hand-rolled FNV-1a (modest win), then **replaced entirely with
  the sort-based dedup** (interleaved 5-round A/B, non-overlapping ranges,
  ~15-20% faster than even the FNV-hashed version) -- this is what's
  shipped now (commit `5d85905`, CI run `41185182257`: wall 1.600s/exec
  1.435s/M 258.71MB).
- **Known bottleneck / next steps** (per README "with another 24 hours",
  the parser now reserves the schema's five fields per record
  (`track-c/src/csv_parser.rs`), removing repeated capacity growth.
  Official hash and holdout verification passed in `41255127247`, and the
  full-scale Track C workflow `41255127261` passed with CI measurements of
  1.2495s wall, 1.0939s exec, and 261,382,144 bytes memory. A board run is
  still needed before treating this as a score win.
- **Phase 4 not started** (conditional on Phases 1-3's outcome). The
  available external Track C fixture uses an older contract and is not a
  valid benchmark for this submission.

### Track D -- Traverse (`d_traverse`, `graph.txt`+`queries.txt` -> `distances.txt`)

- **Design**: mmap `graph.txt`; parallel TWO-PASS CSR build (pass 1: atomic
  per-node degree counts via `Vec<AtomicU32>`, no parsing of `v`/`w`; pass
  2: full parse, atomic per-node cursor claims each edge's final CSR slot
  directly -- the parsed `(u,v,w)` triples are never stored as a separate
  structure); `has_in_edge` bitmap set inline in pass 2 for the O(1)
  no-in-edge -> -1 short circuit; BFS-order node renumbering for cache
  locality (forward-only, since the spec's main body is strongly
  connected) -- confirmed by measurement to matter hugely (identity
  mapping: queries ~380ms -> ~2.7s, a 7x regression) so it stays; per-query
  workspace is a paged sparse array (`PAGE_SIZE=1024`, swept against 256
  and 4096 and chosen as the winner) with direct index arithmetic, no
  hashing; `u64::MAX` sentinel avoids a separate stamp field; dynamic
  per-query work distribution via a shared `AtomicUsize` counter (measured
  97% parallel efficiency, i.e. load-balancing was never the problem).
- **Board**: T 0.631 s, M 112 MB, lines 927, 50.5 pts. `scc` reports 919
  Code lines (close). Board run IDs in `track-d/PERF.md`'s Board runs
  table: step 0 is commit `82161a6` (T 0.772/M 201/46.3pts); step 1 (the
  two-pass CSR + confirmed-necessary BFS reorder, commit `e7fe313`) is
  logged as `*(pending push)*` with only CI numbers (2-core CI *regressed*
  wall 2.689s->3.358s while M improved 235->112MB -- diagnosed as the
  two-pass parse doing ~1.5x the total parsing work, a net win at 12
  threads but a net loss at CI's 2 -- **confirm this already reached the
  board and get a real board run ID into that row**, since the currently
  quoted board T 0.631/M 112 looks like it already reflects this change
  (M matches CI's 112.18MB closely) but the PERF.md row itself still says
  "pending push").
- **Leader**: T 0.127 s, M 71 MB.
- **Target** (mission, Phase 3): T <= 0.17 s.
- **Known bottleneck / next steps**: Phase 3 asks for a fresh phase
  breakdown (parse/CSR build/in-edge bitmap/reorder/queries/output) on the
  CURRENT code first, since the two-pass rewrite changed the balance
  between load and query phases -- do not assume step-1's pre-rewrite
  breakdown (CSR build was the single biggest piece before pass 1/2 were
  split) still holds. If queries still dominate: BinaryHeap -> radix/4-ary
  heap is the mission's suggested next move. If load dominates: the
  two-pass mmap parse is already in place; the open item is re-measuring
  on the BOARD (not CI's 2 cores) whether BFS reorder still pays given the
  two-pass build's own new cost profile -- it was confirmed necessary on
  the OLD single-pass build (7x regression without it) but has not been
  re-A/B'd since the two-pass rewrite landed.
- **Phase 3 status**: attempted; an adaptive bidirectional/ALT port was
  reverted after official full-scale hash mismatch (CI runs
  `41252144661` and `41252144598`). The current known-correct implementation
  remains the 4-ary-heap version from `f6a186a`. A focused post-phase
  optimization (`dd4d718`) now writes query answers directly into disjoint
  result slices, removing per-thread result vectors and the serial merge.
  Release tests passed; official seed-1 and holdout verification passed in
  `41265246669`, and the focused workflow `41265246661` measured 2.9121 s
  wall, 2.7411 s exec, and 111,261,013 bytes on the shared 2-core runner.

### Recent validation notes

- Track B tokenizer fusion was attempted in `c974109`, passed correctness,
  but regressed official CI wall time to 5.0294s (`41255656816`) versus the
  prior ~3.84s baseline; it was reverted in `12b80f6`. The restored tree
  passed Track B verification in `41256132427` and Track B CI in
  `41256132483`.
- A second Track B experiment in `085a62b` used
  `available_parallelism()` to avoid 12-worker oversubscription on CI. It
  reduced CI memory to 485,983,573 bytes but worsened wall time to 5.3816s
  (`41256786776`), so it was reverted in `41a63c7`. The restored tree passed
  verification `41257254868` and Track B CI `41257254870`.
- Track B query workers now write directly into disjoint slices of the final
  results table instead of collecting private answer vectors and copying
  them serially. Commit `6c146f9` passed official verification
  `41258093523` and Track B CI `41258093585`: wall 3.2249s, exec 3.0716s,
  memory 498,372,608 bytes on the 2-core runner. This is the current best
  Track B implementation; board confirmation remains authoritative.
- Track D restored tree passed official verification `41252576250` and
  Track D CI `41252576253` (full-scale hash matched; 2-core CI wall 2.1280s,
  memory 111,282,859 bytes). Documentation-only follow-up is in `8c3f92a`.

### Line-count reconciliation note

Mission-reported board lines (A 659, B 1084-pre-redesign, C 1203, D 927)
and this session's fresh local `scc track-<x>/src` runs (A 651, B 1058
post-redesign, C 1241, D 919) are close but not identical for every track
-- expected, since they're measured on different commits and `scc`'s own
version/config can drift. Before freeze, re-run `scc` on the exact commit
about to ship and reconcile any track whose drift is more than a few
lines (C's 1203 vs 1241, a 38-line/~3% gap, is the one furthest apart and
worth a direct look).

## 4. Phased optimization plan (mission, in order)

| Phase | Track | Budget | Target | Status |
|---|---|---|---|---|
| 1a | B memory | ~8h (shared w/ 1b) | 835MB -> <=50MB | **DONE (pending board confirm)** -- position-index removal shipped (`0465a16`); CI: M -28.5% cgroup / -40.7% massif, T -15.7% bonus |
| 1b | B time | (shared w/ 1a) | 1.084s -> <=0.30s | **DONE for this round (pending board confirm)** -- tokenizer double-lowercase fix shipped (`9cacecd`), ~15% faster on scan_docs locally (12-thread, paired); SIMD tokenization identified as the next lever but out of scope this round |
| 2 | A | ~5h | T<=0.10s, M<=25MB | **DONE for this round (pending board confirm)** -- by-value merge fix shipped (`7f0b249`), CI: M -41.8% cgroup / -24.5% massif; still short of <=25MB (structural latencies-storage floor identified); T unaffected as expected |
| 3 | D | ~6h | T<=0.17s | IN PROGRESS -- 4-ary Dijkstra heap implemented and locally validated; official board confirmation pending |
| 4 | C (conditional) | ~4h | T<=0.12s, M<=40MB | NOT STARTED -- only if C can beat the lowest counted track after 1-3 |
| 5 | Global tuning | ~4h (Nov 1) | thread sweep, MALLOC_ARENA_MAX, PGO | NOT STARTED |
| 6 | Lines + freeze prep | (Nov 2 morning; team freeze 10:00 UTC) | dead code removal, final verification, READMEs/PERF.md complete, LICENSE confirmed | NOT STARTED |

Exit each phase either when its target is hit on the BOARD, or when its
time budget is used -- whichever comes first. The mission says to keep
going phase to phase without waiting for confirmation unless a gate fails
or a decision needs the user.

Task-tracker IDs (numbers only meaningful within this Claude Code session,
not persisted in the repo -- re-derive status from this table and PERF.md
if the tracker is gone): #19=1a, #20=1b, #21=2, #22=3, #23=4, #24=5, #25=6.

## 5. Hard rules (non-negotiable, every change)

1. **Correctness gate**: byte-identical (sha256) vs the official reference
   on seed-1 sample AND full, plus holdout seeds 2/3/4 (via CI's
   `verify-official.yml`). A change failing any of these is reverted
   immediately, however fast it is.
2. **Zero dependencies** (D=1). No locks in hot loops. No allocation in hot
   loops. No repeated hashing in hot loops. Use whichever thread count
   actually measures best (Phase 5 sweep: 6/8/12), not necessarily 12 by
   default, though 12 is the max ever allowed (one P-core's worth of
   logical threads on the scoring box: 6 P-cores = 12 threads).
3. **Measure before and after every change**: local 12-thread wall time,
   CPU/wall ratio (parallelism efficiency check), a phase breakdown when
   diagnosing; CI cgroup `memory.peak` always, `massif` specifically when
   memory is the target of that change.
4. **The board is the final judge.** Push every improved track to
   `speed-demon`. Keep a change only if the board's K actually improved;
   if the board gets worse, revert and record why (CI numbers alone are
   NOT sufficient to decide keep/revert -- CI's shared 2-core runner does
   not reproduce the board's 12-core, pinned-P-core behavior; Track D's
   step 1 is the clearest example in this repo of CI and board disagreeing
   in direction).
5. **Log every attempt** in that track's `PERF.md`, including the board
   run ID, before/after numbers, and kept-or-reverted. Log failed
   experiments too (Track A's step-4 memory hypotheses are the template:
   both reverted, both still written up with real numbers and why).
6. **Dockerfiles must keep passing `check.py`'s allowlist check**
   (digest-pinned FROM lines, every digest present on
   `https://speeddemon.dev/allowlist.json`) and must never use `-C
   target-cpu=native` (see pitfall below) -- `x86-64-v3` is the fixed,
   portable AVX2 baseline actually used everywhere in this repo.
7. **Unsafe code** (mmap's `extern "C"` declarations, the CSR build's
   atomic-then-`into_inner` pattern, etc.) must carry a comment explaining
   why the specific unsafe operation is actually safe in context -- not a
   generic "this is unsafe" note.

## 6. Pitfalls already hit in this repo (don't rediscover these)

- **Running the officially cloned event repo's generator/reference
  scripts locally can hit a sandbox policy block.** CI does `git clone
  https://github.com/hackathon-raptors/speed-demon-2026.git` and then
  `python official/generators/<module>.py ...` routinely (see every
  `track-*.yml` and `verify-official.yml`) -- that's fine there because
  the workflow YAML is reviewed, trusted, already-committed automation.
  Running the SAME command locally, in an interactive agent session, can
  get denied by an auto-mode classifier as "running code from an
  externally cloned repo without explicit user authorization," even
  though it's the identical official URL. Don't try to work around a
  denial like this. If full-scale official data is needed for local
  profiling and this triggers again: either ask the user to authorize it
  explicitly, or synthesize a same-format proxy dataset with your own
  small generator script for RELATIVE timing comparisons only (A/B
  before/after on your own change) -- never for hash-matching or
  absolute numbers, since it won't match the real reference's output.
  This is what Phase 1b's diagnosis did (see `track-b/PERF.md` step 7).
- **Windows memory numbers are not trustworthy.** Local dev-machine memory
  readings (Windows `PeakWorkingSet64`, no cgroup, no containerization) do
  not reliably move in the direction real changes should move them --
  Track A's PERF.md step 4 ran THREE separate, carefully-designed memory
  optimizations locally and measured zero or negative improvement on all
  three, concluded this was very likely a measurement-method ceiling (not
  that the optimizations were wrong), and explicitly flagged "needs the
  board's real number" rather than claiming a win. Treat any Windows-only
  memory number as directional at best; only trust CI's real Linux cgroup
  `memory.peak` (via `ci/bench.sh`/`mem_wrapper.sh`) or the board itself.
- **CI's 2-core runner does not predict board (12-core) wall time, and can
  disagree in DIRECTION, not just magnitude.** Track D's two-pass CSR
  rewrite: local 12-thread measurement showed a genuine ~10-15% T
  improvement; the SAME commit measured on CI's 2-core shared runner
  showed wall time getting 25% WORSE. Root cause: the two-pass design does
  more total parsing work (pass 1 partially parses every line, pass 2
  fully re-parses it, ~1.5x total work) -- a net win when spread across 12
  threads, a net loss squeezed onto 2. The board has 12 P-cores, matching
  the local measurement's thread count, not CI's -- so in a case like
  this, trust the 12-thread local number and the eventual board number,
  not CI's T (CI's M is still trustworthy -- memory totals don't depend on
  core count the way wall time does).
- **The build machine has AVX-512; the scoring machine does not.** CI (and
  likely the actual build step before scoring, on "Depot") runs on
  hardware with AVX-512 available. The scoring box is an i5-13500, 6
  P-cores, AVX2 only, NO AVX-512. `-C target-cpu=native` on the build
  machine would bake in AVX-512 instructions that could crash or silently
  misbehave on the scoring box. Every Dockerfile in this repo instead uses
  the fixed, portable `-C target-cpu=x86-64-v3` (guaranteed AVX2, no
  AVX-512, valid on both machines) -- confirmed (per PERF.md notes made
  while this was being decided) that this string does NOT trip whatever
  native-CPU-opt-in check the board's own build steps apply to `RUSTFLAGS`.
  Never change this to `native` for a "free" speedup; it is not free, it
  is a correctness risk on the only machine that counts.
- **Git on Windows does not reliably track the Unix executable bit.** This
  repo's root `check.py` has a shebang and needs `+x` to run as `./check.py`
  on Linux, but is currently committed as mode `100644` (non-executable) --
  confirmed via `git ls-files --stage check.py`. It still works because
  every invocation in this repo and in CI calls it as `python3 check.py
  ...`, never `./check.py` directly, which sidesteps the bit entirely; but
  if a new script is added and invoked as `./script.sh` anywhere (CI YAML,
  a Makefile-equivalent, etc.), explicitly `git update-index
  --chmod=+x <path>` after adding it on this Windows dev machine, or it
  will fail with "Permission denied" the first time it runs on a real
  Linux runner, even though it looks fine locally. `ci/bench.sh`,
  `ci/docker-run.sh`, and `ci/mem_wrapper.sh` are already correctly
  `100755` in the index and do not need this -- only new scripts are at
  risk.
