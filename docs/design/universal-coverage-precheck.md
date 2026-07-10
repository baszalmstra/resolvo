# Coverage precheck: implementation and measurement

Branch: `claude/implement-handoff-measure-performance-n6fen2`, based on
`universal-solver` @ `a4f2d430`.

## What was implemented (per handoff, stages 1–5)

1. **Oracle-clause index** — `env_oracle_clause_ids` on `SolverState`,
   appended at the central `add_clause` allocation point (before the unwatched
   early return). Initialized in `default`.
2. **Indexed witness assembly** — `rebuild_witness_scratch` merges
   `env_oracle_clause_ids` and `env_clause_ids` by ascending `ClauseId` into
   `WitnessScratch`, reproducing the old exhaustive clause-DB scan. A
   `debug_assert` compares the merged sequence to `exhaustive_environment_clause_ids()`
   on every rebuild.
3. **Existence-only API** — `witness_exists_indexed` (most-constrained-first
   search only) split out of `find_witness_indexed`; `WitnessScratch::has_witness`
   and `Solver::has_environment_witness`. All witness-producing callers keep the
   canonical false-first two-pass search.
4. **Precheck** — before each normal free episode:
   `!seeded && active_witness.is_none() && !has_environment_witness() → break`.
   `COVERAGE_PRECHECK_BREAK` counter, `CoveragePrecheckStats` diagnostics
   (calls/breaks/sizes/build+search time), `set_test_coverage_precheck_disabled`
   and `RESOLVO_DISABLE_COVERAGE_PRECHECK` (diagnostics) A/B levers.

Tests: `cargo test` (74 lib + 155 integration) and
`cargo test --features diagnostics` (74 + 159) pass, including the 1000-seed
property gate, reseed/fixed-point tests, and new structural tests
(index-vs-exhaustive, `has_witness == find_witness.is_some()`, scratch reuse).

## Benchmark setup

- Corpus: real conda-forge linux-64+noarch repodata, snapshot built with
  rattler's `create-resolvo-snapshot` (`--machine`/`--symbolic`/`--env-spec`,
  ported to the current resolvo API). 129 environment version sets, embedded
  oracle relation table (disjoint 660 / subset 448 / superset 675 / equal 2 /
  unknown 1664).
- Driver: `tools/solve-snapshot --features diagnostics`, `--mode universal
  --verify --project`, model `model-linux-64.json`, 1000 problems, seed 0.
- **Pre-existing tool bug**: `solve-snapshot` accumulates interned state in the
  shared snapshot pool across problems; from a cold start this makes a later
  solve hang (non-cancellable witness search / run_sat). Problem i=638 hangs
  even in isolation on **both** base and head (a documented "witness-search
  blowup ignores cancellation" issue), unrelated to the precheck. Worked around
  by running in fixed 25-problem fresh-process batches (bounds accumulation,
  amortizes the ~6s snapshot load); i=638 skipped in both. 999/1000 measured.

## Results (999 common problems, base @a4f2d430 vs head)

| metric | value |
|---|---|
| total wall base → head | 1195.5s → 1182.8s (**−1.07%**) |
| median per-problem base/head ratio | 1.017 (head ~1.7% faster median) |
| coverage-complete precheck breaks | 718 (avoided final `run_sat` refutations) |
| total precheck cost | 107.8s (search-dominated; build 0.3s) |
| delta on sub-100-cell rows (n=980) | **−22.6s** (improvement) |
| delta on ≥100-cell rows (n=19) | **+9.9s** (regression) |

Biggest wins (multi-pass problems whose expensive final refutation is skipped):
`#158` 4.9→1.5s, `#427` 2.3→0.6s, `#262` 7.3→5.7s, `#449` (3 cells) 32.3→31.0s.

Biggest regressions (precheck runs once per cell → O(cells²) search work):
`#490` (727 cells) +2.5s, `#195` (528) +1.8s, `#338` (481) +1.7s,
`#865` (944) +1.2s.

## Exactness (the critical finding)

The handoff's non-negotiable requirement — cells/records/projections/error text
byte-identical — is **violated** by the precheck.

- CSV column comparison flagged 1/999 (`#158`, env_literals 12→11).
- **Content-level** cell-condition diffs (cells-dump) found MORE: the CSV
  count-check misses drifts that keep the literal count (e.g. `#427` drifted in
  content with the *same* env_literal count). High-cell/big-win sample:
  2/25 drifted (`#158`, `#427`). Unbiased every-20th sample:
  **0/40 drifted** (10 of 50 were the pre-existing pathological-slow problems
  that exceed a 150s isolation timeout, so uncompared). So drift is **rare and
  concentrated on multi-pass (trail-reuse-abandonment) problems**, not
  widespread — but it is real and the CSV count-check under-reports it.
- Every observed drift is a **logically-equivalent, verify-clean cover** — e.g.
  head drops `not (__cuda in *)` where `__cuda absent` already implies it. All
  721 ok solves verify clean in head; records and projections match.

### Root cause

`enumerate_universal_with_fallback` runs the pass with **trail reuse** first; a
reuse pass that exhausts its kept-prefix work budget on the coverage-complete
`run_sat` returns `ReuseAbandoned`, which re-enumerates the recorded cells as
seeds in a **non-reuse fallback** that heals them into different (equivalent)
canonical cells. The precheck fires at coverage-complete and breaks to `Done`
**before** that abandonment, returning the unhealed reuse-pass cells. The
optimization's saving (skipping the expensive final refutation) and the drift
(skipping the heal) are the **same event**. Trail reuse postdates the reference
commit `e9b41805`, which is why the precheck was exact there and is not here.

The exact-safe gate (`precheck only on !reuse_trail` passes) is byte-identical
to base — but fires **0 times in the first 100 problems** (single-pass reuse
solves never reach a non-reuse pass), i.e. it disables the optimization.

## Assessment vs handoff assumptions

- ❌ "Ordered cells remain unchanged / zero corpus drift" — violated on
  multiple multi-pass problems (equivalent covers, but not byte-identical).
- ❌ "Repeated checks do not materially regress high-cell rows" — ≥100-cell
  rows regress +9.9s total (up to +2.5s on 727 cells).
- ❌ Reference 1.67→0.53s (3×) does not reproduce — net −1.07% on current head;
  trail reuse already bounds the expensive-refutation cost via abandonment.
- ✅ Index is byte-exact vs the exhaustive scan; existence split agrees with the
  canonical search; run-to-run determinism and reseed fixed points hold within
  the precheck-enabled solver.

## Recommendation

The precheck is a **modest net win on the common case** (−22.6s over 980
problems) but, as specified (fire before every free episode, on all passes),
it (a) fails the zero-drift acceptance criterion on multiple problems and
(b) regresses high-cell rows. The exact-safe gate is a no-op. To land it
cleanly one would need to (1) reconcile trail-reuse pass-1 vs fallback cell
extraction so the pre-empted heal is a no-op, and (2) skip/throttle the
precheck on high-cell rows to remove the O(cells²) regression. Absent those,
this does not meet the handoff's own acceptance bar and should not be
default-enabled.
