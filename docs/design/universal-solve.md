# Universal multi-environment resolution

Status: design agreed, implementation planned (2026-06-09).
Authors: Bas Zalmstra, with research and drafting by Claude.

This document is the working plan for implementing "universal solving" in
resolvo: a single solve whose output is valid for a whole family of
environments (machines with or without CUDA, any glibc above a floor, any
microarchitecture, ...) instead of one concrete machine. It is written so an
implementing agent can execute it milestone by milestone. Read the whole
document before starting any task.

## 1. Motivation

rattler_solve injects concrete "virtual packages" (`__glibc=2.34`,
`__cuda=12.4`, ...) describing one machine's capabilities. The resulting
lockfile is only guaranteed to work on machines with those exact
capabilities. pixi works around this by solving once per platform, but
capabilities within a platform (CUDA present or not, glibc version,
microarchitecture) still force a single choice at lock time.

uv solved the analogous problem for Python with "universal resolution": the
lockfile encodes a dependency graph whose edges carry environment markers,
and installation is a plain graph walk that evaluates markers. No solving
happens at install time.

We want the same for resolvo, as a generic mechanism, with the conda
ecosystem (rattler_solve) as the first consumer.

## 2. Terminology

These are the established terms from the software product line literature;
use them in code docs where helpful.

- **150% model**: a dependency graph containing the union of all variants,
  where elements carry *presence conditions*.
- **Presence condition**: a boolean formula over environment properties that
  says when a node or edge is active.
- **Product derivation**: deriving the install set for one concrete
  environment by evaluating presence conditions (the "graph walk").
- **Safe composition / family-based consistency**: the guarantee that for
  *every* environment in the modeled space, the derived install set is
  well formed: at most one version per package name (disjointness) and every
  active requirement satisfied (coverage). This is the property informally
  described as "the result forms a tree and can be walked without solving".

uv's equivalents: fork markers are presence conditions, `uv.lock` is the
150% model, `resolution-markers` is the persisted partition.

## 3. Agreed design decisions

Decided in design review; do not relitigate during implementation.

1. **Goal**: capabilities-within-platform first. pixi keeps its per-platform
   loop; cross-platform universality must remain expressible later but is
   out of scope now.
2. **Architecture**: in-solver projected model enumeration. One CDCL
   instance. Environment literals are ordinary SAT variables. After each
   solution: generalize the cell, add a blocking clause over environment
   literals only, re-solve. Learnt clauses and the solver cache persist
   across cells because the formula only grows.
3. **Environment model**: explicit and total. The caller supplies a formula
   bounding the environment space. Every cell inside the model must be
   solvable, otherwise the whole universal solve fails with that cell's
   conflict.
4. **Literal model**: every version set used against an environment package
   becomes one boolean variable, plus one "absent" variable per environment
   package that can be absent. The provider implements a pairwise relation
   oracle. Only "disjoint" answers must be sound; "unknown" is always safe.
5. **Declaration**: the provider declares environment packages through the
   return value of `get_candidates`. The environment model and the seed
   partition are per-solve inputs on the problem.
6. **Output**: both the per-cell solutions and a merged
   presence-condition view, plus a verifier and a projection utility.
7. **Stability**: uv-style persisted partition. The caller can feed the cell
   partition from a previous solve back in as a seed; seeded cells are
   solved first under assumptions. Natural determinism of the shared solver
   instance provides cross-cell version coherence on top.
8. **Cell generalization**: clause-support based (section 5.6).
9. **Split policy**: environment literals are never proactively decided;
   they default to false (undecided counts as false). The first cell is the
   "baseline machine". For environment constraints that force a split, the
   absent branch is explored first.
10. **v1 conda scope** (in rattler, not this repo): `__cuda` (presence and
    version), `__glibc` (version), `__osx` (version), `__win` (version,
    presence stays concrete), `__archspec` (with the microarchitecture
    implication DAG hardcoded in rattler's oracle).

## 4. Design overview

```
                 +------------------------------------------------+
 Problem         |  solve_universal loop                          |
  requirements   |                                                |
  env model -----+->  encode model clauses (over env literals)    |
  seed partition |    for each seed cell: assume literals, solve  |
                 |    loop:                                       |
                 |      run_sat (existing CDCL, shared state)     |
                 |      -> solution: extract + generalize cell,   |
                 |         repair disjointness, record,           |
                 |         add blocking clause, continue          |
                 |      -> unsat: env-witness check               |
                 |         - no witness: coverage complete, done  |
                 |         - witness cell: fail with              |
                 |           UniversalConflict { cell, conflict } |
                 +------------------------------------------------+
                                  |
                                  v
                  UniversalSolution { cells } --> merge / verify / project
```

An **environment package** is a package name whose "value" is unknown at
solve time. A version set against it (e.g. `__glibc >=2.17`) does not expand
to candidate solvables; it becomes an **environment literal**: a single SAT
variable meaning "the environment's value for this package exists and
matches this version set". Each absentable environment package additionally
gets an **absent literal** meaning "this package does not exist in the
environment".

A **cell** is a conjunction of signed environment literals together with the
solution that is valid throughout it. Cells are pairwise disjoint and
together cover the environment model. The merged output ORs cells per
solvable.

## 5. Detailed semantics

### 5.1 Environment literals and soundness rules

For an environment package `p` with unknown value `v(p)` (or absent):

- Literal `L_S` (for version set `S` on `p`) is true in an environment iff
  `p` is present and `v(p)` matches `S`.
- Literal `Ab_p` is true iff `p` is absent.

Within the solver these are free variables. Their mutual consistency is
enforced by clauses generated from the provider oracle (5.2). Crucially,
`not L_S` does NOT mean `v(p)` fails to match `S` from the solver's point of
view; a cell that contains `not L_S` describes the set of environments where
`p` is absent or does not match `S`. That is exactly evaluable at install
time, so negative literals in cell conditions are fine.

Undecided literals at solution time count as false (this matches resolvo's
existing solution semantics where undecided solvables are not installed).

### 5.2 The relation oracle

New method on `DependencyProvider` (sync, pure):

```rust
/// Relation between two version sets that refer to the same
/// environment package. Only called for version sets whose
/// `version_set_name` is an environment package.
fn environment_version_set_relation(
    &self,
    a: VersionSetId,
    b: VersionSetId,
) -> VersionSetRelation;

pub enum VersionSetRelation {
    /// No value matches both `a` and `b`.
    Disjoint,
    /// Every value matching `a` also matches `b`.
    Subset,
    /// Every value matching `b` also matches `a`.
    Superset,
    /// `a` and `b` match exactly the same values.
    Equal,
    /// Overlapping, or the relation cannot be determined.
    Unknown,
}
```

Soundness contract (document on the trait): answers other than `Unknown`
must be correct; when in doubt return `Unknown`. A wrong `Disjoint`/`Subset`
produces broken lockfiles; `Unknown` merely risks describing environment
regions no real machine has ("vacuous cells").

When a new literal `L_a` for package `p` is interned, emit consistency
clauses against every previously interned literal `L_b` of `p`:

- `Disjoint`: `(not L_a or not L_b)`
- `Subset`: `(not L_a or L_b)`
- `Superset`: `(not L_b or L_a)`
- `Equal`: both implications
- `Unknown`: nothing

And against the absent literal: `(not Ab_p or not L_a)` always (a present
match contradicts absence). If the package cannot be absent, `Ab_p` does not
exist. This is O(k^2) clauses per environment package where k is the number
of distinct version sets used against it; k is small in practice.

These consistency clauses are tautologies over real environments. They are
never part of a cell's support (5.6) and never blocked.

### 5.3 Declaration via get_candidates

`get_candidates` changes to return an enum:

```rust
pub enum PackageCandidates<S = SolvableId> {
    /// A normal package with concrete candidate solvables.
    Candidates(Candidates<S>),
    /// An environment package whose value is unknown at solve time.
    Environment(EnvironmentPackage),
}

pub struct EnvironmentPackage {
    /// Whether the environment may lack this package entirely.
    /// Controls creation of the absent literal.
    pub can_be_absent: bool,
}

impl<S> From<Candidates<S>> for PackageCandidates<S> { ... }
```

This is a breaking change to the trait (acceptable; resolvo is pre-1.0).
`SolverCache` stores the enum. All existing internal call sites that assume
candidate lists must branch. The C++ bindings in `cpp/` should keep building
by wrapping their existing struct in `PackageCandidates::Candidates`;
exposing universal solving over the C++ API is out of scope.

Behavior of a plain (non-universal) `solve` with environment packages: in
M0 this panics with a clear message (transitional). From M1 onward the
encoding handles environment literals everywhere, and a plain `solve`
legitimately yields the baseline solution (all environment literals false,
per the split policy): the solution for the least capable machine. This is
also what makes M1 testable before `solve_universal` exists. Concrete
solves that want a specific machine keep injecting concrete virtual
packages as today.

### 5.4 Encoding changes

All in `src/solver/encoding.rs` plus helpers. The name-to-kind decision
happens when candidates for a package become available
(`on_candidates_available`); record environment packages in `SolverState`
(e.g. `env_packages: IdMap<NameId, EnvironmentPackage>`), skip
locked/excluded/forbid handling for them.

New `VariableOrigin` variants in `src/solver/variable_map.rs`:

```rust
EnvMatches(VersionSetId),  // literal L_S
EnvAbsent(N),              // literal Ab_p
```

with interning maps (`VersionSetId -> VariableId`, `NameId -> VariableId`)
and the consistency-clause emission from 5.2 hooked into first interning.

Three places where version sets can reference an environment package:

1. **Requirement** (`A` depends on `__glibc >=2.17`): instead of querying
   sorted candidates, the requirement's candidate list for that version set
   is the single literal variable `L_S`. The resulting clause
   `(not A or L_S)` is binary and unit-propagates `L_S = true` as soon as
   `A` is installed; no decision needed. `Requirement::Union` mixing
   environment and concrete version sets is rejected in v1 (clear panic).
   Requirements on environment packages from the root problem are allowed
   (rattler currently injects virtual packages as root requirements; in
   universal mode it should not, but the mechanism is harmless).

2. **Condition** (`A` requires `B` if `__cuda`): in
   `src/solver/conditions.rs` / encoding, the DNF machinery represents
   "condition does not hold" per condition version set. For an environment
   package the complement of `L_S` is simply the negative literal
   `not L_S`; add a `DisjunctionComplement::EnvLiteral(VariableId)` variant
   (no complement-candidate query, no at-least-one tracker). The requires
   clause becomes `(not A or not L_S or B1 ... Bn)`. Note `decide()`
   (src/solver/mod.rs) already skips conditional requires clauses whose
   condition literals are not all false; with `L_S` undecided the clause is
   inactive and the solution legitimately omits `B`. The generalization step
   (5.6) is what records `not L_S` into the cell, and the blocking clause is
   what later forces exploration of the `L_S = true` region. This is exactly
   the agreed "default false, baseline first" split policy and requires no
   change to `decide()` for conditions.

3. **Constraint** (`A` constrains `__cuda >=11`): semantics are "if A is
   installed, the environment must lack cuda or match `>=11`". Encode as a
   clause `(not A or Ab_p or L_S)` (just `(not A or L_S)` when the package
   cannot be absent). Unlike all other non-requires clauses this contains
   positive literals on two variables, so `decide()` must know how to make
   progress on it: treat it like a requires clause whose ordered candidates
   are `[Ab_p, L_S]` (absent branch first, per the split policy). Mechanism:
   give these clauses their own `Clause` variant (suggested:
   `Clause::EnvConstrains(VariableId, N, VersionSetId)`), register them in
   the same structure `decide()` iterates (`requires_clauses`), with the
   candidate pair stored alongside (the existing
   `requirement_to_sorted_candidates` map is keyed by `Requirement`, which
   cannot represent this; a small side table or widening of the map key is
   needed; pick whatever is least invasive).

The environment **model** is supplied as CNF over environment literals
(section 6) and encoded as plain clauses at the start of `solve_universal`.
Model clauses, like oracle clauses, are tautologies within the modeled space
and excluded from cell support.

**Blocking clauses** are `Vec<Literal>` disjunctions over environment
literal variables. Suggested representation: a new `Clause::Blocked` variant
backed by the same arena pattern as learnt clauses (or literally reuse the
learnt-clause arena with an empty `learnt_why`; choose whichever keeps
propagation and conflict analysis simplest). They participate in watching
and propagation like any clause.

### 5.5 The enumeration loop

New entry point (suggested location `src/solver/mod.rs` plus a new
`src/solver/universal.rs` for cell logic):

```rust
pub fn solve_universal(
    &mut self,
    problem: UniversalProblem<D::SolvableId>,
) -> Result<UniversalSolution<D::SolvableId>, UniversalFailure>
```

Loop skeleton:

1. Reset state (like `solve`), encode root clause, encode model clauses.
2. For each seed cell (in given order): set its literals as assumption
   decisions (5.7), run the inner solve; on success record the cell (still
   generalize and repair); on an assumption-level conflict discard the seed
   (its region returns to free enumeration) rather than failing.
3. Free enumeration: run the inner solve repeatedly. After each solution:
   extract and generalize the cell (5.6), repair disjointness against
   previously recorded cells, record `(cell, chosen_solvables)`, add the
   blocking clause `not cell` (disjunction of the negations of the cell's
   signed literals), undo decisions back to level 0, continue.
4. When the inner solve reports unsolvable at the root: run the
   **environment witness check**: is there an assignment of environment
   literals satisfying (model clauses AND oracle consistency clauses AND all
   blocking clauses)? The number of environment literals is small; a
   straightforward dedicated mini-search over only those variables is
   sufficient (do not try to reuse the main CDCL for this in v1).
   - No witness: the model is fully covered. Return all recorded cells.
   - Witness `w`: some region remains and is unsolvable. Re-run the inner
     solve with `w` as assumptions to obtain a proper `Conflict`, and return
     `UniversalFailure::Unsolvable { cell: w, conflict }`.
5. Cancellation (`should_cancel_with_value`) propagates out as today.

Notes:

- The existing `solve` resets `SolverState` per call; `solve_universal` owns
  one state across the whole loop. `run_sat` already supports being called
  with prior decisions present (the soft-requirements path), but the
  enumeration loop additionally needs to retract all decisions between
  cells (`decision_tracker.undo_until(0)`) while keeping clauses; verify
  this leaves watches in a valid state (it should; watches are
  backtrack-stable).
- (Learned in M2, now implemented:) watching and propagation alone do NOT
  guarantee that a solution's undecided-counts-as-false completion satisfies
  a blocking clause whose literals are all positive and undecided, so the
  enumerator would rediscover an already-blocked cell forever. `decide()`
  therefore has a lowest-priority pass over blocking clauses only: when no
  other decision exists and a blocking clause is unsatisfied under the
  completion, its first undecided positive literal is decided true. This is
  a no-op for plain solves (the blocking list is empty) and is what makes
  the disjointness-repair precondition ("the earlier cell's blocking clause
  is satisfied by the current assignment") actually hold. Model clauses are
  deliberately NOT forced this way: leaving them unsatisfied-under-completion
  is harmless (they bound coverage, not solution validity) and preserves the
  baseline-first policy.
- Soft requirements are not supported in universal mode in v1 (panic with a
  clear message if combined).
- `Candidates::locked`/`favored` keep working unchanged for concrete
  packages.
- Determinism: with a stable candidate sort order and identical clause
  iteration order, repeated cells naturally pick the same versions, since
  divergence requires a forcing clause. No explicit cross-cell preference
  mechanism is needed in v1; if benchmarks show drift (e.g. via activity
  scores changing decision order), consider freezing or resetting activity
  between cells.

### 5.6 Cell extraction and generalization (correctness critical)

After a model is found, with the solvable assignment fixed, compute the
cell as the conjunction of exactly the environment literal assignments that
are load bearing:

Scan all clauses that mention at least one environment literal variable
(maintain an index of such clauses as they are created; do not scan the full
clause database). For each, with installed/not-installed status of concrete
variables fixed:

- Requires with environment condition (`not A or not L or B...`): if `A` is
  not installed, no support needed. If some `B` is installed, no support
  needed (the requirement is satisfied regardless of the environment). If
  `A` is installed and no `B` is installed, the clause is satisfied only by
  a condition complement literal `not L` being true. Record exactly ONE
  such literal: the first env literal in the disjunction's encoding-fixed
  order whose assigned value is not true. One true `not L` keeps the clause
  satisfied throughout the cell; recording more fragments cells, and
  recording a complement whose literal is TRUE in the assignment (possible
  for AND conditions where another clause forces one conjunct) is unsound.
  `L` may be merely undecided; undecided counts as false and must still be
  recorded. (Rule refined during M2 review.)
- Requires on an environment package (`not A or L_S`): if `A` installed,
  add `L_S` (it was propagated true).
- EnvConstrains (`not A or Ab or L_S`): if `A` installed, add whichever of
  `Ab`/`L_S` is true in the assignment (prefer recording exactly the
  satisfying one; if both are somehow true that is an oracle violation,
  assert in debug).
- Oracle consistency clauses, model clauses, blocking clauses: never
  contribute support (tautological over the modeled space, or handled by
  the repair step below).
- Learnt clauses: never contribute support (they are implied by the other
  clauses).

**Disjointness repair**: generalization can in principle widen a cell into a
previously recorded cell. After generalizing, check the new cell for
syntactic overlap with every previously recorded cell (two conjunctions are
provably disjoint if they contain complementary assignments of the same
literal, or two true literals the oracle calls Disjoint, or a true literal
and a true absent literal of the same package). If overlap with a cell that
has a *different* solvable set cannot be excluded, re-add distinguishing
literals from the full assignment until disjointness is provable; the full
assignments of two cells are always distinguishable because the blocking
clause of the earlier cell forced at least one literal to differ. Overlap
between cells with *identical* solvable sets is harmless (the merge step
ORs them).

The post-hoc verifier (5.8) re-checks all of this independently; the repair
step exists so the verifier should never fire in practice.

### 5.7 Assumptions (seeded cells)

Classic incremental-SAT assumptions, implemented as decisions:

- Before calling the inner solve for a seeded cell, push each of the cell's
  signed literals as a decision at its own level (1..n), derived from a new
  sentinel `ClauseId::assumption()` (pattern after `ClauseId::install_root`).
- The inner solve starts at `starting_level = n` (the machinery in `run_sat`
  already takes a starting level from the decision stack).
- If conflict analysis (`learn_from_conflict` / `analyze`) computes a
  backjump level below the assumption levels, or `analyze_unsolvable` is
  reached while assumptions are present, the seeded cell is unsolvable:
  do not treat it as a global conflict. Surface this to the enumeration
  loop, which drops the seed (the region is then handled by free
  enumeration; if it is genuinely unsolvable the witness check will fail
  the solve with a proper conflict later).
- After the seeded cell completes, retract to level 0 as usual.

This is the trickiest internal change; it needs careful tests around
backjumping (a learnt clause asserting at an assumption level, conflicts at
level n+1, etc.).

(Learned in M4, now implemented:) the exact mechanics that proved out:

- `ClauseId::assumption()` is deliberately NOT backed by a real clause: it
  sits at the top of the id space, is only ever compared against, and an
  accidental dereference panics out of bounds. Conflict analysis never
  resolves on assumption decisions: the analysis pop loop only consumes
  decisions at the conflict level, which all sit above the assumptions on
  the level-monotone stack (debug asserted). Learnt clauses therefore stay
  globally valid; assumptions enter them only as literals.
- The "root level" generalizes from 1 to n+1 everywhere: the root install
  sits directly above the assumption prefix. `learn_from_conflict` treats a
  conflict at level <= n+1 as "unsolvable as seeded" (internal
  `ResolveError::AssumptionConflict`), which `run_sat` surfaces as
  `Ok(false)`, the same contract as the existing prior-decisions
  unsolvable path; while assumptions are active every unsolvable outcome
  means exactly that, so no extra distinction is needed. `analyze` clamps
  the backjump target to n+1: a raw target inside the assumption prefix
  (every non-UIP literal at assumption levels) would pop the root install,
  which nothing reinstalls mid-run; asserting the UIP literal at the root
  level instead is a valid, merely later-than-necessary unit propagation.
- Cell solvables are recorded in canonical variable-interning order, not
  decision-stack order: propagation order (watchlist history) legitimately
  differs between a free and a seeded solve of the same cell.
- Seeded re-solves are NOT always byte-identical: a cell's original
  solution can be steered by transient learnt state from conflicts in
  earlier cells of the same run, while the seeded replay avoids those
  conflicts and finds a better solution whose support can be more general
  (healing applies to solvables, not just stale conditions; about a fifth
  of the conflict-heavy random universes in the property test heal). One
  reseed round is a byte-identical fixed point, and conflict-free replays
  reproduce exactly. Callers that want version-level stickiness on top
  should pass the previous solution as `Candidates::favored`, which keeps
  working unchanged in universal mode.

### 5.8 Output types, merge, verify, project

```rust
/// A signed environment literal.
pub struct EnvLiteral<N> {
    pub package: N,
    pub kind: EnvLiteralKind, // Matches(VersionSetId) | Absent
}

/// Conjunction of signed environment literals. Empty = "all environments".
pub struct CellCondition<N>(Vec<(EnvLiteral<N>, bool)>);

pub struct UniversalProblem<Id> {
    // reuses Problem requirements/constraints (no soft requirements)
    pub requirements: Vec<ConditionalRequirement>,
    pub constraints: Vec<VersionSetId>,
    /// CNF over environment literals bounding the environment space.
    pub environment_model: Vec<Vec<(EnvLiteral<N>, bool)>>,
    /// Cells from a previous solve, solved first under assumptions.
    pub seed_partition: Vec<CellCondition<N>>,
}

pub struct UniversalSolution<Id, N> {
    /// Pairwise disjoint, together covering the environment model.
    pub cells: Vec<(CellCondition<N>, Vec<Id>)>,
}

pub enum UniversalFailure { /* Unsolvable { cell, conflict } | Cancelled */ }
```

Methods on `UniversalSolution`:

- `merged(&self, provider) -> Vec<(Id, Presence<N>)>` where `Presence` is a
  disjunction of cell conditions, with cheap simplifications: a solvable in
  every cell gets the always-true presence; pairs of cells differing in one
  literal's sign merge by dropping that literal. Also expose conditional
  edges: for each installed parent and requirement, the condition under
  which the edge is active (derivable from the recorded cells plus
  requirement conditions); a minimal `(parent, requirement, presence)` list
  is enough for rattler_lock to build a conditional lockfile.
- `verify(&self, oracle) -> Result<(), Vec<Violation>)`: independent
  re-check of pairwise disjointness (per package name with differing
  chosen solvables) and model coverage. Uses only the oracle and the cell
  conditions, no solver state, so it can also validate deserialized
  lockfiles later.
- `project(&self, eval: impl Fn(&EnvLiteral<N>) -> bool) -> Option<&[Id]>`:
  find the unique cell whose condition evaluates true. This is the runtime
  "walker" entry point.

### 5.9 Conflict reporting

`Conflict`/`DisplayUnsat` (src/conflict.rs) must not panic when clauses
reference environment literal variables or blocking clauses. v1 bar:

- Environment literal variables display via the existing version-set/name
  display methods ("environment `__cuda >=11`", "environment `__cuda`
  absent").
- Blocking clauses surface as a generic "environment partition constraint"
  cause.
- `UniversalFailure::Unsolvable` carries the witness `CellCondition` so the
  caller can print "for environments where __glibc >=2.17 and __cuda is
  absent: <existing conflict rendering>".

## 6. Milestones and tasks

Each milestone must compile, pass `cargo test --all-features`, pass
`cargo clippy` and `cargo fmt --check`, and be committed before the next
starts. The repo uses pixi: prefer `pixi run` tasks if defined (see
pixi.toml), plain cargo otherwise.

**TDD discipline (mandatory).** Every task follows red-green-refactor:

1. Write the test first, including the expected output, and run it to watch
   it fail for the *expected reason* (a wrong expected reason means the
   mental model is off; stop and reassess). Behavior tests use insta
   *inline* snapshots (`assert_snapshot!(result, @"...")`) with the
   expectation hand-written from first principles, never generated by
   running the code. File snapshots and `INSTA_UPDATE` are only acceptable
   for pre-existing tests whose output legitimately changes.
2. Implement the minimum to make it pass.
3. Refactor with the suite green; commit.

Where a task is pure plumbing with no new behavior (parts of M0), the
"test" is the existing suite staying green plus compile-time checks; say so
explicitly in the commit message. Unit-test pure logic directly (the
oracle-to-clause emission, DNF conversion, cell generalization,
disjointness repair, witness check are all deliberately testable as
functions on constructed inputs; keep them that way). Scenario tests go
through `BundleBoxProvider`. When fixing a bug found during review or by
the property test, first add a minimal failing regression test, then fix.

### M0: API plumbing (breaking, no behavior change)

- Add `VersionSetRelation`, `EnvironmentPackage`, `PackageCandidates`,
  `EnvLiteral`, `EnvLiteralKind`, `CellCondition` to the public API.
- Change `DependencyProvider::get_candidates` to return
  `Option<PackageCandidates<...>>`; add
  `environment_version_set_relation` with a default body of
  `unreachable!("provider declared no environment packages")`.
- Thread the enum through `SolverCache` and `encoding.rs`; non-universal
  `solve` panics on `PackageCandidates::Environment`.
- Migrate `tests/solver/bundle_box` (wrap in `Candidates(..)`), the
  examples, the snapshot module, and keep `cpp/` compiling.
- Extend `BundleBoxProvider`: `add_environment_package(name, can_be_absent)`
  and an oracle implementation via `version_ranges::Ranges` operations
  (`is_disjoint`, `subset_of` exist on that type). Implementation notes
  from reading the provider (tests/solver/bundle_box/mod.rs):
  - `get_candidates` asserts on duplicate requests and returns `None` for
    unknown names; store environment packages in their own map and check
    it before the `self.packages` lookup.
  - Keep the test DSL string based: environment model literals reuse the
    existing `Spec` parser (e.g. `"cuda 11.."`), conditional dependencies
    on environment packages already work through `SpecCondition` with no
    parser changes.
  - `into_snapshot` / `DependencySnapshot::from_provider` serializes
    candidates; decide explicitly how snapshots treat
    `PackageCandidates::Environment` (an error is acceptable in v1, a
    silent skip is not).
- Acceptance: all existing tests pass unchanged in behavior.

### M1: Environment literals in the encoder

- `VariableOrigin::{EnvMatches, EnvAbsent}` + interning + display.
- Consistency-clause emission on literal interning (5.2).
- Requirement-on-env-package encoding (binary clause to the literal).
- Condition-on-env-package encoding (`DisjunctionComplement::EnvLiteral`).
- `Clause::EnvConstrains` + `decide()` integration with `[absent, matches]`
  candidate order (5.4 item 3).
- Reject in v1: `Requirement::Union` mixing env and concrete version sets;
  locked/favored/excluded on environment packages.
- Tests (write before the corresponding encoding work; bundle_box, plain
  `solve` with hand-set env literal decisions is NOT needed; test through
  encoding + a single forced cell): unit tests asserting the generated
  clauses for each of the three encodings (hand-write the expected clause
  list), plus inline-snapshot tests where a normal solve over env packages
  yields the baseline (all-literals-false) solution: conditional dependency
  not activated, constraint forcing the absent branch, requirement forcing
  a literal true.
- Acceptance: snapshots reviewed; no regression in existing tests.

### M2: Enumeration loop

- `UniversalProblem`, `solve_universal`, model-clause encoding, blocking
  clauses, the loop from 5.5 (without seeds: free enumeration only).
- Cell extraction + clause-support generalization + disjointness repair
  (5.6), including the clause index for env-literal-mentioning clauses.
- Environment witness check + `UniversalFailure::Unsolvable`.
- Tests (write all five scenarios first as inline snapshots with
  hand-authored cell partitions; they define the loop's contract and
  double as the executable spec for 5.5/5.6):
  - conditional dependency on `cuda` with model `cuda absent or cuda >=11`:
    two cells, baseline first.
  - candidate-level split: package with build A requiring `glibc >=2.28`
    and build B requiring `glibc >=2.17`, model `glibc >=2.17`: two cells,
    correct generalization (cell 2 is `>=2.17 and not >=2.28`).
  - constrains split: `A constrains cuda >=11`, model allows absent and
    `>=10`: cells for absent and `>=11`; region `cuda <11 present` must
    fail the solve with a witness (total coverage semantics).
  - non-fragmentation: an env package whose literals are never load
    bearing must yield exactly one cell with the empty condition.
  - unsolvable region: model region where no candidate fits fails with
    `UniversalFailure::Unsolvable` carrying the right cell.
- Acceptance: all scenario snapshots correct on first principles review.

### M3: Output layer

- `UniversalSolution::{merged, verify, project}` per 5.8, conditional edge
  extraction.
- Property test (write against the M3 API signatures before implementing
  merge/verify/project; it should fail to compile, then fail, then pass):
  generate random small universes with env packages
  (extend the existing proptest-style generators if present, otherwise a
  seeded loop), run `solve_universal`, then for a sample of concrete
  environments drawn from the model: (a) `project` returns exactly one
  cell; (b) the projected set is a valid solution (every active
  requirement satisfied, no constraint violated, at most one solvable per
  name), checked directly against the BundleBoxProvider metadata; (c)
  `verify` passes.
- Acceptance: property test runs clean for a large iteration count.

### M4: Seeded partition (assumptions)

- Assumption decisions + sentinel clause id + backjump handling (5.7).
- Seed loop in `solve_universal`; invalid seeds dropped, not fatal.
- Tests (write first; the assumption-boundary cases below are exactly
  where 5.7 warns the implementation is delicate, so the tests must exist
  before the level arithmetic is touched): re-solving with the previous
  result's cells as seeds reproduces identical cells and solutions; a seed
  invalidated by a changed universe is dropped and the region
  re-enumerated; assumption-level conflict does not corrupt later cells
  (run a full enumeration after a dropped seed); a learnt clause that
  asserts at an assumption level; a conflict at exactly level n+1.
- Acceptance: determinism test (same input twice, byte-identical output)
  and seed-stability test pass.

### M5: Conflict reporting and docs

- 5.9 display work; `DisplayUnsat` snapshot for an unsolvable cell.
- Crate-level docs for the universal API, CHANGELOG entry.

### M6: rattler integration proof (in F:/projects/rattler)

- Oracle for `NamelessMatchSpec` on virtual packages: version-range
  intersection over `VersionSpec` (implement or reuse range logic in
  rattler_conda_types); exact-string build matchers; the archspec
  implication DAG hardcoded for `__archspec`; `Unknown` for anything else.
- `CondaDependencyProvider`: a mode where a configured subset of virtual
  packages is symbolic (`PackageCandidates::Environment`) instead of
  injected; do not add symbolic ones as root requirements.
- `SolverTask` additions: symbolic virtual package set, environment model,
  seed partition; a `solve_universal` entry next to the existing solve.
- Integration tests on real repodata snapshots (the repo has test data
  infrastructure): a cuda/no-cuda scenario (e.g. pytorch-style cpu/gpu
  builds via `__cuda`), a glibc floor scenario, an archspec scenario.
- Benchmark: universal solve vs N concrete solves for the same coverage.
- Acceptance: integration tests green; benchmark report attached to the PR.

## 7. Risks and watch items

- **Vacuous cells**: `Unknown` oracle answers can let the solver explore
  environment regions no machine has; a conflict there fails the whole
  solve. Mitigation: precise oracle for the v1 conda set; if it bites,
  add a "best effort" flag later (explicitly out of scope now).
- **Cell fragmentation**: `(not A or Ab or L)` constraints split absent and
  matching regions even when the solution is identical; the merge step
  reabsorbs this, but watch partition sizes in the rattler integration.
- **Assumption backjumping** (M4) is the most delicate solver-internal
  change; review `analyze`/`learn_from_conflict` level arithmetic with
  extreme care and test conflicts exactly at the assumption boundary.
- **Activity drift across cells** could harm version coherence; benchmark,
  and freeze/reset activity between cells if observed.
- **Witness check correctness**: it must use exactly the same clause set
  semantics (model + oracle + blocking) as the main solve or coverage
  termination is wrong.
- **C++ API**: keep compiling, do not extend.

## 8. Instructions for implementing agents

- This repository uses **jj**, not git. Use `jj` commands exclusively
  (`jj new`, `jj commit -m`, `jj diff`, `jj log`). Never run `git` mutation
  commands, never create git worktrees. For parallel work use jj
  workspaces.
- One commit per completed task minimum; every commit compiles and passes
  tests. Commit messages explain why, follow the existing style in
  `jj log` (conventional commits: `feat:`, `test:`, `refactor:`).
- Work test-first per the TDD discipline in section 6. A task's first
  action is writing its failing test; if you cannot express a failing test
  for a task, stop and report that to the supervisor instead of
  implementing untested.
- Never use em dashes in code, comments, docs, or commit messages.
- Run `cargo fmt`, `cargo clippy --all-targets`, `cargo test` (or the pixi
  equivalents) before declaring a task done. Review every insta snapshot
  diff; never blanket-accept.
- Do not change public API beyond what this document specifies without
  flagging it to the supervisor first.
- If blocked or after 3 failed attempts at a task, stop and report instead
  of thrashing.
- Reference reading before starting: `src/solver/conditions.rs` (module
  docs explain condition encoding), `src/solver/encoding.rs`,
  `src/solver/mod.rs` (`run_sat`, `decide`), `src/solver/variable_map.rs`,
  `tests/solver/bundle_box/`.
