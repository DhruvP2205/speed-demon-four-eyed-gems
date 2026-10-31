# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, std-only. Two phases, each bounded to 12 worker threads:

1. **Graph build.** `graph.txt`'s header gives `n`/`m`; the edge body
   is split into up to 12 line-aligned byte ranges (no edge line split
   across a thread boundary) and parsed in parallel into per-thread
   `(u, v, w)` triples (`src/parse.rs`). A counting sort over all
   threads' triples builds one forward CSR (`src/csr.rs`): node `u`'s
   out-edges live at a contiguous slice, grouped by `u`. Parallel
   edges, duplicate lines and self-loops are all kept as separate CSR
   entries, exactly as the reference's adjacency list keeps them.
   The build also does a single linear scan over the same parsed
   triples to produce `has_in_edge`: an `n`-sized bitmap of which
   nodes have at least one in-edge (`csr::has_in_edge_bitmap`).
   The CSR and bitmap are then immediately renumbered by BFS
   visitation order (`src/reorder.rs`): the input's node ids are a
   random relabelling with no positional correlation to graph
   structure (spec section 2), so a Dijkstra search's actual working
   set is scattered across the full id range -- BFS order puts nodes
   visited close together in time (hence close in graph distance) at
   numerically close ids. Forward edges only: the spec's main body is
   strongly connected, so a forward-only BFS from any node already
   covers the whole main body, with no need for a second, reverse
   adjacency just for this pass. Every query is translated through the
   same `old id -> new id` mapping before it touches the CSR or
   bitmap; the answer itself (a distance, or unreachable) is invariant
   under renumbering.
2. **Queries.** The 2,000 (`s`, `t`) pairs are claimed one at a time
   from a shared atomic counter, not a static per-thread split --
   query cost is skewed by Dijkstra rank, so a static split risks one
   thread drawing several of the expensive high-rank queries while
   others sit idle; measurement confirmed the dynamic version keeps
   all 12 threads busy the whole time (97% parallel efficiency, see
   PERF.md step 5). Before running anything, a query with `t != s` and
   no in-edge at `t` is answered `-1` in O(1) -- no edge points at
   `t`, so no path can ever end there. This is exactly the
   "no-in-target" query kind the reference's own notes flag as its
   worst case (settling the whole main body, about 3 s each in
   Python), and it's also why every *real* Dijkstra run below is
   bounded by the spec's Dijkstra-rank distribution (at most 131,071
   nodes, median 512) rather than by `n` -- the one query kind that
   could touch the whole graph never reaches it (confirmed by
   measurement: max settled was 130,414 across the full `full`-scale
   query set, zero queries anywhere near 1.74M). Every other query
   runs a fresh single-source Dijkstra over a `Workspace`
   (`src/workspace.rs`): a `BinaryHeap` plus a paged sparse array.
   Node ids are split into fixed-size pages (1,024 ids/page, picked by
   sweeping 256/1,024/4,096 and measuring); a small per-thread
   `page_table` holds one pointer per page (almost all `None`), and
   the actual page memory comes from a per-thread pool, allocated on
   first touch. `get`/`set` are direct index arithmetic -- no hashing,
   no probing -- which matters because the open-addressing table this
   replaced needed ~2 hash probes per relaxed edge, real cost on top
   of the cache misses BFS reordering was already fighting. At the end
   of a query, every page it touched is zeroed and returned to the
   pool, so memory tracks the busiest single query's distinct-page
   count, not `n`. The search stops the instant `t` is popped off the
   heap, before relaxing `t`'s own out-edges. A stale heap entry (a
   node already settled at a shorter distance) is skipped, same as
   the reference. `d(s, s) = 0` falls out of the algorithm directly:
   `s` is always the first thing popped.

`csr::build_forward` also takes its parsed edge triples by value and
consumes them while filling the CSR, so each chunk's data drops as
soon as it's copied in rather than the whole ~60 MB parsed edge list
sitting alongside the finished CSR; the transient per-chunk degree
counts (~84 MB at `full`) are dropped immediately after use rather
than lingering to the end of the function.

This is still a deliberately simple design: forward-only CSR and plain
unidirectional Dijkstra, no ALT, no bidirectional search. It is
already far from the Python reference's per-query overhead (no
interpreter cost, no full object-list adjacency, no hashing in the hot
relax loop), and the single worst query class the reference calls out
by name is now answered without running Dijkstra at all.

Verified byte-identical (sha256) to the reference on seed 1 at both
`sample` and `full` scale, and against the spec's own worked example,
after every change above.

## Dependencies

None (Rust standard library only). No banned graph library is used or
needed -- just `std::collections::BinaryHeap`.

## With another 24 hours

- A phase breakdown (parse, CSR build, queries, output) showed queries
  are 85-90%+ of total wall time, with graph loading well under 10%.
  A parallelism check (sum of per-thread wall time vs. overall wall
  time) confirmed this isn't a load-balancing problem (97% efficient
  across 12 threads); per-query settled-node logging confirmed no
  query comes close to settling the whole graph (max 130,414 of
  1,740,512 nodes). BFS-order reordering was added on the strength of
  that diagnosis and measured a real, repeatable ~24% improvement in
  a controlled local A/B (identity mapping vs. BFS order, non-
  overlapping timing distributions across 5 runs each).
- The paged workspace measured a real ~3x wall-time improvement over
  the open-addressing version it replaced (median 988 ms vs. ~3,018
  ms, full scale, local), consistent with the diagnosis that hashing/
  probing cost -- not a load-balancing or correctness problem -- was
  the query phase's dominant cost. Confirm this on the board's actual
  numbers, not just local timing.
- Push `M` further toward `Mref/50` (where the M-axis contribution
  maxes out): the workspace is already sized to touched nodes rather
  than `n`, but the 12-thread parse phase's own read buffers plus the
  parsed edge triples still overlap briefly before the CSR exists.
- Re-measure whether an early-exit bound or a cheap landmark-distance
  heuristic (ALT) pays for its added lines under the break-even rule,
  now that a correct, simple baseline exists to compare against.
- PGO (train on a seed other than 1, per the organiser's allowlisted
  `llvm-tools` image) if it clears the break-even bar for the lines it
  costs.
