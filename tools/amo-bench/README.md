# At-most-one encoding benchmark

`amo-bench` compares the encodings of the at-most-one ("forbid multiple
instances") constraint that ensures only a single candidate of a package is
selected. The encoding is selected with `Solver::with_amo_encoding`, the
`RESOLVO_AMO_ENCODING` environment variable, or the `--encodings` flag of this
tool.

The tool generates synthetic layered dependency problems in which packages
have many candidate versions and shared dependencies whose acceptable version
ranges shift with the version of the depending package. This forces the solver
to backtrack through many candidates of the same package, which is exactly the
workload where the at-most-one clauses are exercised.

```sh
cargo run --release -p amo-bench -- --versions 20,100,500 --seeds 5
```

## Encodings

All encodings are incremental (candidates are discovered lazily) and emit only
binary clauses. For a package with `n` reachable candidates:

| Encoding       | Clauses        | Aux vars   | Memory (32 B/clause)  |
| -------------- | -------------- | ---------- | --------------------- |
| `pairwise`     | n(n−1)/2       | 0          | O(n²): ≈ 16·n² B      |
| `binary`       | ≈ n·⌈log₂n⌉    | ⌈log₂n⌉    | O(n log n)            |
| `sequential`   | 3n − 4         | n − 1      | O(n): ≈ 125·n B       |
| `commander:g`  | ≈ n(g+1)/(g−1) | ≈ n/(g−1)  | O(n): ≈ 110·n B (g=3) |
| `bimander:g`   | ≈ n((g−1)/2 + log₂(n/g)) | ⌈log₂(n/g)⌉ | O(n log n)  |
| `hybrid:t`     | pairwise ≤ t, then binary | | O(n log n)         |

The product encoding was not implemented because it lays variables out on a
√n×√n grid and therefore needs the candidate count up front, which conflicts
with the incremental discovery of candidates.

## Results (2026-06-12)

### Synthetic (6 layers × 8 packages, 4 deps/solvable, spread 0.02, offset 0.5, 5 seeds, 60 s timeout)

| Encoding    | V=20 mean | V=100 mean | V=500 mean (sat)   | V=500 clauses |
| ----------- | --------- | ---------- | ------------------ | ------------- |
| binary      | 0.011 s   | 0.29 s     | 54.7 s (1/5 sat)   | 202 k         |
| pairwise    | 0.016 s   | 2.09 s     | 60 s (0/5 sat)     | 1 730 k       |
| sequential  | 0.008 s   | 0.16 s     | 21.6 s (5/5 sat)   | 107 k         |
| commander:3 | 0.009 s   | 0.17 s     | 28.0 s (4/5 sat)   | 115 k         |
| bimander:2  | 0.010 s   | 0.29 s     | 54.8 s (1/5 sat)   | 193 k         |
| hybrid:16   | 0.016 s   | 0.33 s     | 55.1 s (1/5 sat)   | 207 k         |

### conda-forge (real snapshot via rattler `create-resolvo-snapshot`, 150 random problems, 20 s timeout, `solve-snapshot --amo-encoding`)

| Encoding    | mean    | median  | p95     | total    | vs binary (non-trivial problems) |
| ----------- | ------- | ------- | ------- | -------- | -------------------------------- |
| binary      | 0.666 s | 0.591 s | 1.59 s  | 99.9 s   | —                                |
| sequential  | 0.774 s | 0.542 s | 1.57 s  | 116.1 s  | 64 faster / 20 slower, one 20 s timeout outlier |
| commander:3 | 0.641 s | 0.553 s | 1.54 s  | 96.2 s   | 70 faster / 16 slower            |
| bimander:2  | 0.691 s | 0.607 s | 1.52 s  | 103.7 s  | 14 faster / 90 slower            |
| hybrid:16   | 0.664 s | 0.589 s | 1.53 s  | 99.7 s   | 57 faster / 30 slower            |
| pairwise    | 1.655 s | 1.694 s | 3.65 s  | 248.3 s  | 0 faster / 133 slower            |

All encodings agreed on sat/unsat for every problem.

A second seed (150 different problems, binary/sequential/commander:3 only)
put all three within ±5% on aggregate (sequential 159.9 s < binary 163.6 s <
commander 172.2 s total) with the means dominated by a handful of
timeout-boundary problems: sequential solved one problem in 2.4 s on which
both binary and commander timed out at 20 s, while commander had the best
per-problem win ratio on the bulk (54 faster / 22 slower vs binary;
sequential 39 / 45). Solution sizes were identical across encodings on all
300 problems of both seeds.

### Full test suite under every encoding

Running the solver test suite with each encoding (via `RESOLVO_AMO_ENCODING`):

- Wall time is indistinguishable (0.27–0.32 s, dominated by process startup).
- **Solutions never change**: every exact-solution snapshot passes under all
  six encodings.
- **Error traces**: only 4 unsat-message snapshots differ, and only for some
  encodings (sequential: 1, bimander: 2, pairwise/commander/hybrid: 4). Every
  difference is a cosmetic reordering of the same conflict (which side of the
  incompatibility is listed first, or which candidate versions are merged
  into one line, e.g. `c 101` vs `c 101 | 103`); none is wrong or less
  informative.

### Takeaways

- The linear encodings (sequential, commander) clearly beat the current
  binary encoding when packages have many reachable candidates and the solver
  backtracks: ~1.8× faster at 100 candidates/package and the difference
  between solving and timing out at 500.
- On typical conda-forge problems the difference is small (~4–8% mean/median)
  because most packages only have a handful of reachable candidates, but the
  direction is consistent (commander:3 faster on 70 of 129 non-trivial
  problems, slower on 16, and the largest regression was only +0.6 s).
- Bimander behaves like the binary encoding on both workloads (its group-index
  bits dominate its behavior), despite being a frequent winner in the SAT
  literature. The likely explanation also explains why the linear encodings
  win here: the ladder and commander auxiliary variables represent
  *contiguous ranges* of the preference-sorted candidate list, so learnt
  clauses over them generalize across whole version windows; binary/bimander
  bit patterns do not align with the candidate order, so conflicts cannot be
  expressed compactly.
- Sequential and commander:3 are the two viable replacements. Sequential is
  the strongest at the extremes (only encoding to solve all synthetic V=500
  instances; rescued a conda-forge problem on which binary and commander both
  timed out) and has the least error-trace churn, but showed one pathological
  tail of its own (a 20 s timeout on a problem every other encoding solves in
  ~2 s). Commander:3 is the most consistent on the bulk of typical problems.
  On aggregate over typical conda-forge problems all three are within noise
  of each other; the decisive differences are in the candidate-heavy regime,
  where both linear encodings dominate binary.
- The hybrid (pairwise below a threshold) does not pay off: the binary
  encoding's helper overhead at small n is already negligible, and the
  pairwise clauses are pure overhead once the threshold is crossed.
- Pairwise is uniformly worse on anything but tiny candidate sets, both in
  time and in memory (O(n²) clauses; 259 MB peak RSS for four packages with
  2 000 candidates, vs ~3 MB for the others).
