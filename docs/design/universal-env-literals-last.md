# Env-literals-last decision ordering for the universal solver

Status: prototyped and measured, 2026-06-11. Follow-up to trail-prefix
preservation (`universal-trail-reuse.md` and the "Trail-prefix
preservation: implementation and verdict" section of
`universal-solve-benchmark.md`), which identified this ordering as the
prerequisite for the per-cell collapse that trail reuse alone could not
deliver.

## Problem statement

Trail-prefix preservation keeps, between universal cells, the trail
prefix that does not falsify the new blocking clause (retract target =
deepest blocking-literal assignment level minus 1). On the linux-64
conda-forge corpus this bounded the win well short of the design goal:

- Problem 370 (360 cells): baseline 19-24 s, with trail reuse 11-13 s.
  The kept prefix covers 83% of decision levels on average (retract
  target ~200 of depth ~242), but the re-derived suffix still costs
  ~40% of a fresh solve in propagations per cell.
- The bound is structural: environment literals are assigned mid-trail,
  at the decision level where the package whose clause forces them is
  installed. Every cell whose deepest load-bearing literal sits at such
  a level retracts to just below it and re-derives everything above.
- Hard-corner problems (46, 450, 491, 168) regress under reuse and are
  contained by the work budgets (`PREFIX_BUDGET_FACTOR` machinery in
  `src/solver/mod.rs`).

Instrumented evidence from problem 370 at the current head (per-cell
trace of trail depth, retract target, and the assignment level and
reason of every cell literal):

- Retract distribution over 360 cells at the baseline: 144 cells
  retract within 2 levels of the trail depth, 72 within 16, and 144
  re-derive MORE than 64 of ~242 levels. Median transition cost: 7,242
  propagated decisions (p90 13,509).
- The mid-trail cell literals are propagated by a handful of variant
  parents on install: `_x86_64-microarch-level=*` builds (the microarch
  literal, level ~122), `sysroot_linux-64=*` builds (a glibc literal,
  level ~151) and `libsystemd0` (level ~236). The `EnvConstrains`
  (cuda) literals are already decided at the very top by `decide()`.
- Crucially, those variant parents are themselves installed by UNIT
  PROPAGATION (they are the single remaining candidate of some
  ordinary package's requirement), so no decision-ordering heuristic
  alone can reposition them.
- Reverse-dependency shape of the snapshot: `__glibc` is required
  directly by 6,942 packages (a baseline literal entailed by every
  model region), while the discriminating parents are rare
  (`_x86_64-microarch-level`: 36 dependers, `libsystemd0`: 8,
  `sysroot_linux-64`: 688). Any static "defer everything env-touching"
  rule therefore degenerates: it would defer most of the trail.

Target: make environment literals land at the top of the trail so the
retract target sits near depth-1 and the per-cell re-derivation
collapses to the actual delta between cells. This is the analog of
uv's maximal shared prefix: uv's eager marker forking gets it by
construction because the fork happens at the first marker-divergent
decision; in a single-instance backtracking enumeration the same
effect must come from where env-sensitive installs and env literal
assignments are placed on the trail.

## Design

Four cooperating mechanisms, all gated on
`SolverState::env_ordering_active` (set when the first environment
literal variable is interned, so a plain concrete solve is bit-for-bit
unaffected):

1. **Deferral classes in `decide()`** (`src/solver/mod.rs`). One scan
   fills three slots with the existing heuristics (explicit first,
   then activity, then fewest candidates) applied within each slot:
   ordinary decisions; candidates whose install would assign a still
   unassigned env literal (see 2); and candidates that are environment
   literals themselves (requirements on env packages with several
   version sets, and every `EnvConstrains` absent/matches pick). The
   classes resolve in that order; the blocking-clause completion pass
   stays last.

2. **A dynamic, not static, deferral test**
   (`SolverState::env_install_pending`). The encoder records, per
   parent solvable, WHICH env literal variables its clauses assign
   (`env_sensitive_parents`: the env candidates of its `Requires`
   clauses, the absent/matches literals of its `EnvConstrains`
   clauses). A candidate is deferred only while one of its literals is
   UNASSIGNED. This keeps the ordering selective on conda-like
   corpora: the first install from the 6,942-package `__glibc` cone
   assigns the baseline literal once, after which the entire cone is
   ordinary again; only parents of literals that still discriminate
   between regions (microarch levels, cuda ranges, newer-glibc
   variants) stay deferred to the top.

3. **Unit-propagation deferral for pending variant parents**
   (`Solver::propagate_impl`). A `Requires` clause that becomes unit
   on a positive, undecided solvable whose install would assign an
   unassigned env literal is not propagated; `decide()` installs that
   solvable later (class 2) because `Requires` clauses are indexed by
   their installed parent. This is what handles parents that never
   appear as decisions (the microarch-level chain). Soundness: the
   clause stays watched, so a later falsification of the skipped
   candidate fires the watch and reports the conflict through the
   already-false other watched literal; deferring a forced assignment
   merely turns it into a decision, which conflict analysis treats
   like any other decision (a decision is always the last cause
   standing at its level, so its motivating clause is never resolved
   through). The skip tests `assigned_value(..).is_none()` so a unit
   target that is already assigned false still conflicts immediately.

4. **Trail reshape** (`enumerate_universal`,
   `TRAIL_RESHAPE_ORDINARY_LEVELS`). When a transition would pop more
   than 8 ordinary decision levels (levels started by variables that
   are neither env literals nor env-sensitive parents), the kept
   prefix has env-sensitive state buried under env-independent
   packages: the shape the trail was given before the cross-cell
   knowledge existed. A partial retraction would re-derive that suffix
   on every later transition and the shape would never heal (the new
   blocking clause's cascade re-assigns the env literals at the bottom
   of the re-derived range, recreating the situation). Retracting
   fully instead rebuilds the trail with the current knowledge once.
   With a well-shaped trail only env literals and env-sensitive
   parents sit above the target, so this does not trigger in steady
   state.

One supporting change: when only deferred decisions are available but
installed solvables still have their clauses pending encoding,
`decide()` returns no decision so that `run_sat`'s encode round runs
first (`Solver::has_pending_clause_encodes`). Without this, the lazy
batch encoding forces env literals onto the trail at every batch
boundary, defeating the ordering. With pending encodes the outer loop
always re-enters the decision loop, so this cannot terminate the solve
early.

### Cross-cell knowledge and warm-up

A candidate's dependencies are encoded only after its first install,
so `env_sensitive_parents` knows nothing about never-installed builds.
The first cell is solved mostly blind and keeps a mid-trail shape; the
first transition that conflicts against a pre-knowledge position
backjumps deep once (or triggers the reshape), and the rebuild places
everything correctly. The many-cells regime that matters amortizes
this warm-up; on problem 370, 321 of 360 transitions run from a trail
kept within 2 levels of full depth.

### Concrete mode is a no-op

`env_ordering_active` is only set when an environment literal is
interned, which never happens in `solve()` without environment
packages. All four mechanisms collapse to the previous behavior (the
deferral slots stay empty and the propagation skip cannot match).
Verified: outcomes and records of the first 50 concrete-mode problems
are identical to `full-linux-64-concrete.csv`.

## Soundness

- **Decision order is a free heuristic in CDCL**: classes 1-3 only
  reorder decisions; completeness and termination of the enumeration
  (each blocking clause strictly shrinks the satisfiable env region)
  are untouched.
- **Skipped unit propagations** are the one non-classical piece; the
  argument is in (3) above. The skip only ever delays an assignment
  that `decide()` is guaranteed to make (or that a conflict
  preempts); it never suppresses a conflict because it requires the
  target to be undecided.
- **Trail reuse**: the retract-target computation and the
  blocking-clause watch initialization are unchanged; the reshape only
  ever retracts MORE than required, and a full retraction is exactly
  the no-reuse path. The budgets stay armed for prefix-started runs
  and the fallback enumeration is unchanged; with the ordering in
  place they are dormant (0 aborts on the whole spot set).
- **Cell extraction and pinning**: extraction reads the full
  assignment at solution time. Deferred env decisions change which
  literals are assigned versus undecided-counts-as-false, so cells and
  cell order can legitimately differ from the previous build; every
  partition is still disjoint, covering and verifier-clean. The
  observed effect is beneficial: fewer load-bearing assignments mean
  more general cells (problem 5 of the corpus drops from 196 to 48
  cells, problem 3 from 17 to 4).
- **Seeded phase**: untouched. Assumptions are pushed before `run_sat`
  and pin the env literals at levels `1..=n`; the ordering only steers
  the free search inside the region.
- **Determinism**: the classification reads the variable origin, the
  deterministic `env_sensitive_parents` map and the current
  assignment, all functions of the deterministic encoding and search.
  Two runs of problem 370 produce byte-identical cells, records and
  per-cell literal traces.

## Measured results

Shared linux-64 corpus (`bench-universal/snapshot-linux-64.json`,
model `model-linux-64.json`, `--verify`), release build with
`resolvo/diagnostics`, same machine and build for both arms (the
baseline arm is the same binary with the ordering disabled via a
temporary kill switch, which reproduces the head behavior; its 10.9 s
on problem 370 matches the 11.5 s recorded in
`full-linux-64-universal-trailreuse2.csv`).

### Problem 370 (the mechanical 360-cell outlier)

| metric | before | after |
| --- | --- | --- |
| wall time | 10.9-11.5 s (~32 ms/cell) | 3.2-3.7 s (~9 ms/cell) |
| propagated decisions per transition, median | 7,242 | 234 |
| p90 | 13,509 | 912 |
| total over 359 transitions | 7.17 M | 2.30 M |
| transitions kept within 2 levels of depth | 144 / 360 | 321 / 360 |
| transitions re-deriving > 64 levels | 144 | 1 |
| prefix budget aborts | 0 | 0 |

The propagation work per transition collapses to the cell delta; the
remaining wall time is dominated by `decide()`'s full clause scan per
decision, which is a separate work item. The residual 2.3 M total is
concentrated in 6 warm-up/search transitions (p99 156 k).

### Spot set

| idx | cells before -> after | before | after | note |
| --- | --- | --- | --- | --- |
| 370 | 360 -> 360 | 11.5 s | 3.2 s | mechanical, collapses as designed |
| 95 | 193 -> 192 | 23.8 s | 8.8 s | mechanical, collapses |
| 450 | 360 -> 360 | 28.3 s | 11.7 s | former hard corner, now mechanical |
| 46 | 11 -> 11 | 38.2 s | 3.6 s | former hard corner, 10x |
| 168 | 27 -> 32 | 28.9 s | 17.5 s | hard corner, still real search |
| 491 | timeout -> 361 | 60 s timeout | 29.1 s | now completes, verifier-clean |

Budget aborts are 0 on every spot problem: the protection machinery is
dormant, as predicted when transitions become tiny edits.

### First-50 sweep (cheap problems, no-regression)

Versus `full-linux-64-universal-trailreuse2.csv`: every outcome equal
or better (idx 34: timeout becomes ok with 54 cells), all solutions
verifier-clean, total duration 167.9 s to 65.2 s. Seven problems
produce different, still-valid partitions; the direction is less
fragmentation (idx 5: 196 to 48 cells, idx 3: 17 to 4). Concrete mode:
outcomes and records identical to `full-linux-64-concrete.csv` for all
50.

## Risks

- **Cell churn on upgrade**: enumeration order and cell conditions
  differ from the previous build for multi-cell problems (usually
  fewer, more general cells). Seeding (the lockfile replay path) pins
  regions regardless of free-phase order, and the verifier re-checks
  every partition.
- **Pruning loss from the unit-propagation skip**: deferring a variant
  parent delays the conflicts its install would trigger. An earlier
  prototype that skipped ALL env-literal unit propagations (instead
  of only pending variant-parent installs) lost so much pruning that
  problem 370 ran 2x WORSE (15.7 M propagations); the shipped rule
  defers only installs that assign a still-unassigned literal, and the
  spot set shows no residual regression. Other datasets could expose
  corners; the budget fallback bounds the damage exactly as before.
- **Reshape threshold**: a deferred variant parent with a dependency
  subtree larger than 8 levels would trigger full rebuilds on every
  flip of its literal. Not observed on this corpus (variant parents
  are leaf-like); if it appears, the threshold can scale with the
  measured env-tail size.
- **First-cell shape persists for a transition or two** before the
  trail re-forms with full knowledge; tiny enumerations gain little
  but also pay at most one extra rebuild.

## Test plan

- `test_universal_env_literals_decided_last` (diagnostics, in
  `tests/solver/main.rs`): a two-axis variant grid over a chain of
  real decisions; asserts via the new `universal_cell_retracts()`
  counter that the first cell's retract target is within 2 levels of
  the trail depth (the env-literal class works without knowledge) and
  that at most one transition retracts deep (the single warm-up
  rebuild). Fails on the old ordering with targets at ~5 of ~13.
- Existing suites: all solver, conflict, snapshot and seeded tests
  pass unchanged under both feature sets; no inline snapshot needed
  updating (the small test universes produce identical partitions).
- `solver::universal_prop::test_universal_solve_property` (1000
  seeds): green, including the reseed fixed-point checks.
- Corpus: spot set and first-50 sweeps above; concrete first-50
  byte-identical; double-run determinism on problem 370.
