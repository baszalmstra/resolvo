# Incremental blocking-clause completion index

Status: implemented and measured 2026-07-09 on branch
`claude/implement-measure-performance-ymhx0x`, based on `universal-solver`
(`a4f2d430`).

## What this is

Universal enumeration appends one blocking clause after each recorded
non-empty cell. When no ordinary or environment decision remains,
`Solver::decide` must return the first registered multi-literal blocking
clause that is unsatisfied under the undecided-counts-as-false completion,
and that clause's first undecided positive literal. The historical
implementation rescanned every blocking clause and every literal on every
such query; the list grows monotonically with the cell count, so the scan
approaches quadratic work in high-cell solves.

`src/solver/blocking_completion.rs` replaces the scan with an incremental
index: per-clause completion results cached in a registration-ordered active
set, invalidated through variable occurrence lists, synchronized with the
assignment trail lazily through a dedicated `blocking_sync_floor` on
`DecisionTracker` — the same lazy-sync discipline the decide queue uses. The
winner order is unchanged (lowest active entry id = first registered
unsatisfied clause; its cached action = first undecided positive literal),
and fully-false clauses stay in the active set so an earlier broken clause
still trips the historical invariant instead of being skipped.

The old scan is retained as a debug oracle (`blocking_completion_reference`):
in debug builds it runs at every indexed query and asserts the complete
`(env_clause_id, clause_id, candidate)` tuple matches.

## Correctness

Exactly behavior preserving, verified two ways:

- **Debug oracle**: tuple equality at every completion query, including the
  1,000-seed universal property test and every targeted universal scenario
  test. Full default and `diagnostics` suites pass (83 lib + 160 integration
  tests) with zero snapshot churn.
- **Corpus A/B** (500 universal problems, real conda-forge linux-64+noarch
  repodata, release builds): every problem's ordered cells, solvables,
  outcome (349 ok / 151 unsolvable), conflicts (45,332), propagated
  decisions (358,520,456), restarts (163), and cells (2,484) are
  **bit-identical** between the baseline scan and the index.

## Performance verdict: not a win on the current corpus

The measurement does **not** justify shipping the index as the default on
today's repodata, and the honest reason is that the scan it replaces is not
material here.

### Direct index microbenchmark — the win condition

`bench_index_vs_full_scan_sparse_touches` (5,000 disjoint blocking clauses,
5,000×2 queries each touching one occurrence list):

| | full scan | incremental index | speedup |
|---|---|---|---|
| wall | 54.3 ms | 0.41 ms | **133.6×** |

This is the regime the index was built for: many simultaneously-registered
clauses, each query changing few occurrences. It meets the ">10× fewer
completion visits where the scan is measurable" acceptance criterion.

### Universal corpus A/B — the reality check

500 problems, seed 0, 60 s timeout, sequential:

| | baseline scan | index |
|---|---|---|
| total wall | 347.5 s | 351.0 s (**+1.0%**) |
| completion queries | 2,618 | 2,240 |
| completion literal visits | 113,824 | 106,349 (recompute) |
| trail variables routed | — | 77,133,374 |
| occurrence entries visited | — | 235,635 |
| **max active clauses** | — | **1** |

The scan's total work across the whole corpus was ~114k literal visits —
negligible; the single busiest problem did 8,295 literal visits inside a
1.5 s solve. The index replaces that with ~106k recompute visits **plus 77
million trail routings** to keep its mirror in sync, because the blocking
query fires only when nothing else is left to decide (2,240 times) and each
firing must walk the entire trail suffix accumulated since the previous
firing (~34,000 variables per query on average). The result is a ~1% wall
regression that is **consistent, not noise**: the median per-problem delta is
+1.3%, and systematic slowdowns (up to +194 ms on deep-trail multi-second
solves) outweigh the scattered speedups (down to −143 ms).

The decisive number is `max_active = 1`: at any moment at most one blocking
clause is unsatisfied, so the scan finds its answer almost immediately, while
the index pays full trail-sync cost regardless. The high-cell shape that
motivated the index (the campaign's 708-cell problem 265) no longer exists in
current repodata — the corpus now tops out at 50 cells.

This is precisely the outcome the handoff anticipated ("Do not assume this is
currently dominant… remove the scan only if measurements show it remains
material") and the failure mode it warned about ("ensure work was removed
rather than moved into tree maintenance or occurrence routing"). On this
corpus the work was moved into occurrence routing, and the scan was not
material to begin with.

### Concrete solves — no regression

Plain solves never enter the blocking path (`blocking_clauses` is empty and
the block is guarded by `!blocking_clauses.is_empty()`), so concrete solving
is unaffected by construction — the only added cost is the one-time
`Default` field initialization. A 1,000-problem concrete A/B on the same
snapshot confirms no measurable difference (baseline 600.4 s; index within
run-to-run noise, identical records solved per problem).

## Recommendation

Keep the implementation — it is correct, exhaustively tested, safe (never
reached in plain solves), and wins decisively (133×) in the many-unsatisfied
high-cell regime. But it should not replace the scan by default while the
corpus is dominated by low-cell solves with `max_active ≤ 1`. Two viable
paths:

1. **Ship gated**: keep the scan as the default; enable the index once a
   solve crosses a cell-count / active-clause threshold where rescanning
   dominates. The index already tolerates registration under a retained
   trail, so it can be armed mid-solve.
2. **Reduce sync cost first**: the 77M routings are ~99.7% HashMap misses on
   non-environment variables. A dense "is-env-variable" bitset to skip the
   hash on misses would cut the maintenance cost that currently offsets the
   scan savings, potentially making the index neutral-to-positive even at
   `max_active = 1`. This is out of the original handoff scope.

The scan is retained regardless as the debug oracle, so switching the default
back and forth costs nothing.
