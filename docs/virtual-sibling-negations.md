# Design: virtual sibling negations for at-most-one constraints

Status: **implemented** as `AmoEncoding::Virtual` and
`AmoEncoding::VirtualLadder` (`src/solver/decision_map.rs` holds the
machinery and its module documentation). This document is the original
design followed by the research log that shaped the final implementation;
read it in order to follow how each decision was reached, or read the
`decision_map` module documentation for the final architecture only.

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

## Learning-gap research (measured)

Why does virtual-ladder still conflict ~1.6× more often than the clausal
sequential encoding on conflict-heavy workloads? A series of controlled
experiments (conflict-heavy synthetic, V=100, seed 0):

| configuration                          | conflicts | notes                          |
| -------------------------------------- | --------- | ------------------------------ |
| clausal sequential                     | 3 347     | reference                      |
| virtual-ladder, boundaries only        | 5 485     | shipping configuration         |
| + tight prefix explanations            | 5 485     | clauses −40% (memoization), conflicts unchanged |
| + helper-literal activity bumps        | ~same     | hypothesis rejected            |
| + exponential anchors, pairwise reasons| 6 436     | worse: resolutions collapse to the selected candidate |
| + exponential anchors, telescoping reasons | 5 283 | marginal, wall cost exceeds gain |
| **full prefix chain on the trail** (`RESOLVO_FULL_CHAIN=1`) | **3 003** | **beats sequential** — hypothesis confirmed |

Findings:

1. It is *not* learnt-clause expressiveness: virtual-ladder's learnt clauses
   are shorter (avg 2.25 vs 2.84 literals) and contain range literals more
   often (59% vs 35%) than sequential's, with equal backjump distances.
2. It is *not* explanation tightness: switching member explanations from the
   interval driver to the tightest prefix bound (the LCG "most general
   explanation" principle) eliminated 40% of clause allocations but changed
   no conflict counts.
3. It *is* trail residency of the intermediate prefix variables: pushing the
   full chain explicitly (justified by monotone chain clauses, in chain
   order) makes virtual-ladder learn *better* than clausal sequential
   (3 003 vs 3 347 conflicts) — but reintroduces the O(n) trail churn the
   virtual design removes, and the overlapping package-event walks make it
   quadratic, so it is not the shipping configuration.
4. Static log-spaced anchors do not capture the chain's value, even with
   telescoping prefix-to-prefix reasons: the benefit comes from
   *adjacent-step* granularity near the moving conflict frontier, which
   exponential spacing misses.

The remaining promising direction is **adaptive chain windows**: when a
package is re-selected at index k' after a previous selection at k, push
explicit chain entries only over `[min(k,k'), max(k,k')]` — the window the
conflict frontier just moved through. That is O(|k−k'|) per re-selection,
which amortizes to exactly the incremental trail cost the clausal sequential
encoding pays, while keeping the first selection of each package at O(1).

## Resolution (2026-06-12, late)

The constant-factor gap was closed by two changes:

1. **O(1) prefix undo.** `recompute_interval` on every popped prefix entry
   was an O(n) scan, making backjumps over chain entries quadratic — this
   was the entire "learning is slow" cost (217 ms → 21 ms of conflict
   analysis on the conflict-heavy benchmark). An undo stack aligned with the
   trail restores the interval in O(1).
2. **Conflict-gated full chains.** Chain entries are pure learning aids, so
   they are only pushed once the solver has conflicted. Conflict-free storms
   keep pure-boundary trail costs; conflicting workloads get the full
   adjacent-step anchor chain (which the experiments showed is what
   sequential-grade learning requires). Driver-gated package events keep the
   chain pushes from triggering redundant watcher walks.

With those, the virtual-ladder encoding wins or ties every benchmark in the
suite (storms, conflict-heavy at V=100, V=500 solve rate, both conda-forge
seeds — on seed 1 it is the only configuration with zero timeouts), with the
single exception of the V=500 mean where clausal sequential remains ~14%
faster at the same solve rate.

## Future applications: interval-encoded requirements

The prefix variables make "which candidate of package P" an integer variable
with bounds literals, and the at-most-one constraint was only the first
consumer. Whenever a clause talks about a *contiguous member-index range* of
one package, it can be rewritten over the existing prefix variables:

- **Requires** `(¬parent ∨ c_a ∨ … ∨ c_b)` becomes three binary clauses:
  `(¬parent ∨ S_pkg)` (the existing at-least-one auxiliary),
  `(¬parent ∨ p_b)` and `(¬parent ∨ ¬p_{a−1})`. This removes the
  O(range) `next_unwatched_literal` scans, the per-requirement sorted
  candidate vectors (a large share of solver memory on big snapshots), and
  lets `decide` jump to `max(lo, a)` instead of skip-scanning.
- **Constrains** excludes the complement of a range — at most two end
  ranges, so two prefix clauses replace both the pairwise and the shared
  auxiliary-variable constrains encodings where the range is contiguous.
- **Locks** become the interval `[k, k]`: two prefix assertions instead of a
  clause per other candidate.
- **Conditions**: the `DisjunctionComplement` candidate slices are again
  range literals over one package.
- With requirements and reasons both in prefix language, learnt clauses
  derive incompatibilities natively as intervals — PubGrub-style range
  unions inside CDCL.

**Implemented for requirements** (encoder: `add_pending_requirement_ranges`):
unconditional single-set requirements over a contiguous member range get the
two redundant strengtheners `(¬parent ∨ p_b)` and `(¬parent ∨ ¬p_{a−1})`.
These are sound regardless of later candidate registrations: the requires
clause plus at-most-one forces the selection into `[a, b]` whenever the
parent is selected, and new members never join an existing requirement's
candidate set. Measured effect: conflict-heavy V=500 went from 10% behind
clausal sequential to 32% ahead (19.5 s vs 28.7 s, 5/5), V=100 to 0.152 s
(sequential 0.23 s), storms unchanged.

**Constrains needs an extra gate.** A constraint forbids a fixed candidate
set; the *allowed* set implicitly includes candidates registered later. An
upper bound `(¬parent ∨ p_b)` derived from today's members would wrongly
exclude an allowed candidate that registers at an index above `b` tomorrow.
Interval-encoding constrains is therefore only sound when every candidate of
the package is already registered (`member_count == total candidates` from
`get_candidates`) so that indices are final — or with machinery to revisit
the clause on registration.

Costs and prerequisites: contiguity must be checked per version set at
encode time (with the current clause forms as fallback); candidates that are
only reachable through constrains/conditions must also be registered as
package members; and every consumer of `Clause::Requires` literals —
`decide`, conflict-graph construction and user-facing error messages — needs
an interval-aware path that maps prefix literals back to version-set ranges.
This is a larger-surface change than the virtual encoding itself and should
be attempted as its own project.

## Materialization vs derivation: the measured tradeoff (2026-06-13)

Callgrind located the cheap-case regression precisely: `derived_value_and_level`
(the per-lookup sibling derivation) costs +8.26 B instructions on real
conda-forge solves (9.21 B vs Binary's 0.95 B), while virtual-ladder is
*cheaper* than Binary in propagation/run_sat (fewer clauses). Two fixes were
prototyped and measured same-epoch (conda-forge seed 5, n=120 paired vs
binary; storm n=1400; V=500 ×3):

| approach | cheap band (median binary/X) | storm n=1400 | V=500 |
| --- | --- | --- | --- |
| virtual-ladder (current) | 0.98× (slightly slower) | 0.14 s (1.6× vs bin) | 3/3, 16 s |
| **bulk-materialize siblings** | **1.05× (faster!)** | 0.26 s + ~1 M clauses (slower than bin) | **0/3 (timeout)** |
| generation-cached derivation | 0.99–1.02× (neutral) | — | — |

Findings:

- **Bulk-materialization confirms the cheap-case win is real**: writing the
  sibling falsifications as trail entries at selection removes the per-lookup
  derivation and makes the *median* cheap problem ~5 % faster than Binary
  (vs virtual-ladder's ~2 % slower). It also matches Binary's solutions
  exactly (0 divergences). BUT it reintroduces O(n) work per selection, which
  is catastrophic where virtual was the whole point: it is slower than Binary
  on the storm (and balloons to ~1 M memoized pairwise reason clauses) and
  solves 0/3 V=500 instances. It is a trivial-solve specialist that sacrifices
  exactly the uv/backtracking regime.
- **The generation cache is a dud**: 82–89 % hit rates, but each hit's
  `Cell` load + generation compare costs about as much as the derivation it
  replaces, so wall-clock is net-neutral to slightly negative. High hit rate
  did not convert to speed.
- **There is no free lunch in a single static encoding.** The at-most-one
  cost is paid either per-lookup (derive: taxes cheap solves, wins storms and
  hard search) or per-selection (materialize/binary: wins cheap solves, taxes
  storms and hard search). The two prototypes sit at opposite ends; neither
  dominates.

The only way to win both ends is **per-package adaptivity**: materialize
siblings for packages selected rarely (the common case — cheap-solve speed of
Binary/bulk), and switch a package to virtual derivation once it is
re-selected past a threshold (the storm signature — virtual's O(1)/selection).
The per-package `bulk` flag needed for this already exists in the bulk
prototype; promotion on re-selection count is the remaining piece. This is the
motivated next step for a genuinely universal encoding.
