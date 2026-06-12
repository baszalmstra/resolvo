# Handoff: incremental assertion scans for main

Mission: make the per-propagate assertion rescans in `Solver::propagate`
incremental on main, exactly behavior preserving. This is item 2 of the
mainline porting order in `docs/design/mainline-techniques-handoff.md`
and composes two pieces that should land together (or unit-only first):

1. Unit-only registration for the learnt-clause assertion scan.
2. A cursor-plus-watermark scheme that skips assertions that are
   already applied and not invalidated.

## The problem

`propagate_impl` starts EVERY call with full rescans:

- `decide_assertions`: iterates all of `negative_assertions`, calling
  `try_add_decision` per entry (a no-op when already decided).
- `decide_learned`: iterates ALL `learnt_clause_ids` only to act on
  single-literal clauses, re-asserting each.

Propagate runs at least once per decision and once per conflict, so the
cost is O(list length x propagate calls). On conflict-heavy solves this
is the dominant waste: the measurement campaign found one problem
scanning 11.84e9 learnt-list entries over a solve with ZERO of those
scans ever applying anything, and total assertion-scan work dropping
from 12.0e9 to about 105k entries with the fix (a four to five orders
of magnitude reduction), worth up to 2.7x wall on the conflict-heavy
class and neutral elsewhere.

## Reference implementations (read these first)

All on origin bookmarks of this repository; they target the
universal-solve lineage, so they contain a third list
(`decide_env_assertions`) that main does not have. Port the idea, use
the code as reference, drop the env parts.

- `research-assertion-watermark`, implementation commit `da338c3f`,
  design note `docs/design/assertion-watermark.md` (the exactness
  argument, the rejected simpler design, baseline waste counters).
- `research-cdcl-modernization`, commit `75d5341f` (unit-only
  registration, independent and trivial).
- `research-universal-night2`, note `docs/design/night2-integration.md`
  section "reconciliation A": the two pieces composed (cursors over the
  unit-only list) and the argument why the composition stays exact.

## Mechanism specification

Why the rescans exist at all: an assertion must hold at all times, and
a backjump may pop its decision, so the next propagate must re-apply
it. Between backjumps the rescans are pure waste. Only two things can
make an assertion need attention:

- (a) NEW entries appended since the last propagate. Capture with one
  cursor per list. Verify the append-only property on main
  (`negative_assertions` written by the encoder, learnt clauses by
  `analyze`) and note that `SolverState::default()` resets cursors
  together with the lists between solves.
- (b) Applied assertions whose decision was POPPED by backtracking.
  The reference captures these precisely: when an assertion is applied
  (or verified already-decided), record its trail position in a
  max-heap; a truncation watermark on the decision tracker (minimum
  stack length since last observation) tells the scan which recorded
  positions died, and exactly those assertions are re-applied.

Tracker note: the reference adds `DecisionTracker::take_assert_floor`,
a SECOND floor next to the decide queue's `take_sync_floor` (PR #4).
Two consumers cannot share one floor because taking it resets it.
Either add the second field (what the reference does) or generalize to
per-consumer floors; do not reuse the queue's.

The simpler alternative (rescan everything, but only after a backjump)
was measured and rejected: conflicts and propagate calls have the same
order of magnitude on the hard class (one problem: 104k conflicts vs
225k propagate calls), so it keeps roughly half the waste.

## Exactness requirements (non-negotiable)

The port must be EXACTLY behavior preserving. Specifically:

- Scan order among pending entries is preserved within each list and
  across the lists (the list processing order in `propagate_impl`
  stays as is): which conflict fires first is observable behavior.
- An assertion that conflicts (`try_add_decision` returns `Err`) must
  surface the identical conflict at the identical point.
- Re-applied assertions record the CURRENT level, exactly like today
  (an assertion re-applied after a backjump can legitimately sit at a
  different level than its first application; do not "fix" this).
- The soft-requirement path (`solve` iterating soft requirements with
  the previous solution on the stack) flows through the same propagate
  code; nothing special is needed but it must be in the test net.

Unit-only piece: `decide_learned` only acts on single-literal clauses,
so register only those in the list it scans. Check all readers of
`learnt_clause_ids` on main before narrowing it; if other code iterates
it for different purposes, add a dedicated `unit_learnt_clause_ids`
instead (that is what the reference did) and leave the full list
untouched.

## Validation protocol

1. First, counters: add temporary diagnostics counters for entries
   scanned vs entries applied per list, run the heavy corpus problems,
   and record the waste ratio in the PR (the reference design note has
   the comparison format). Remove scratch instrumentation afterwards.
2. Exactness: full test suite under both feature sets with ZERO inline
   snapshot churn; per-solve conflict counts and propagate-call counts
   bit-identical on spot problems (this is the witness that the search
   trajectory is untouched, the same standard PR #4 used).
3. Corpus: identical outcomes, records, and cells everywhere; wall
   improvements concentrated on conflict-heavy problems. Use the
   solve-snapshot harness as in PR #4. Good targets from the campaign
   data: concrete problems 186 and 232 (about 1.5x each from this
   change alone, pre-restarts), plus whatever the current corpus shows
   as conflict-heavy.
4. One watch item from the reference campaign: a problem showed ~5%
   wall regression with bit-identical work counters, suspected code
   layout of the restructured `propagate_impl`. Profile one
   conflict-heavy problem before declaring done; do not chase ghosts
   if counters are identical and the corpus is net positive.

## Acceptance criteria

- Zero snapshot churn, both suites green, no behavior deviation
  detectable by counters.
- Waste-ratio counters in the PR description, before and after.
- Corpus net positive with no problem materially regressed.
- Composes with PR #4 (decide queue) if that has landed: separate
  floors, shared trail-mirror philosophy, no interference.
