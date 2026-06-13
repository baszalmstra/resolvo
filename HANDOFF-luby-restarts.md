# Handoff: Luby restarts for main

Mission: give resolvo a classic CDCL restart policy. This is the
single biggest lever found in the optimization campaign for
conflict-heavy and refutation-bound solves, it benefits plain solves
directly, and it is item 4 (deliberately last) of the porting order in
`docs/design/mainline-techniques-handoff.md`: it requires one product
decision before merging (solution drift, below).

## Why

Without restarts, a wrong early decision poisons an entire long proof:
the solver grinds tens of thousands of conflicts in a subtree that a
fresh start with the learnt clauses avoids outright. Measured on the
linux-64 conda-forge corpus, restarts in isolation (reference commit
below):

- concrete problem 539: 60 s timeout to ok in ~35 s
- concrete problem 186: 118,749 conflicts to 8,998 (~182 s to ~12 s)
- concrete problem 232: 85,268 conflicts to 11,588
- universal problem 138: 30,509 conflicts to 3,400 (64 s to ~10 s)

Combined with the other ported techniques the concrete corpus went
from 1736 s and 7 timeouts to 1233 s and 2 timeouts. Restarts are the
main contributor on the timeout class.

## Reference implementation

- `research-cdcl-modernization`, commit `93020df5` (the policy), design
  note `docs/design/cdcl-modernization.md` (tuning, interactions, and
  the two negative results below).
- `research-universal-night2`, commit `7cbd165d` plus
  `docs/design/night2-integration.md` reconciliation B: the restart
  factored as a single `restart_to_run_root()` primitive with a shared
  `run_root_level`. Port that shape even though main has only one
  caller today; a second restart trigger showed up within a day on the
  research branch.

Both live on the universal-solve lineage; the universal-specific parts
(prefix budgets, env ordering) do not exist on main and the port
shrinks accordingly.

## Mechanism specification

- Luby sequence over a base interval of 128 conflicts. The conflict
  counter is per `run_sat` call and resets on entry.
- A restart backtracks to `starting_level + 1` and KEEPS all learnt
  clauses. It asserts nothing: single-literal learnt clauses re-apply
  through the assertion scan on the next propagate, multi-literal
  clauses are handled by their watches. (If the assertion-watermark
  handoff has landed, its invalidation path covers this automatically;
  the two compose without special handling.)
- THE CONTRACT THAT MUST HOLD: `run_sat`'s `starting_level` is derived
  from the pre-existing stack and is the boundary of preserved prior
  state for the soft-requirement entry mode (the previous solution
  must survive while a soft requirement is being decided). A restart
  must never undo at or below `starting_level`. Keep the existing
  conflict-at-`starting_level + 1` unsolvable detection intact; a
  restart lands exactly at that level and must not be confused with
  it (the reference gates the restart check inside the conflict
  handling after the unsolvable checks).
- Add a restarts-fired diagnostics counter (the reference has one);
  it is the first thing anyone asks for when tuning.

Interaction with the decide queue (PR #4): zero code changes needed; a
restart is a deep undo, which the queue's sync floor already handles.
Run the debug suite anyway, and if the decide oracle was kept, it
verifies the composition for free.

## The product decision: solution drift

Restarts reorder exploration, so a small tail of problems resolves to
a DIFFERENT BUT VALID solution: 12 of ~791 ok problems (1.5%) on the
campaign corpus, worst single-problem wall regression 0.28 s, one
record-set change in the first-50 sweep (idx 49: 517 to 519 records,
reviewed and justified in the reference). The candidate-preference
heuristics still apply; what changes is which valid model the search
reaches first.

This must be decided before merging, once, as policy:

- Option A (recommended): accept, document in the changelog as "valid
  solutions may differ between versions when solver heuristics
  improve". Any future heuristic improvement faces the same question,
  so deciding it as policy here buys room for all subsequent work.
- Option B: a solver option gating restarts (default on, escape hatch
  for byte-stable consumers). Costs API surface and a second
  configuration to keep healthy.

## Validation protocol

1. Suites under both feature sets. Snapshot diffs are EXPECTED to be
   few and must each be reviewed by hand and justified in the commit
   message (different valid solution, version preference still
   respected). Zero outcome flips allowed.
2. Corpus, concrete mode, against the recorded ground truth CSV:
   outcomes identical except timeout-to-completed recoveries; expect
   186, 232 and 539 recovered at the 60 s cap, likely 204 and 935 as
   well when combined with the assertion watermark. Per-row review of
   record-set diffs; report the count and the worst wall regression.
3. Spot-check that cheap solves are unaffected (restarts should
   essentially never fire below a few hundred conflicts; verify with
   the counter that the fired count is zero across the cheap class).
4. Watch item from the campaign: one universal problem doubled its
   conflict count under restarts (bounded, still completed faster than
   the cap; persisted at 4x the base interval, so it is not a pacing
   artifact). Restart pacing can disrupt a lucky trajectory; expect
   one or two bounded cases like this on any corpus, flag them in the
   PR rather than tuning the base interval against single problems.

## Explicitly out of scope (measured negative, do not bundle)

- Learnt-clause database reduction (Glucose-style LBD freezing): made
  the corpus strictly worse; learnt clauses are 2 to 3% of propagation
  visits on this workload and hard proofs reuse broad clause sets
  (88% of frozen clauses were re-learned). Evidence and revert are in
  `research-cdcl-modernization` history; a re-learned-clause counter
  was kept as the early-warning signal if workloads ever change.
- Phase saving: structurally a no-op in resolvo (decisions carry no
  polarity freedom; candidate order belongs to version preference and
  activity).

## Acceptance criteria

- The starting-level contract proven by the soft-requirement tests.
- Concrete corpus: timeout recoveries with zero outcome flips, drift
  within the expected band, per-row justifications attached.
- Restart counter in diagnostics; base interval 128 documented as the
  campaign-tuned value.
- The drift policy decision recorded (changelog entry or option), not
  implied.
