# Design: virtual sibling negations for at-most-one constraints

Status: design + measured motivation. Not implemented; written so that the
at-most-one encoding work does not foreclose it.

## Why clausal encodings hit a floor

For the at-most-one ("forbid multiple") constraint, any clausal encoding has
the same invariant: a requires clause only sees a candidate as false if
`¬candidate` is physically assigned. Deciding `pkg == X` must therefore
unit-propagate all `n−1` sibling negations onto the trail, whatever shape the
clauses have. In backtracking storms (the solver walking ~1k versions of a
package pair, rebuilding the trail each step) this dominates everything.

Measured on the `amo-bench` storm instance (6×8 packages, 100 candidates
each, `--spread 0.02 --max-offset 0.5`, seed 0, diagnostics feature):

| encoding    | FM clauses | FM visits | visits/assignment | propagated | wall    |
| ----------- | ---------- | --------- | ----------------- | ---------- | ------- |
| binary      | 20 548     | 8.63 M    | 4.6               | 2.32 M     | 0.44 s  |
| commander:3 | 9 050      | 2.82 M    | 1.8               | 2.58 M     | 0.31 s  |
| sequential  | 9 052      | 3.29 M    | 1.6               | 3.12 M     | 0.24 s  |

Switching binary → commander/sequential cuts clause **visits** ~3× (each
sibling negation is derived through 1–2 clause visits instead of ~5), but the
**propagated assignments** stay in the same band: the trail churn itself is
encoding-invariant. PubGrub-style solvers do the same walk orders of
magnitude faster because their assignments are package-level terms, not
per-candidate booleans. Getting there inside resolvo means making sibling
negations *virtual*.

## Core idea

When a candidate `s` of package `P` is assigned **true**, do not propagate
`¬c` for the other candidates. Instead record a package-level selection, and
make every value lookup consult it:

- `DecisionMap` gains `package_selection: Vec<Option<(VariableId, level)>>`,
  indexed by a dense per-tracked-package index.
- A side table `var_to_package: Vec<Option<PackageIdx>>` maps solvable
  variables to that index. It is populated in `register_forbid_target`
  (`src/solver/encoding.rs`), which is already the single point where
  candidates enter at-most-one tracking.
- `DecisionMap::value(v)`: explicit assignment wins; otherwise, if
  `package(v)` has selection `(s, l)` with `s ≠ v`, return `Some(false)`
  (and `level(v) = l`). `Literal::eval` flows through this, so decide,
  propagate checks, and analyze all see the virtual value.

Trail semantics: only `s`'s positive assignment is on the trail. The
selection record is set when that decision is added and cleared when
backjumping pops it (`DecisionTracker::undo_*`). Virtual falsifications
therefore appear and disappear in O(1) per package instead of O(n) trail
entries plus O(n) decide-queue routing per rebuild.

## The hard part: the watch invariant

Watches only fire on physical assignments. Clauses watching a **positive**
literal of a non-selected candidate (requires clauses, AnyOf, learnt clauses)
would never observe the virtual falsification, breaking unit propagation.

Fix: per-package watch buckets. In `WatchMap`, watches on positive literals
of tracked solvables are keyed by the package instead of the literal. On a
selection event the propagation loop walks that one bucket: for each clause
it looks up which candidate literal is being watched, and runs the standard
logic (early-skip if the other watch is true; find an unwatched literal using
the virtual-aware `eval`; otherwise unit-propagate or report a conflict).
Watch moves re-register in the bucket of the target literal's package, or
under the plain literal key if the target is not a tracked candidate.

Cost per selection: O(#clauses watching any candidate of `P`) — the parents
plus learnt clauses, independent of the candidate count. In a storm most of
these visits are satisfied-clause early-skips (the parent literal is already
false), the same ~30–50 instruction path as today's ForbidMultiple skips, but
there are ~5× fewer of them and zero trail/queue traffic behind them.

Negative literals (`¬c` in Constrains/Lock clauses) need no action: selection
makes them *true*, and watches only react to their literal becoming false.

## Conflict analysis

Virtual assignments are not on the trail, so `analyze()` needs two hooks:

1. `level()` of a virtually-false variable is the selection level (handled by
   the `DecisionMap` change).
2. When resolution reaches a literal `c` that is only virtually false, its
   reason is materialized lazily as a pairwise
   `Clause::ForbidMultipleInstances(s, ¬c, name)` — allocated on demand, used
   as the reason clause (for `learnt_why` provenance and error reporting),
   semantically replacing `c` with `¬s` in the resolvent. The same clause is
   used when `try_add_decision` detects an attempt to set a virtually-false
   variable to true.

Conflict reporting already supports solvable↔solvable pairwise forbid
clauses since the encoding prototypes (see `src/conflict.rs`); materialized
reasons render as direct conflict edges.

## Expected effect (falsifiable)

On a storm with `n` candidates and `k` rebuilds: trail assignments drop from
O(n·k) to O(k); decide-queue routing likewise; clause visits drop to the
package-bucket walks. For the measured boto3-style storm profile (4.67 M
assignments, 9.3 M queue touches, 24.7 M visits, ~1 s wall) the model
predicts a ≥10× wall reduction. If propagated-assignment counts do *not*
collapse under this change, the model is wrong and the per-assignment cost
lives elsewhere.

## What the encoding work must preserve (and does)

- `register_forbid_target` stays the single candidate-registration point.
- Helper variables (binary bits, ladder, commanders) are one origin class
  (`VariableOrigin::ForbidMultiple`); analyze/decide already treat them
  opaquely. Virtual mode simply allocates none.
- The conflict graph accepts pairwise (solvable, solvable) forbid clauses.
- Clause emission is deterministic (insertion-ordered `IndexSet`/`IndexMap`
  iteration only).
- `AmoEncoding` is the natural switch: a `Virtual` variant would emit no
  clauses in `add_pending_forbid_clauses` and instead populate
  `var_to_package`.

## Follow-up: range learning through virtual prefix (ladder) variables

The implemented `AmoEncoding::Virtual` materializes *pairwise* reasons, so
learnt clauses name individual candidates and conflict-heavy workloads search
worse than the ladder/commander clausal encodings (measured: ~2× the
conflicts). Materializing commander-shaped reasons does not fix this by
itself: resolving through the chain ends at the selected candidate again, and
stopping at a group literal only helps if that literal is *assignable* so the
learnt clause can unit-propagate.

The strong version of the idea uses ladder/prefix variables instead of a
commander tree: `pᵢ` ≡ "the selected candidate has index ≤ i" in the
preference-sorted candidate order.

- Derived semantics are O(1): with selection at index `k`, `pᵢ = (k ≤ i)`;
  explicit `pᵢ` assignments collapse to an interval `[lo, hi]` of allowed
  indices per package, and a candidate's value lookup is one comparison.
- All reasons are valid standalone lemmas, materializable on demand:
  `(¬s ∨ pᵢ)` for `k ≤ i`, `(¬s ∨ ¬pⱼ)` for `k > j`, `(¬pⱼ₋₁ ∨ ¬cⱼ)`,
  `(pⱼ ∨ ¬cⱼ)`, and the chain `(¬pᵢ ∨ pⱼ)` for `i ≤ j` — the sequential
  encoding's clauses, never stored.
- `analyze` substitutes a virtually-false candidate `cⱼ` with `¬pⱼ₋₁` / `pⱼ`;
  the learnt clause then asserts an explicit prefix assignment on backjump:
  one trail entry that excludes a whole version range, which `decide` skips
  via the interval.

This combines virtual trail mechanics with learning at least as strong as
the sequential encoding (the best clausal learner in the benchmarks) — in
effect PubGrub-style interval terms inside CDCL. Additional cost over the
implemented prototype: prefix-variable allocation, interval maintenance with
undo and level attribution, watcher walks for prefix variables and excluded
ranges on selection/prefix events, and the substitution policy in conflict
analysis.

## Open questions

- Learnt clauses watching many distinct packages could make bucket
  re-registration churn noticeable; may need the bucket key cached in
  `WatchedLiterals`.
- Incremental clause addition while a selection is active must evaluate
  literals virtual-aware in the `add_pending_forbid_clauses` decision/conflict
  back-check (it already uses `Literal::eval`, so this follows from the
  `DecisionMap` change, but needs a regression test).
- The `(Some(false), Some(false))` late-clause conflict path needs an
  equivalent: registering a second true candidate for an already-selected
  package must surface a conflict with a materialized pairwise reason.
