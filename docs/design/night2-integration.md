# Night-2 integration: watermark + CDCL restarts + refutation switch + forensics

Status: integrated on `research-universal-night2` (2026-06-12), merging
four validated research branches, all based on
`research-universal-combined` (49ab6387):

1. `research-assertion-watermark` (e4a3af46): incremental per-list
   cursors plus a trail-position max-heap for the three per-propagate
   assertion scans. Exactly behavior-preserving.
2. `research-cdcl-modernization` (b0f73561): Luby restarts in `run_sat`
   (base interval 128) and unit-only registration for the
   `decide_learned` scan; the learnt-DB-reduction experiment and its
   revert stay in history; phase saving was never built (structurally a
   no-op in resolvo). Plus a diagnostics statistics hook on every solve
   outcome.
3. `research-refutation-ordering` (d1b71ee9): the two-stage per-run
   switch that suspends the env-literals-last ordering for refuting
   runs (stage 1: 32 conflicts, suspend + restart, 4x cooldown;
   stage 2: work deadline armed at the run's first conflict, restart +
   activity rescore). Plus solve-snapshot counters and override flags.
4. `research-outlier-forensics` (228314de): diagnostics only (cell-pin
   attribution counters, `solve-snapshot --cells-dump`).

The merge is three commits (forensics+watermark, +cdcl, +refutation),
each compiling and passing both suites (`cargo test` and
`cargo test --features diagnostics`, including the 1000-seed
`solver::universal_prop::test_universal_solve_property` with the
decide-queue debug oracle asserting `decide() == decide_reference()` on
every call).

## Reconciliation A: decide_learned (watermark x unit-only registration)

The watermark branch made `decide_learned` scan `learnt_clause_ids`
through cursor + pending sets; the cdcl branch renamed that list to
`unit_learnt_clause_ids` and registers only single-literal learnt
clauses. The integration COMPOSES both: the list receives only unit
clauses, and the watermark cursor/heap scheme runs over that filtered
list.

The watermark's exactness argument re-checked against the filtered
list:

- The historical scan visited every entry and `continue`d on
  multi-literal ones without any state change; the watermark inspected
  each multi-literal entry exactly once (cursor advance, no record)
  and never again. Unit-only registration removes exactly those
  entries from the list, i.e. only entries on which neither scheme
  ever acted.
- Append order of the unit entries is preserved (the relative order of
  two unit clauses is unchanged by dropping the multi-literal entries
  between them), so the first non-no-op visit, and therefore the trail
  order of assertion decisions and which conflict surfaces first, is
  identical.
- `apply_learnt_assertion` loses its `Option` return: a multi-literal
  entry can no longer reach it, and it now `debug_assert`s the
  registered clause is unit.

Verified: zero inline snapshot churn across both suites, and the
universal first-50 sweep is row-identical to the pre-integration
campaign CSV (see the validation table).

## Reconciliation B: one restart primitive, two callers

Two restart mechanisms arrive in this merge: the Luby pacing
(`maybe_restart`, conflict-scheduled) and the refutation switch's stage
restarts (`take_env_ordering_restart`, triggered by ordering
suspension). They are factored around a single primitive:

- `Solver::restart_to_run_root(level)` retracts the trail to
  `SolverState::run_root_level` (`starting_level + 1`, the run's root
  install level) and is the only retract path of both triggers. The
  cdcl branch's `restart_floor` (`starting_level`, retract target
  `floor + 1`) and the switch's `run_root_level` were the same value
  under different conventions; `restart_floor` is gone.
- Everything at or below the root level (assumption decisions of a
  seeded cell, a preserved prior solution under a soft requirement)
  survives by construction; learnt clauses and activity scores carry
  over; the decide queue absorbs the truncation through its sync floor
  and the assertion watermark through its assert floor (both
  maintained inside `undo_last`/`clear`, which `undo_until` funnels
  through).

The decided and documented interplay:

- **Consumption order**: in `propagate_and_learn`'s conflict path a
  pending switch restart is consumed first; the Luby check runs only
  when no switch restart fired. Both retract to the same level, so
  running both would only advance the Luby pacing for a no-op retract.
- **Luby counter on a switch trip**: consuming a switch restart resets
  `conflicts_since_restart` to zero. A new descent begins, and the
  conflicts that led to the trip must not also be billed to the next
  Luby interval (otherwise a stage-1 trip at 32 conflicts would fire
  the first Luby restart 96 conflicts into the fresh descent for
  free). The Luby index (`restarts_performed`) advances ONLY when the
  Luby trigger itself fires, so the pacing sequence stays the textbook
  one.
- **Suspension across Luby restarts**: a Luby restart inside a tripped
  run keeps the suspension. It is per-run state, reset only at
  `run_sat` entry; a restart is a retract, not a new run.
- **Stage-2 rescore vs Luby pacing**: the activity rescore does not
  touch the Luby state; subsequent Luby restarts of the same run
  simply re-descend under the rebuilt scores. No coupling was added
  because none is needed: the rescore is a heuristic event, the pacing
  a schedule.
- **Counters stay distinguishable**: `restarts` (Luby) vs
  `env_ordering_suspensions`/`env_ordering_restarts` (switch stages);
  `solve-snapshot` prints all three per problem (new
  `Solver::restart_count()` accessor).

## Reconciliation C: SolverState fields and the decide oracle

All three code branches add `SolverState` fields and touch
`run_sat`/`propagate_impl`; the merge keeps each group of fields with
its own documentation block, with `run_root_level` shared as described
above. The oracle-coherence invariant of the refutation switch is
unchanged by the integration: the switch flags flip only inside
`learn_from_conflict` and `propagate_impl`, never between the
incremental queue fold and the reference scan of a single `decide()`
selection, and both read the one gate `env_ordering_enabled()`. The
Luby restart adds no new flip site (it is a pure retract), so the
debug oracle remains exact; both suites run with it active.

## Reconciliation D: kept-prefix work budgets

The `PREFIX_BUDGET` machinery is untouched. The stage-2 work deadline
(<= 8M propagated decisions) still fires far below the per-run
abandonment budget (16x fresh cost), and Luby restarts do not pause
either budget (a restarted run is doing real search, exactly the case
the budget exists to bound).

Observed on the integrated head: ZERO budget aborts on every run of
the validation set, including 265 at both 240 s and 600 s and 212 at
600 s, the two problems where the forensics note had documented the
abort false-firing on intrinsic walls (it cost 265 a 550-cell
re-enumeration and doubled 212's bill). With restarts landed, the wall
transitions burn their work in restarted descents that stay under the
per-run deadline, and both problems now complete WITHOUT the fallback
re-enumeration. The budget machinery is fully dormant on the spot set:
no longer harmful here, kept as the outer safety net, nothing changed.

## Validation

Quiet machine, release build with `--features diagnostics`,
`--verify --timeout 240` (212 and the 265 re-run at 600 s), binary
`solve-snapshot-integrate2.exe` at the merge head. The expectation
column is each problem's BEST branch number from the four campaign
notes (their spot runs were also release + diagnostics unless noted).
"No-switch probe" rows re-ran the same binary with the refutation
switch configured out of reach through the diagnostics env overrides,
to attribute deltas.

### Refutation counterexamples (must improve)

| idx | expectation | integrated | verdict |
| --- | --- | --- | --- |
| u577 | ~9 s unsolvable | 9.4 s unsolvable | match |
| u768 | 55-65 s unsolvable, 121.0M propagated | 82.4 s, 124.5M propagated | counters at parity (+3%); wall above band, unattributed (single run) |
| u506 | 13-21 s unsolvable | 9.8 s | better |
| u669 | ~12 s unsolvable | 11.6 s | match |
| u587 | ~5 s ok, 9 cells / 629 records | 6.4 s, identical solution shape | match |

### CDCL targets

| idx | expectation | integrated | verdict |
| --- | --- | --- | --- |
| c539 | ~35 s ok, 281 records (was timeout) | 33.6 s ok, 281 records | match |
| c186 | ~12 s ok, 359 records | 11.8 s ok, 359 records | match |
| c232 | ~20 s unsolvable | 16.6 s unsolvable | better |
| u138 | ~10 s ok, 4 cells / 387 records | 9.1 s, identical | match |
| u632 | cdcl head 38.8 s, 9,735 conflicts | 34.8 s, 20,037 conflicts; no-switch probe: 26.3 s, 9,735 conflicts / 54.6M propagated, BIT-IDENTICAL to cdcl-alone | wall match; conflict delta is switch-driven |
| u881 | cdcl head 32.2 s, 38,017 conflicts | 50.5 s, 35,064 conflicts / 251.5M propagated; no-switch probe: 32.1 s, 38,017 / 138.3M, BIT-IDENTICAL to cdcl-alone | switch-driven +82% propagated, open |
| u516 | measure (cdcl saw 77.9k conflicts vs base 37.2k) | 57.6 s, 56.6k conflicts / 425M propagated | regression halves with the watermark; wall beats every prior measurement (base 72.8-76.7 s, cdcl-alone 101+ s) |

The two bit-identical no-switch probes are the strongest correctness
evidence for reconciliations A and B: with the switch quiescent, the
integrated watermark + Luby + unit-only-scan head reproduces the cdcl
branch's counters exactly, on a 138M-decision unsolvability proof.

### Watermark targets

| idx | expectation | integrated | verdict |
| --- | --- | --- | --- |
| u433 | watermark-alone 83.2 s, 7 cells | 93.5 s, 7 cells / 318 records, 66.2k conflicts (base trajectory: 104k) | ok (diagnostics build; restarts cut conflicts 36%) |
| u704 | watermark-alone 36.2 s, 216 cells | 174.4 s, 216 cells / 780 records, 78 ordering suspensions, 1.03G propagated; no-switch probe: 40.8 s, 118.8M | SWITCH REGRESSION (4.3x), open; see below |
| u783 | campaign 26.1 s, 187 cells / 821 records | 6.0 s, 192 cells / 814 records | 4x faster; small switch-driven partition change, verifier-clean |
| u168 | campaign 14.5 s, 32 cells / 325 records | 4.8 s, identical partition | 3x faster |

### Guards (must not regress materially)

| idx | expectation | integrated | verdict |
| --- | --- | --- | --- |
| u370 | 2.6-3.4 s, 360 cells / 508 records, identical | 3.2 s, identical | pass |
| u95 | 192 cells / 685 records | 3.0 s, 192 cells / 684 records | one-record solution change (restart trajectory), verifier-clean |
| u46 | 11 cells / 563 records | 1.9 s, identical | pass |
| u450 | 360 cells / 570 records | 5.2 s, identical | pass |
| u491 | the decide-only 348-cell trajectory (switch's known cap) | 25.1 s, 348 cells / 338 records, 100.0M propagated (decide-only: 95.9M) | as predicted |
| u265 | 180-240 s, 708 cells; "restarts may cut the wall" | cancelled at 240 s (550 cells recorded, then the cuda [11,12) wall); at 600 s: ok in 292 s, 712 cells / 910 records, verifier-clean | wall NOT cut: the wall transition alone burns 1.39G decisions through 178+ Luby restarts; but zero budget aborts and no fallback re-enumeration (vs the forensics run's abort + re-pay) |

### The mystery: problem 212

**Problem 212 completes for the first time in any benchmarked
configuration**: ok in 237.5 s (cap 600 s), 120 cells, 796 distinct
records, verifier-clean, zero budget aborts (the forensics run died
in the wall after cell 117 with the budget abort doubling the bill).
The no-switch probe also completes (245.6 s, 120 cells / 799
records): the crack is the watermark (propagation cost) plus Luby
restarts (wall refutations); the refutation switch is roughly neutral
here. 77,975 conflicts, 705.6M propagated decisions, 224 Luby
restarts, 19 suspensions, 5 stage-2 restarts.

### Sweeps (identity checks)

- Universal first-50 (cap 60 s): 48 of 50 rows identical to
  `full-linux-64-universal-combined.csv`. Two diffs, both
  verifier-clean and individually justified:
  - idx 5: 48 -> 171 cells, 292 -> 400 records, wall parity (3.36 s vs
    3.38 s reference). Switch-attributed: a no-switch probe reproduces
    the reference 48 cells / 292 records exactly, even with 5 Luby
    restarts firing. The 491-class partition bistability on a sweep
    problem.
  - idx 49: 559 -> 570 records, 64 cells unchanged. Restart
    trajectory; the cdcl branch saw the same problem's concrete twin
    move (below).
- Concrete first-50 (cap 60 s): outcomes and record counts identical
  to `full-linux-64-concrete.csv` except idx 49 (517 -> 519 records),
  exactly the cdcl branch's one documented record-set change.

### Open regressions (all attributed to the refutation switch, none to the integration)

1. **u704, 4.3x wall**: 78 stage-1 trips across a 216-cell
   enumeration; the cooldown does not prevent a sustained
   trip-rebuild pattern here. 704 was not in the refut branch's guard
   set; this is new evidence that the 491-class is not always capped
   at "decide-only cost" (704's trips also burn 8.7x the propagation
   of the no-switch run). The strongest argument for a switch retune.
2. **u881, +57% wall / +82% propagated** and **u632, 2x conflicts at
   wall parity**: one suspension + one stage-2 rescore perturbs these
   unsolvability proofs.
3. **Sweep idx 5 partition blowup** (48 -> 171 cells at wall parity):
   lockfile-churn concern, not a cost one.
4. u768's wall sits above the reference band at counter parity
   (+3% propagated); not reproduced or attributed, likely
   build/machine variance.

Counterbalancing: the switch's own targets all hold (577 from timeout
to 9 s, 506/669/587 at or better than the decide-only walls, 168 at
4.5x) and 491 lands exactly on its predicted cap.

### Recommendation

The integration itself is sound: oracle-checked suites green, exact
composition confirmed bit-identically by the no-switch probes, zero
inline snapshot churn, concrete identity preserved. Watermark + Luby
restarts + unit-only scan look unconditionally mainlinable on this
evidence. The refutation switch carries the corpus-level risk: its
wins are real and large on refutation outliers, but 704/881/632/idx-5
show the trip cost class is broader than its original guard set
suggested. Before mainlining the switch defaults, run the full
1000-problem campaign on this head twice (switch on vs off via the
existing env overrides) and revisit the stage-1 limit / cooldown
against the 704 trip-thrash pattern.
