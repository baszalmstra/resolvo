# An incremental watermark for the per-propagate assertion scans

Status: prototyped on `research-assertion-watermark` (2026-06-12), exact
behavior preservation verified by the full test suite (zero inline
snapshot churn), the 1000-seed universal property test, bit-identical
work counters on the release spot set and row-identical outcomes on
the universal and concrete first-50 sweeps.

## Problem

Assertions are single-literal clauses: they have no watches, so their
decision must hold whenever propagation runs. `Solver::propagate`
historically started every call with three full rescans, in a fixed
order:

1. `decide_assertions`: every entry of `negative_assertions`,
2. `decide_learned`: every entry of `learnt_clause_ids` (skipping
   multi-literal clauses),
3. `decide_env_assertions`: every entry of `env_clause_ids` (same
   skip).

Each visit is a `try_add_decision`, which no-ops when the variable is
already assigned the asserted value. Propagate runs at least once per
decision and once per conflict, and hard problems learn tens of
thousands of clauses, so the learnt scan alone is
O(learnt clauses x propagate calls).

### Baseline scan waste (linux-64 corpus, diagnostics counters)

Counters: entries iterated vs entries whose visit actually added a
decision (`Ok(true)`), totals per problem, universal mode.

| problem | propagate calls | conflicts | neg scanned / applied | learnt scanned / applied | env scanned / applied | waste ratio |
|---|---|---|---|---|---|---|
| 433 (7 cells) | 225,331 | 103,945 | 198.5M / 846 | 11.84e9 / 0 | 1.13M / 4 | 1.4e7 : 1 |
| 783 (187 cells) | 393,864 | 5,788 | 183.5M / 82,908 | 985.1M / 0 | 60.0M / 752 | 1.5e4 : 1 |
| 168 (32 cells) | 96,408 | 24,221 | 37.5M / 12,012 | 1.22e9 / 0 | 3.40M / 132 | 1.0e5 : 1 |
| 881 (unsolvable) | 124,582 | 45,144 | 108.9M / 1,678 | 2.92e9 / 0 | 0.62M / 36 | 1.8e6 : 1 |
| 370 (guard, 360 cells) | 10,815 | 744 | 9.58M / 1,702 | 5.01M / 0 | 2.68M / 8 | 1.0e4 : 1 |

`learnt applied` is zero across the corpus spots: `learn_from_conflict`
asserts the UIP literal directly after the backjump, so the learnt scan
only ever matters for re-application after a deeper backjump pops a
single-literal learnt clause's assertion, which the spot problems never
hit. The learnt rescan, the dominant term (billions of entries on the
conflict-heavy class), is essentially pure waste.

## Why the rescans exist at all

Between two propagate calls only two things can make an assertion entry
actionable:

- (a) the entry was appended since the last scan. All three lists are
  append-only: `negative_assertions` grows only in the encoder
  (`encoding.rs`), `learnt_clause_ids` only in `analyze`,
  `env_clause_ids` only in `add_env_clause`. Nothing ever removes or
  reorders entries; `solve_universal` resets between enumerations via
  `SolverState::default()`, which resets the watermark with it.
- (b) the assignment that satisfied the entry was popped by
  backtracking (`undo_until` / `undo_last` / `clear`).

Everything else is a no-op visit.

## Chosen design: per-list cursors + position records + an undo floor

Implementation: `src/solver/assertion_watermark.rs`, integrated in
`Solver::propagate_impl` and the three scan functions
(`src/solver/mod.rs`); `DecisionTracker::take_assert_floor`
(`src/solver/decision_tracker.rs`).

- **(a) Cursors.** One cursor per list: entries at or beyond it have
  never been scanned. Scanning advances the cursor; the lists only
  grow, so a single index per list is exact.
- **(b) Position records + floor.** Every verified entry (its variable
  holds the asserted value) gets one record `(position, group, index)`
  in a max-heap, where `position` is the trail position of the
  verifying assignment: exact (`stack_len() - 1`) when the scan itself
  added the decision, a conservative upper bound (also
  `stack_len() - 1`, the current trail top) when the variable was
  already assigned by other propagation (the tracker does not store
  stack positions per variable, and adding them to the hot
  `DecisionMap` is not warranted; see Risks). The tracker keeps
  `assert_floor`, the minimum stack length observed since the last
  `take_assert_floor()`, maintained by the existing `undo_last` path
  (every truncation goes through it, or through `clear`, which resets
  the floor to zero via `Default`). A truncation to length `n` pops
  exactly the positions `>= n`, so at the start of every propagate the
  watermark pops all records with `position >= floor` back into a
  per-list ordered pending set. When nothing was undone the floor is
  `usize::MAX` and the sync is a single heap peek.
- **Scan.** Each scan function visits its pending set in ascending
  index order, then the unscanned tail, applying the byte-for-byte
  original per-entry code. Multi-literal learnt/env entries are
  inspected once and skipped forever (their literal lists are
  immutable after allocation; watches handle them).

The alternative "rescan everything after any backjump" was rejected on
the counter evidence: propagate runs once per conflict (plus once per
decision), and on the conflict-heavy class conflicts are a large
fraction of propagate calls (103,945 conflicts vs 225,331 propagate
calls on 433), so post-backjump full rescans would keep roughly half
the waste. The precise scheme costs one heap record per assertion and
removes it entirely.

## Exactness argument

The claim: every propagate call performs exactly the same sequence of
`try_add_decision` calls that change state, and returns exactly the
same result, as the historical full rescan.

1. **Skips are no-ops.** An entry is skipped only while it holds a
   verified record. The record invariant: the variable holds the
   asserted value through an assignment at position `<= position`.
   Established at verification; an assignment only disappears through
   trail truncation; any truncation to `n <= position` since the last
   sync implies `floor <= position`, so the sync (which runs before
   every scan, and no truncation can happen mid-propagate) re-pends
   the entry first. A skipped entry therefore satisfies
   `try_add_decision == Ok(false)`: no decision, no level change, no
   trace output, and no possible conflict. Removing no-ops from the
   visit sequence changes nothing observable.
2. **Order is preserved.** The historical order is: groups in the
   fixed order negative, learnt, env; ascending index within each
   group. Pending indices are always below the cursor, so
   pending-then-tail is ascending within each group, and the group
   order is unchanged. Hence the first non-no-op visit (a new
   decision, or the conflict `Err`) is the same entry as in the full
   rescan, which preserves both the trail order of assertion decisions
   and which conflict surfaces first.
3. **Conflicts leave state consistent.** An `Err` returns immediately;
   the conflicting entry stays pending (or unscanned), entries after
   it stay untouched, exactly like the historical early return. The
   subsequent backjump is captured by the floor like any other.
4. **Levels.** A skipped entry's assignment keeps its original level
   (the historical no-op did not touch it either); a re-applied
   assertion records the current propagate level, exactly as the
   historical rescan re-applied it after a backjump. This preserves
   the documented env-assertion behavior (re-application at the
   current level).
5. **Resets.** `undo_until(0)` goes through `clear()`, whose `Default`
   floor of zero invalidates every record; `SolverState::default()`
   (between universal enumerations) resets watermark, tracker and
   lists together.

The conservative position for already-assigned entries errs only
toward extra re-verification (a re-pended entry whose assignment
survived re-verifies as `Ok(false)` and records a tighter bound), never
toward a missed invalidation: the recorded upper bound can only be
popped after the actual assignment position is popped or at the same
truncation.

## Measured results

### Counters (diagnostics builds, same machine, concurrent load)

Entries scanned per list, before -> after; `propagate calls` and
`conflicts` are identical before and after on every problem, which
means the full search trajectory is preserved, not just the outcome.

| problem | neg scanned | learnt scanned | env scanned | total scan reduction |
|---|---|---|---|---|
| 433 | 198.5M -> 881 | 11.84e9 -> 103,944 | 1.13M -> 12 | 115,000x |
| 783 | 183.5M -> 87,608 | 985.1M -> 5,787 | 60.0M -> 940 | 13,000x |
| 168 | 37.5M -> 12,837 | 1.22e9 -> 24,220 | 3.40M -> 165 | 34,000x |
| 881 | 108.9M -> 1,748 | 2.92e9 -> 45,142 | 0.62M -> 41 | 65,000x |
| 370 | 9.58M -> 1,772 | 5.01M -> 743 | 2.68M -> 369 | 6,000x |

After the change the scan visit count decomposes as expected into
one visit per appended entry (each learnt clause is inspected exactly
once: 103,944 visits for 103,945 conflicts on 433) plus one visit per
actual application plus a small re-verification overhead from the
conservative positions (783: 87,608 negative visits for 82,908
applications plus 4,295 appended entries).

### Wall times (release builds without diagnostics, --timeout 240)

The campaign reference CSV times were not reproducible on the shared,
noisy benchmark machine (the unmodified base binary ran 30-50% slower
than the campaign on several problems), so all wall claims below are
same-session A/B pairs: the base commit (`research-universal-combined`,
49ab6387) and the watermark build run back to back, alternating.
Outcomes, cell counts and record counts are identical on every run.

| problem | base | watermark | speedup |
|---|---|---|---|
| universal 433 (7 cells) | 125.8 s | 83.2 s | 1.51x |
| universal 704 (216 cells) | 99.0 s | 36.2 s | 2.74x |
| universal 783 (187 cells) | 37.4 s | 33.0 s | 1.13x |
| universal 168 (32 cells) | 21.9 s | 18.6 s | 1.18x |
| universal 516 (unsolvable) | 72.8 s | 64.9 s | 1.12x |
| universal 881 (unsolvable, median of 3 pairs) | 81.6 s | 84.9 s | 0.96x |
| concrete 186 | 118.4 s | 76.9 s | 1.54x |
| concrete 232 (unsolvable) | 97.9 s | 66.6 s | 1.47x |

Concrete 186 and 232 both time out at the campaign's 60 s cap on the
combined head; with the watermark 186 solves in 77 s and 232 proves
unsolvability in 67 s under a 240 s cap (the base build also finishes
under that cap, slower).

### Guards (cheap classes)

| guard | base | watermark | identity |
|---|---|---|---|
| universal 370 (360 cells) | 3.59 s | 2.84 s | identical cells/records |
| universal 95 (192 cells) | 4.79 s | 4.62 s | identical cells/records |
| universal sweep 0-49 | 70.2 s | 74.1 s | 0 of 50 rows differ vs campaign CSV |
| concrete 0-49 | 51.3 s | 53.7 s | 0 of 50 rows differ vs `full-linux-64-concrete.csv` |

The two sweep-level deltas (-5%/+5%) and the 881 result sit inside the
machine's drift band. Phase-timing attribution on the concrete 0-49 set
(diagnostics builds, `tracing` report): propagation, the only touched
phase, moves 2418 ms to 2483 ms (+65 ms over 50 problems), while
encoding, an untouched phase, drifts +234 ms in the same comparison.
The watermark adds no measurable per-propagate overhead; the wall
deltas on the cheap classes are environmental. The 881 regression
(consistent across three interleaved pairs at roughly -5%) remains
unattributed: its work counters are bit-identical and its propagation
phase is dominated by the watch-list walk, so code layout of the
restructured `propagate_impl` is the prime suspect; profile on a quiet
machine before mainlining.

One first-50 universal sweep terminated silently after problem 33
(exit without panic output); the re-run and all subsequent sweeps
completed with identical outcomes. The same one-off was observed once
during the decide-worklist campaign on this machine and never
reproduced; noted here for the record.

## Risks

- **Heap/pending memory**: one `(usize, u8, u32)` record per
  single-literal assertion plus transient pending sets. Bounded by the
  assertion lists themselves; negligible next to clauses and watches.
- **Conservative positions**: an assertion satisfied by foreign
  propagation deep in the trail records the scan-time trail top; every
  backjump below that top re-verifies it once until the bound
  tightens. Cost is O(stale entries) per backjump, observed
  negligible; storing exact positions in `DecisionMap` would eliminate
  it at the cost of widening the hottest map in the solver.
- **New truncation paths**: any future code that removes trail entries
  without `undo_last`/`clear` would break the floor invariant (this
  also holds for the decide queue's `sync_floor`; the two floors are
  maintained at the same two sites).

## Test plan

- `DecisionTracker::take_assert_floor` unit test (truncation,
  re-arming, clear).
- `AssertionWatermark::sync` unit test (invalidation at the floor,
  multi-literal permanence, cross-group invalidation).
- Full suite with and without `--features diagnostics`, including the
  1000-seed `test_universal_solve_property`; zero inline snapshot
  churn expected and observed.
- Release spot runs: universal targets and guards
  outcome/cell/record-identical (433/704/783/168/516/881/370/95);
  universal 0-49 sweep row-identical to the campaign CSV; concrete
  0-49 row-identical to `full-linux-64-concrete.csv`; concrete 186 and
  232 record-identical between base and watermark builds.
- Recommended before mainlining: a full-corpus universal + concrete
  campaign on a quiet machine, plus a profile of problem 881 (see the
  wall-time section).
