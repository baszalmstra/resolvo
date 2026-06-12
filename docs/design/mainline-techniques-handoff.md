# Handoff: solver techniques worth porting to main

Status: handoff, 2026-06-12. Written after the two universal-solve
optimization nights (see `universal-solve-benchmark.md` for the full
history). Four techniques and two bug fixes developed on the
universal-solve branch stack are valuable for PLAIN solves and can be
ported to main independently of universal solving. Two negative results
are recorded so nobody re-spends the effort.

All numbers come from the linux-64 conda-forge corpus (1000 problems,
seed 0, 60 s cap, `bench-universal/` artifacts, ground truth
`full-linux-64-concrete.csv`). Concrete mode means a plain
`Solver::solve` against simulated machine candidates: it exercises
exactly the code paths main has.

Combined effect of items A through D on the concrete corpus: total wall
1736 s to 1233 s, timeouts 7 to 2 (186, 204, 232, 539, 935 recovered;
553 and 982 remain).

## Where the code lives

| bookmark | content |
|---|---|
| `research-cdcl-modernization` | A (restarts), D (unit-only scan), F (negative results, prototype and revert in history) |
| `research-decide-worklist` | B (decide queue plus oracle) |
| `research-assertion-watermark` | C (assertion watermark) |
| `research-universal-night2` | everything merged and reconciled (note `night2-integration.md`) |

Design notes: `cdcl-modernization.md`, `decide-worklist.md`,
`assertion-watermark.md`. Each technique was committed independently
green (`cargo test` and `cargo test --features diagnostics`).

A practical note before porting: main has no benchmark harness. The
`tools/solve-snapshot` driver and the snapshot extensions live on the
branch stack. Either cherry-pick the tool together with the first
technique, or accept the branch corpus results as the performance
evidence and validate ports on main with the test suite plus targeted
concrete spot checks. When building the tool for sweeps, build with
`--features diagnostics` (the TOOL feature, which forwards to
resolvo/diagnostics); building with `--features resolvo/diagnostics`
alone silently compiles out the tool's env overrides, which invalidated
a whole A/B run before we caught it.

## A. Luby restarts (the big one)

What: classic CDCL restart policy in `run_sat`. Luby sequence over a
base interval of 128 conflicts, counted per `run_sat` call; a restart
backtracks to the level just above `starting_level` and keeps all
learnt clauses. Implemented as a single `restart_to_run_root()`
primitive (night-2 reconciliation B) so other restart triggers can
share it.

Measured, restarts in isolation (commit `93020df5`): concrete 539 from
timeout to ok in ~35 s, concrete 186 from 118,749 conflicts and ~182 s
to 8,998 conflicts and ~12 s, concrete 232 from 85,268 to 11,588
conflicts, universal 138 from 30,509 to 3,400 conflicts. Restarts are
what crack long refutations: without them a wrong early decision
poisons the entire proof.

Port notes:
- Main's `run_sat` derives `starting_level` from the stack for the
  soft-requirement entry mode (the previous solution is preserved prior
  state). The restart floor must respect that: never undo at or below
  `starting_level`. The branch implementation already honors the
  equivalent contract; port the floor logic together with the policy.
- The restart asserts nothing after backtracking: single-literal learnt
  clauses re-apply through the assertion scan, watches handle the rest.

THE DECISION MAIN HAS TO MAKE, solution drift: restarts reorder
exploration, so a small tail of problems resolves to a different but
still valid solution (12 of ~791 ok problems on the corpus, 1.5%; the
per-row list is in `night2-integration.md`). The candidate-preference
heuristics still apply, but resolvo today is bit-stable run to run and
some downstreams may rely on byte-identical lockfiles across resolvo
upgrades anyway (any heuristic change breaks that, so this is arguably
no different from any other solver improvement). Options: accept the
drift (recommended; document it in the changelog), or ship restarts
behind a solver option defaulting on with an escape hatch.

## B. Incremental decide() queue

What: replaces decide()'s full rescan (every requires clause of every
installed parent, every call) with a position-ordered eligible queue,
folded in the exact original scan order. Key facts discovered on the
way, documented in `decide-worklist.md`:
- The selection fold is NOT heap-shaped: the replacement rule (strictly
  higher activity AND strictly lower candidate count, plus the explicit
  gate) is not a total order, so scan order is semantically load
  bearing. Exact preservation requires replaying the fold in scan
  order over eligible items; a priority heap would change solutions.
- Only "hot" names (activity ever bumped) can ever replace a running
  best, because untouched activity is exactly zero and the rule
  requires strictly higher. This is what makes the incremental fold
  cheap: phase 1 takes the first eligible item, phase 2 folds hot items
  only, with early stops when replacement is provably impossible.
- Sync against backtracking uses a trail truncation watermark
  (`DecisionTracker::take_sync_floor`), no undo hooks.
- The original scan stays alive in debug builds as `decide_reference()`
  and every `decide()` call asserts tuple equality. Keep this oracle on
  main: it caught two real bugs during development within seconds.

Measured: decide time on heavy universal problems drops 9 to 16x;
concrete decide time on the first 50 problems 0.95 s to 0.13 s with
identical outcomes and records everywhere (corpus-scale exactness was
verified: zero cells/records deviations across 1000 problems against
the pre-queue head). Cheap problems are wall-neutral after the
deferred occurrence registration commit (registration costs moved to
first evaluation; without that the cheap class regressed 11%, that
intermediate state is in the history for reference).

Port notes: main's decide() is SIMPLER than the branch version (no
EnvConstrains pass, no blocking pass, no deferral classes), so the port
shrinks: drop the env item kinds and the class fold, keep queue, walk
cache, hot fold, sync floor, oracle. One open flag from the author:
problem 881 ran ~5% slower despite bit-identical work counters,
suspected code layout of the restructured `propagate_impl`; profile
once on main before declaring done.

## C. Assertion-scan watermark

What: `propagate_impl` starts every call by rescanning ALL of
`negative_assertions` and ALL single-literal learnt clauses (on main;
the branch also has env assertion lists). Almost every iteration is a
no-op `try_add_decision`. The watermark makes these scans incremental:
per-list cursors capture appended entries, and a per-assertion trail
position in a max-heap, invalidated through a tracker sync floor,
captures assertions popped by backtracking. Exactly behavior
preserving, including which conflict fires first (scan order among
pending entries is preserved) and the level an assertion is recorded
at.

Measured: scan work drops four to five orders of magnitude (problem
433: 12.0e9 scanned entries to 104,837; its learnt-list scan alone was
11.84e9 entries with ZERO applications ever). Wall up to 2.7x on
conflict-heavy problems; concrete 186 and 232 1.5x before restarts made
them cheap anyway. Guards bit-identical (propagate calls and conflicts
unchanged on every problem).

Port notes: self-contained (decision tracker plus propagate_impl plus
the lists). Compose with D: the learnt-clause list should only ever
receive unit clauses (D), and the cursors run over that filtered list
(night-2 reconciliation A proved the composition exact).

## D. Unit-only registration for the learnt assertion scan

What: `decide_learned` scans `learnt_clause_ids` only to act on
single-literal clauses, but the list contains every learnt clause, so
the scan is O(all learnt clauses) per propagate call. Register only
unit learnt clauses in the scanned list (commit `75d5341f`). Trivial,
exactly behavior preserving, roughly halves wall on learn-heavy
problems on its own. If C is ported, port D first or together.

## E. Bug fixes to cherry-pick regardless

Both found during the benchmark campaign, both in mainline code, both
have regression tests on the branch:
1. `MappingIter` drops entries of sparse mappings (iteration stopped at
   `len` instead of `max`); fix in commit `37aca23f`.
2. `SnapshotProvider` version-set id shadowing: the first additional
   version set allocated on top of a deserialized snapshot shadows the
   snapshot's last version set (`Mapping::max()` off-by-one in the
   base offset); fix in the snapshot extension commit (`4e79e9bb`,
   `additional_base` field), extractable independently.

## F. Negative results, do not re-spend this effort

1. Learnt-clause database reduction (Glucose-style LBD freezing): made
   everything worse on this workload. Learnt clauses are only 2 to 3%
   of propagation visits (Requires clauses dominate at ~91%), so the
   DB costs little to keep, and the hard proofs REUSE broad clause
   sets: learn-time-LBD freezing re-learned 88% of frozen clauses on
   problem 881 and pushed concrete 539 back to timeout; usage
   protection plus a grace interval still left it strictly worse.
   Prototype and measured revert are both in
   `research-cdcl-modernization` history, and a re-learned-clause
   diagnostics counter was kept: if main's workloads ever show high
   re-learn rates, that counter is the early warning to revisit.
   Caveat: verdict is for conda-shaped dependency problems; do not
   generalize it to other SAT workloads.
2. Phase saving: structurally a no-op in resolvo. Decisions carry no
   polarity freedom (`set_propagate_learn` hardcodes installing the
   candidate, value true) and candidate order is owned by version
   preference, activity and counts, which phase saving must not
   override because solution quality depends on them.

## Suggested porting order

1. E (bug fixes): independent, small, tested.
2. D then C (or together): exactly behavior preserving, zero snapshot
   churn expected, easy review.
3. B (decide queue): behavior preserving with an oracle enforcing it,
   but a large change; the oracle plus zero snapshot churn is the
   review story.
4. A (restarts): biggest payoff, only after the solution-drift decision
   is made, with the concrete corpus identity diff attached to the PR.

Each step should run the full suite under both feature sets and, until
main grows a benchmark harness, at least concrete spot checks of
problems 186, 232, 539 (the timeout class) and a first-50 outcome
comparison against `full-linux-64-concrete.csv`.
