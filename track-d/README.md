# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, std-only. Both input files are mapped read-only via `mmap`
(`src/mmap_file.rs`, a hand-declared `extern "C"` call into libc --
already linked into every Rust binary on Linux, so not a new
dependency) instead of streamed through `pread_at`.

1. **Graph build.** `graph.txt`'s header gives `n`/`m`; the edge body
   is split into up to 12 line-aligned byte ranges (`src/parse.rs`).
   `csr::build_forward_and_in_edge` then builds the forward CSR AND
   the `has_in_edge` bitmap directly off those ranges, in two parallel
   passes, WITHOUT ever storing the parsed `(u, v, w)` triples (an
   earlier version did; phase instrumentation found that single-
   threaded counting-sort-over-stored-triples build costing more than
   either the parse or query phases by itself -- see PERF.md):
   - Pass 1 (12 threads, same ranges): parse only `u` per line --
     skip straight to the next line without even parsing `v`/`w` --
     and atomically increment a shared per-node degree counter. A
     single O(n) sequential prefix sum over those counts gives
     `offsets`.
   - Pass 2 (same ranges, re-parsed): parse the full triple, and
     atomically claim the next free slot within node `u`'s own
     `[offsets[u], offsets[u+1])` range via a per-node atomic cursor,
     storing `(v, w)` directly into that slot and setting
     `has_in_edge[v]`. `adj_node`/`adj_weight` are `Vec<AtomicU32>`
     during this phase (concurrent stores to different threads are
     well-defined Rust as long as no two writers ever target the same
     index, which the cursor guarantees), converted to plain
     `Vec<u32>` once the parallel section ends -- no `unsafe` anywhere
     in the new build.

   The CSR and bitmap are then renumbered by BFS visitation order
   (`src/reorder.rs`): the input's node ids are a random relabelling
   with no positional correlation to graph structure (spec section
   2), so a Dijkstra search's actual working set is scattered across
   the full id range -- BFS order puts nodes visited close together
   in time (hence close in graph distance) at numerically close ids.
   This step is NOT a given despite costing real sequential time
   (~250ms combined with its remap step): a direct, measured A/B on
   this exact rewritten CSR build (identity mapping vs. BFS order)
   showed the query phase going from ~380ms to ~2.7 SECONDS without
   it -- a ~7x cost from lost cache locality that dwarfs reorder's own
   price, confirmed by measurement before deciding to keep it, not
   assumed. Forward edges only: the spec's main body is strongly
   connected, so a forward-only BFS from any node already covers it,
   no reverse adjacency needed. Every query is translated through the
   same `old id -> new id` mapping before touching the CSR or bitmap;
   the answer itself (a distance, or unreachable) is invariant under
   renumbering.
2. **Queries.** The 2,000 (`s`, `t`) pairs are claimed one at a time
   from a shared atomic counter, not a static per-thread split --
   query cost is skewed by Dijkstra rank, so a static split risks one
   thread drawing several of the expensive high-rank queries while
   others sit idle; measurement confirmed the dynamic version keeps
   all 12 threads busy the whole time (ratio ~11.6-11.7 of 12). Before
   running anything, a query with `t != s` and no in-edge at `t` is
   answered `-1` in O(1) -- no edge points at `t`, so no path can ever
   end there. This is exactly the "no-in-target" query kind the
   reference's own notes flag as its worst case (settling the whole
   main body, about 3 s each in Python). Every other query runs a
   fresh single-source Dijkstra over a `Workspace`
   (`src/workspace.rs`): a `BinaryHeap` plus a paged sparse array.
   Node ids are split into fixed-size pages (1,024 ids/page, picked by
   sweeping 256/1,024/4,096 and measuring); a small per-thread
   `page_table` holds one pointer per page (almost all `None`), and
   the actual page memory comes from a per-thread pool, allocated on
   first touch. `get`/`set` are direct index arithmetic -- no hashing,
   no probing. At the end of a query, every page it touched is zeroed
   and returned to the pool, so memory tracks the busiest single
   query's distinct-page count, not `n`. The search stops the instant
   `t` is popped off the heap, before relaxing `t`'s own out-edges. A
   stale heap entry (a node already settled at a shorter distance) is
   skipped, same as the reference. `d(s, s) = 0` falls out of the
   algorithm directly: `s` is always the first thing popped.

This is still a deliberately simple design: forward-only CSR and plain
unidirectional Dijkstra, no ALT, no bidirectional search. It is
already far from the Python reference's per-query overhead (no
interpreter cost, no full object-list adjacency, no hashing in the hot
relax loop), and the single worst query class the reference calls out
by name is now answered without running Dijkstra at all.

Verified byte-identical (sha256) to the reference on seed 1 at both
`sample` and `full` scale, against holdout seeds 2/3/4, and against
the spec's own worked example. Also verified that `build_forward_and_
in_edge`'s adjacency order -- which CAN legitimately vary run to run
under concurrent construction, since which thread's atomic cursor
claim lands first isn't deterministic -- never changes a single
shortest-path answer, directly (not just argued): `csr.rs`'s own unit
tests build the same graph under thread counts 1 through 12 and
confirm every (s, t) distance agrees.

## Dependencies

None (Rust standard library only). No banned graph library is used or
needed -- just `std::collections::BinaryHeap`.

## With another 24 hours

- The new two-pass CSR build improved over the old single-threaded
  counting sort by only ~25-30% (not the larger gain hoped for from
  parallelizing it) -- worth profiling specifically for atomic
  contention on hot nodes (a few nodes with very high out-degree
  would see many threads hammering the same `fetch_add` cursor) and
  whether pass 1's partial re-parse of `u` is adding more total work
  than the parallelism recovers.
- Push `M` further toward `Mref/50` (where the M-axis contribution
  maxes out): the workspace is already sized to touched nodes rather
  than `n`, but `adj_node`/`adj_weight` briefly exist as both
  `Vec<AtomicU32>` and (after conversion) `Vec<u32>` during the build.
- Re-measure whether an early-exit bound or a cheap landmark-distance
  heuristic (ALT) pays for its added lines under the break-even rule,
  now that a correct, simple baseline exists to compare against.
- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines it
  costs.
