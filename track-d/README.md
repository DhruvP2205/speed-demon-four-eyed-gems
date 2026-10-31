# Track D: Traverse

The task, input files, and exact output format are defined by the
[official specification](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, std-only. `graph.txt` is split into line-aligned byte ranges and
parsed in parallel with `pread_at`; no input-sized owned copy or external
dependency is required.

1. **Graph build.** Counting-sort construction produces both forward and
   reverse CSR arrays. The reverse CSR lets the query phase walk incoming
   edges directly for backward search.
2. **Adaptive preprocessing.** Cheap structural signals measure whether BFS
   renumbering can improve locality and whether reverse-edge weights support
   useful landmark bounds. Reordering is skipped for already-local inputs.
   ALT is enabled only when the graph signal says its bounds are useful and a
   sample of the actual queries is difficult enough to justify preprocessing.
3. **Queries.** Each worker processes a contiguous query chunk with
   bidirectional Dijkstra and a reusable touched-node workspace. The search
   stops when the two frontier minima cannot improve the best meeting
   distance. On ALT-selected inputs, consistent averaged landmark potentials
   change only priority ordering, not relaxation or output semantics.

## Validation

The ported implementation has 48 release unit tests, including brute-force
shortest-path checks, zero-weight and tie cases, adaptive-signal checks, and
both CSR directions. It matched the available edge fixture, full random
fixture, and full grid fixture byte-for-byte. The full random fixture measured
3.18-4.67 seconds on the development machine, with SHA-256
`eff6439e11b88f342e479cd9a31f0fc8a8065e754f2b918d50dbc9125b1a65b2`.

## Dependencies

None. No graph library or other direct dependency is used.

## With another 24 hours

- Re-measure the adaptive thresholds on the scoring board and remove
  measurement-only environment overrides during final cleanup.
- Profile reverse-CSR and landmark-table memory on the board; ALT is
  deliberately disabled when its preprocessing is unlikely to pay.
- Evaluate PGO using a holdout seed only if the resulting binary clears the
  source-line break-even threshold.
