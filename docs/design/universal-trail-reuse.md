# Trail-prefix preservation across universal cells

Status: implemented and benchmarked, 2026-06-11. The corpus verdict and
the protection machinery that the implementation turned out to need
(work-budget abandonment with a from-scratch fallback) are documented
in `universal-solve-benchmark.md`, section "Trail-prefix preservation:
implementation and verdict"; the headline finding is that the win is
bounded by env literals sitting mid-trail, making the env-literals-last
decision ordering (listed under follow-ups below) the prerequisite for
the collapse this note aimed at.

Original design follows. Follow-up to the benchmark report
(`universal-solve-benchmark.md`), which motivates this with a profile:
high-cell enumerations spend 94% of their time re-deciding and
re-propagating a nearly conflict-free, nearly identical assignment per
cell (360 cells, 3 conflicts/cell). uv's forking resolver validates the
architecture: each fork continues from the full solver state at the
divergence point. The single-instance variant below gets the same prefix
reuse without state cloning and keeps learnt clauses flowing between
sibling regions.

## Semantics

Today, after recording a cell, `solve_universal` retracts the entire
trail (`undo_until(0)`), adds the blocking clause, and re-solves from
scratch. The proposal: retract only far enough that the blocking clause
is no longer falsified, and continue from the surviving prefix.

Soundness rests on one fact: adding a clause to the formula only
invalidates assignments that falsify it. Any trail prefix under which
the new blocking clause still has an unassigned or true literal is a
valid partial assignment of the grown formula. The blocking clause is
the disjunction of the negations of the cell's literals; under the
just-recorded solution all of them are false, and the shallowest level
that can make one of them non-false is just below the deepest assignment
level among the cell's literals. Therefore:

- retract target = (max assignment level over the blocking clause's
  variables) - 1, computed BEFORE retracting, then `undo_until(target)`.
- add the blocking clause afterwards; under the surviving prefix it has
  at least one non-false literal, so standard watch initialization on
  two non-false literals applies. If it is unit under the prefix, it
  must propagate immediately (same handling as clause learning).

## The prefix is not an assumption

This is the critical distinction against the seeded path (M4):

- Seeded cells push assumption decisions; a conflict that would backjump
  below the assumption prefix means "the seed is unsolvable as seeded"
  and surfaces as `Ok(false)`.
- The kept trail prefix here is ordinary solver state. A later conflict
  MUST be allowed to backjump through it (the prefix is heuristic; the
  region it pins may genuinely require different earlier choices, e.g.
  a blocking clause can force a different build of a package decided in
  the prefix). No new `Ok(false)` semantics, no clamping: conflict
  analysis treats prefix decisions like any other decisions.

Consequences for `run_sat`:

- `starting_level` is currently derived from the existing stack and
  doubles as the "prior decisions" boundary for the `Ok(false)`
  contract and for `analyze`'s backjump clamping. With a kept prefix in
  the free phase, the unsolvable-as-seeded machinery must stay bound to
  `assumption_levels` (which is 0 in the free phase), NOT to the stack
  depth. Concretely: the free-phase re-entry must present the kept
  prefix as restartable state, so a root-level conflict still means
  globally UNSAT (coverage check), never "drop a seed".
- The root install decision sits at level 1 and is part of every kept
  prefix, so the existing invariant (nothing reinstalls the root
  mid-run) is preserved by construction as long as the retract target
  is >= 1. A retract target of 0 only happens when a cell literal was
  assigned at level <= 1, which degenerates to today's behavior.

## Interactions checked in review

- **Cell extraction** reads the full assignment at solution time and is
  unaffected (extraction happens before retraction).
- **Disjointness repair** appends literals from the full assignment;
  the retract target must be computed from the FINAL (repaired) cell,
  which is also exactly the blocking clause's literal set.
- **decide() blocking pass** (lowest-priority pass over blocking
  clauses) is unaffected: it already operates on whatever the current
  assignment is.
- **Seeded phase**: unchanged, full retraction between seeds. Trail
  reuse applies to the free phase only (first implementation). A seed
  following a free cell cannot happen (seeds run first), so no
  interaction.
- **Empty cell** (no load-bearing literals): terminates the loop today;
  unchanged.
- **Witness check / termination**: unaffected; it reads clauses, not
  the trail. The enumeration still terminates because each blocking
  clause strictly shrinks the satisfiable region of the env space; the
  kept prefix changes where search resumes, not what is blocked.
- **Determinism**: the kept prefix is exactly the prefix the restarted
  search would re-derive when no conflict intervenes (same decision
  order, same propagation), EXCEPT where the new blocking clause or
  learnt clauses would have steered an earlier decision differently.
  Cells can therefore come out in a different order or with different
  (still valid, still disjoint) conditions than the restart-from-zero
  enumeration. The output contract (disjoint cover with per-cell-valid
  solutions, deterministic across runs of the same build) is preserved;
  byte-identical equality with the old enumeration is NOT promised.
  The property test validates solutions independently of enumeration
  order, and the verifier re-checks every partition.

## Test plan (TDD order)

1. Unit test on a constructed two-axis universe: enumeration produces a
   disjoint, covering, verifier-clean partition with trail reuse, and
   the per-cell decision counter (diagnostics) shows the second cell
   deciding strictly fewer variables than the first. Write against the
   diagnostics counters; expected failure first (counts equal).
2. Regression: blocking clause unit-under-prefix (cell whose condition
   is a single literal assigned at level 2): after retraction the
   clause must propagate immediately and flip the literal.
3. Regression: conflict that must backjump through the kept prefix (a
   blocking clause that forces a different build of a prefix-decided
   package); asserts the final partition is still complete and the
   solve does not report unsolvable.
4. Seeded path untouched: existing M4 tests must stay green unchanged.
5. Property test (`universal_prop.rs`) for a large iteration count: the
   independent checks (projection validity per sampled environment,
   verify, determinism across two runs) all hold with trail reuse.
6. Bench acceptance: full linux-64 universal corpus; expectation is the
   mechanical outlier class collapses (problem 370 run_sat time from
   ~20 s toward the concrete-solve scale) with cells/solutions still
   verifier-clean, and no regression for 1-cell solves.

## Out of scope (follow-ups)

- Env-literals-last decision ordering to maximize the kept prefix (the
  uv-eager-fork analog). Orthogonal and composable.
- Trail reuse for the seeded phase.
- Parallel cell exploration (uv-style independent fork states).
