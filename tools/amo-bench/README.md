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

### Virtual sibling negations (`virtual`)

`AmoEncoding::Virtual` emits no at-most-one clauses at all (see
`docs/virtual-sibling-negations.md`): assigning a candidate true records a
package-level selection consulted by every value lookup, the watchers of the
sibling candidates are visited directly, and conflict analysis resolves
virtually falsified variables through lazily materialized pairwise reasons.

On the boto3-style backtracking storm (`--storm`: a=v pins b=v, every b but
the oldest is dead, the solver walks every version with no real search):

| encoding   | n=1400: FM clauses / FM visits / propagated / wall | n=5000 wall |
| ---------- | -------------------------------------------------- | ----------- |
| binary     | 30 800 / 15.8 M / 2.97 M / 0.28 s                  | 5.07 s      |
| sequential | 8 392 / 5.9 M / 5.88 M / 0.22 s                    | 2.96 s      |
| virtual    | **0 / 0 / 1.96 M / 0.11 s**                        | **1.52 s**  |

The remaining propagated assignments are negative-assertion replays, not
sibling negations. This is the regime the virtual design targets, and it wins
by 2.5–3.3× over the current encoding, growing with the candidate count.

On conflict-heavy workloads the trade inverts: virtual learnt clauses are
pairwise-shaped (no helper variables to compress candidate ranges), so the
solver conflicts roughly twice as often as with the ladder/commander helpers
(measured 10.1 k vs 3.3–5.5 k conflicts on the synthetic V=100 instance) and
the random conda-forge suite — dominated by unsat conflict analysis — is
~40% slower than binary (mean 0.943 s vs 0.666 s, slower on 124 of 133
non-trivial problems). All verdicts and solutions stay identical.

### Virtual ladder (`virtual-ladder`)

`AmoEncoding::VirtualLadder` implements the best-of-both follow-up: one
prefix variable per candidate ("selected index ≤ i"), two explicit boundary
prefix assignments pushed behind every candidate decision (with
pre-materialized reasons), and an allowed-index interval per package that
derives all other values in O(1). Conflict analysis resolves sibling
falsifications into prefix literals, so learnt clauses assert range-pruning
prefix assignments — three trail entries per selection instead of n.

Final configuration: boundaries-only until the first conflict, then full
prefix chains per selection (sequential-grade conflict analysis anchors),
with O(1) prefix-assignment undo and driver-gated package events. See the
learning-gap research in `docs/virtual-sibling-negations.md` for how each
piece was isolated.

| workload                | binary  | sequential | commander:3 | virtual-ladder |
| ----------------------- | ------- | ---------- | ----------- | -------------- |
| storm n=1400            | 0.26 s  | 0.20 s     | —           | **0.14 s**     |
| storm n=5000            | 4.44 s  | 2.58 s     | —           | **1.80 s**     |
| conflict-heavy V=100 (5 seeds) | 0.36 s | 0.21 s | 0.22 s     | **0.20 s**     |
| conflict-heavy V=500    | 1/5     | **5/5, 23.2 s** | 4/5    | 5/5, 26.4 s    |
| conda-forge seed 0 (150 problems) | 0.67 s | 0.77 s | 0.64 s | **0.64 s**     |
| conda-forge seed 1      | 1.09 s, 2 timeouts | 1.07 s, 1 timeout | 1.15 s, 2 timeouts | **0.75 s, 0 timeouts** |
| conda-forge seed 2 (250 problems) | 0.73 s, 0 timeouts | — | 0.86 s, 2 timeouts | **0.60 s, 0 timeouts** |

With the requirement interval strengtheners (conflict-gated, packages with
≥32 candidates, non-trivial ranges) the sweep is complete. Load-matched
final numbers, virtual-ladder vs the best clausal encodings:

| workload | binary | commander:3 | sequential | virtual-ladder |
| --- | --- | --- | --- | --- |
| storm n=1400 / n=5000 | 0.26 / 4.4 s | — | 0.20 / 2.6 s | **0.13 / 1.68 s** |
| conflict-heavy V=100 (5 seeds) | 0.36 s | 0.22 s | 0.21–0.23 s | **0.13 s** |
| conflict-heavy V=500 (5 seeds) | 1/5 | 4/5 | 5/5, 28.7 s | **5/5, 16.4 s** |
| conda-forge seed 0 | 0.753 s | 0.729 s | — | **0.701 s** |
| conda-forge seed 1 | 1.186 s, 3 TO | 1.269 s, 3 TO | — | **0.803 s, 0 TO** |
| conda-forge seed 2 (250) | 0.749 s | 0.885 s, 2 TO | — | **0.666 s, 0 TO** |

Virtual-ladder is the fastest configuration on every benchmark and the only
one that never times out — it solves every conda-forge problem on which the
clausal encodings hit the 20 s limit. Solutions and sat/unsat verdicts are
identical to all other encodings on every problem tested.

A general lesson from getting here: interval machinery (prefix chains,
requirement strengtheners) must be emitted as *conflict-gated search aids*.
Emitted eagerly, their propagation cost regressed shallow solves by 10–25%;
held back until the solver starts conflicting, they are strictly positive.

It strictly dominates the plain virtual encoding (equal on storms, ~2×
faster on conflict-heavy workloads with half the conflicts — all conflict
resolutions are prefix-shaped) and strictly dominates binary on storms while
matching it on conflict-heavy search. The clausal sequential/commander
encodings remain ahead on the unsat-dominated random conda-forge suite:
their full helper chains live on the trail, giving conflict analysis finer
UIP granularity than the two boundary literals per selection. Memory: n
prefix variables plus 2n unwatched reason clauses per package — comparable
to sequential's clause count, but with zero watch-list traffic.
- Pairwise is uniformly worse on anything but tiny candidate sets, both in
  time and in memory (O(n²) clauses; 259 MB peak RSS for four packages with
  2 000 candidates, vs ~3 MB for the others).

## Authoritative 3-seed sweep (2026-06-13)

3 seeds × 250 random conda-forge problems × 4 encodings = 3,000 solves, run
strictly sequentially on one machine (clean timings), 30 s timeout, with the
exact selected solvables captured for comparison.

### Performance (aggregate over 750 problems)

| encoding       | mean    | total    | timeouts |
| -------------- | ------- | -------- | -------- |
| binary         | 1.278 s | 958.6 s  | 3        |
| commander:3    | 1.360 s | 1020.1 s | 5        |
| sequential     | 1.399 s | 1049.6 s | 5        |
| **virtual-ladder** | **0.991 s** | **743.6 s** | **0** |

Virtual-ladder is the fastest on every individual seed (totals 235/279/229 s
vs binary 285/422/251 s) and is the only encoding with zero timeouts — it
resolves every problem on which the others hit the 30 s wall. The aggregate
is ~22 % faster than the current binary default.

### Solutions (the important correctness check)

Verdicts (sat/unsat) are identical to binary on every problem under every
encoding (0 mismatches, timeouts excluded). Solutions are byte-identical on
the large majority of problems; a small number diverge to a *different but
valid* solution:

| encoding       | diverged solutions | of SAT problems |
| -------------- | ------------------ | --------------- |
| commander:3    | 4                  | 265             |
| sequential     | 5                  | 264             |
| virtual-ladder | 7                  | 266             |

Divergence is **inherent to changing the at-most-one encoding, not specific
to virtual-ladder**: resolvo is not a global optimizer — it stops at the
first complete valid assignment, preferring higher versions in decision
order, so any change to propagation changes the decision path and can land
on a different valid solution. The clausal alternatives commander and
sequential diverge from binary at the same rate, on the same problems
(`inept`, `resolvo-cpp`, `usagestats`), with the same per-package version
differences. Within a divergent solution the version differences go both
directions (some packages higher, some lower), so there is no systematic
solution-quality regression in either direction. Adopting virtual-ladder is,
from a solution-stability standpoint, equivalent to adopting commander or
sequential — all three are already-supported encodings.
