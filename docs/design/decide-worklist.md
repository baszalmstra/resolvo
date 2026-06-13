# An incremental work queue for decide()

Status: prototyped on `research-decide-worklist` (2026-06-11), exact
decision-sequence preservation verified by a debug-build oracle and the
full test suite with zero snapshot churn.

## Problem

`Solver::decide()` selects the next decision by scanning
`requires_clauses` (every requires clause of every parent variable, in
map insertion order), skipping parents not assigned true, skipping
clauses with unsatisfied conditions, walking each remaining clause's
sorted candidate list to find the satisfying or first undecided
candidate, and folding a best-decision accumulator keyed by
(explicit-requirement-first, higher package activity, lower candidate
count). Universal solving adds an `EnvConstrains` pass with the same
shape and a low-priority blocking-clause pass.

The scan is repeated from scratch on every call. On large problems this
is the dominant solver cost: the trail-reuse campaign
(`universal-solve-benchmark.md`) measured the decide loop's full clause
scan as the bulk of the per-cell cost that survives trail-prefix
preservation.

### Baseline scan work (linux-64 corpus, diagnostics counters)

| problem | decide calls | parent entries scanned | clause entries scanned | candidate entries visited | decide time | total |
|---|---|---|---|---|---|---|
| universal 370 (360 cells) | 74,181 | 7.52e9 | 122.4M | 365.8M | 6.50 s | 10.34 s |
| universal 95 (193 cells) | 141,738 | 15.3e9 | 214.4M | 2.61e9 | 15.02 s | 19.56 s |
| universal 46 (11,685 conflicts) | 248,100 | 12.7e9 | 245.5M | 4.31e9 | 15.67 s | 30.80 s |
| concrete 0-49 (41 solved) | - | 1.12e9 | - | 176.5M | 0.95 s | 36.3 s wall |

Two distinct cost classes:

- The **parent loop** dominates the mechanical universal class: problem
  370 iterates ~101k parent map entries per call (clauses are encoded
  for *every candidate* of every reached package, so almost all parents
  are never installed and fail the assigned-true check, but the
  iteration itself is paid every call).
- **Candidate walks** dominate conflict-heavy problems (46, 95):
  satisfied clauses are walked up to the satisfying candidate, and
  frontier clauses are walked fully, on every call.

## Why not a plain priority heap

The fold's replacement rule is not a total order. The running best is
replaced only when the candidate is strictly higher in activity AND
strictly lower in candidate count (plus an explicit gate). With equal
activities the first-encountered item wins, and two items can each fail
to replace the other, so the outcome depends on scan order. A heap
keyed by the heuristic tuple cannot reproduce this; exact preservation
requires replaying the fold *in scan order* over the eligible items.

## Chosen design: position-ordered eligible queue + hot-item fold

Implementation: `src/solver/decide_queue.rs`, integrated in
`Solver::decide` (`src/solver/mod.rs`) and the encoder
(`src/solver/encoding.rs`).

### Items and order keys

Every requires clause and every env-constrains clause is an *item*
with a packed `u64` key `(segment, parent map position, clause
position)`: exactly its position in the original two-pass scan order.
Positions are stable: the parent maps only append, clause lists only
append.

### The eligible queue

A `BTreeSet<(key, item)>` holds a superset of the *eligible* items
(parent assigned true, condition all-false, clause not satisfied by a
true candidate), restricted to items whose parent is currently true:

- Items leave the queue only when an inspection during the fold proves
  them ineligible at the current assignment snapshot.
- Every state change that could make an unqueued item eligible
  re-inserts it (wake-ups below).
- `enqueue` filters on `parent == Some(true)`, so the constant churn of
  candidate variables being forbidden never touches the queue.

### Trail-mirror sync (no hooks in propagate or undo)

All assignments flow through `DecisionTracker::try_add_decision` and
all undos through `undo_last`/`clear`. The tracker records a *sync
floor*: the minimum stack length since the last observation
(`take_sync_floor`). At the start of each `decide()` call the queue
catches up: mirror entries at or beyond the floor are treated as
undone, trail entries at or beyond it as newly assigned, and each
changed variable is routed through occurrence lists. Assignments made
and undone between two calls cancel out, which is exactly right for a
consumer that only compares snapshots. Propagation and the undo paths
pay one `min` per undo and nothing else.

### Wake-up rules (occurrence lists)

For a touched variable with final value `v`:

- `parent_occ` (variable is a clause parent): wake its items only when
  `v == Some(true)`; any other value keeps them ineligible.
- `cand_occ` (variable is a requirement candidate; see the cache
  below): if the requirement entry is already dirty, nothing to do. If
  it is `Satisfied{by}` and `by` is still true, the clause stays
  satisfied regardless of this variable: O(1) skip (the satisfying
  candidate acts like a satisfying watch literal). If the satisfaction
  broke, mark dirty and wake the requirement's items. If the entry is a
  frontier, mark dirty only: a frontier requirement's eligible items
  never left the queue (the fold only dequeues items of satisfied
  requirements; parent/condition dequeues are woken by their own
  lists).
- `cond_occ` (variable appears in a condition disjunction): wake
  unconditionally. Condition literals carry polarity, so a true
  assignment can complete an all-false condition; an earlier draft that
  woke only on `Some(false)` was caught by the cross-check oracle
  within seconds of running the property test.
- `env_occ` (variable is an env-constrains absent/matches candidate):
  wake unless `v == Some(true)` (a true assignment can only satisfy,
  never unsatisfy; undos and net true-to-false flips must wake).

### Per-requirement walk cache

The candidate walk (first undecided candidate, its version set, the
undecided count in that version set, or the satisfying candidate)
depends only on the requirement and the current assignment, so it is
cached once per *requirement* and shared by all clauses with that
requirement. The walk is a verbatim port of the original; activity is
read fresh at fold time (as the original reads it at scan time), so
activity values are never stale.

Occurrence registration for the cache is deferred to the requirement's
first evaluation: most encoded requirements belong to candidate
solvables that are never installed and are never evaluated. Before the
first evaluation the entry is permanently dirty, so missed wake-ups
cannot lose information. This deferral is what makes the queue free on
the encoding-bound cheap class (see measurements).

### The fold: first eligible item + hot items

Replacement requires strictly higher activity. Activities are
non-negative (bumps add a positive constant, decay multiplies by a
factor in [0, 1]) and a package never involved in a conflict has
activity exactly 0. Therefore only items whose requirement mentions a
*hot* name (one whose activity was ever bumped) can ever replace a
running best; cold items matter only as the initial best.

`decide()` therefore:

1. Phase 1: takes the first eligible item in scan order as the initial
   best. Ineligible items reached on the way are dequeued (amortized
   against their insertions).
2. Phase 2: folds the exact original replacement rule over the
   eligible *hot* items after it, in scan order, tracked in a second,
   much smaller `BTreeSet` maintained in lockstep. Names become hot in
   the conflict-analysis activity bump (`mark_name_hot`) and never
   become cold (decay underflow to 0.0 after thousands of conflicts
   only makes the hot set a conservative superset: a hot item with
   activity 0 simply never wins).
3. Stops as soon as replacement is impossible: when the best's
   candidate count is 1 (strictly fewer is impossible) or its activity
   equals the tracked global maximum (strictly higher is impossible).
   `max_activity` is maintained next to `name_activity`: bumps take the
   max, and the uniform decay scales it with the identical f32
   multiplication applied to the identical value, so it stays bit-exact
   with the largest stored activity.
4. The blocking-clause pass is unchanged (it only runs when no
   requires/env decision exists, which is rare and cheap).

## Exactness argument

The original selection is a sequential dominance fold over the
eligible items in a fixed global order. The prototype preserves it
exactly, by construction:

1. **Same sequence.** The queue iterates eligible items in the original
   scan order (the order key is the scan position). The queue invariant
   (every eligible item is queued; proven by case analysis over the
   wake-up rules, and over the dequeue sites which only remove items
   proven ineligible at the current snapshot) guarantees no eligible
   item is missed; ineligible items are skipped exactly as the original
   scan skips them.
2. **Same fold.** The replacement rule is byte-for-byte the original
   (including the explicit gate and the >=/<= asymmetries), applied to
   the same (explicit, activity, count, candidate) tuples: the walk is
   a verbatim port, cached values are invalidated on any candidate
   assignment change, and activity is read at fold time.
3. **Skips cannot change the result.** Cold items after the first
   eligible item cannot replace any best (activity 0 is never strictly
   higher than a non-negative best). The early stops only fire when
   replacement is provably impossible for every remaining item. The
   explicit gate is applied per item, as in the original.

One deliberate, unobservable deviation: the original scan would panic
(`unreachable!`) on an eligible clause whose candidates are all false,
wherever it appears in the scan. The incremental fold panics on the
same condition only when the fold actually reaches that item (an early
stop can skip it). The condition is an internal invariant violation
that propagation makes impossible; on any valid run the behavior is
identical.

**Verification.** The original implementation is kept as
`decide_reference()` in debug builds, and every `decide()` call asserts
tuple equality of the results. The full test suite (52 unit, 118
integration with inline snapshots) and the 1000-seed
`solver::universal_prop::test_universal_solve_property` run with this
oracle active and pass with zero snapshot churn. The release spot runs
reproduce identical outcomes, cell counts, record counts, and conflict
counts (the conflict count matching on problem 46, 11,685, means the
entire search trajectory is identical, not just the result).

## Invariants

- *Activation*: an item enters the queue when its parent is assigned
  true (parent wake-up), when any input of a previous negative
  inspection changes (cand/cond/env wake-ups), or at clause
  registration if its parent is already true.
- *Backtrack safety*: no undo hooks; the sync floor plus the trail
  mirror replay every net change, and net-cancelling changes are
  correctly invisible. `undo_until(0)`/`clear` reset the floor to 0.
- *Staleness bounds*: requirement caches are only used clean; an entry
  is dirtied by any assignment change of any of its candidates (once
  occurrence-registered; before that it is permanently dirty).
  Activity values are never cached. The hot set and `max_activity` are
  conservative in the only safe direction (superset / exact).
- *Assumptions on activity arithmetic*: `activity_add > 0` and
  `0 <= activity_decay <= 1` (both are hardcoded today: 1.0 and 0.95).
  A negative add or decay would break the non-negativity lemma behind
  the hot-set restriction; if these ever become configurable the queue
  needs a guard.

## Measured results (same machine, concurrent load possible; counters
are per-problem totals from temporary instrumentation, since removed)

| problem | decide before | decide after | total before | total after | outcome |
|---|---|---|---|---|---|
| universal 370 | 6.50 s | 0.41 s (15.9x) | 10.34 s | 3.56 s | identical (360 cells, 508 records) |
| universal 95 | 15.02 s | 0.93 s (16.2x) | 19.56 s | 5.24 s | identical (193 cells, 776 records) |
| universal 46 | 15.67 s | 1.74 s (9.0x) | 30.80 s | 11.21 s | identical (11 cells, 556 records, 11,685 conflicts) |
| concrete 0-49 | 0.95 s | 0.13 s (7.6x) | 36.3 s wall | 36.2 s wall | identical to ground-truth CSV |

Scan-work counters on 370 (before vs after): 7.52e9 parent entries +
122M clause entries + 366M candidate entries, versus 572k fold visits +
7.7k hot visits + 210k cached walks (4.7M candidate entries) + 14.3M
sync touches. On 46 the candidate-entry count drops from 4.31e9 to
28.4M.

The cheap concrete class pays a residual ~5% encoding overhead for
item registration (17.7 s to 18.6 s over problems 0-49), fully offset
by the decide() win on this set (wall 36.2 s vs 36.3 s). Before the
deferred occurrence registration this overhead was 11% and a net
regression; that variant is in the history for reference.

## What did not work (kept for the record)

- A single eligible-ordered queue folded in full per call: correct,
  but 243M ordered-set probes on problem 370 made decide *slower*
  (34.5 s). The fold must not visit every eligible item per call;
  the hot-set restriction is what makes it cheap, not the queue alone.
- Copying the hot range into a scratch vector per call: O(hot-queue)
  per call brought it right back (15 s). Lazy ordered probes, paid only
  per visited item, are required on both queues.
- Waking whole requirement item lists on every candidate touch: 133M
  wake iterations. The Satisfied-watch skip, the dirty skip, and the
  frontier no-wake rule cut this to 30M, and the value-filtered
  parent/env wake-ups (with the condition-polarity correction) cut the
  rest.

## Risks

- **Memory**: items (~40 B each, one per requires/env clause) plus
  occurrence lists for evaluated requirements plus the trail mirror.
  Bounded by what the solver already allocates for clauses and watches;
  no corpus problem showed measurable memory pressure, but a dedicated
  measurement is fair follow-up work.
- **The debug oracle doubles decide() cost in debug builds** (tests,
  downstream dev profiles). Acceptable for a research branch; before
  mainlining, consider gating it on a feature or `cfg(test)`-only
  property tests.
- **f32 reasoning**: the `max_activity` bit-exactness argument relies
  on the decay being applied with the same operation to the same
  value. `for_each_mut` and the scalar multiply are both plain f32
  multiplies; if the decay path ever vectorizes differently or the add
  becomes non-uniform, the early stop must fall back to a conservative
  bound (or be dropped: it is an optimization, not a correctness
  requirement).
- **One silent process termination** was observed once during a 50
  problem concrete run mid-campaign and never reproduced (the same
  binary and inputs ran clean twice afterwards, exit 0, identical
  outcomes); the shared volatile target directory of this machine is
  the suspected cause (a foreign binary was definitively observed once
  producing different outputs after a failed overwrite). Worth watching
  in CI.

## Test plan

- Debug-build oracle: every `decide()` call asserts equality with the
  original scan (active in all dev-profile test runs, including the
  1000-seed universal property test).
- `DecisionTracker::take_sync_floor` unit test for truncation
  semantics.
- Full suite green with and without `--features diagnostics`; zero
  inline-snapshot churn.
- Release spot set: universal 370/95/46 outcome-, cell-, record- and
  conflict-identical; concrete 0-49 outcome- and record-identical to
  `full-linux-64-concrete.csv`.
- Recommended before mainlining: a full-corpus universal + concrete
  campaign (the spot set covers the three known cost classes, but the
  corpus tails found every surprise so far).

## Integration with env-literals-last (research-universal-combined)

The queue and the env-literals-last ordering
(`universal-env-literals-last.md`) are merged on
`research-universal-combined`. The reference scan (`decide_reference`,
the debug-build oracle) carries the authoritative three-class
selection: ordinary decisions, installs that would assign a still
unassigned environment literal, and environment literals themselves,
resolved in that order with the original heuristics applied within
each class. The queue reproduces it as a three-slot fold:

- Class membership is dynamic (`SolverState::env_install_pending`
  reads the current assignment), so each eligible item is classified
  at fold time; the class is never part of the order key and never
  affects queue membership. Eligible deferred items simply stay
  queued.
- Phase 1 walks eligible items in scan order and stops at the first
  *ordinary* item, which becomes the ordinary slot's initial best;
  deferred items encountered on the way fold into their own slots
  with the exact replacement rule. When no eligible ordinary item
  exists the walk visits every eligible item, which is the complete
  per-class fold of the reference scan (a cold item can never replace
  a non-empty slot, so no hot filtering is needed on that path; the
  full walk only happens in the short env tail of a universal solve).
- Phase 2 (the hot fold with its two early stops) only ever replaces
  the ordinary best; eligible hot items of the deferred classes are
  skipped because class precedence makes the ordinary result final.
- The encoder-yield check of the env branch
  (`has_pending_clause_encodes`) wraps the selection exactly as in
  its `decide()`: when only deferred decisions or unsatisfied
  blocking clauses remain and clause encodes are pending, `decide()`
  returns no decision so the encode round runs first. The
  blocking-clause pass stays last and unchanged.
- The unit-propagation deferral of pending variant parents needs no
  queue hook: a skipped unit leaves its clause unsatisfied with an
  installed parent, so the item is (or gets woken) in the queue
  through the ordinary parent occurrence wake-up and is later
  selected as a class-1 decision.

Without environment literals (`env_ordering_active` unset) every item
is ordinary and the fold is byte-for-byte the single-class version
above. One unobservable deviation from the reference: when a higher
class is non-empty the reference short-circuits differently (it skips
the env-constrains scan entirely when class 0 or 1 has a best, and
the queue stops filling deferred slots once an ordinary best exists);
the final tuples are identical because class precedence makes the
lower slots unobservable in exactly those cases. The oracle asserts
tuple equality on every `decide()` call through the full suite and
the 1000-seed universal property test.
