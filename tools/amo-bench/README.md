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
| `hybrid:t`     | pairwise ≤ t, then binary | | O(n log n)         |

The product encoding was not implemented because it lays variables out on a
√n×√n grid and therefore needs the candidate count up front, which conflicts
with the incremental discovery of candidates. The bimander encoding (binary ×
commander) could be added on top of the commander infrastructure if desired.

## Results (2026-06-12)

### Synthetic (6 layers × 8 packages, 4 deps/solvable, spread 0.02, offset 0.5, 5 seeds, 60 s timeout)

| Encoding    | V=20 mean | V=100 mean | V=500 mean (sat)   | V=500 clauses |
| ----------- | --------- | ---------- | ------------------ | ------------- |
| binary      | 0.011 s   | 0.29 s     | 54.7 s (1/5 sat)   | 202 k         |
| pairwise    | 0.016 s   | 2.09 s     | 60 s (0/5 sat)     | 1 730 k       |
| sequential  | 0.008 s   | 0.16 s     | 21.6 s (5/5 sat)   | 107 k         |
| commander:3 | 0.009 s   | 0.17 s     | 28.0 s (4/5 sat)   | 115 k         |
| hybrid:16   | 0.016 s   | 0.33 s     | 55.1 s (1/5 sat)   | 207 k         |

### conda-forge (real snapshot via rattler `create-resolvo-snapshot`, 150 random problems, 20 s timeout, `solve-snapshot --amo-encoding`)

| Encoding    | mean    | median  | p95     | total    | vs binary (non-trivial problems) |
| ----------- | ------- | ------- | ------- | -------- | -------------------------------- |
| binary      | 0.666 s | 0.591 s | 1.59 s  | 99.9 s   | —                                |
| sequential  | 0.774 s | 0.542 s | 1.57 s  | 116.1 s  | 64 faster / 20 slower, one 20 s timeout outlier |
| commander:3 | 0.641 s | 0.553 s | 1.54 s  | 96.2 s   | 70 faster / 16 slower            |
| hybrid:16   | 0.664 s | 0.589 s | 1.53 s  | 99.7 s   | 57 faster / 30 slower            |
| pairwise    | 1.655 s | 1.694 s | 3.65 s  | 248.3 s  | 0 faster / 133 slower            |

All encodings agreed on sat/unsat for every problem.

### Takeaways

- The linear encodings (sequential, commander) clearly beat the current
  binary encoding when packages have many reachable candidates and the solver
  backtracks: ~1.8× faster at 100 candidates/package and the difference
  between solving and timing out at 500.
- On typical conda-forge problems the difference is small (~4–8% mean/median)
  because most packages only have a handful of reachable candidates, but the
  direction is consistent (commander:3 faster on 70 of 129 non-trivial
  problems, slower on 16, and the largest regression was only +0.6 s).
- Sequential has the best raw numbers on candidate-heavy workloads but showed
  one pathological tail on conda-forge (a 20 s timeout on a problem every
  other encoding solves in ~2 s). Commander:3 had the best tail behavior
  overall.
- The hybrid (pairwise below a threshold) does not pay off: the binary
  encoding's helper overhead at small n is already negligible, and the
  pairwise clauses are pure overhead once the threshold is crossed.
- Pairwise is uniformly worse on anything but tiny candidate sets, both in
  time and in memory (O(n²) clauses; 259 MB peak RSS for four packages with
  2 000 candidates, vs ~3 MB for the others).
