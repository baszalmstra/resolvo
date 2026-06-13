# A refutation switch for the env-literals-last ordering

> **Status (2026-06): DISABLED BY DEFAULT.** A corrected full-corpus A/B run
> after Luby restarts landed found the switch to be a net negative, so it
> ships off: `ENV_ORDERING_CONFLICT_LIMIT = u64::MAX` and the work co-trigger
> factors are `0` (see the rationale on `SolverState::env_ordering_suspended`
> and the constants in `src/solver/mod.rs`). The mechanism is retained behind
> the `diagnostics` setters for research only. The tuned constants and
> benchmark narrative below describe the prototype configuration, which does
> not ship.

Status: prototyped and spot-measured on `research-refutation-ordering`
(2026-06-12), on top of `research-universal-combined` (49ab6387).
Follow-up to `universal-env-literals-last.md` and
`decide-worklist.md`.

## The measured problem

The env-literals-last decision ordering is optimal for FINDING
solutions: env literals land at the top of the trail, cell transitions
retract almost nothing, and the linux-64 corpus total fell from 3350 s
to 2267 s with 12 timeouts instead of 23. It is anti-optimal for
REFUTATIONS: deferring env-sensitive installs makes the search exhaust
concrete subtrees before it can reach the env contradictions that
prune them, so proving a region (or the whole formula) unsolvable
explores the largest possible tree first. The regression shows up both
in dead-end region refutations during enumeration and in the final
coverage-complete UNSAT proof.

Combined-head versus the decide-queue-only branch (no env ordering),
linux-64 corpus, quiet-machine CSV numbers:

| idx | combined | decide-only | class |
| --- | --- | --- | --- |
| 577 | cancelled at 240 s | unsolvable, 6.2 s | final UNSAT proof |
| 768 | unsolvable, 165.8 s | unsolvable, 35.5 s | monster transitions |
| 506 | unsolvable, 52.8 s | unsolvable, 18.5 s | final UNSAT proof |
| 669 | unsolvable, 28.3 s | unsolvable, 12.5 s | final UNSAT proof |
| 587 | ok at ~93 s | ok, 17.2 s, 9 cells | heavy mixed search |

## The shipped design: a two-stage per-run switch

All credit assignment below is from solver counters (propagated
decisions, conflicts), not wall time: three concurrent agents shared
the benchmark machine and identical-counter runs differed by 3x in
wall time.

Per `run_sat` call (one cell solve, one seeded attempt, or the final
UNSAT proof), the env-literals-last ordering starts enabled and can be
suspended for the rest of that run; the next run re-arms it. Two
stages, in `src/solver/mod.rs`:

1. **Stage 1, conflict limit** (`ENV_ORDERING_CONFLICT_LIMIT = 32`,
   checked in `learn_from_conflict`): a run that conflicts 32 times is
   probably refuting. Suspend the ordering AND restart the run
   (retract to the root install; learnt clauses and activity carry
   over, a classic CDCL restart). The restart is essential: suspension
   only changes future decisions, while the trail at trip time still
   has the deferral shape (a deep concrete subtree with env literals
   undecided at the bottom), and the search would keep standing in it.
   Measured on 577: suspend-only 39 s / 37k conflicts, suspend+restart
   9 s / 1.5k conflicts.
2. **Stage 2, work deadline** (armed at the FIRST conflict of a run,
   checked per propagated decision in `propagate_impl`): budget
   `min(max(4 * fresh_solve_cost, 2M), 8M)` propagated decisions. A
   run that burned that much after conflicting is refuting even when
   its conflict count stays low (deferral-shaped refutations are
   propagation-heavy and conflict-light). Escalates to a restart WITH
   a package-activity reset: the bumps accumulated under the deferral
   keep steering a long neutral refutation back into the concrete
   subtrees (measured 3.2x on 768).

Two guard rails, both measured as necessary:

- **Never arm the work deadline at run entry.** A conflict-free run is
  marching to a solution over a kept trail prefix; restarting it
  throws trail reuse away wholesale. Tripping conflict-free runs
  reverted 768 to its pre-trail-reuse cost (208M propagated, 133 s).
- **Cooldown after a stage-1 trip** (`ENV_ORDERING_COOLDOWN_FACTOR =
  4`): a tripped run records its cell with a neutral-shaped trail, so
  the next run re-derives that trail with warm-up-like conflicts.
  Without the cooldown the rebuild itself re-trips at the limit and
  the enumeration locks into a trip-rebuild-trip loop (21 consecutive
  trips on 491).

Tunables are diagnostics-only setters
(`set_env_ordering_conflict_limit`,
`set_env_ordering_work_budget`); `solve-snapshot` exposes them as
`RESOLVO_ENV_ORDERING_CONFLICT_LIMIT` / `_WORK_FACTOR` / `_WORK_FLOOR`
environment overrides for sweeps.

## Results (linux-64 spot set, release + diagnostics)

"off" is the same binary with both triggers configured out of reach;
propagated-decision counters, with noisy wall times for scale. decide
walls are same-machine runs of the archived decide-only binary where
available.

### Counterexamples (must improve)

| idx | off (counters / wall) | switch (counters / wall) | decide wall | verdict |
| --- | --- | --- | --- | --- |
| 577 | cancelled at 240 s | 18.6M / 9.3 s, unsolvable | 9.8 s | recovered |
| 768 | ~212.6M / 165.8 s quiet | 121.0M / 59.7-64.9 s | 55.1 s | 1.8x, partial |
| 506 | 138.7M / 49.6 s | 83.7M / 12.5-20.7 s | 32.5 s | 1.7x counters, wall at or below decide |
| 669 | 123.7M / 28.3 s quiet | 43.1M / 11.5 s | 20.6 s | 2.9x, recovered |
| 587 | ok at ~93 s quiet | 12.5M / 4.5 s, 9 cells, 629 records | 17.2 s quiet | recovered, beats decide |

### Guards (must not regress materially)

| idx | off | switch | note |
| --- | --- | --- | --- |
| 370 | 2.40M / 360 cells | 4.60M / 360 cells, 508 records | +92% counters, wall noise-level (4 warm-up trips; transitions median 277) |
| 95 | 6.43M | 5.19M | better |
| 46 | 2.36M | 2.03M | better |
| 450 | 21.2M | 22.1M | +4% |
| 491 | 39.9M / 361 cells | 95.9M / 348 cells | REGRESSION 2.4x, see below |
| 168 | 115.1M / 32 cells | 25.4M / 32 cells | 4.5x WIN |

Concrete mode: outcomes and record counts of the first 50 problems are
identical to `full-linux-64-concrete.csv` (the switch reads
`env_ordering_active`, which a concrete solve never sets; the restart
and reset paths are unreachable).

### The 491 ceiling

491 is the one material guard regression. Its first runs trip
legitimately (warm-up needs > 32 conflicts), and one trip permanently
moves the enumeration onto a different partition: 348 cells, exactly
the decide-only branch's partition (its CSV also records 348 cells,
19.5 s quiet vs combined's 361 cells, 9.25 s). The switch's downside
is therefore CAPPED AT THE DECIDE-ONLY TRAJECTORY: a problem that
trips early behaves as if the ordering never existed, it never does
worse than the pre-ordering branch. The corpus-level bet is that this
cap is the right trade: 168 (a former hard corner) wins 4.5x from the
same mechanism, and every refutation outlier improves.

## Designs tried and rejected (counters in parentheses)

1. **Suspend-only conflict threshold** (the design-space option 1
   verbatim): N=8/16/32/64 swept. Catastrophic cliff: 577 went 10.9 s
   at N=8 but 28-39 s at N=16-64 (26k-37k conflicts) because the
   suspended run keeps searching under the deferral-shaped trail; 370
   needed N >= 128 for zero damage. Rejected for the cliff;
   superseded by trip+restart, which made N=8..64 equivalent on 577.
2. **Restart with activity reset at stage 1**: fixes 768 fully
   (212.6M -> 66.3M) but one reset wrecked the otherwise healthy 95
   (5.3M -> 26.4M, transition median 297 -> 103k). The reset needs
   stronger evidence than 32 conflicts, hence stage 2.
3. **Suspend-only stage 1 + restart stage 2**: regressed 577 (45.9M,
   360 cells enumerated before the verdict) and 669 (152.6M); both
   need the restart at the conflict trip.
4. **Work deadline armed at run entry** (ungated): reverts
   refutation-heavy problems to pre-trail-reuse cost by restarting
   conflict-free reuse runs (768: 208M).
5. **Tighter stage-2 budgets for 768** (flat 4M / 2M): no
   improvement (114-125M); its monster transition burns ~70M even
   when restarted and rescored 2M in. The residual mechanism is the
   one stage-1-reset configuration fixes; see open questions.
6. **Conflict limit 64 + cooldown**: does not help 491 (96.7M) and
   costs 669 its win (43.1M -> 86.3M). 32 stays.

Asymmetric env-first ordering after the trip (design-space option 2)
was not built: with the trip+restart in place, neutral mode already
matches the decide-only trajectories on 577/587/669/506 within noise,
and an exact env-first fold requires the decide queue to walk every
eligible item per call (the phase-1 early stop is only correct for
ordinary-first), a measured 10x decide() regression risk for a gap
that no longer exists on the counterexamples.

## Interaction analysis

- **decide()/oracle coherence**: both `decide()` and
  `decide_reference()` read one gate,
  `SolverState::env_ordering_enabled()` (active AND not suspended).
  The flags only flip inside `learn_from_conflict` and
  `propagate_impl`, never between the queue fold and the reference
  scan of one selection, so the debug oracle stays exact. The full
  suite and the 1000-seed property test run with the oracle asserting
  queue == reference on every call.
- **Unit-propagation deferral**: `propagate_impl`'s skip of pending
  variant-parent units tests the same two flags inline (the watch
  cursor partially borrows the state). A suspended run therefore
  installs forced parents immediately again; an already-skipped unit
  is selected by `decide()` as an ordinary decision (the clause's
  queue item is eligible), so no assignment is lost across the flip.
- **Encoder yield**: the `has_pending_clause_encodes` yield is gated
  on the same flag; a suspended run decides deferred candidates and
  blocking completions directly, which is exactly the pre-ordering
  behavior.
- **Restart soundness**: the restart retracts to `run_root_level`
  (`starting_level + 1`, the root install level), the same trail
  surgery `run_sat`'s own reset path performs; learnt clauses are
  watched or re-asserted as always. The tripping conflict is analyzed
  and learnt BEFORE the restart, so knowledge grows monotonically;
  each stage fires at most once per run, so termination is untouched.
- **Activity reset soundness**: activity is a fold-time heuristic;
  the decide queue's hot set stays a conservative superset and
  `max_activity` scales to zero with the scores, so the queue's early
  stops remain exact.
- **Work budgets (PREFIX_BUDGET machinery)**: untouched and even more
  dormant. The stage-2 budget (<= 8M) fires far below the abandonment
  budget (16x fresh cost), and a restarted run continues under the
  armed prefix deadline, so the wholesale-abandonment fallback remains
  the outer safety net. Zero budget aborts on every spot run.
- **Trail reshape**: a tripped run leaves a neutral-shaped trail; the
  next transition's reshape (full retraction beyond 8 ordinary
  levels) rebuilds it under the ordering, which is the healing path
  the cooldown protects.
- **Seeded phase**: assumptions sit at levels `1..=n` below
  `run_root_level`, so a restart never touches them; a seeded
  refutation trips the switch like a free run and still surfaces
  `AssumptionConflict` correctly.

## Risks

- **491-class bistability**: problems whose healthy ordered
  enumeration needs > 32 warm-up conflicts fall to the decide-only
  trajectory (bounded, but a 2.4x guard regression here). A
  corpus-wide run must count how common this class is.
- **768 residual**: 1.8x improvement against the 3.2x that stage-1
  rescoring achieves; the interaction between rescoring points and
  the monster transition is not fully explained.
- **More tunables**: limit 32, factor 4, floor 2M, cap 8M, cooldown
  4x; all picked from five counterexamples and six guards on one
  corpus. The defaults are compiled constants; the setters are
  diagnostics-only.
- **Cell partitions can change** for problems that trip (491: 361 ->
  348 cells); partitions remain verifier-clean and the property test
  fixed points hold, but lockfile churn on upgrade is possible for
  conflict-heavy problems.

## Test plan

- `test_universal_refutation_switch_trips_and_keeps_verdict`
  (diagnostics): conflict-heavy env-independent unsolvable core under
  a cuda model; with the limit lowered to 2 the free run must trip
  stage 1 (counter >= 1) and the verdict stays
  `UniversalFailure::Unsolvable`.
- `test_universal_refutation_switch_work_deadline_restarts`
  (diagnostics): same universe, conflict limit at `u64::MAX` and a
  one-propagation work budget; stage 2 must fire (restarts >= 1,
  suspensions == 0) and the verdict is unchanged.
- `test_universal_env_literals_decided_last` extended: a mechanical
  enumeration must record zero suspensions (the switch never robs the
  ordering from the class it was built for).
- Full suite green with and without `diagnostics`, zero inline
  snapshot churn, debug oracle active throughout;
  `solver::universal_prop::test_universal_solve_property` (1000
  seeds) green.
- Spot set and concrete-identity runs above. Outstanding before
  mainlining: the full 1000-problem universal + concrete campaign on
  a quiet machine (the coordinator's acceptance run).
