# CDCL modernization: restarts, learnt-clause reduction, phase saving

Status: research prototype (branch `research-cdcl-modernization`).

resolvo's CDCL core predates three pieces of machinery that modern SAT
solvers consider table stakes: restart policies, learnt-clause database
reduction, and phase saving. This note documents how each maps onto
resolvo's solver, the interactions that constrain the design, and what the
prototypes measured on the intrinsically hard solve class (universal and
concrete problems that burn tens of seconds to minutes).

All measurements use the linux-64 snapshot corpus
(`bench-universal/snapshot-linux-64.json`, env model
`tools/solve-snapshot/models/model-linux-64.json`). Counters (conflicts,
decisions propagated) are the primary metric; wall times were collected
while other builds ran concurrently and are indicative only.

## Baseline (head `49ab6387` plus the diagnostics-reporting commit)

| problem | mode | outcome | wall | conflicts | decisions propagated |
|---|---|---|---|---|---|
| 881 | universal | unsolvable | 63.9 s | 45,144 | 162.5 M |
| 516 | universal | unsolvable | 76.7 s | 37,233 | 340.8 M |
| 632 | universal | unsolvable | 55.1 s | 15,296 | 76.5 M |
| 138 | universal | ok, 4 cells / 387 records | 63.7 s | 30,509 | 85.2 M |
| 212 | universal | timeout (240 s) | 240 s | 71,346 | 521.5 M |
| 370 (guard) | universal | ok, 360 cells / 508 records | 2.7 s | 744 | 2.4 M |
| 186 | concrete | ok, 359 records | 182.1 s | 118,749 | 456.7 M |
| 232 | concrete | unsolvable | 111.3 s | 85,268 | 67.3 M |
| 539 | concrete | timeout (240 s) | 240 s | 161,088 | 300.7 M |

A notable baseline observation: on problem 881 only 2.3% of clause visits
during propagation are learnt clauses; `Requires` clauses dominate at 91%.
Learnt-DB reduction therefore cannot win much propagation time directly on
this corpus; its value would have to come from keeping `decide_learned`'s
scan short and from cache effects. Restarts, by contrast, attack the
decision/conflict counts themselves.

## Feature 1: Luby restarts

### Design

- One restart episode per `run_sat` call. `conflicts_since_restart` counts
  clauses learnt by `analyze`; when it reaches
  `RESTART_BASE_INTERVAL * luby(n)` the solver backtracks to
  `starting_level + 1` (the run's root install level) and reschedules with
  the next Luby index. Learnt clauses, activity scores, and the decide
  queue survive; only the partial assignment is abandoned.
- Implemented in `Solver::maybe_restart`, called from `propagate_and_learn`
  directly after `learn_from_conflict`.

### Interaction analysis

- Assumptions (seeded universal cells): assumption decisions occupy levels
  `1..=assumption_levels = starting_level`, so the restart floor pins them.
  `learn_from_conflict` already ends the run before any conflict at or
  below the boundary reaches `analyze`.
- Soft requirements: the preserved prior solution lives at levels
  `1..=starting_level`; same floor argument.
- Kept trail prefix (universal free phase): enters above
  `starting_level = 0` and is restartable scratch state by contract; a
  restart discards it. The prefix work budget stays armed, which is the
  desired behavior: a run that restarts is doing real search, exactly the
  case the budget exists to abort.
- Budgets: `propagated_total`/`prefix_spent` keep counting across restarts;
  no interaction beyond the above.
- Env-literals-last ordering: the deferral classification is dynamic (reads
  the current assignment), so after a restart `decide()` re-derives the
  preferred trail shape from scratch.
- Decide queue oracle: a restart is only an `undo_until`, which the queue
  absorbs through the sync floor; debug builds assert
  `decide() == decide_reference()` on every call and stayed green.
- Unit learnt clauses are re-asserted by `decide_learned` on the next
  propagation round; multi-literal learnt clauses keep their watches, and
  backtracking only unassigns literals, which preserves the watch
  invariant.

### Results

`RESTART_BASE_INTERVAL = 128`, counters vs the baseline table above:

| problem | outcome | wall | conflicts | propagated | restarts |
|---|---|---|---|---|---|
| u881 | unsolvable (same) | 61.4 s | 38,017 (-16%) | 138.3 M (-15%) | 108 |
| u516 | unsolvable (same) | 134.2 s | 77,860 (+109%) | 456.8 M (+34%) | 189 |
| u632 | unsolvable (same) | 31.1 s | 9,735 (-36%) | 54.6 M (-29%) | 30 |
| u138 | ok, 4 cells / 387 records (same) | 8.3 s | 3,400 (-89%) | 48.3 M (-43%) | 14 |
| u212 | timeout (same) | 240 s | 30,400 | 264.5 M | 114 |
| c186 | ok, 359 records (same) | 13.4 s | 8,998 (-92%) | 43.8 M (-90%) | 30 |
| c232 | unsolvable (same) | 21.6 s | 11,588 (-86%) | 14.1 M (-79%) | 37 |
| c539 | ok, 281 records (was timeout) | 63.6 s | 54,465 | 204.8 M | 126 |

Guards: universal 370/95/46 identical outcomes, cells and record counts to
the reference; 370's counters are bit-identical to baseline (no run
reached 128 conflicts, 0 restarts). Concrete first-50: no outcome flips;
one record-set difference (index 49: 517 -> 519 records, 604 conflicts and
3 restarts in that solve, a legitimately changed search trajectory landing
on a different valid solution; per-decision highest-version preference is
untouched by restarts).

The c539 result is the headline: a problem that timed out at 240 s now
solves in about a minute. c186 and c232 shrink by an order of magnitude.
The one regression is u516 (about 2x more conflicts): restart timing
variance on an unsolvability proof; see the interval sensitivity check
below.

u212 still times out; at-timeout counters are throughput-bound and the
runs were collected under concurrent load, so the conflict deltas there
are not comparable. Its diagnostics show 15.4 s of decide() time and 108 s
of propagation at cancel; whatever ails 212 is not (only) a restart
problem.

Interval sensitivity (`RESTART_BASE_INTERVAL = 512` vs 128, conflicts):
u516 71,945 vs 77,860 (bad either way; the baseline without restarts is
37,233, so 516 dislikes restarts at any pacing), u632 14,508 vs 9,735,
u138 9,663 vs 3,400, c186 10,191 vs 8,998. 128 dominates on every winner;
516 is the cost of admission and is bounded (about 2x on one problem
against 10x to infinite wins elsewhere).

## Feature 2: learnt-clause database reduction (freezing)

### Design

True deletion is off the table: the clause arena is append-only and
`ClauseId`s index it positionally, `learnt_why` must stay complete for
`analyze_unsolvable`'s error reporting, and other id spaces index clause
kinds by index. Freezing captures the propagation win without arena
surgery:

- `analyze` computes the LBD (number of distinct decision levels among the
  learnt literals) at learn time and stores it per learnt clause.
- After `REDUCE_FIRST_INTERVAL` (2000) learnt conflicts, and on a growing
  interval thereafter (+500 per reduction performed; the pacing is global
  to the solver state, since a universal enumeration shares one learnt DB
  across cells), the worst half of the deletable learnt clauses is frozen,
  ordered by LBD descending with older clauses frozen first on ties.
  Deletable means: more than two literals, LBD > 2 (glue clauses are kept
  forever), and not currently the reason (`derived_from`) of any trail
  assignment.
- Freezing removes the clause id from `learnt_clause_ids` (which
  `decide_learned` scans on every propagation round, an O(learnt) cost
  that reduction directly attacks) and marks the clause in a frozen
  bitmap. The watch lists are NOT walked eagerly: the watch lists are
  singly linked, so the propagation loop unlinks frozen clauses lazily
  when a traversal encounters them (`WatchMapCursor::remove`). Frozen
  clauses in lists that are never traversed again cost nothing.
- The arena entry, `Clause::Learnt` kind and `learnt_why` entry survive,
  so conflict reporting and `visit_literals` on historical reasons keep
  working; the clause merely stops participating in propagation.

Soundness: a learnt clause is implied by the original formula, so removing
it from propagation can never produce wrong results, only longer searches.
Backtracking never falsifies watch invariants of surviving clauses.

The universal enumeration deliberately shares learnt clauses across cells;
over-deletion would destroy that sharing. A diagnostics counter records
re-learned clauses (a newly learnt clause whose literal set hashes
identically to a previously frozen one) to detect over-deletion.

### Results

Variant A (freeze worst half by learn-time LBD alone, first reduction at
2000 conflicts, +500 per reduction), measured on top of restarts,
conflicts vs the restarts-only run:

| problem | conflicts | frozen (re-learned) | outcome |
|---|---|---|---|
| u881 | 146,262 (was 38,017) | 119,135 (104,958 = 88%) | unsolvable, 222 s |
| u516 | 69,630 (was 77,860) | 55,689 (12,981 = 23%) | unsolvable |
| u632 | 9,356 (was 9,735) | 3,196 (690 = 22%) | unsolvable |
| u138 | 3,406 (was 3,400) | 569 (0) | ok, same solution |
| u212 | 60,310 at cancel | 35,894 (3,647) | still timeout |
| c186 | 9,611 (was 8,998) | 4,220 (547) | ok, same records |
| c232 | 13,199 (was 11,588) | 547 (751) | unsolvable |
| c539 | 244,952 (was 54,465) | 213,795 (33,492) | TIMEOUT AGAIN (was ok in 64 s) |

Variant A is harmful: learn-time LBD alone is a bad usefulness predictor
on this corpus. The unsolvability proof of 881 re-learns 88% of what was
frozen, and c539, the headline restart win, regresses back to a timeout.

Variant B adds the two standard protections the corpus evidently needs:
a clause that participated in any conflict analysis since the previous
reduction is protected (usage flag, cleared each round), and clauses
younger than one full interval are never frozen (grace watermark).

Variant B results (vs restarts-only):

| problem | conflicts | propagated | frozen (re-learned) | outcome |
|---|---|---|---|---|
| u881 | 38,449 (was 38,017) | 193.7 M (was 138.3 M) | 20,033 (5,389 = 27%) | unsolvable, 79.9 s |
| c539 | 121,282 (was 54,465) | 523.8 M (was 204.8 M) | 89,353 (9,289 = 10%) | ok 281 records, 122.6 s |

Even with usage protection and the grace interval, reduction loses:
c539 needs 2.3x the conflicts and u881 propagates 40% more decisions at
conflict parity (its pruning power dropped). The mechanism is consistent
across all runs: these proofs re-use broad clause sets, and resolvo's
propagation is dominated by `Requires` clauses (learnt clauses are 2-3%
of clause visits), so the learnt DB is cheap to keep and expensive to
lose. Verdict: drop (the prototype commit is reverted on the branch; both
stay in history for evaluation).

The investigation did expose one real learnt-DB cost: `decide_learned`
re-scans the learnt clause id list once per `propagate()` call, only to
act on the single-literal clauses (assertions, which have no watches).
On u212 that is about 700k propagate calls times up to 30k learnt ids.
The fix is to register only unit learnt clauses for that scan
(`unit_learnt_clause_ids`), which is behaviorally identical and removes
an O(propagate calls x learnt clauses) term; it is committed as a
separate perf change. Results: see the recommendation table.

## Feature 3: phase saving

### Investigation

Classic phase saving stores the last assigned polarity of every variable
and reuses it when the decision heuristic picks that variable, so a
restart (or a far backjump) does not throw away the satisfying fragment of
the assignment the solver had already built.

In resolvo the decision step has no polarity freedom to exploit:

- `decide()` always assigns `candidate = true`, and the candidate is the
  first undecided variable of the highest-priority unsatisfied
  `Requires`/`EnvConstrains` clause, in sorted candidate order. The
  "phase" of a decision variable is fixed by construction.
- The variable choice itself is what phase saving would have to influence,
  and that is already fully determined by candidate order (version
  preference, a solution-quality invariant that must not be overridden),
  package activity (which restarts deliberately want to re-apply), and
  candidate counts.
- Negative assignments only ever enter the trail through propagation and
  learnt assertions, never through decisions, so there is no "saved
  negative phase" that a future decision could reuse.

Re-picking the previously installed (possibly lower) version of a package
after a restart, the closest analogue of saved phases, would trade
solution quality (highest-version preference) for search speed, which is
the wrong trade for a dependency resolver, and the sorted-candidate
restart descent already reuses everything propagation forces.

### Results

No prototype was built: `set_propagate_learn` hardcodes `value = true`
for every decision, so phase saving is a structural no-op in resolvo;
there is no polarity to save or restore. Verdict: drop.

## Recommendations

| feature | verdict |
|---|---|
| Luby restarts (base 128) | ADOPT: order-of-magnitude wins on three of the hard problems, one bounded ~2x regression (u516) |
| learnt-DB reduction | DROP: net loss in both variants; learnt DB is cheap to keep here |
| unit-only decide_learned scan | ADOPT: behaviorally identical, removes an O(propagations x learnt) term |
| phase saving | DROP: structurally a no-op (decisions have no polarity freedom) |

Combined branch head (restarts + unit-only scan; deletion reverted),
measured on a quiet machine, against the baseline table at the top:

| problem | baseline | final head | conflicts |
|---|---|---|---|
| u881 | unsolvable, 63.9 s | unsolvable, 32.2 s | 45,144 -> 38,017 |
| u516 | unsolvable, 76.7 s | unsolvable, 101.0 s | 37,233 -> 77,860 |
| u632 | unsolvable, 55.1 s | unsolvable, 38.8 s | 15,296 -> 9,735 |
| u138 | ok, 63.7 s | ok (same solution), 10.5 s | 30,509 -> 3,400 |
| u212 | timeout 240 s | timeout 240 s | n/a |
| c186 | ok, 182.1 s | ok (same records), 11.6 s | 118,749 -> 8,998 |
| c232 | unsolvable, 111.3 s | unsolvable, 20.3 s | 85,268 -> 11,588 |
| c539 | timeout 240 s | ok 281 records, 35.2 s | n/a -> 54,465 |

Guards: universal 370/95/46 identical solutions (370 bit-identical
counters); concrete first-50 identical outcomes, one justified record-set
change (index 49).

Open thread: u212 still times out at 240 s with every combination tried.
Its diagnostics at cancel show heavy decide() time (about 15 s) on top of
propagation; it churns through hundreds of thousands of propagate calls
in a universal enumeration with many cells. Cracking it likely needs
something other than classic CDCL machinery (e.g. a look at why its
enumeration revisits so much work per cell).
