//! Universal multi-environment resolution: a single solve whose output is
//! valid for a whole family of environments.
//!
//! [`Solver::solve_universal`] implements in-solver projected model
//! enumeration (design doc section 5.5): one CDCL instance in which
//! environment literals are ordinary SAT variables. After each solution the
//! load-bearing environment literal assignments are extracted and generalized
//! into a *cell* (section 5.6), a blocking clause over the cell's literals is
//! added, and the solver re-runs. When the formula becomes unsolvable, a
//! dedicated witness search over the environment-only clauses decides whether
//! the environment model is fully covered (success) or an uncovered region
//! remains that is unsolvable (failure).
//!
//! # Worked example
//!
//! The example below shows the full life cycle: declaring an environment
//! package, implementing the relation oracle, building and solving a
//! [`UniversalProblem`], consuming the solution, and seeding the next solve.
//! The code is illustrative (`ignore`); `tests/solver/main.rs` contains
//! executable equivalents of every step (see the `test_universal_*` tests).
//!
//! ```ignore
//! use resolvo::{
//!     Candidates, DependencyProvider, EnvClause, EnvLiteral, EnvironmentModel,
//!     EnvironmentPackage, SignedEnvLiteral, Solver, UniversalDependencyProvider,
//!     UniversalProblem, VersionSetRelation,
//! };
//!
//! // 1. `get_candidates` describes concrete packages only; it never sees an
//! //    environment package (enforced at the type level).
//! impl DependencyProvider for MyProvider {
//!     async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
//!         Some(Candidates { candidates: self.candidates_for(name), ..Default::default() })
//!     }
//!
//!     // ... the remaining DependencyProvider methods as usual ...
//! }
//!
//! // 2. Implement `UniversalDependencyProvider` to classify environment
//! //    packages and answer the relation oracle. These methods are consulted
//! //    only by `solve_universal`, never by a plain `solve`.
//! impl UniversalDependencyProvider for MyProvider {
//!     fn environment_package(&self, name: NameId) -> Option<EnvironmentPackage> {
//!         // e.g. `cuda`: a package whose value is unknown at solve time.
//!         // Machines without CUDA exist, so the absent literal is part of
//!         // the environment space.
//!         self.is_environment_package(name)
//!             .then_some(EnvironmentPackage { can_be_absent: true })
//!     }
//!
//!     // Soundness contract: answers other than `Unknown` must be correct;
//!     // when in doubt return `Unknown`. A wrong `Disjoint` or `Subset`
//!     // answer produces broken solutions, `Unknown` merely risks describing
//!     // environment regions no real machine has.
//!     fn environment_version_set_relation(
//!         &self,
//!         a: VersionSetId,
//!         b: VersionSetId,
//!     ) -> VersionSetRelation {
//!         let (a, b) = (self.range(a), self.range(b));
//!         if a == b {
//!             VersionSetRelation::Equal
//!         } else if a.is_disjoint(&b) {
//!             VersionSetRelation::Disjoint
//!         } else if b.contains(&a) {
//!             VersionSetRelation::Subset
//!         } else {
//!             VersionSetRelation::Unknown
//!         }
//!     }
//! }
//!
//! // 3. Build the problem: requirements as usual, plus an environment model
//! //    bounding the environment space the solution must cover. The model is
//! //    a CNF over signed environment literals; this one says "cuda is
//! //    absent, or cuda matches >=11". An empty model means "all
//! //    environments".
//! let model = EnvironmentModel::new(vec![EnvClause::new(vec![
//!     SignedEnvLiteral::new(EnvLiteral::Absent(cuda_name), true),
//!     SignedEnvLiteral::new(EnvLiteral::Matches(cuda_ge_11), true),
//! ])]);
//! let problem = UniversalProblem::new()
//!     .requirements(requirements)
//!     .environment_model(model.clone());
//!
//! let mut solver = Solver::new(provider);
//! let solution = solver.solve_universal(problem)?;
//!
//! // 4. Consume the solution. `cells()` partitions the environment space:
//! //    each cell pairs a region (a conjunction of signed environment
//! //    literals) with the solvables valid throughout that region.
//! for cell in solution.cells() {
//!     println!(
//!         "{}: {:?}",
//!         cell.condition().display(solver.provider()),
//!         cell.solvables(),
//!     );
//! }
//! // Per-solvable presence conditions, simplified within the model bounds.
//! let merged = solution.merged();
//! // The conditional dependency graph: what a lockfile serializer stores.
//! let edges = solution.edges();
//! // Project onto one concrete machine by evaluating each literal.
//! let on_this_machine = solution.project(|literal| my_machine.satisfies(literal));
//! // Re-check pairwise disjointness and model coverage, e.g. after
//! // reconstructing a solution from a lockfile.
//! solution.verify(solver.provider()).expect("disjoint cells covering the model");
//!
//! // 5. Seed the next solve with the previous partition: stable regions
//! //    re-solve first and keep their cell identity, which minimizes churn.
//! let next = UniversalProblem::new()
//!     .requirements(new_requirements)
//!     .environment_model(model)
//!     .seed_partition(solution.cells().iter().map(|c| c.condition().clone()).collect());
//! ```
//!
//! # Conflict reporting
//!
//! When [`Solver::solve_universal`] returns
//! [`UniversalFailure::Unsolvable`], the embedded [`Conflict`] has been
//! scoped to the failing witness region by a targeted re-solve. This means
//! the conflict graph returned by [`Conflict::graph`] and the human-readable
//! message returned by [`Conflict::display_user_friendly`] only reference
//! clauses that are relevant within the specific cell where the formula is
//! unsatisfiable.
//!
//! Environment packages are symbolic, so the conflict graph never treats
//! them as missing dependencies. A requirement on an environment package
//! becomes a requires edge to the
//! [`ConflictNode::EnvMatches`](crate::conflict::ConflictNode::EnvMatches)
//! node for its version set, rendered as a requirement on the environment.
//! Constraints placed on environment packages by solvables, and
//! mutual-exclusivity relations between environment literals (oracle
//! consistency clauses), appear as conflict edges between solvable nodes and
//! environment-literal nodes
//! ([`ConflictNode::EnvMatches`](crate::conflict::ConflictNode::EnvMatches) /
//! [`ConflictNode::EnvAbsent`](crate::conflict::ConflictNode::EnvAbsent)).

use std::{any::Any, fmt, marker::PhantomData};

use crate::{
    CellCondition, ConditionalRequirement, DenseIndex, Dependencies, DependencyProvider, EnvClause,
    EnvLiteral, EnvironmentPackage, Interner, KnownDependencies, NameId, PackageCandidates,
    Presence, Requirement, SignedEnvLiteral, SolvableId, UniversalDependencyProvider, VariableId,
    VersionSetId, VersionSetRelation,
    conflict::Conflict,
    internal::{id::ClauseId, solver_id::SolvableIdOrRoot},
    runtime::AsyncRuntime,
    solver::{
        PrefixBudgetExhausted, Solver, SolverState, UnsolvableOrCancelled, WitnessProbeTripped,
        clause::{Clause, EnvClauseKind, Literal, WatchedLiterals},
        decision::Decision,
        prop_counters::prop_hit,
        variable_map::VariableOrigin,
    },
    solver_id::IdMap,
};

/// The result of one enumeration pass (see `Solver::enumerate_universal`):
/// either a complete partition, or the trail-reuse attempt was abandoned and
/// the enumeration must re-run from scratch without it.
enum EnumerationOutcome<Id, N> {
    Done(UniversalSolution<Id, N>),
    /// The trail-reuse attempt exceeded its work budget. The payload is the
    /// seed list for the reuse-free retry: the cells the attempt recorded
    /// before the abort (in recording order; each a verified-disjoint
    /// region whose seeded replay is an assumption-driven solve, the cheap
    /// path), followed by any original seeds the attempt had not yet
    /// processed. Every seed that WAS processed is either reflected in a
    /// recorded cell or was legitimately dropped as stale, so the
    /// concatenation preserves the original seed partition's influence and
    /// the stale-seed-drop semantics while saving the abandoned attempt's
    /// coverage work (measured: idx-33-class problems used to pay 40-48%
    /// of their cost re-deriving the discarded first attempt).
    ReuseAbandoned(Vec<CellCondition<N>>),
}

/// The number of ordinary decision levels (decisions on variables that are
/// neither environment literals nor env-sensitive parents) a cell-to-cell
/// retraction may pop before the enumeration prefers a full retraction that
/// rebuilds the trail in the env-literals-last shape (see the trail-reshape
/// comment in `enumerate_universal`). Small enough to catch a buried
/// env-independent suffix early, large enough to tolerate the dependency
/// subtree that installing a deferred variant parent drags above the env
/// tail.
const TRAIL_RESHAPE_ORDINARY_LEVELS: u32 = 8;

/// Work budget, in propagated decisions, of one FREE-phase `run_sat`
/// episode of a universal enumeration before the episode is abandoned in
/// favor of the environment-witness search (witness-guided escalation; see
/// the `WitnessProbeTripped` handling in `Solver::enumerate_universal`).
///
/// The budget is FLAT rather than relative to the recorded fresh-solve cost
/// on purpose: the monster episodes this probe exists for (8-21M propagated
/// decisions between cells on the unsolvable tail of the conda-forge
/// corpus) can be the FIRST run of an enumeration, where no fresh-solve
/// cost has been recorded yet, so a fresh-solve-relative budget has nothing
/// to calibrate against exactly where it is needed most (measured: the
/// worst tail problem's fresh solve alone costs 2.7M propagations, past any
/// reasonable multiple of an unknown baseline). Two million propagated
/// decisions is comfortably above every healthy free episode observed
/// across the benchmark corpus and two-plus orders of magnitude below the
/// pathological ones; a tripped episode costs one bounded wasted search
/// plus a <15ms witness check.
const WITNESS_PROBE_BUDGET: u64 = 2_000_000;

/// The maximum number of enumeration passes a SEEDED `solve_universal` may
/// take to reach the reseed fixed point (see `solve_universal_impl`). Each
/// pass re-enumerates from a fresh solver state with the previous pass's
/// cells as seeds; convergence normally takes one pass (the seeds replay
/// exactly) or two (one healing pass plus its confirmation). On exhaustion
/// the last pass's solution is returned: still a verified disjoint cover,
/// at worst not yet reproducing byte-identically under one more reseed.
const RESEED_FIXED_POINT_MAX_ROUNDS: u32 = 8;

/// The number of cell literals each pinning rule contributed to one recorded
/// cell of a universal enumeration (a diagnostics observation point, see
/// `Solver::universal_cell_pins`).
///
/// Every literal of a recorded cell is attributed to exactly one rule: the
/// rule that first pushed it during cell extraction or disjointness repair.
/// The split separates honest load-bearing extraction (the first six
/// fields) from the disjointness-repair appends, which is the load-bearing
/// distinction when investigating condition fragmentation: repair literals
/// do not create cells, they only specialize the conditions of solutions
/// that generalization would otherwise overlap with earlier cells.
#[derive(Default, Clone, Copy, Debug)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub struct CellPinCounts {
    /// Environment literals installed as requirement candidates (a
    /// requirement on an environment package satisfied by the literal).
    pub req_env: u32,
    /// Guards of an active conditional requirement with an installed target
    /// (recorded positively).
    pub guard: u32,
    /// True environment literals skipped by the support scan (pinned
    /// positively).
    pub support_skip: u32,
    /// Condition complement literals carrying a clause's support (recorded
    /// negatively).
    pub support_neg: u32,
    /// `EnvConstrains` absent/matches pins.
    pub constrains: u32,
    /// Environment assignments in the implication cone of a falsified
    /// more-preferred candidate that `decide()` skipped (pinned with their
    /// trail sign, so a re-solve of the cell excludes the same candidates
    /// and reproduces the same picks).
    pub steering: u32,
    /// Agreement pins added by the disjointness repair scan.
    pub repair_agreement: u32,
    /// Distinguishing literals appended by the disjointness repair.
    pub repair_distinguishing: u32,
}

#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
impl CellPinCounts {
    /// The total number of literals attributed across all rules; equals the
    /// recorded cell's literal count.
    pub fn total(&self) -> u32 {
        self.req_env
            + self.guard
            + self.support_skip
            + self.support_neg
            + self.constrains
            + self.steering
            + self.repair_agreement
            + self.repair_distinguishing
    }
}

/// Inverted index over the recorded cells' literals of one enumeration
/// pass: variable id -> a pair of cell-index bitsets, one per literal sign
/// (`[negative occurrences, positive occurrences]`, bit `i` = recorded
/// cell `i` contains the literal with that sign). Maintained by
/// `Solver::enumerate_universal` as cells are recorded;
/// `Solver::repair_disjointness` ORs the complementary-sign bitsets of the
/// new cell's literals to find the earlier cells that share a variable at
/// the opposite sign (which are provably disjoint and need no repair)
/// word-parallel, without scanning every cell pair.
#[derive(Default)]
struct CellLiteralIndex {
    by_variable: ahash::HashMap<VariableId, [Vec<u64>; 2]>,
}

impl CellLiteralIndex {
    /// Records that cell `cell_index` contains `(variable, value)`.
    fn record(&mut self, variable: VariableId, value: bool, cell_index: usize) {
        let bits = &mut self.by_variable.entry(variable).or_default()[usize::from(value)];
        let word = cell_index / 64;
        if word >= bits.len() {
            bits.resize(word + 1, 0);
        }
        bits[word] |= 1 << (cell_index % 64);
    }

    /// ORs the bitset of cells containing `variable` with the sign opposite
    /// to `value` into `marks` (sized by the caller to cover every recorded
    /// cell).
    fn mark_complementary(&self, variable: VariableId, value: bool, marks: &mut [u64]) {
        if let Some(bits) = self.by_variable.get(&variable) {
            for (mark, &bits) in marks.iter_mut().zip(&bits[usize::from(!value)]) {
                *mark |= bits;
            }
        }
    }
}

/// The per-cell capture inputs shared by `Solver::extract_cell` and
/// `Solver::capture_cell_edges`, collected in one trail scan by
/// `Solver::collect_cell_capture_inputs` (see there for the exact
/// semantics and ordering guarantees).
struct CellCaptureInputs {
    /// Registration indices of the installed requires parents, ascending.
    requires_parents: Vec<u32>,
    /// Support clauses of installed parents, in registration order.
    support_clauses: Vec<ClauseId>,
}

/// One diagnostics observation of a free-phase cell's trail retraction during
/// a universal enumeration (see `Solver::universal_cell_retracts`).
///
/// Records how far the trail was retracted before the cell's blocking clause
/// was added, relative to how deep the trail was at that point. A
/// [`target`](CellRetract::target) close to the [`depth`](CellRetract::depth)
/// means trail-prefix preservation kept most of the trail, which is the
/// load-bearing metric of the env-literals-last decision ordering.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub struct CellRetract {
    /// The retract target chosen before adding the cell's blocking clause,
    /// clamped to the trail depth (an unfalsified blocking clause needs no
    /// retraction).
    pub target: u32,

    /// The trail depth at that point.
    pub depth: u32,
}

/// Returns true when the variable represents an environment literal (either
/// a matches or an absent literal).
fn is_env_variable<D: DependencyProvider>(state: &SolverState<D>, variable: VariableId) -> bool {
    matches!(
        state.variable_map.origin(variable),
        VariableOrigin::EnvMatches(_) | VariableOrigin::EnvAbsent(_)
    )
}

/// An environment model: a CNF over signed environment literals. It is a
/// conjunction of [`EnvClause`]s (each a disjunction of [`SignedEnvLiteral`]s)
/// that together bound the environment space a universal solve must cover. An
/// empty CNF means "all environments".
///
/// This is a newtype rather than a bare `Vec<Vec<_>>` so a CNF model cannot be
/// confused with the DNF [`Presence`] type or with a single [`CellCondition`].
///
/// Both [`Serialize`](serde::Serialize) and [`Deserialize`](serde::Deserialize)
/// are derived (behind the `serde` feature): unlike [`CellCondition`] and
/// [`Presence`] the model has no construction-enforced invariant to bypass —
/// the one structural requirement (no empty disjunction) is checked when the
/// model is consumed, by [`Solver::solve_universal`] and
/// [`UniversalSolution::from_cells`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EnvironmentModel<N = NameId>(Vec<EnvClause<N>>);

impl<N> Default for EnvironmentModel<N> {
    /// The empty model, which bounds nothing ("all environments").
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<N> EnvironmentModel<N> {
    /// Creates a model from its clauses (a conjunction of disjunctions).
    pub fn new(clauses: Vec<EnvClause<N>>) -> Self {
        Self(clauses)
    }

    /// Returns an iterator over the model's clauses, in order. The model holds
    /// in an environment when every clause holds.
    pub fn clauses(&self) -> impl ExactSizeIterator<Item = &EnvClause<N>> + '_ {
        self.0.iter()
    }

    /// The number of clauses in the model.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the model has no clauses, i.e. bounds nothing ("all
    /// environments").
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<N> FromIterator<EnvClause<N>> for EnvironmentModel<N> {
    fn from_iter<T: IntoIterator<Item = EnvClause<N>>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Describes a universal resolution problem: requirements and constraints
/// like a [`crate::Problem`], plus an explicit environment model bounding the
/// space of environments the solution must cover.
///
/// Soft requirements are not supported in universal mode.
pub struct UniversalProblem<Id = SolvableId, N = NameId> {
    requirements: Vec<ConditionalRequirement>,
    constraints: Vec<VersionSetId>,
    environment_model: EnvironmentModel<N>,
    seed_partition: Vec<CellCondition<N>>,
    _marker: PhantomData<fn(Id) -> Id>,
}

impl<Id, N> Default for UniversalProblem<Id, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id, N> UniversalProblem<Id, N> {
    /// Creates a new empty [`UniversalProblem`]. Use the setter methods to
    /// build the problem before passing it to
    /// [`Solver::solve_universal`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            requirements: Vec::new(),
            constraints: Vec::new(),
            environment_model: EnvironmentModel::new(Vec::new()),
            seed_partition: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Sets the requirements that _must_ have one candidate solvable included
    /// in the solution of every cell.
    #[must_use]
    pub fn requirements(self, requirements: Vec<ConditionalRequirement>) -> Self {
        Self {
            requirements,
            ..self
        }
    }

    /// Sets the additional constraints imposed on individual packages that
    /// the solvable (if any) chosen for that package _must_ adhere to.
    #[must_use]
    pub fn constraints(self, constraints: Vec<VersionSetId>) -> Self {
        Self {
            constraints,
            ..self
        }
    }

    /// Sets the environment model: a CNF over signed environment literals
    /// (each inner `Vec` is a disjunction) bounding the environment space.
    ///
    /// The model is explicit and total: every region inside the model must be
    /// solvable, otherwise the universal solve fails with
    /// [`UniversalFailure::Unsolvable`]. An empty CNF means "all
    /// environments".
    #[must_use]
    pub fn environment_model(self, environment_model: EnvironmentModel<N>) -> Self {
        Self {
            environment_model,
            ..self
        }
    }

    /// Sets the seed partition: cell conditions from a previous solve (or any
    /// caller-supplied conjunctions of environment literals) that are solved
    /// first, in the given order, under assumptions (design doc 5.7).
    ///
    /// Seeding makes re-solves stable: a seed whose region is still solvable
    /// reproduces a cell for that region (possibly with a *more general*
    /// condition than the seed when the new solution depends on fewer
    /// environment literals; a stale over-specific seed heals). A seed whose
    /// region became unsolvable, contradicts the environment model, or
    /// contradicts itself is dropped, and the region it described is covered
    /// by the free enumeration that runs after all seeds.
    ///
    /// Seeds may only reference environment packages; an absent literal for a
    /// package declared with `can_be_absent: false` is also a caller error.
    /// Both are rejected up front by [`Solver::solve_universal`] as
    /// [`UniversalFailure::InvalidInput`] (see [`InvalidUniversalInput`]), as
    /// distinct from a merely stale seed, which heals silently.
    #[must_use]
    pub fn seed_partition(self, seed_partition: Vec<CellCondition<N>>) -> Self {
        Self {
            seed_partition,
            ..self
        }
    }
}

/// One cell of a [`UniversalSolution`]: a region of the environment space
/// together with everything the solve determined for it.
///
/// Bundling the three parts — the [`condition`](Cell::condition) describing
/// the region, the [`solvables`](Cell::solvables) valid throughout it, and the
/// dependency [`edges`](Cell::edges) active in it — keeps them consistent by
/// construction. The previous representation stored the edges in a vector
/// parallel to the cells, an invariant that had to be maintained by hand;
/// making a cell own its edges removes that class of bug.
#[derive(Clone, Debug)]
pub struct Cell<Id = SolvableId, N = NameId> {
    condition: CellCondition<N>,
    solvables: Vec<Id>,
    edges: Vec<CellEdge<Id>>,
}

impl<Id, N> Cell<Id, N> {
    /// Assembles a cell from its parts. Intended for reconstructing a
    /// [`UniversalSolution`] from serialized data (see
    /// [`UniversalSolution::from_cells`]); the enumerator builds cells
    /// internally.
    pub fn new(condition: CellCondition<N>, solvables: Vec<Id>, edges: Vec<CellEdge<Id>>) -> Self {
        Self {
            condition,
            solvables,
            edges,
        }
    }

    /// The conjunction of environment literals describing the region of the
    /// environment space this cell covers.
    pub fn condition(&self) -> &CellCondition<N> {
        &self.condition
    }

    /// The solvables chosen for this cell, valid throughout its region, in
    /// canonical (solver-variable-id) order.
    pub fn solvables(&self) -> &[Id] {
        &self.solvables
    }

    /// The dependency edges active in this cell.
    pub fn edges(&self) -> &[CellEdge<Id>] {
        &self.edges
    }
}

/// The result of a successful [`Solver::solve_universal`] call.
#[derive(Debug)]
pub struct UniversalSolution<Id = SolvableId, N = NameId> {
    /// The enumerated cells (see [`UniversalSolution::cells`]).
    cells: Vec<Cell<Id, N>>,

    /// The environment model the solve was bounded by. Stored so that
    /// [`UniversalSolution::verify`] can re-check model coverage without any
    /// solver state (e.g. on a solution reconstructed from a lockfile).
    environment_model: EnvironmentModel<N>,
}

impl<Id, N> UniversalSolution<Id, N> {
    /// The enumerated cells: each pairs the conjunction of environment
    /// literals describing a region of the environment space with the
    /// solvables chosen for that region and the dependency edges active there.
    ///
    /// For a solution produced by [`Solver::solve_universal`] the cells are
    /// pairwise disjoint, listed in deterministic enumeration order (the
    /// baseline cell first), and together cover the environment model; the
    /// returned slice is read-only, so those invariants cannot be broken
    /// afterwards. A solution reconstructed with
    /// [`UniversalSolution::from_cells`] is only as consistent as its input —
    /// re-check it with [`UniversalSolution::verify`].
    pub fn cells(&self) -> &[Cell<Id, N>] {
        &self.cells
    }

    /// The environment model the solve was bounded by.
    pub fn environment_model(&self) -> &EnvironmentModel<N> {
        &self.environment_model
    }

    /// Reconstructs a solution from previously serialized cells and the
    /// environment model it was solved against — the lockfile-reconstruction
    /// path.
    ///
    /// The parallel-vector hazard of the old representation is gone: each
    /// [`Cell`] carries its own condition, solvables and edges, so the only
    /// structural check left is the model, which must contain no empty
    /// disjunction (an empty disjunction is unsatisfiable and would make the
    /// whole model, and every coverage check against it, vacuous).
    ///
    /// Provider-dependent invariants — pairwise cell disjointness, model
    /// coverage, and that every literal names an environment package — are
    /// *not* checked here; call [`UniversalSolution::verify`] with the
    /// provider for those.
    pub fn from_cells(
        cells: Vec<Cell<Id, N>>,
        environment_model: EnvironmentModel<N>,
    ) -> Result<Self, InvalidUniversalInput<N>> {
        if environment_model.clauses().any(|clause| clause.is_empty()) {
            return Err(InvalidUniversalInput::EmptyModelDisjunction);
        }
        Ok(Self {
            cells,
            environment_model,
        })
    }
}

/// A single dependency edge of one cell of a [`UniversalSolution`]: within
/// that cell, `parent` is installed and its `requirement` is active and
/// satisfied by `target`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CellEdge<Id = SolvableId> {
    /// The solvable whose requirement this edge satisfies, or `None` when the
    /// requirement comes from the root problem.
    pub parent: Option<Id>,

    /// The requirement this edge satisfies.
    pub requirement: Requirement,

    /// The solvable chosen to satisfy the requirement, or `None` when the
    /// requirement is on an environment package (the environment itself
    /// satisfies it; there is no solvable to install).
    pub target: Option<Id>,
}

/// A consistency violation reported by [`UniversalSolution::verify`].
///
/// `verify` can *prove* violations, but it cannot always prove their absence:
/// when the relation oracle answers [`VersionSetRelation::Unknown`] for a
/// pair of version sets, disjointness of two cells may be unprovable without
/// being false. Such cases are reported as
/// [`Violation::UnprovenDisjointness`], which callers may choose to treat as
/// a warning instead of an error.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Violation<N = NameId> {
    /// The conditions of two cells with different solvable sets are not
    /// provably disjoint, and all relevant oracle answers were definite: the
    /// cells genuinely describe overlapping environment regions.
    OverlappingCells {
        /// Index of the first cell in [`UniversalSolution::cells`].
        first: usize,
        /// Index of the second cell in [`UniversalSolution::cells`].
        second: usize,
    },

    /// The conditions of two cells with different solvable sets could not be
    /// proven disjoint because the relation oracle answered
    /// [`VersionSetRelation::Unknown`] for at least one pair of version sets.
    /// The cells may still be disjoint in reality.
    UnprovenDisjointness {
        /// Index of the first cell in [`UniversalSolution::cells`].
        first: usize,
        /// Index of the second cell in [`UniversalSolution::cells`].
        second: usize,
    },

    /// A region of the environment model is not covered by any cell. The
    /// reported condition describes one such region (it may be vacuous when
    /// the oracle answered [`VersionSetRelation::Unknown`] for literals
    /// involved in it, but a complete partition never produces this
    /// violation).
    UncoveredRegion(CellCondition<N>),
}

impl<Id: Copy + Eq, N: Copy + Eq> UniversalSolution<Id, N> {
    /// Returns the merged presence-condition view of the solution: one entry
    /// per distinct solvable, paired with the OR of the conditions of the
    /// cells that contain it.
    ///
    /// The presence is simplified within the bounds of the environment model:
    /// a solvable that appears in every cell gets the always-true presence
    /// (the cells together cover the model), and disjuncts that are identical
    /// except for one literal appearing with opposite signs merge by dropping
    /// that literal (run to fixpoint).
    ///
    /// Both the solvable order (first occurrence across cells) and the
    /// disjunct order are deterministic.
    pub fn merged(&self) -> Vec<(Id, Presence<N>)> {
        let mut order: Vec<Id> = Vec::new();
        for cell in &self.cells {
            for &solvable in &cell.solvables {
                if !order.contains(&solvable) {
                    order.push(solvable);
                }
            }
        }
        order
            .into_iter()
            .map(|solvable| {
                let presence = self
                    .presence_for_cells(|index| self.cells[index].solvables.contains(&solvable));
                (solvable, presence)
            })
            .collect()
    }

    /// Returns the aggregated dependency edges of the solution: one entry per
    /// distinct `(parent, requirement, target)` edge, paired with the OR of
    /// the conditions of the cells in which the edge is active, simplified
    /// exactly like [`UniversalSolution::merged`].
    ///
    /// The edge order (first occurrence across cells) and the disjunct order
    /// are deterministic. This is the view a lockfile serializer needs to
    /// store a conditional dependency graph.
    pub fn edges(&self) -> Vec<(CellEdge<Id>, Presence<N>)> {
        let mut order: Vec<CellEdge<Id>> = Vec::new();
        for cell in &self.cells {
            for &edge in &cell.edges {
                if !order.contains(&edge) {
                    order.push(edge);
                }
            }
        }
        order
            .into_iter()
            .map(|edge| {
                let presence =
                    self.presence_for_cells(|index| self.cells[index].edges.contains(&edge));
                (edge, presence)
            })
            .collect()
    }

    /// Independently re-checks the solution invariants. Uses only the
    /// relation oracle of `provider` and the data stored in the solution (no
    /// solver state), so it can also validate a solution reconstructed from a
    /// serialized lockfile.
    ///
    /// Two invariants are checked, both in [`EnvLiteral`] space:
    ///
    /// - **Pairwise disjointness**: every pair of cells whose solvable sets
    ///   differ must have provably disjoint conditions (complementary signs
    ///   of the same literal, two positive matches literals the oracle calls
    ///   [`VersionSetRelation::Disjoint`], or a positive absent and a
    ///   positive matches literal of the same package). Overlap between
    ///   cells with identical solvable sets is harmless and not reported.
    /// - **Model coverage**: a backtracking search looks for an assignment of
    ///   the environment literals that satisfies the environment model and
    ///   the oracle consistency constraints but the condition of no cell;
    ///   such an assignment describes an uncovered region.
    ///
    /// Note the asymmetry: `verify` *proves* violations, but it cannot
    /// always prove their absence. When the oracle answers
    /// [`VersionSetRelation::Unknown`], disjointness may be unprovable
    /// without being false (reported as
    /// [`Violation::UnprovenDisjointness`], which callers may treat as a
    /// warning), and a reported uncovered region may be vacuous (describing
    /// environments no real machine has). With a complete oracle both checks
    /// are exact.
    pub fn verify<D>(&self, provider: &D) -> Result<(), Vec<Violation<N>>>
    where
        D: UniversalDependencyProvider + Interner<SolvableId = Id, NameId = N>,
    {
        let mut violations = Vec::new();

        // Pairwise disjointness for cells whose solvable sets differ.
        for first in 0..self.cells.len() {
            for second in first + 1..self.cells.len() {
                if same_solvable_set(&self.cells[first].solvables, &self.cells[second].solvables) {
                    continue;
                }
                match prove_env_disjoint(
                    provider,
                    &self.cells[first].condition,
                    &self.cells[second].condition,
                ) {
                    Disjointness::Disjoint => {}
                    Disjointness::Overlapping => {
                        violations.push(Violation::OverlappingCells { first, second });
                    }
                    Disjointness::Unproven => {
                        violations.push(Violation::UnprovenDisjointness { first, second });
                    }
                }
            }
        }

        // Model coverage.
        if let Some(region) = self.find_uncovered_region(provider) {
            violations.push(Violation::UncoveredRegion(region));
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// Searches for a region of the environment model that no cell covers:
    /// an assignment of the environment literals satisfying the model
    /// clauses, the oracle consistency constraints (derived exactly like
    /// `SolverState::intern_env_matches_with_oracle_clauses` derives its
    /// clauses) and the negation of every cell condition. Returns `None`
    /// when the cells cover the entire model.
    fn find_uncovered_region<D>(&self, provider: &D) -> Option<CellCondition<N>>
    where
        D: UniversalDependencyProvider + Interner<NameId = N>,
    {
        // Collect the distinct environment literals of the model and the
        // cells, in deterministic first-occurrence order. Every collected
        // literal occurs in at least one clause below.
        let mut literals: Vec<EnvLiteral<N>> = Vec::new();
        let cell_literals = self.cells.iter().flat_map(|cell| cell.condition.literals());
        let model_literals = self
            .environment_model
            .clauses()
            .flat_map(|clause| clause.literals());
        for signed in model_literals.chain(cell_literals) {
            if !literals.contains(&signed.literal) {
                literals.push(signed.literal);
            }
        }
        let index_of = |literal: &EnvLiteral<N>| {
            literals
                .iter()
                .position(|known| known == literal)
                .expect("every model and cell literal was collected")
        };

        let mut clauses: Vec<Vec<IndexedLiteral>> = Vec::new();

        // Consistency constraints between same-package literals, mirroring
        // the clauses the solver emits on literal interning (5.2): Disjoint
        // is mutual exclusion, Subset/Superset/Equal are implications, and
        // an absent literal excludes every matches literal of its package.
        for i in 0..literals.len() {
            for j in i + 1..literals.len() {
                if literals[i].package(provider) != literals[j].package(provider) {
                    continue;
                }
                match (literals[i], literals[j]) {
                    (EnvLiteral::Matches(vs_i), EnvLiteral::Matches(vs_j)) => {
                        match provider.environment_version_set_relation(vs_i, vs_j) {
                            VersionSetRelation::Disjoint => {
                                clauses.push(vec![(i, true), (j, true)]);
                            }
                            VersionSetRelation::Subset => {
                                clauses.push(vec![(i, true), (j, false)]);
                            }
                            VersionSetRelation::Superset => {
                                clauses.push(vec![(j, true), (i, false)]);
                            }
                            VersionSetRelation::Equal => {
                                clauses.push(vec![(i, true), (j, false)]);
                                clauses.push(vec![(j, true), (i, false)]);
                            }
                            VersionSetRelation::Unknown => {}
                        }
                    }
                    (EnvLiteral::Matches(_), EnvLiteral::Absent(_))
                    | (EnvLiteral::Absent(_), EnvLiteral::Matches(_)) => {
                        clauses.push(vec![(i, true), (j, true)]);
                    }
                    (EnvLiteral::Absent(_), EnvLiteral::Absent(_)) => {
                        unreachable!("two absent literals of the same package are equal")
                    }
                }
            }
        }

        // The model clauses.
        for clause in self.environment_model.clauses() {
            clauses.push(
                clause
                    .literals()
                    .map(|signed| (index_of(&signed.literal), !signed.positive))
                    .collect(),
            );
        }

        // The negation of every cell condition: at least one of the cell's
        // literals must evaluate opposite. A cell with the empty condition
        // yields the empty (unsatisfiable) clause: it covers everything.
        for cell in &self.cells {
            clauses.push(
                cell.condition
                    .literals()
                    .map(|signed| (index_of(&signed.literal), signed.positive))
                    .collect(),
            );
        }

        let assignment = find_witness_indexed_nested(literals.len(), &clauses)?;
        Some(CellCondition::from_literals_unchecked(
            literals
                .into_iter()
                .zip(assignment)
                .map(|(literal, positive)| SignedEnvLiteral::new(literal, positive))
                .collect(),
        ))
    }

    /// Returns the solvables of the unique cell whose condition holds in the
    /// concrete environment described by `eval`: a `(literal, true)` entry
    /// requires `eval(literal)` to be true and a `(literal, false)` entry
    /// requires it to be false. This is the runtime "walker" entry point: an
    /// installer evaluates the environment literals against the actual
    /// machine and installs the projected cell, no solving required.
    ///
    /// Returns `None` when no cell matches, which only happens for
    /// environments outside the environment model. More than one matching
    /// cell is a broken pairwise-disjointness invariant: this is a
    /// `debug_assert`, and release builds return the first match.
    pub fn project(&self, eval: impl Fn(&EnvLiteral<N>) -> bool) -> Option<&[Id]> {
        // The environment must satisfy the model the solution was bounded by:
        // a cell can be broader than the model (when no dependency is
        // load-bearing on an env literal), so matching a cell's condition is
        // not sufficient. A machine outside the model has no cell.
        let in_model = self.environment_model.clauses().all(|clause| {
            clause
                .literals()
                .any(|signed| eval(&signed.literal) == signed.positive)
        });
        if !in_model {
            return None;
        }

        let mut found: Option<&[Id]> = None;
        for cell in &self.cells {
            let matches = cell
                .condition
                .literals()
                .all(|signed| eval(&signed.literal) == signed.positive);
            if !matches {
                continue;
            }
            debug_assert!(
                found.is_none(),
                "broken invariant: multiple cells match the same environment"
            );
            if found.is_none() {
                found = Some(&cell.solvables);
            }
        }
        found
    }

    /// Computes the presence condition for the cells selected by `member`:
    /// the always-true presence when every cell is a member (the cells
    /// together cover the environment model), otherwise the simplified OR of
    /// the member cells' conditions.
    fn presence_for_cells(&self, member: impl Fn(usize) -> bool) -> Presence<N> {
        if (0..self.cells.len()).all(&member) {
            return Presence::from_disjuncts_unchecked(vec![
                CellCondition::from_literals_unchecked(Vec::new()),
            ]);
        }
        let disjuncts = (0..self.cells.len())
            .filter(|&index| member(index))
            .map(|index| self.cells[index].condition.clone())
            .collect();
        Presence::from_disjuncts_unchecked(simplify_disjuncts(disjuncts))
    }
}

/// Names the [`UniversalProblem`] input that carried an invalid environment
/// literal, so an [`InvalidUniversalInput`] can point at the offending field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvInputSource {
    /// The environment model ([`UniversalProblem::environment_model`]).
    EnvironmentModel,
    /// The seed partition ([`UniversalProblem::seed_partition`]).
    SeedPartition,
}

impl fmt::Display for EnvInputSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnvInputSource::EnvironmentModel => f.write_str("environment model"),
            EnvInputSource::SeedPartition => f.write_str("seed partition"),
        }
    }
}

/// A structurally invalid caller input to a [`UniversalProblem`], detected by
/// [`Solver::solve_universal`] up front, before any solver state is built.
///
/// These are caller errors in the shape of the problem itself (a literal
/// naming the wrong kind of package, an internally inconsistent literal, or an
/// unsatisfiable model clause), not resolution outcomes: a well-formed problem
/// that merely has no solution fails with [`UniversalFailure::Unsolvable`]
/// instead. A stale seed that references still-valid environment packages but
/// no longer matches any solution is *not* invalid input; it heals silently
/// (see [`UniversalProblem::seed_partition`]).
///
/// Non-exhaustive: new structural checks may be added as further variants.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidUniversalInput<N = NameId> {
    /// A literal references a package the provider does not classify as an
    /// environment package
    /// ([`UniversalDependencyProvider::environment_package`](crate::UniversalDependencyProvider::environment_package)
    /// returned `None`). Only environment packages may appear in an
    /// environment model or a seed partition.
    NotAnEnvironmentPackage {
        /// The input that referenced the package.
        source: EnvInputSource,
        /// The offending package.
        package: N,
    },
    /// An [`Absent`](EnvLiteral::Absent) literal references an environment
    /// package declared with `can_be_absent: false`, which therefore has no
    /// absent literal.
    AbsentLiteralForPresentPackage {
        /// The input that referenced the literal.
        source: EnvInputSource,
        /// The offending package.
        package: N,
    },
    /// The environment model contains an empty disjunction. An empty clause is
    /// unsatisfiable, so it would make the whole model unsatisfiable.
    EmptyModelDisjunction,
    /// A caller-supplied requirement union mixes concrete package version sets
    /// with environment package version sets. Such a union cannot share one
    /// candidate list.
    MixedEnvironmentAndConcreteRequirement {
        /// The formatted requirement that mixed package kinds.
        requirement: String,
    },
}

impl<N: fmt::Debug> fmt::Display for InvalidUniversalInput<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidUniversalInput::NotAnEnvironmentPackage { source, package } => write!(
                f,
                "the {source} references package {package:?} which is not an environment package; \
                 only packages classified by `UniversalDependencyProvider::environment_package` \
                 may appear there",
            ),
            InvalidUniversalInput::AbsentLiteralForPresentPackage { source, package } => write!(
                f,
                "the {source} contains an absent literal for package {package:?} which was \
                 declared with `can_be_absent: false`",
            ),
            InvalidUniversalInput::EmptyModelDisjunction => f.write_str(
                "the environment model contains an empty disjunction, which makes the model \
                 unsatisfiable",
            ),
            InvalidUniversalInput::MixedEnvironmentAndConcreteRequirement { requirement } => {
                write!(
                    f,
                    "requirement `{requirement}` mixes environment and concrete version sets, \
                     which cannot share one candidate list"
                )
            }
        }
    }
}

impl<N: fmt::Debug> std::error::Error for InvalidUniversalInput<N> {}

/// The errors of an unsuccessful [`Solver::solve_universal`] call.
#[derive(Debug)]
#[non_exhaustive]
pub enum UniversalFailure<N = NameId> {
    /// Some region of the environment model has no solution. The whole
    /// universal solve fails because the model is total: every modeled
    /// environment must be solvable.
    Unsolvable {
        /// A witness region of the environment model that is unsolvable.
        cell: CellCondition<N>,
        /// The conflict produced by the scoped re-solve, tightly bound to
        /// the witness region. This conflict's display only references
        /// clauses that are relevant within the identified cell.
        conflict: Conflict,
    },
    /// A caller-supplied input was structurally invalid. Detected up front,
    /// before any solving, so the solver state is untouched. See
    /// [`InvalidUniversalInput`].
    InvalidInput(InvalidUniversalInput<N>),
    /// The solving process was cancelled.
    Cancelled(Box<dyn Any>),
}

impl<D: UniversalDependencyProvider, RT: AsyncRuntime> Solver<D, RT> {
    /// Solves the given [`UniversalProblem`], producing a partition of the
    /// environment model into cells, each with the solvables valid throughout
    /// that cell.
    ///
    /// See the module documentation and the universal-solve design document
    /// for the underlying algorithm.
    ///
    /// # Errors
    ///
    /// Structurally invalid inputs are rejected up front, before any solver
    /// state is built, as [`UniversalFailure::InvalidInput`]: root requirement
    /// unions may not mix concrete and environment packages; every literal in
    /// the [`environment_model`](UniversalProblem::environment_model) and in
    /// every [`seed_partition`](UniversalProblem::seed_partition) condition
    /// must reference an environment package (one classified by
    /// [`UniversalDependencyProvider::environment_package`](crate::UniversalDependencyProvider::environment_package));
    /// an [`Absent`](EnvLiteral::Absent) literal requires `can_be_absent: true`,
    /// and no model disjunction may be empty. (A [`Matches`](EnvLiteral::Matches)
    /// literal cannot name a version set of the wrong package: its package is
    /// derived from the version set, so that mismatch is unrepresentable.) See
    /// [`InvalidUniversalInput`] for the individual checks.
    ///
    /// ```ignore
    /// // `some_pkg` is a concrete package, not an environment package.
    /// let model = EnvironmentModel::new(vec![EnvClause::new(vec![
    ///     SignedEnvLiteral::new(EnvLiteral::Absent(some_pkg), true),
    /// ])]);
    /// let problem = UniversalProblem::new().environment_model(model);
    /// let err = solver.solve_universal(problem).unwrap_err();
    /// assert!(matches!(
    ///     err,
    ///     UniversalFailure::InvalidInput(InvalidUniversalInput::NotAnEnvironmentPackage { .. }),
    /// ));
    /// ```
    ///
    /// A modeled region that is genuinely unsolvable fails with
    /// [`UniversalFailure::Unsolvable`] instead, and a cancellation with
    /// [`UniversalFailure::Cancelled`].
    ///
    /// # Reproducibility
    ///
    /// The enumerated partition is a deterministic function of the problem
    /// *and* of the order in which the encoder registers requires clauses:
    /// that order is the `decide()` scan order, so it shapes which cell is
    /// found first and hence the exact partition. Registration order is stable
    /// under the default synchronous runtime (`NowOrNeverRuntime`, where
    /// encoder futures resolve in submission order). Conditional requirements
    /// whose condition has not fired yet are registered later, when their
    /// condition first holds (the lazy deferred path, shared with plain
    /// solves); that point is itself a deterministic function of the solve,
    /// and the reseed fixed-point iteration (design doc 5.7) verifies that a
    /// replay re-fires them identically before returning. A runtime that
    /// resolves the provider's futures in a nondeterministic order would make
    /// the registration order, and therefore the cell partition,
    /// nondeterministic; for reproducible solutions (e.g. lockfiles) drive
    /// `solve_universal` with a deterministic runtime.
    #[allow(clippy::type_complexity)]
    pub fn solve_universal(
        &mut self,
        problem: UniversalProblem<D::SolvableId, D::NameId>,
    ) -> Result<UniversalSolution<D::SolvableId, D::NameId>, UniversalFailure<D::NameId>> {
        let result = self.solve_universal_impl(problem);
        // Report after every outcome (mirroring `solve`). Note: a rebuild
        // after an abandoned trail-reuse attempt resets the solver state, so
        // the report covers only the final enumeration.
        #[cfg(feature = "diagnostics")]
        self.report_diagnostics();
        result
    }

    /// Returns the number of enumeration passes the most recent
    /// [`Self::solve_universal`] call performed: `1` for an unseeded solve
    /// (a single enumeration) and, for a seeded solve, the number of reseed
    /// fixed-point rounds it iterated before returning (design doc 5.7). A
    /// pass whose trail-reuse attempt was abandoned and re-enumerated from
    /// scratch still counts as one pass. Returns `0` when
    /// [`Self::solve_universal`] was never called or rejected its input
    /// before enumerating.
    ///
    /// This is a measurement observation point (benchmarks and tests assert
    /// on the cost of the reseed flow); it carries no solver semantics.
    pub fn universal_enumeration_passes(&self) -> u32 {
        self.universal_passes
    }

    #[allow(clippy::type_complexity)]
    fn solve_universal_impl(
        &mut self,
        problem: UniversalProblem<D::SolvableId, D::NameId>,
    ) -> Result<UniversalSolution<D::SolvableId, D::NameId>, UniversalFailure<D::NameId>> {
        let UniversalProblem {
            requirements,
            constraints,
            environment_model,
            seed_partition,
            _marker,
        } = problem;

        // Reject structurally invalid inputs before touching solver state, so
        // caller errors surface as `Err(InvalidInput(..))` rather than as a
        // panic buried in the encode/seed path. A valid problem that is merely
        // unsolvable is still reported by the enumeration below.
        self.universal_passes = 0;
        self.validate_universal_input(&requirements, &environment_model, &seed_partition)
            .map_err(UniversalFailure::InvalidInput)?;

        // A seeded solve must return a reseed fixed point: enumerating with
        // the returned cells as seeds reproduces them byte-identically (the
        // M4 seeding contract, design doc 5.7). One enumeration pass does
        // not guarantee this by itself: a recorded cell is shaped by
        // transient per-run search state (learnt clauses and activity from
        // earlier cells of the same run, kept trail prefixes of the free
        // phase), so replaying its condition from the cleaner state of the
        // next run can legitimately extract a different, healthier cell.
        // Iterate the enumeration on its own output until a pass reproduces
        // its seed list; each pass starts from a fresh solver state, so the
        // pass that confirms stability runs under exactly the conditions the
        // caller's next reseed would. An unseeded solve returns its first
        // enumeration unchanged (its healing round is the caller's first
        // reseed, which the design documents).
        //
        // Whether the solver has ever fetched from the provider is part of
        // those conditions: cache warmth gates when the encoder's futures
        // complete and thereby the clause registration order (a cache hit
        // resolves synchronously, a miss completes whenever the provider
        // does), so the convergence check below distinguishes a solver that
        // was verifiably cold at entry from one carrying earlier fetches.
        let cold_at_entry = self.cache.fetch_count() == 0;
        let mut seeds = seed_partition;
        let confirming = !seeds.is_empty();
        let mut rounds = 0;
        let mut inputs_tried: Vec<Vec<CellCondition<D::NameId>>> = Vec::new();
        loop {
            let fetches_before = self.cache.fetch_count();
            rounds += 1;
            self.universal_passes = rounds;
            let solution = self.enumerate_universal_with_fallback(
                &requirements,
                &constraints,
                &environment_model,
                &seeds,
            )?;
            if !confirming {
                return Ok(solution);
            }
            let output: Vec<CellCondition<D::NameId>> = solution
                .cells()
                .iter()
                .map(|cell| cell.condition().clone())
                .collect();
            // Convergence requires a pass that reproduced its seed list AND
            // that the identical later call the fixed point promises would
            // replay exactly. Two cases qualify:
            //
            //  - A pass that learned nothing new from the provider: cached
            //    dependencies gate the encoder's eager cascade (and thereby
            //    the clause registration order), so a pass over a saturated
            //    cache is replayed exactly by an identical later call on
            //    this (equally saturated) solver.
            //
            //  - The first pass of a solver that was verifiably cold at
            //    entry (zero fetches ever, so the cache holds nothing a
            //    fresh one would not). The caller this case serves is a
            //    fresh process re-resolving against a lockfile: its replay
            //    constructs a fresh solver, starts from the same empty
            //    cache, and runs the identical deterministic enumeration,
            //    fetches included (deterministic provider and runtime, per
            //    the "Reproducibility" contract of `solve_universal`).
            //    Demanding a saturated-cache confirmation here would rerun
            //    a byte-identical enumeration just to watch it fetch
            //    nothing, doubling the cost of every cold seeded solve --
            //    the primary lockfile flow (measured 2.19x a warm seeded
            //    solve on the conda-forge corpus).
            let cache_grew = self.cache.fetch_count() != fetches_before;
            let replayable = !cache_grew || (rounds == 1 && cold_at_entry);
            if replayable && output == seeds {
                return Ok(solution);
            }
            if !cache_grew {
                // An output equal to an earlier input closes an orbit with
                // no fixed point on it (possible for seed lists that no
                // enumeration produced, e.g. reordered ones; an in-order
                // reseed heals towards stability instead). Iterating further
                // would only walk the cycle; return the last pass, which is
                // still a verified disjoint cover.
                if inputs_tried.contains(&output) {
                    prop_hit!(RESEED_ORBIT_CLOSED);
                    tracing::debug!(
                        "reseed iteration closed a cycle after {rounds} passes without \
                         finding a fixed point; returning the last enumeration"
                    );
                    return Ok(solution);
                }
            }
            if rounds >= RESEED_FIXED_POINT_MAX_ROUNDS {
                tracing::debug!(
                    "reseed iteration did not converge within {rounds} passes; returning \
                     the last enumeration"
                );
                return Ok(solution);
            }
            inputs_tried.push(std::mem::replace(&mut seeds, output));
        }
    }

    /// One full enumeration: first with trail-prefix preservation; when a
    /// prefix-started run exceeds its work budget the attempt is abandoned
    /// wholesale (the solver state shaped by reused transitions performs
    /// badly under real search and is not repairable in place) and the
    /// enumeration re-runs from a fresh state with reuse disabled, seeded
    /// by the cells the abandoned attempt already recorded (followed by its
    /// unprocessed original seeds, see
    /// [`EnumerationOutcome::ReuseAbandoned`]) so the coverage work spent
    /// before the abort is replayed as cheap assumption-driven solves
    /// instead of being re-searched from zero. The wasted attempt is
    /// bounded by the work budgets; the fallback never aborts, so at most
    /// one rebuild happens.
    #[allow(clippy::type_complexity)]
    fn enumerate_universal_with_fallback(
        &mut self,
        requirements: &[ConditionalRequirement],
        constraints: &[VersionSetId],
        environment_model: &EnvironmentModel<D::NameId>,
        seed_partition: &[CellCondition<D::NameId>],
    ) -> Result<UniversalSolution<D::SolvableId, D::NameId>, UniversalFailure<D::NameId>> {
        match self.enumerate_universal(
            requirements.to_vec(),
            constraints.to_vec(),
            environment_model.clone(),
            seed_partition.to_vec(),
            true,
        )? {
            EnumerationOutcome::Done(solution) => Ok(solution),
            EnumerationOutcome::ReuseAbandoned(retry_seeds) => {
                prop_hit!(UNIVERSAL_REUSE_ABANDONED);
                tracing::debug!(
                    "trail reuse exceeded its work budget; re-enumerating without it, \
                     seeded by the {} cells found so far (plus unprocessed seeds)",
                    retry_seeds.len(),
                );
                match self.enumerate_universal(
                    requirements.to_vec(),
                    constraints.to_vec(),
                    environment_model.clone(),
                    retry_seeds,
                    false,
                )? {
                    EnumerationOutcome::Done(solution) => Ok(solution),
                    EnumerationOutcome::ReuseAbandoned(_) => {
                        unreachable!("the budget is never armed when trail reuse is disabled")
                    }
                }
            }
        }
    }

    /// Validates the environment model and seed partition against the cheap
    /// structural invariants of a [`UniversalProblem`], using only the
    /// (synchronous) provider classification. Runs before any solver state is
    /// built so caller errors surface through the [`Result`] channel; once it
    /// returns `Ok`, the corresponding checks in the encode and seed paths are
    /// internal invariants (`debug_assert!`) that this pass has already
    /// enforced.
    ///
    /// A structurally valid but stale seed is accepted here: this pass only
    /// rejects literals that can never denote a region of the environment
    /// space, not seeds that merely no longer match a solution (those heal
    /// during enumeration).
    fn validate_universal_input(
        &self,
        requirements: &[ConditionalRequirement],
        environment_model: &EnvironmentModel<D::NameId>,
        seed_partition: &[CellCondition<D::NameId>],
    ) -> Result<(), InvalidUniversalInput<D::NameId>> {
        for requirement in requirements {
            self.validate_requirement(&requirement.requirement)?;
        }
        for clause in environment_model.clauses() {
            if clause.is_empty() {
                return Err(InvalidUniversalInput::EmptyModelDisjunction);
            }
            for signed in clause.literals() {
                self.validate_env_literal(&signed.literal, EnvInputSource::EnvironmentModel)?;
            }
        }
        for seed in seed_partition {
            for signed in seed.literals() {
                self.validate_env_literal(&signed.literal, EnvInputSource::SeedPartition)?;
            }
        }
        Ok(())
    }

    /// Validates caller-supplied root requirements before solving. A union may
    /// combine multiple concrete packages or multiple environment packages, but
    /// not both: concrete candidates and symbolic environment literals cannot
    /// share a single candidate list.
    fn validate_requirement(
        &self,
        requirement: &Requirement,
    ) -> Result<(), InvalidUniversalInput<D::NameId>> {
        let mut has_environment = false;
        let mut has_concrete = false;
        for version_set in requirement.version_sets(self.provider()) {
            let package = self.provider().version_set_name(version_set);
            if self.provider().environment_package(package).is_some() {
                has_environment = true;
            } else {
                has_concrete = true;
            }
            if has_environment && has_concrete {
                return Err(
                    InvalidUniversalInput::MixedEnvironmentAndConcreteRequirement {
                        requirement: requirement.display(self.provider()).to_string(),
                    },
                );
            }
        }
        Ok(())
    }

    /// Validates a single environment literal: its package must classify as an
    /// environment package and an [`Absent`](EnvLiteral::Absent) literal
    /// requires the package to be declared `can_be_absent: true`. A
    /// [`Matches`](EnvLiteral::Matches) literal derives its package from the
    /// version set, so it can never reference the wrong package. `source` names
    /// the input for the error.
    fn validate_env_literal(
        &self,
        literal: &EnvLiteral<D::NameId>,
        source: EnvInputSource,
    ) -> Result<(), InvalidUniversalInput<D::NameId>> {
        match *literal {
            EnvLiteral::Matches(version_set) => {
                // A matches literal derives its package from the version set, so
                // it can only ever reference that package; the previously
                // possible package/version-set mismatch is now unrepresentable.
                let package = self.provider().version_set_name(version_set);
                if self.provider().environment_package(package).is_none() {
                    return Err(InvalidUniversalInput::NotAnEnvironmentPackage { source, package });
                }
            }
            EnvLiteral::Absent(package) => match self.provider().environment_package(package) {
                None => {
                    return Err(InvalidUniversalInput::NotAnEnvironmentPackage { source, package });
                }
                Some(env_pkg) if !env_pkg.can_be_absent => {
                    return Err(InvalidUniversalInput::AbsentLiteralForPresentPackage {
                        source,
                        package,
                    });
                }
                Some(_) => {}
            },
        }
        Ok(())
    }

    /// One enumeration pass over the environment model: the body of
    /// [`Self::solve_universal`]. With `reuse_trail` the free phase keeps
    /// the trail prefix that does not falsify each new blocking clause;
    /// without it every cell restarts from a fully retracted trail.
    #[allow(clippy::type_complexity)]
    fn enumerate_universal(
        &mut self,
        requirements: Vec<ConditionalRequirement>,
        constraints: Vec<VersionSetId>,
        environment_model: EnvironmentModel<D::NameId>,
        seed_partition: Vec<CellCondition<D::NameId>>,
        reuse_trail: bool,
    ) -> Result<EnumerationOutcome<D::SolvableId, D::NameId>, UniversalFailure<D::NameId>> {
        // Re-initialize the solver state, like `solve` does. One state is
        // shared across the whole enumeration loop: the formula only grows,
        // so learnt clauses and interned variables stay valid across cells.
        self.state = SolverState::default();

        // Install the environment hooks the shared solver internals consult.
        // They live on the cache (which outlives the per-solve state reset), so
        // a plain `solve` leaves them `None` and cannot observe environment
        // packages: the cache classifies every package as concrete and the
        // relation oracle is never queried. Reinstalling on every enumeration
        // pass is harmless and idempotent.
        self.cache
            .set_env_classify(<D as UniversalDependencyProvider>::environment_package);
        self.cache
            .set_env_relation(<D as UniversalDependencyProvider>::environment_version_set_relation);

        // Enable the universal-only bookkeeping (requires-clause capture
        // indexes) that cell-edge capture reads (see
        // `SolverState::universal_mode`). Must be set after the reset above.
        self.state.universal_mode = true;

        let root_dependencies = Dependencies::Known(KnownDependencies {
            requirements,
            constrains: constraints,
        });

        // The first clause must be the install-root clause (same invariant as
        // `solve`).
        let root_clause = {
            let (watched_literals, kind) = WatchedLiterals::root();
            self.state.add_clause(watched_literals, kind)
        };
        assert_eq!(root_clause, ClauseId::install_root());

        // Encode the environment model before the first solve so that the
        // model bounds every enumerated cell and the oracle consistency
        // clauses between model literals and later requirement literals are
        // emitted on first interning.
        self.encode_environment_model(&environment_model)?;

        let mut cells: Vec<Cell<D::SolvableId, D::NameId>> = Vec::new();
        // The same cells in solver variable space, used for disjointness
        // checks against new cells, plus the inverted literal index the
        // disjointness repair uses to skip provably disjoint pairs.
        let mut cell_assignments: Vec<Vec<(VariableId, bool)>> = Vec::new();
        let mut cell_literal_index = CellLiteralIndex::default();

        // Cells from a previous solve are solved first, in order, each under
        // its condition pushed as assumption decisions (design doc 5.7). When
        // the seeds run out the loop continues as free enumeration.
        let mut pending_seeds = seed_partition.into_iter();

        // The uncovered environment region the witness probe most recently
        // escalated to (see the `WitnessProbeTripped` arm below): the next
        // iteration solves it under assumption decisions like a seeded
        // cell, except that `Ok(false)` is then an unsolvability verdict
        // rather than a droppable stale seed. `None` during normal seed
        // processing and free enumeration.
        let mut pending_witness: Option<Vec<(VariableId, bool)>> = None;

        // Snapshot of the propagated-decision counter at the previous cell
        // recording, used to attribute propagation work to individual cells.
        #[cfg(feature = "diagnostics")]
        let mut decisions_at_last_cell = 0u64;

        // Whether the from-scratch solve cost that calibrates the kept-prefix
        // work budget has been recorded yet (the first recorded cell always
        // runs from a fresh or fully retracted trail).
        let mut fresh_cost_recorded = false;

        loop {
            // Set up the assumptions of the witness region the probe most
            // recently escalated to, if any (see the `WitnessProbeTripped`
            // arm below), or of the next viable seed, if any remain. Seeds
            // that contribute no assumptions are skipped: an empty
            // condition describes the whole environment space (which free
            // enumeration covers anyway) and a self-contradictory condition
            // describes no environment at all.
            let mut seeded = false;
            let mut probe_suppressed = false;
            let active_witness = pending_witness.take();
            if let Some(witness) = &active_witness {
                // The witness is a consistent assignment over already
                // interned environment variables, so pushing it through the
                // seeded-cell machinery can only fail when it is EMPTY:
                // nothing constrains the environment space yet (no model
                // clauses, no recorded cells) and the uncovered "region" is
                // the whole space. Solving that region under assumptions IS
                // the free episode that just tripped, so let the free
                // episode run to its natural end this once instead of
                // re-arming the probe, which would only reproduce the same
                // empty witness.
                let condition = self.cell_to_condition(witness);
                seeded = self.push_seed_assumptions(&condition)?;
                probe_suppressed = !seeded;
            }
            let witness_directed = seeded;
            while !seeded {
                let Some(seed) = pending_seeds.next() else {
                    break;
                };
                seeded = self.push_seed_assumptions(&seed)?;
            }

            // Arm the witness probe for a free episode (see
            // `WITNESS_PROBE_BUDGET`). Seeded and witness-directed episodes
            // never arm it: they are the probe's own escape hatch and must
            // run to their Ok(true)/Ok(false) conclusion (a witness-directed
            // episode interrupted by its own probe could make no progress),
            // and the budget applying only to the free phase is what keeps
            // seeded replays -- the reseed fixed-point contract of design
            // doc 5.7 -- untouched. The deadline is a deterministic
            // function of the (deterministic) propagation count, so
            // partitions stay deterministic.
            self.state.arm_witness_probe(if seeded || probe_suppressed {
                None
            } else {
                self.witness_probe_budget()
            });

            // Assumption decisions of a seeded cell are preserved prior
            // state (unsolvable surfaces as `Ok(false)`); a trail prefix
            // kept by the free phase is restartable scratch state above
            // `starting_level = 0`.
            let sat_result = self.run_sat(
                SolvableIdOrRoot::root(),
                &root_dependencies,
                self.state.assumption_levels,
            );
            // Disarm unconditionally: no later `run_sat` call (the
            // witness-directed solve of the next iteration, the
            // scoped-conflict re-solve, a concrete solve on this solver)
            // may observe a stale deadline.
            self.state.arm_witness_probe(None);

            match sat_result {
                Ok(true) => {
                    // Extract the load-bearing environment literal
                    // assignments and restore provable disjointness against
                    // all previously recorded cells. For a seeded cell the
                    // extraction may produce a condition that is MORE GENERAL
                    // than the seed: the recorded cell reflects what the new
                    // solution actually depends on, so a stale over-specific
                    // seed heals (the repair step still re-specializes where
                    // needed to keep cells disjoint).
                    // The capture inputs are shared by cell extraction and
                    // edge capture; nothing below touches the trail until
                    // the retraction after the edges are captured.
                    let capture_inputs = self.collect_cell_capture_inputs();
                    let (mut cell, mut pins) = self.extract_cell(&capture_inputs);
                    self.repair_disjointness(
                        &mut cell,
                        &cell_assignments,
                        &cell_literal_index,
                        &mut pins,
                    );
                    #[cfg(feature = "diagnostics")]
                    self.state.propagation_counters.cell_pins.push(pins);

                    let condition = self.cell_to_condition(&cell);
                    let solvables = self.chosen_solvables_canonical();
                    // Capture the edges while the cell's assignment is still on
                    // the decision stack, then bundle everything into one cell.
                    let edges = self.capture_cell_edges(&capture_inputs);
                    let cell_is_empty = cell.is_empty();
                    cells.push(Cell::new(condition, solvables, edges));
                    for &(variable, value) in &cell {
                        cell_literal_index.record(variable, value, cell_assignments.len());
                    }
                    cell_assignments.push(cell.clone());

                    #[cfg(feature = "diagnostics")]
                    {
                        let total = self.state.propagation_counters.decisions_propagated;
                        self.state
                            .propagation_counters
                            .cell_decisions
                            .push(total - decisions_at_last_cell);
                        decisions_at_last_cell = total;
                    }

                    if !fresh_cost_recorded {
                        fresh_cost_recorded = true;
                        self.state.record_fresh_solve_cost();
                    }
                    self.state.extend_prefix_budget(cells.len());

                    if cell_is_empty {
                        // No environment literal was load bearing: the
                        // solution is valid in every environment, so coverage
                        // is complete. (An empty cell can only be the first
                        // cell; the repair step extends any later cell.)
                        break;
                    }

                    // Retract the trail only as far as needed for the new
                    // blocking clause to no longer be falsified, and continue
                    // the next solve from the surviving prefix (trail-prefix
                    // preservation, see docs/design/universal-trail-reuse.md).
                    // The kept prefix is ordinary backtrackable state, not an
                    // assumption: later conflicts may backjump through it. A
                    // seeded cell still retracts fully: its trail starts with
                    // assumption decisions, which must never leak into the
                    // next solve.
                    let retract_target = if self.state.assumption_levels > 0 || !reuse_trail {
                        0
                    } else {
                        let target = self.blocking_clause_retract_target(&cell);
                        // Trail reshape: when the retraction would pop more
                        // than a handful of ordinary decision levels, the
                        // kept prefix has env-sensitive state buried under
                        // env-independent packages (the shape the trail was
                        // given before the env-literals-last knowledge
                        // existed). A partial retraction would re-derive
                        // that suffix on EVERY later transition and the
                        // shape would never heal, because the cascade of
                        // the new blocking clause re-assigns the env
                        // literals at the bottom of the re-derived range.
                        // Retract fully instead: the rebuild orders the
                        // trail with the current knowledge (env literals on
                        // top), which collapses subsequent transitions to
                        // the env tail. With a well-shaped trail only env
                        // literals and env-sensitive parents sit above the
                        // target, so this triggers at most once per
                        // knowledge change.
                        if self.state.env_ordering_active
                            && self.ordinary_levels_above(target) > TRAIL_RESHAPE_ORDINARY_LEVELS
                        {
                            prop_hit!(TRAIL_RESHAPE_FULL_RETRACT);
                            0
                        } else {
                            target
                        }
                    };

                    // Observe the retract target relative to the trail depth
                    // (diagnostics): how much of the trail each transition
                    // keeps is the load-bearing metric of the
                    // env-literals-last decision ordering.
                    #[cfg(feature = "diagnostics")]
                    if self.state.assumption_levels == 0 && reuse_trail {
                        let depth = self.state.decision_tracker.deepest_level();
                        self.state
                            .propagation_counters
                            .cell_retracts
                            .push(CellRetract {
                                target: retract_target.min(depth),
                                depth,
                            });
                    }

                    self.state.assumption_levels = 0;
                    self.state.decision_tracker.undo_until(retract_target);

                    // Block the recorded cell: the disjunction of the
                    // negations of the cell's signed literals. A cell literal
                    // `(var, value)` is negated by `Literal::new(var, value)`
                    // because the `negate` flag of a literal equals the value
                    // it forbids the variable to take here. Blocking clauses
                    // from seeded cells constrain later seeds and the free
                    // phase exactly like free-phase blocking clauses do.
                    let blocking = cell
                        .iter()
                        .map(|&(variable, value)| Literal::new(variable, value))
                        .collect();
                    self.state.add_env_clause(blocking, EnvClauseKind::Blocking);
                }
                Ok(false) if witness_directed => {
                    // The probe-escalated witness region is unsolvable: the
                    // whole universal solve fails NOW, reported exactly as
                    // the terminal path below reports it after exhaustive
                    // coverage.
                    //
                    // SOUNDNESS: the witness satisfies the environment
                    // model, every oracle consistency clause and every
                    // blocking clause (it is a model of exactly that clause
                    // set, see `find_environment_witness`), so any solution
                    // whose environment extends the witness would have
                    // survived all of them and been found by this
                    // assumption-pinned solve. `Ok(false)` under exactly
                    // the witness assumptions therefore proves that no
                    // solution exists anywhere in the witness region, and
                    // the environment model is total (every modeled
                    // environment must be covered), so an uncoverable
                    // region fails the whole solve.
                    let witness = active_witness
                        .expect("bug: a witness-directed solve holds the active witness");
                    prop_hit!(WITNESS_PROBE_VERDICT);
                    let scoped_conflict =
                        self.run_scoped_conflict(&witness, None, &root_dependencies)?;
                    return Err(UniversalFailure::Unsolvable {
                        cell: self.cell_to_condition(&witness),
                        conflict: scoped_conflict,
                    });
                }
                Ok(false) => {
                    // The seeded cell is unsolvable AS SEEDED (`run_sat`
                    // surfaces every unsolvable outcome as `Ok(false)` while
                    // prior decisions, here the assumptions, exist). This is
                    // not a global conflict: drop the seed, retract its
                    // assumptions and continue; the region it described is
                    // covered by free enumeration (and if it is genuinely
                    // unsolvable inside the model, the witness check fails
                    // the solve with a proper conflict later).
                    debug_assert!(
                        seeded,
                        "bug: run_sat only returns Ok(false) when prior decisions exist, \
                         which in a universal solve means assumptions were pushed"
                    );
                    self.state.assumption_levels = 0;
                    self.state.decision_tracker.undo_until(0);
                }
                Err(UnsolvableOrCancelled::Cancelled(value)) => {
                    // A prefix-started run that exceeded its conflict budget
                    // aborts the whole trail-reuse attempt; the caller
                    // re-enumerates from scratch with the cells found so far
                    // as seeds. Real cancellations pass through.
                    if value.downcast_ref::<PrefixBudgetExhausted>().is_some() {
                        debug_assert!(
                            reuse_trail,
                            "bug: the prefix budget is never armed without trail reuse"
                        );
                        // Carry the abandoned attempt's coverage work into
                        // the retry: the recorded cells (in order), then any
                        // original seeds not yet processed at the abort (the
                        // prefix budget only arms after the seeds ran out,
                        // so the leftover is normally empty; chaining keeps
                        // the contract exact either way). See
                        // `EnumerationOutcome::ReuseAbandoned`.
                        let retry_seeds = cells
                            .iter()
                            .map(|cell| cell.condition().clone())
                            .chain(pending_seeds)
                            .collect();
                        return Ok(EnumerationOutcome::ReuseAbandoned(retry_seeds));
                    }
                    // A free episode that exceeded the witness-probe budget
                    // (see `WITNESS_PROBE_BUDGET`) escalates to the
                    // coverage witness search -- the same model + oracle +
                    // blocking clause set that decides coverage termination
                    // -- instead of wandering on. Each trip makes bounded
                    // progress: the witness-directed solve of the next
                    // iteration either records a cell (whose blocking
                    // clause strictly shrinks the witness space) or proves
                    // the region uncoverable and ends the whole solve, so
                    // trips cannot livelock.
                    if value.downcast_ref::<WitnessProbeTripped>().is_some() {
                        debug_assert!(
                            !seeded,
                            "bug: the witness probe is never armed under assumptions"
                        );
                        // Retract the abandoned episode entirely; the next
                        // iteration starts from a clean stack (the witness
                        // assumptions require one, and a fully retracted
                        // trail never falsifies the next blocking clause).
                        self.state.assumption_levels = 0;
                        self.state.decision_tracker.undo_until(0);
                        match self.find_environment_witness() {
                            None => {
                                // No environment assignment satisfies the
                                // model, oracle and blocking clauses: the
                                // recorded cells already cover the entire
                                // model and the abandoned episode was the
                                // (expensive) final refutation. Terminate
                                // exactly like the terminal path below.
                                prop_hit!(WITNESS_PROBE_COVERAGE_BREAK);
                                break;
                            }
                            Some(witness) => {
                                // An uncovered region exists: solve it
                                // directly under assumptions on the next
                                // iteration.
                                prop_hit!(WITNESS_PROBE_ESCALATED);
                                pending_witness = Some(witness);
                                continue;
                            }
                        }
                    }
                    return Err(UniversalFailure::Cancelled(value));
                }
                Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
                    // The formula is unsolvable. This only happens in the
                    // free phase (a seeded solve reports `Ok(false)`); check
                    // whether any region of the environment model remains
                    // uncovered.
                    debug_assert!(
                        !seeded,
                        "bug: a seeded solve surfaces unsolvability as Ok(false)"
                    );
                    match self.find_environment_witness() {
                        None => {
                            // No environment assignment satisfies the model,
                            // oracle and blocking clauses: the recorded cells
                            // cover the entire model.
                            break;
                        }
                        Some(witness) => {
                            // An uncovered region exists and is unsolvable:
                            // the whole universal solve fails (the model is
                            // total).
                            //
                            // Scope the conflict to the witness region by
                            // re-solving with unit Model clauses that pin each
                            // witness literal to its value. The scoped re-solve
                            // must also be UNSAT (the region is unsolvable),
                            // and its conflict is scoped to exactly the clauses
                            // that fire in that region.
                            let scoped_conflict = self.run_scoped_conflict(
                                &witness,
                                Some(conflict),
                                &root_dependencies,
                            )?;
                            return Err(UniversalFailure::Unsolvable {
                                cell: self.cell_to_condition(&witness),
                                conflict: scoped_conflict,
                            });
                        }
                    }
                }
            }
        }

        Ok(EnumerationOutcome::Done(UniversalSolution {
            cells,
            environment_model,
        }))
    }

    /// Returns, for each cell recorded by the last [`Self::solve_universal`]
    /// call, the number of decisions propagated while solving that cell.
    ///
    /// This is a diagnostics-only observation point: tests use it to verify
    /// that trail-prefix preservation makes later cells cheaper than a
    /// restart-from-scratch enumeration would.
    #[cfg(feature = "diagnostics")]
    pub fn universal_cell_decisions(&self) -> &[u64] {
        &self.state.propagation_counters.cell_decisions
    }

    /// Returns, for each free-phase cell recorded with trail reuse by the
    /// last [`Self::solve_universal`] call, the retract target chosen before
    /// adding the cell's blocking clause (clamped to the trail depth) and
    /// the trail depth at that point.
    ///
    /// This is a diagnostics-only observation point: tests use it to verify
    /// that the env-literals-last decision ordering keeps the retract target
    /// within a few levels of the trail depth, which is what makes trail
    /// reuse collapse per-cell costs.
    #[cfg(feature = "diagnostics")]
    pub fn universal_cell_retracts(&self) -> &[CellRetract] {
        &self.state.propagation_counters.cell_retracts
    }

    /// Returns, for each cell recorded by the last [`Self::solve_universal`]
    /// call, the number of cell literals each pinning rule contributed (see
    /// [`CellPinCounts`]). The entries are parallel to the solution's cells;
    /// each entry's [`CellPinCounts::total`] equals the cell's condition
    /// length.
    ///
    /// This is a diagnostics-only observation point: it attributes condition
    /// fragmentation to load-bearing extraction versus disjointness-repair
    /// appends when investigating high-cell-count outliers.
    #[cfg(feature = "diagnostics")]
    pub fn universal_cell_pins(&self) -> &[CellPinCounts] {
        &self.state.propagation_counters.cell_pins
    }

    /// Performs a scoped re-solve to produce a conflict that is tightly
    /// bound to the witness region identified by `witness`.
    ///
    /// The witness region is known to be unsolvable: either the whole
    /// formula just went UNSAT (the exhaustion path, which passes the
    /// unscoped conflict as `fallback`), or a witness-directed solve just
    /// returned `Ok(false)` under exactly the witness assumptions (the
    /// witness-probe path, which has no unscoped conflict and passes
    /// `None`). Adding unit [`EnvClauseKind::Model`] clauses that pin each
    /// witness literal to its witnessed value keeps the formula UNSAT while
    /// scoping the CDCL to the specific region. The resulting conflict
    /// graph will only reference clauses that matter in that region.
    ///
    /// If for any reason the re-solve produces `Ok` (which should not happen
    /// for a well-formed witness), the `fallback` conflict is returned
    /// unchanged; without a fallback that outcome would contradict the
    /// `Ok(false)` the same pinned assignment just produced, so it is a
    /// solver bug. A provider cancellation during the re-solve falls back
    /// to the unscoped conflict when one exists (preserving the historical
    /// behavior of the exhaustion path) and surfaces as
    /// [`UniversalFailure::Cancelled`] otherwise.
    fn run_scoped_conflict(
        &mut self,
        witness: &[(VariableId, bool)],
        fallback: Option<Conflict>,
        root_dependencies: &Dependencies,
    ) -> Result<Conflict, UniversalFailure<D::NameId>> {
        // Undo all decisions so that add_env_clause can initialize its
        // watch lists on a clean decision stack.
        self.state.assumption_levels = 0;
        self.state.decision_tracker.undo_until(0);

        // Add each witness literal as a unit Model clause. Unit clauses are
        // re-asserted on every propagation pass (mirroring single-literal
        // learnt clauses), so they will immediately force the variable.
        for &(variable, value) in witness {
            // A unit clause [L] forces L to be true. For a positive literal
            // (value = true) L is satisfied iff variable = true, so
            // negate = false. For a negative literal (value = false) L is
            // satisfied iff variable = false, so negate = true.
            let literal = Literal::new(variable, !value);
            self.state
                .add_env_clause(vec![literal], EnvClauseKind::Model);
        }

        // Re-solve. The formula is UNSAT in this region, so we expect an
        // Unsolvable result.
        match self.run_sat(SolvableIdOrRoot::root(), root_dependencies, 0) {
            Err(UnsolvableOrCancelled::Unsolvable(scoped)) => Ok(scoped),
            Err(UnsolvableOrCancelled::Cancelled(value)) => match fallback {
                Some(conflict) => Ok(conflict),
                None => Err(UniversalFailure::Cancelled(value)),
            },
            // Unexpected: fall back to the unscoped conflict.
            Ok(_) => Ok(fallback.unwrap_or_else(|| {
                unreachable!(
                    "bug: the scoped re-solve found a solution in a region a \
                     witness-directed solve just proved unsolvable under the same pinned \
                     assignment"
                )
            })),
        }
    }

    /// Returns the solvables chosen by the current solution in canonical
    /// order: by variable id, which is the (deterministic, encoding-driven)
    /// interning order. The decision stack order is NOT used because it
    /// depends on unit-propagation internals (watchlist traversal order),
    /// which legitimately differ between a free and a seeded solve of the
    /// same cell; the canonical order makes re-solves byte-identical.
    fn chosen_solvables_canonical(&self) -> Vec<D::SolvableId> {
        let mut chosen: Vec<(VariableId, D::SolvableId)> = self
            .state
            .decision_tracker
            .stack()
            .filter(|decision| decision.value)
            .filter_map(|decision| {
                decision
                    .variable
                    .as_solvable(&self.state.variable_map)
                    .map(|solvable| (decision.variable, solvable))
            })
            .collect();
        chosen.sort_by_key(|&(variable, _)| variable.to_index());
        chosen.into_iter().map(|(_, solvable)| solvable).collect()
    }

    /// Collects, in one trail scan, the per-cell capture inputs of
    /// [`Self::extract_cell`] and [`Self::capture_cell_edges`]: the clauses
    /// and requires entries of the INSTALLED parents only, instead of every
    /// registered entry per cell.
    ///
    /// - `requires_parents`: the registration indices (into
    ///   `SolverState::requires_clauses`) of the installed parents
    ///   (variables assigned true) that have requires entries, sorted
    ///   ascending. Visiting these entries, each with its clause list in
    ///   insertion order, reproduces exactly the visit order of iterating
    ///   the whole map and skipping parents that are not installed.
    /// - `support_clauses`: the support clauses
    ///   (`SolverState::env_support_clauses`) whose parent is installed, in
    ///   clause registration order. The per-parent lists are ascending in
    ///   clause id (clauses are appended at creation), so a global sort by
    ///   clause id restores exactly the order in which a full
    ///   registration-order scan visits the clauses it does not skip.
    ///
    /// Uninstalled parents cannot contribute: a clause whose parent is not
    /// assigned true is satisfied by `not parent` in every environment, and
    /// both consumers skip exactly those entries. The trail is scanned with
    /// one dense array read per assignment (see
    /// `SolverState::cell_capture_index`).
    ///
    /// Must be called while the solution's decisions are still on the
    /// decision stack; the result stays valid only while the trail is
    /// untouched.
    fn collect_cell_capture_inputs(&self) -> CellCaptureInputs {
        let state = &self.state;
        let mut requires_parents: Vec<u32> = Vec::new();
        let mut support_clauses: Vec<ClauseId> = Vec::new();
        for decision in state.decision_tracker.stack() {
            if !decision.value {
                continue;
            }
            // Both dense indices are stored offset by one; 0 = none.
            let (parent_index, support_index) = state.cell_capture_index.get(decision.variable);
            if parent_index != 0 {
                requires_parents.push(parent_index - 1);
            }
            if support_index != 0 {
                support_clauses
                    .extend_from_slice(&state.env_support_clauses[support_index as usize - 1]);
            }
        }
        // The trail order is not the registration order; restore it. Every
        // variable occurs at most once on the trail and holds at most one
        // registration index, so the indices are unique.
        requires_parents.sort_unstable();
        support_clauses.sort_unstable_by_key(|clause_id| clause_id.to_index());
        CellCaptureInputs {
            requires_parents,
            support_clauses,
        }
    }

    /// Captures the dependency edges that are active in the current solution
    /// (design doc 5.8, "conditional edges"). Must be called while the
    /// solution's decisions are still on the decision stack.
    ///
    /// An edge exists for every requires clause whose parent (a solvable or
    /// the root) is installed and whose condition holds, where "holds" means
    /// every condition complement literal is ASSIGNED false (the same rule
    /// `decide()` uses to enforce the requirement; an undecided complement
    /// literal means the requirement was never active). The edge's target is
    /// the first
    /// installed candidate of the requirement, or `None` when the requirement
    /// is on an environment package (its candidate is an environment literal,
    /// not a solvable).
    fn capture_cell_edges(&self, inputs: &CellCaptureInputs) -> Vec<CellEdge<D::SolvableId>> {
        let state = &self.state;
        let decision_map = state.decision_tracker.map();
        let mut edges: Vec<CellEdge<D::SolvableId>> = Vec::new();
        // First-occurrence order, with an O(1) membership guard so the
        // per-cell dedup stays linear rather than O(edges^2).
        let mut seen: ahash::HashSet<CellEdge<D::SolvableId>> = ahash::HashSet::default();

        for &parent_index in &inputs.requires_parents {
            let (&parent_var, requirements) = state
                .requires_clauses
                .get_index(parent_index as usize)
                .expect("collect_cell_capture_inputs yields valid registration indices");
            let parent = match state.variable_map.origin(parent_var) {
                VariableOrigin::Root => None,
                VariableOrigin::Solvable(solvable) => Some(solvable),
                // Auxiliary variables (at-least-one trackers) re-encode
                // requirements that are already captured for their real
                // parent; they are not part of the dependency graph.
                _ => continue,
            };

            for (requirement, disjunction, _clause_id) in requirements {
                // The edge is active when the clause has no condition, or
                // when the condition holds: every complement literal of the
                // disjunction is ASSIGNED false. This is exactly the
                // eligibility rule of `decide()` (see
                // `DecideQueue::inspect`): a merely undecided complement
                // literal (e.g. all candidates of an untouched concrete
                // condition package) means the requirement was never
                // enforced, so no edge exists.
                if let Some(disjunction) = *disjunction {
                    let condition_holds = state.disjunctions[disjunction]
                        .literals
                        .iter()
                        .all(|literal| literal.eval(decision_map) == Some(false));
                    if !condition_holds {
                        continue;
                    }
                }

                // The target is the first candidate of the requirement that
                // is installed. An active requirement always has one: for a
                // concrete requirement propagation forces a candidate, and
                // for a requirement on an environment package the candidate
                // is the (propagated true) environment literal.
                let installed_candidate = state.requirement_to_sorted_candidates[*requirement]
                    .iter()
                    .flatten()
                    .find(|&&candidate| {
                        state.decision_tracker.assigned_value(candidate) == Some(true)
                    });
                let target = match installed_candidate {
                    Some(&candidate) => match state.variable_map.origin(candidate) {
                        VariableOrigin::Solvable(solvable) => Some(solvable),
                        VariableOrigin::EnvMatches(_) => None,
                        origin => unreachable!(
                            "requirement candidates are solvables or environment literals, \
                             not {origin:?}"
                        ),
                    },
                    None => {
                        debug_assert!(
                            false,
                            "bug: an active requirement of an installed parent has no \
                             installed candidate"
                        );
                        continue;
                    }
                };

                // A requirement with an OR condition produces one clause per
                // DNF disjunct; deduplicate so the edge is recorded once.
                let edge = CellEdge {
                    parent,
                    requirement: *requirement,
                    target,
                };
                if seen.insert(edge) {
                    edges.push(edge);
                }
            }
        }

        edges
    }

    /// Encodes the environment model CNF as [`Clause::EnvClause`] clauses.
    ///
    /// For every package referenced by a model literal the candidates are
    /// resolved to record its environment metadata in the solver state; the
    /// literal variables are interned together with their oracle consistency
    /// clauses. The structural invariants asserted below (non-empty
    /// disjunctions, environment-package classification, version-set/package
    /// agreement, `can_be_absent` for absent literals) were already enforced
    /// up front by [`Self::validate_universal_input`], so here they are
    /// internal invariants rather than caller-facing errors.
    fn encode_environment_model(
        &mut self,
        model: &EnvironmentModel<D::NameId>,
    ) -> Result<(), UniversalFailure<D::NameId>> {
        for clause in model.clauses() {
            debug_assert!(
                !clause.is_empty(),
                "invariant: empty model disjunctions are rejected by validate_universal_input"
            );

            let mut literals = Vec::with_capacity(clause.len());
            for signed in clause.literals() {
                let variable = match signed.literal {
                    EnvLiteral::Matches(version_set) => {
                        let package_name = self.cache.provider().version_set_name(version_set);
                        self.declare_environment_package(package_name, "environment model")?;
                        self.state.intern_env_matches_with_oracle_clauses(
                            &self.cache,
                            version_set,
                            package_name,
                        )
                    }
                    EnvLiteral::Absent(package_name) => {
                        let env_pkg =
                            self.declare_environment_package(package_name, "environment model")?;
                        debug_assert!(
                            env_pkg.can_be_absent,
                            "invariant: absent literals for can_be_absent: false packages are \
                             rejected by validate_universal_input"
                        );
                        self.state
                            .intern_env_absent_with_oracle_clauses(package_name)
                    }
                };
                // A positive model literal (`positive`, the public "holds"
                // convention) asserts the variable; a negative one asserts its
                // negation. This `!` is the sole boundary flip between the
                // public sign and the solver's internal `Literal` negate flag.
                literals.push(Literal::new(variable, !signed.positive));
            }
            self.state.add_env_clause(literals, EnvClauseKind::Model);
        }
        Ok(())
    }

    /// Resolves the candidates for `package_name` and records its environment
    /// metadata in the solver state (mirroring what the encoder does in
    /// `on_candidates_available`).
    ///
    /// `context` names the input that referenced the package (the environment
    /// model or the seed partition) for the internal-invariant message. The
    /// package is guaranteed to be an environment package: caller inputs are
    /// screened by [`Self::validate_universal_input`] before this runs, so the
    /// non-environment arm below is unreachable rather than a caller error.
    fn declare_environment_package(
        &mut self,
        package_name: D::NameId,
        context: &str,
    ) -> Result<EnvironmentPackage, UniversalFailure<D::NameId>> {
        let package_candidates = self
            .async_runtime
            .block_on(self.cache.get_or_cache_candidates(package_name))
            .map_err(UniversalFailure::Cancelled)?;
        let env_pkg = match package_candidates {
            PackageCandidates::Environment(env_pkg) => *env_pkg,
            PackageCandidates::Candidates(_) => unreachable!(
                "invariant: the {context} was screened by validate_universal_input, so package \
                 '{}' must be an environment package",
                self.provider().display_name(package_name),
            ),
        };
        self.state.env_packages.set(package_name, Some(env_pkg));
        Ok(env_pkg)
    }

    /// Validates one seed cell condition and pushes its literals as
    /// assumption decisions at levels `1..=n`, one level per literal (design
    /// doc 5.7), interning environment literal variables (and emitting their
    /// oracle consistency clauses) as needed: a seed may reference version
    /// sets no other input mentions. Returns whether at least one assumption
    /// is now active; `SolverState::assumption_levels` is set accordingly.
    ///
    /// Seeds that yield no assumptions are skipped with `Ok(false)`: an
    /// empty condition describes the whole environment space, which free
    /// enumeration covers anyway, and a condition that contradicts itself on
    /// a literal describes no environment at all (its partially pushed
    /// assumptions are retracted again).
    ///
    /// Structural validity of the seed literals (environment-package
    /// classification, version-set/package agreement, `can_be_absent` for
    /// absent literals) is enforced up front by
    /// [`Self::validate_universal_input`]; the assertions below are therefore
    /// internal invariants, not caller-facing errors.
    fn push_seed_assumptions(
        &mut self,
        seed: &CellCondition<D::NameId>,
    ) -> Result<bool, UniversalFailure<D::NameId>> {
        debug_assert_eq!(
            self.state.assumption_levels, 0,
            "bug: the previous seed's assumptions were not cleared"
        );
        debug_assert!(
            self.state.decision_tracker.stack().next().is_none(),
            "bug: assumptions must be pushed on an empty decision stack"
        );

        // Intern (and validate) every literal before pushing any decision:
        // interning emits oracle consistency clauses, whose watch
        // initialization assumes the involved variables are undecided.
        let mut assumptions = Vec::with_capacity(seed.len());
        for signed in seed.literals() {
            let variable = match signed.literal {
                EnvLiteral::Matches(version_set) => {
                    let package_name = self.cache.provider().version_set_name(version_set);
                    self.declare_environment_package(package_name, "seed partition")?;
                    self.state.intern_env_matches_with_oracle_clauses(
                        &self.cache,
                        version_set,
                        package_name,
                    )
                }
                EnvLiteral::Absent(package_name) => {
                    let env_pkg =
                        self.declare_environment_package(package_name, "seed partition")?;
                    debug_assert!(
                        env_pkg.can_be_absent,
                        "invariant: absent literals for can_be_absent: false packages are \
                         rejected by validate_universal_input"
                    );
                    self.state
                        .intern_env_absent_with_oracle_clauses(package_name)
                }
            };
            // The public `positive` sign is the assumption's decision value
            // directly (`true` = the literal holds); no negation flip here.
            assumptions.push((variable, signed.positive));
        }

        // Push each assumption as a decision at its own level, derived from
        // the assumption sentinel. A literal repeated with the same sign is
        // redundant and skipped (no level of its own); a literal repeated
        // with the opposite sign makes the seed self-contradictory.
        let mut level = 0u32;
        for (variable, value) in assumptions {
            match self.state.decision_tracker.try_add_decision(
                Decision::new(variable, value, ClauseId::assumption()),
                level + 1,
            ) {
                Ok(true) => level += 1,
                Ok(false) => {}
                Err(()) => {
                    self.state.decision_tracker.undo_until(0);
                    return Ok(false);
                }
            }
        }
        self.state.assumption_levels = level;
        Ok(level > 0)
    }

    /// Computes the cell for the current solution: a map from environment
    /// literal variables to exactly the load-bearing assignments, sorted by
    /// variable id for determinism (design doc 5.6).
    ///
    /// Scans the indexed support clauses
    /// (`SolverState::env_support_clauses`) for validity support, then all
    /// requires clauses of installed parents for steering pins (environment
    /// assignments that excluded a more-preferred candidate; see the
    /// steering block below). Oracle consistency, model and blocking clauses
    /// never contribute support: the former two are tautologies over the
    /// modeled environment space and the latter are handled by the
    /// disjointness repair. Learnt clauses are implied by the other clauses
    /// and never contribute support either (they can appear as reasons
    /// inside a steering cone).
    ///
    /// Also returns, per pinning rule, how many literals the rule
    /// contributed (a [`CellPinCounts`]; the disjointness repair adds its
    /// own counts afterwards).
    fn extract_cell(&self, inputs: &CellCaptureInputs) -> (Vec<(VariableId, bool)>, CellPinCounts) {
        let state = &self.state;
        let decision_map = state.decision_tracker.map();
        let mut cell: Vec<(VariableId, bool)> = Vec::new();
        let mut pins = CellPinCounts::default();

        let record =
            |cell: &mut Vec<(VariableId, bool)>, variable: VariableId, value: bool| -> bool {
                match cell.iter().find(|&&(v, _)| v == variable) {
                    Some(&(_, existing)) => {
                        debug_assert_eq!(
                            existing, value,
                            "bug: a cell literal was recorded with both signs"
                        );
                        false
                    }
                    None => {
                        cell.push((variable, value));
                        true
                    }
                }
            };

        // Only the clauses of installed parents can contribute support: a
        // clause whose parent is not installed is satisfied by `not parent`
        // in every environment. The inputs hold exactly those clauses, in
        // the registration order a full scan would visit them in.
        for &clause_id in &inputs.support_clauses {
            match state.clauses.kinds[clause_id.to_index()] {
                Clause::Requires(parent, disjunction, requirement) => {
                    debug_assert_eq!(
                        state.decision_tracker.assigned_value(parent),
                        Some(true),
                        "bug: env_support_clauses is keyed by the clause's parent"
                    );

                    // Some candidate installed: the requirement is satisfied
                    // regardless of the environment. If the satisfying
                    // candidate is itself an environment literal (requirement
                    // on an environment package), it was propagated true and
                    // is load bearing.
                    let mut satisfied = false;
                    for &candidate in state.requirement_to_sorted_candidates[requirement]
                        .iter()
                        .flatten()
                    {
                        if state.decision_tracker.assigned_value(candidate) == Some(true) {
                            if matches!(
                                state.variable_map.origin(candidate),
                                VariableOrigin::EnvMatches(_)
                            ) && record(&mut cell, candidate, true)
                            {
                                pins.req_env += 1;
                            }
                            satisfied = true;
                            break;
                        }
                    }
                    if satisfied {
                        // A conditional clause whose condition HOLDS shaped
                        // the solution: the installed candidate is only
                        // required where the condition's env guards are true,
                        // so those guards are load bearing and must pin the
                        // cell. Without this, a candidate installed under a
                        // kept trail prefix (trail-prefix preservation) could
                        // be claimed by a cell that extends into regions
                        // where its guard is false: still a valid solution,
                        // but needlessly installed there and not reproducible
                        // by a from-scratch re-solve of the cell.
                        if let Some(disjunction) = disjunction {
                            let literals = &state.disjunctions[disjunction].literals;
                            // "Holds" means every complement literal is
                            // ASSIGNED false, matching `decide()`; an
                            // undecided complement literal means the clause
                            // never enforced anything.
                            let condition_holds = literals
                                .iter()
                                .all(|literal| literal.eval(decision_map) == Some(false));
                            if condition_holds {
                                for literal in literals {
                                    if is_env_variable(state, literal.variable())
                                        && record(&mut cell, literal.variable(), true)
                                    {
                                        // The complement literal is false, so
                                        // the guard variable is assigned true.
                                        pins.guard += 1;
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    // Parent installed and no candidate installed: the clause
                    // is satisfied only by its condition complement
                    // disjunction.
                    let Some(disjunction) = disjunction else {
                        debug_assert!(
                            false,
                            "bug: an unconditional requires clause with an installed parent \
                             must have an installed candidate"
                        );
                        continue;
                    };
                    let literals = &state.disjunctions[disjunction].literals;

                    // If a concrete complement literal is not assigned false
                    // (it is assigned true, or merely undecided: e.g. all
                    // candidates of an untouched condition package), the
                    // condition can never fire in this solution regardless
                    // of the environment (`decide()` requires every
                    // complement literal to be ASSIGNED false), so the
                    // clause is inactive in every environment of the cell
                    // and needs no environment support.
                    let satisfied_by_concrete = literals.iter().any(|literal| {
                        !is_env_variable(state, literal.variable())
                            && literal.eval(decision_map) != Some(false)
                    });
                    if satisfied_by_concrete {
                        prop_hit!(EXTRACT_SATISFIED_BY_CONCRETE);
                        continue;
                    }

                    // Otherwise an environment condition complement literal
                    // carries the support. A single true complement literal
                    // `not L` keeps the clause satisfied throughout the cell,
                    // so record exactly the FIRST env literal in disjunction
                    // order whose assigned value is not true (deterministic:
                    // the disjunction's literal order is fixed at encoding).
                    // `L` may be merely undecided; undecided counts as false
                    // and must still be recorded (soundness critical: a later
                    // environment where `L` is true would activate the
                    // requirement, which this solution does not satisfy).
                    // Env literals that are assigned TRUE cannot carry the
                    // support: their complement is false (e.g. an AND
                    // condition where another clause forces one of the
                    // literals true). Skipping one is itself a choice
                    // conditioned on its truth, so the skipped literal is
                    // recorded POSITIVELY: without that pin, a re-solve of
                    // the cell under its own condition (where the skipped
                    // variable is unassigned) would pick the skipped literal
                    // as support and extract a different cell. Recording a
                    // true literal only shrinks the cell, so this is always
                    // sound.
                    let supporting = literals.iter().find(|literal| {
                        if !is_env_variable(state, literal.variable()) {
                            return false;
                        }
                        if state.decision_tracker.assigned_value(literal.variable()) == Some(true) {
                            if record(&mut cell, literal.variable(), true) {
                                pins.support_skip += 1;
                            }
                            return false;
                        }
                        true
                    });
                    let Some(literal) = supporting else {
                        debug_assert!(
                            false,
                            "bug: requires clause with installed parent has neither an \
                             installed candidate nor a satisfying condition complement"
                        );
                        continue;
                    };
                    debug_assert!(
                        literal.negate(),
                        "bug: condition complements of env literals are negative literals"
                    );
                    if record(&mut cell, literal.variable(), false) {
                        pins.support_neg += 1;
                    }
                }
                Clause::EnvConstrains(env_constrains_id) => {
                    let payload = state.env_constrains[env_constrains_id];
                    debug_assert_eq!(
                        state.decision_tracker.assigned_value(payload.parent),
                        Some(true),
                        "bug: env_support_clauses is keyed by the clause's parent"
                    );
                    let absent_true = payload.absent_var.is_some_and(|absent| {
                        state.decision_tracker.assigned_value(absent) == Some(true)
                    });
                    let matches_true =
                        state.decision_tracker.assigned_value(payload.matches_var) == Some(true);
                    debug_assert!(
                        !(absent_true && matches_true),
                        "oracle violation: the absent and matches literal of the same package \
                         are both true"
                    );
                    if absent_true {
                        if record(
                            &mut cell,
                            payload.absent_var.expect("checked by absent_true"),
                            true,
                        ) {
                            pins.constrains += 1;
                        }
                    } else if matches_true {
                        if record(&mut cell, payload.matches_var, true) {
                            pins.constrains += 1;
                        }
                    } else {
                        debug_assert!(
                            false,
                            "bug: an EnvConstrains clause with an installed parent is \
                             unsatisfied at solution time"
                        );
                    }
                }
                _ => unreachable!(
                    "only Requires and EnvConstrains clauses are indexed for cell support"
                ),
            }
        }

        // Steering pins: the support rules above cover only clauses whose
        // parent is installed, so they miss environment assignments that
        // steered a CANDIDATE CHOICE by falsifying a more-preferred
        // candidate. Example: under the assumption `E = true`, a preferred
        // candidate `x` is propagated false through x's own conditional
        // requires clause (`not x OR not E OR y`, with `y` false), so the
        // solver installs the next candidate instead. `x` is not installed,
        // so no support rule fires, `E` is dropped by generalization, and a
        // re-solve of the cell under its own (generalized) condition prefers
        // `x` again and extracts a DIFFERENT cell — breaking the reseed
        // fixed point. The same mechanism applies to free-phase cells whose
        // kept trail prefix falsified a preferred candidate for reasons that
        // involve environment literals.
        //
        // For every requires clause with an installed parent whose condition
        // holds, walk the implication cone of each falsified candidate that
        // precedes the installed pick in sorted-candidate order (exactly the
        // candidates `decide()` skipped), and pin every environment literal
        // assignment the cone rests on. The walk stops at installed
        // solvables and the root (they are part of the recorded solution and
        // re-derive in a re-solve) and at decide() picks (free choices, not
        // implications). Pinning trail-consistent literals only shrinks the
        // cell, so this is always sound.
        {
            let mut visited: ahash::HashSet<VariableId> = ahash::HashSet::default();
            let mut cone: Vec<VariableId> = Vec::new();
            for &parent_index in &inputs.requires_parents {
                let (_, requirements) = state
                    .requires_clauses
                    .get_index(parent_index as usize)
                    .expect("collect_cell_capture_inputs yields valid registration indices");
                for (requirement, disjunction, _clause_id) in requirements {
                    // Only clauses whose condition holds drove a pick; a
                    // clause satisfied by its condition complement never
                    // reached decide().
                    if let Some(disjunction) = *disjunction {
                        // Same "assigned false" rule as `decide()`; see
                        // `capture_cell_edges`.
                        let condition_holds = state.disjunctions[disjunction]
                            .literals
                            .iter()
                            .all(|literal| literal.eval(decision_map) == Some(false));
                        if !condition_holds {
                            continue;
                        }
                    }
                    // The falsified prefix of the sorted candidates is
                    // exactly what decide() skipped before the installed
                    // pick (an undecided candidate means the clause was
                    // satisfied through another clause's install before
                    // decide() ever drove it: no steering happened here).
                    for &candidate in state.requirement_to_sorted_candidates[*requirement]
                        .iter()
                        .flatten()
                    {
                        match state.decision_tracker.assigned_value(candidate) {
                            Some(false) => cone.push(candidate),
                            _ => break,
                        }
                    }
                }
            }

            while let Some(variable) = cone.pop() {
                if !visited.insert(variable) {
                    continue;
                }
                let Some(assigned) = state.decision_tracker.assigned_value(variable) else {
                    // Cone entries and reason antecedents are assigned by
                    // construction; tolerate gaps defensively in release.
                    debug_assert!(false, "bug: an implication-cone variable is unassigned");
                    continue;
                };
                if is_env_variable(state, variable) {
                    if record(&mut cell, variable, assigned) {
                        pins.steering += 1;
                    }
                    continue;
                }
                // Installed solvables and the root are part of the recorded
                // solution; a re-solve re-installs them, and their own
                // steering is pinned through their own clauses.
                if assigned && variable.as_solvable_or_root(&state.variable_map).is_some() {
                    continue;
                }
                // Level-starting assignments are decide() picks (free
                // choices, not implications) or assumption decisions
                // (environment literals, pinned above); their recorded
                // clause is not an implying reason.
                if state.decision_tracker.starts_level(variable) {
                    continue;
                }
                let Some(reason) = state.decision_tracker.find_clause_for_assignment(variable)
                else {
                    debug_assert!(false, "bug: a propagated assignment has no reason clause");
                    continue;
                };
                debug_assert_ne!(
                    reason,
                    ClauseId::assumption(),
                    "bug: assumption decisions start a level and are environment literals"
                );
                // Every literal of the reason clause other than the
                // propagated one was false when the propagation fired; the
                // cone rests on those assignments.
                state.clauses.kinds[reason.to_index()].visit_literals(
                    &state.learnt_clauses,
                    &state.requirement_to_sorted_candidates,
                    &state.disjunctions,
                    &state.env_constrains,
                    &state.env_clauses,
                    |literal| {
                        if literal.variable() != variable {
                            cone.push(literal.variable());
                        }
                    },
                );
            }
        }

        cell.sort_by_key(|&(variable, _)| variable.to_index());
        (cell, pins)
    }

    /// Counts the decision levels above `target` whose level-starting
    /// decision is ordinary: neither an environment literal nor an
    /// env-sensitive parent (a variable in
    /// `SolverState::env_sensitive_parents`). With a trail in the
    /// env-literals-last shape this is at most the dependency subtree of a
    /// deferred parent; a larger count means env-independent packages are
    /// stacked above the retract target and every transition would re-derive
    /// them (see the trail-reshape logic in `enumerate_universal`).
    fn ordinary_levels_above(&self, target: u32) -> u32 {
        let state = &self.state;
        let tracker = &state.decision_tracker;
        let mut count = 0;
        let mut previous_level = 0;
        for decision in tracker.stack() {
            let level = tracker.level(decision.variable);
            let starts_level = level > previous_level;
            previous_level = previous_level.max(level);
            if !starts_level || level <= target {
                continue;
            }
            if is_env_variable(state, decision.variable)
                || state.env_sensitive_parents.contains_key(&decision.variable)
            {
                continue;
            }
            count += 1;
        }
        count
    }

    /// The level to retract the trail to before adding the blocking clause
    /// for `cell`, chosen so that the clause has at least one non-false
    /// literal under the surviving prefix (the soundness requirement of
    /// trail-prefix preservation): one below the deepest assignment level
    /// among the cell's literals.
    ///
    /// A cell literal may be undecided on the trail (extraction and repair
    /// record undecided-counts-as-false literals). Its blocking literal is
    /// then already non-false, the blocking clause is not falsified by the
    /// full trail, and nothing needs to be retracted at all: `u32::MAX`
    /// makes `undo_until` a no-op.
    fn blocking_clause_retract_target(&self, cell: &[(VariableId, bool)]) -> u32 {
        let tracker = &self.state.decision_tracker;
        let mut deepest_falsified = 0;
        for &(variable, value) in cell {
            if tracker.assigned_value(variable) != Some(value) {
                return u32::MAX;
            }
            deepest_falsified = deepest_falsified.max(tracker.level(variable));
        }
        deepest_falsified.saturating_sub(1)
    }

    /// Restores provable pairwise disjointness between `cell` and every
    /// previously recorded cell (design doc 5.6, "disjointness repair").
    ///
    /// Generalization can widen a cell into a previously recorded one. For
    /// every earlier cell that is not provably disjoint, a distinguishing
    /// literal is appended to the new cell: the earlier cell's blocking
    /// clause is part of the formula and `decide()` guarantees it is
    /// satisfied under the undecided-counts-as-false completion of the
    /// current assignment, so at least one of the earlier cell's literals
    /// evaluates opposite; appending the current value of that literal makes
    /// the two cells complementary on it. The repair runs unconditionally,
    /// even when the solvable sets are identical, to keep the documented
    /// pairwise-disjoint invariant.
    ///
    /// `pins` is the literal-attribution record started by
    /// [`Self::extract_cell`]; the repair adds its agreement pins and
    /// distinguishing appends to it. `literal_index` is the inverted index
    /// over the earlier cells' literals maintained by the enumeration loop
    /// (see [`CellLiteralIndex`]).
    fn repair_disjointness(
        &self,
        cell: &mut Vec<(VariableId, bool)>,
        earlier_cells: &[Vec<(VariableId, bool)>],
        literal_index: &CellLiteralIndex,
        pins: &mut CellPinCounts,
    ) {
        // Fast path: an earlier cell that shares a variable with `cell` at
        // the COMPLEMENTARY sign is provably disjoint (the first rule of
        // [`Self::provably_disjoint`]), and a provably disjoint earlier cell
        // contributes nothing to the repair, so it can be skipped without
        // running the full pairwise check. The inverted index marks those
        // cells word-parallel in O(|cell| x cells/64) instead of a literal
        // pair scan per cell. The marks stay valid throughout the loop below
        // even though the repair appends literals to `cell`: appending never
        // removes the complementary pair, so the full check would still
        // conclude "disjoint" (and skip) for every marked cell.
        let mut complementary = vec![0u64; earlier_cells.len().div_ceil(64)];
        for &(variable, value) in cell.iter() {
            literal_index.mark_complementary(variable, value, &mut complementary);
        }

        for (earlier_index, earlier) in earlier_cells.iter().enumerate() {
            if complementary[earlier_index / 64] & (1 << (earlier_index % 64)) != 0 {
                debug_assert!(self.provably_disjoint(cell, earlier));
                continue;
            }
            if self.provably_disjoint(cell, earlier) {
                continue;
            }

            let mut distinguishing = None;
            for &(variable, value) in earlier {
                let current = self.state.decision_tracker.assigned_value(variable) == Some(true);
                if current != value {
                    distinguishing = Some((variable, value));
                    break;
                }
                // The scan relied on this agreement to move past the
                // literal, so the agreement must pin the cell: without it, a
                // re-solve of the cell under its own condition (where the
                // variable may be unassigned and disagree under the
                // undecided-counts-as-false completion) would pick an
                // earlier distinguishing literal and repair to a different
                // cell. Recording a trail-consistent literal only shrinks
                // the cell, so this is always sound.
                if !cell.iter().any(|&(v, _)| v == variable) {
                    cell.push((variable, current));
                    pins.repair_agreement += 1;
                }
            }
            let (variable, value) = distinguishing.expect(
                "bug: the earlier cell's blocking clause is satisfied by the current \
                 assignment, so a distinguishing literal must exist",
            );
            // The current assignment's value for the variable is the opposite
            // of the earlier cell's sign.
            cell.push((variable, !value));
            pins.repair_distinguishing += 1;
            cell.sort_by_key(|&(v, _)| v.to_index());

            debug_assert!(self.provably_disjoint(cell, earlier));
        }

        debug_assert!(
            earlier_cells
                .iter()
                .all(|earlier| self.provably_disjoint(cell, earlier)),
            "bug: the repaired cell must be provably disjoint from all earlier cells"
        );
    }

    /// Returns true if the two cell conjunctions are provably disjoint:
    /// they contain complementary signs of the same variable, two true
    /// matches literals of the same package whose version sets the oracle
    /// calls [`VersionSetRelation::Disjoint`], or a true absent and a true
    /// matches literal of the same package.
    fn provably_disjoint(&self, a: &[(VariableId, bool)], b: &[(VariableId, bool)]) -> bool {
        for &(var_a, sign_a) in a {
            for &(var_b, sign_b) in b {
                if var_a == var_b {
                    if sign_a != sign_b {
                        return true;
                    }
                    continue;
                }
                if !(sign_a && sign_b) {
                    continue;
                }
                match (
                    self.state.variable_map.origin(var_a),
                    self.state.variable_map.origin(var_b),
                ) {
                    (VariableOrigin::EnvMatches(vs_a), VariableOrigin::EnvMatches(vs_b)) => {
                        let provider = self.provider();
                        // The relation oracle is only defined for version
                        // sets of the same package. Queried through the
                        // cache's memo ([`SolverCache::env_version_set_relation`]):
                        // the repair re-asks about the same literal pairs for
                        // every later cell.
                        if provider.version_set_name(vs_a) == provider.version_set_name(vs_b)
                            && self.cache.env_version_set_relation(vs_a, vs_b)
                                == VersionSetRelation::Disjoint
                        {
                            return true;
                        }
                    }
                    (VariableOrigin::EnvAbsent(name), VariableOrigin::EnvMatches(vs))
                    | (VariableOrigin::EnvMatches(vs), VariableOrigin::EnvAbsent(name)) => {
                        if self.provider().version_set_name(vs) == name {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        false
    }

    /// Converts a cell in solver variable space to the public
    /// [`CellCondition`] representation via the variable origins.
    fn cell_to_condition(&self, cell: &[(VariableId, bool)]) -> CellCondition<D::NameId> {
        let mut literals: Vec<SignedEnvLiteral<D::NameId>> = cell
            .iter()
            .map(|&(variable, value)| {
                let literal = match self.state.variable_map.origin(variable) {
                    VariableOrigin::EnvMatches(version_set) => EnvLiteral::Matches(version_set),
                    VariableOrigin::EnvAbsent(package) => EnvLiteral::Absent(package),
                    _ => unreachable!("cell literals are always environment literal variables"),
                };
                SignedEnvLiteral::new(literal, value)
            })
            .collect();

        // Order the condition canonically: by version set id, with absent
        // literals (ordered by package name) at the end. Cell extraction
        // orders literals by solver variable id, which depends on interning
        // order and therefore on the enumeration path: the same cell reached
        // through different seed partitions would render differently. The
        // ids used here come from the dependency provider and are stable
        // across solves.
        literals.sort_by(|a, b| {
            use std::cmp::Ordering;
            match (a.literal, b.literal) {
                (EnvLiteral::Matches(vs_a), EnvLiteral::Matches(vs_b)) => {
                    vs_a.to_index().cmp(&vs_b.to_index())
                }
                (EnvLiteral::Matches(_), EnvLiteral::Absent(_)) => Ordering::Less,
                (EnvLiteral::Absent(_), EnvLiteral::Matches(_)) => Ordering::Greater,
                (EnvLiteral::Absent(name_a), EnvLiteral::Absent(name_b)) => self
                    .provider()
                    .display_name(name_a)
                    .to_string()
                    .cmp(&self.provider().display_name(name_b).to_string()),
            }
        });

        CellCondition::from_literals_unchecked(literals)
    }

    /// The effective witness-probe budget for one free enumeration episode:
    /// the flat [`WITNESS_PROBE_BUDGET`], or the test override (see
    /// `Solver::test_witness_probe_override`). `None` disables the probe.
    fn witness_probe_budget(&self) -> Option<u64> {
        #[cfg(test)]
        if let Some(override_budget) = self.test_witness_probe_override {
            return override_budget;
        }
        Some(WITNESS_PROBE_BUDGET)
    }

    /// Searches for an assignment of the environment literal variables that
    /// satisfies all environment-only clauses: oracle consistency clauses,
    /// model clauses and blocking clauses (the same clause-set semantics as
    /// the main solve, which is what makes coverage termination correct).
    ///
    /// Returns `None` if no such assignment exists (the recorded cells cover
    /// the entire model), or the witness assignment otherwise. Environment
    /// variables that occur in no environment-only clause are unconstrained
    /// and left out of the witness, mirroring how cells record only
    /// load-bearing literals.
    ///
    /// The clause set is dominated by the O(m^2) pairwise oracle consistency
    /// clauses, so the input is assembled into the solver's reusable
    /// [`WitnessScratch`] flat arena (one bulk copy, no per-clause
    /// allocation) instead of a fresh `Vec<Vec<Literal>>` per call; at a few
    /// hundred cells that construction used to dominate the check.
    fn find_environment_witness(&mut self) -> Option<Vec<(VariableId, bool)>> {
        let scratch = &mut self.witness_scratch;
        scratch.clear();
        for kind in &self.state.clauses.kinds {
            match *kind {
                // The only clauses that constrain the environment space
                // itself: oracle consistency (relations between env literals)
                // and the model/blocking clauses.
                Clause::EnvOracleConsistency(lit_a, lit_b) => {
                    scratch.push_clause(&[lit_a, lit_b]);
                }
                Clause::EnvClause(env_clause_id) => {
                    scratch.push_clause(&self.state.env_clauses[env_clause_id].literals);
                }
                // Everything else constrains which SOLVABLES are valid GIVEN
                // an environment, not which environments exist, so it must NOT
                // enter the environment-space witness search (design 5.5):
                // `EnvConstrains` and env-conditioned `Requires` are gated on a
                // solvable being installed, so including them could make a
                // coverable region look uncoverable. This arm is intentionally
                // exhaustive (no `_`): a new clause kind that genuinely bounds
                // the environment space must be added above deliberately.
                Clause::InstallRoot
                | Clause::Requires(..)
                | Clause::Constrains(..)
                | Clause::ConstrainsExcluded(..)
                | Clause::ConstrainsParent(..)
                | Clause::ForbidMultipleInstances(..)
                | Clause::Lock(..)
                | Clause::Learnt(..)
                | Clause::Excluded(..)
                | Clause::AnyOf(..)
                | Clause::EnvConstrains(..) => {}
            }
        }
        scratch.find_witness()
    }
}

/// Whether two cell conditions are provably disjoint, and when they are not,
/// whether the lack of a proof involved an Unknown oracle answer.
enum Disjointness {
    /// The conditions are provably disjoint: no environment satisfies both.
    Disjoint,
    /// No disjointness proof exists and every relevant oracle answer was
    /// definite: the conditions describe overlapping regions.
    Overlapping,
    /// No disjointness proof exists, but the oracle answered
    /// [`VersionSetRelation::Unknown`] for at least one pair of positive
    /// matches literals; the conditions may still be disjoint in reality.
    Unproven,
}

/// Returns true when `a` and `b` contain the same solvables (as sets).
fn same_solvable_set<Id: Eq>(a: &[Id], b: &[Id]) -> bool {
    a.len() == b.len()
        && a.iter().all(|solvable| b.contains(solvable))
        && b.iter().all(|solvable| a.contains(solvable))
}

/// Decides whether two cell conditions are provably disjoint. This mirrors
/// the solver-side `Solver::provably_disjoint` but works over public
/// [`EnvLiteral`]s via the relation oracle: two conditions are provably
/// disjoint when they contain complementary signs of the same literal, two
/// positive matches literals of the same package whose version sets the
/// oracle calls [`VersionSetRelation::Disjoint`], or a positive absent and a
/// positive matches literal of the same package.
fn prove_env_disjoint<N: Copy + Eq, D>(
    provider: &D,
    a: &CellCondition<N>,
    b: &CellCondition<N>,
) -> Disjointness
where
    D: UniversalDependencyProvider + Interner<NameId = N>,
{
    let mut unknown_involved = false;
    for sa in a.literals() {
        for sb in b.literals() {
            if sa.literal == sb.literal {
                if sa.positive != sb.positive {
                    return Disjointness::Disjoint;
                }
                continue;
            }
            if !(sa.positive && sb.positive)
                || sa.literal.package(provider) != sb.literal.package(provider)
            {
                continue;
            }
            match (sa.literal, sb.literal) {
                (EnvLiteral::Matches(vs_a), EnvLiteral::Matches(vs_b)) => {
                    match provider.environment_version_set_relation(vs_a, vs_b) {
                        VersionSetRelation::Disjoint => return Disjointness::Disjoint,
                        VersionSetRelation::Unknown => unknown_involved = true,
                        VersionSetRelation::Subset
                        | VersionSetRelation::Superset
                        | VersionSetRelation::Equal => {}
                    }
                }
                (EnvLiteral::Matches(_), EnvLiteral::Absent(_))
                | (EnvLiteral::Absent(_), EnvLiteral::Matches(_)) => {
                    return Disjointness::Disjoint;
                }
                (EnvLiteral::Absent(_), EnvLiteral::Absent(_)) => {
                    unreachable!("two absent literals of the same package are equal")
                }
            }
        }
    }
    if unknown_involved {
        Disjointness::Unproven
    } else {
        Disjointness::Overlapping
    }
}

/// Simplifies a disjunction of conjunctions (the disjuncts of a
/// [`Presence`]): repeatedly merges any two disjuncts that contain exactly
/// the same literals and differ in the sign of exactly one (dropping that
/// literal), and deduplicates identical disjuncts, until a fixpoint is
/// reached. The disjunct counts are small (bounded by the cell count), so a
/// quadratic pass per round is fine.
///
/// When a merge empties a disjunct the whole disjunction is always true and
/// collapses to a single empty conjunction.
fn simplify_disjuncts<N: Copy + Eq>(mut disjuncts: Vec<CellCondition<N>>) -> Vec<CellCondition<N>> {
    'merge: loop {
        for first in 0..disjuncts.len() {
            for second in first + 1..disjuncts.len() {
                let Some(merged) = merge_disjunct_pair(&disjuncts[first], &disjuncts[second])
                else {
                    continue;
                };
                if merged.is_empty() {
                    // The merged disjunct holds in every environment, which
                    // makes every other disjunct redundant.
                    return vec![CellCondition::from_literals_unchecked(Vec::new())];
                }
                disjuncts[first] = merged;
                disjuncts.remove(second);
                continue 'merge;
            }
        }
        return disjuncts;
    }
}

/// Merges two conjunctions when they contain exactly the same literals
/// (order insensitive) and differ in the sign of at most one: identical
/// conjunctions merge to either of them, and conjunctions differing in
/// exactly one sign merge by dropping that literal (`(C and x) or
/// (C and not x)` simplifies to `C`). Returns `None` when the pair is not
/// mergeable.
///
/// Assumes no conjunction mentions the same literal twice, which holds for
/// cell conditions (each records one sign per environment literal variable).
fn merge_disjunct_pair<N: Copy + Eq>(
    a: &CellCondition<N>,
    b: &CellCondition<N>,
) -> Option<CellCondition<N>> {
    if a.len() != b.len() {
        return None;
    }
    let mut differing = None;
    for (index, sa) in a.literals().enumerate() {
        let sb = b.literals().find(|sb| sb.literal == sa.literal)?;
        if sa.positive != sb.positive {
            if differing.is_some() {
                return None;
            }
            differing = Some(index);
        }
    }
    // Equal lengths and every literal of `a` found in `b`: with no duplicate
    // literals this is a bijection, so the literal sets are identical.
    let merged = match differing {
        None => a.literals().cloned().collect(),
        Some(drop_index) => a
            .literals()
            .enumerate()
            .filter(|&(index, _)| index != drop_index)
            .map(|(_, signed)| *signed)
            .collect(),
    };
    Some(CellCondition::from_literals_unchecked(merged))
}

/// A signed literal over a dense witness-search variable index: `(index,
/// negate)`. The literal evaluates to true when the variable at `index` is
/// assigned the opposite of `negate`.
type IndexedLiteral = (usize, bool);

/// A borrowed set of witness-search clauses stored in one flat literal
/// arena: clause `i` spans `literals[clause_ends[i - 1]..clause_ends[i]]`
/// (the first clause starts at 0). The flat layout lets the witness input be
/// assembled with bulk copies instead of one heap allocation per clause,
/// which matters because the input is dominated by the O(m^2) pairwise
/// oracle consistency clauses.
#[derive(Copy, Clone)]
struct IndexedClauses<'a> {
    literals: &'a [IndexedLiteral],
    clause_ends: &'a [usize],
}

impl<'a> IndexedClauses<'a> {
    /// The clauses in insertion order, each as a slice of the flat arena.
    fn iter(self) -> impl Iterator<Item = &'a [IndexedLiteral]> {
        let literals = self.literals;
        self.clause_ends.iter().scan(0, move |start, &end| {
            let clause = &literals[*start..end];
            *start = end;
            Some(clause)
        })
    }
}

/// Reusable scratch buffers for [`Solver::find_environment_witness`]: the
/// witness formula in flat indexed form, plus the variable table mapping
/// witness indices back to solver [`VariableId`]s.
///
/// The buffers live on the `Solver` (not on [`SolverState`]) purely to reuse
/// their allocations across enumeration passes and reseed rounds: every
/// `find_witness` call rebuilds the contents from the clauses pushed since
/// `clear`, so no stale state can survive the per-pass
/// `SolverState::default()` reset.
#[derive(Default)]
pub(crate) struct WitnessScratch {
    /// Flat clause arena. [`Self::push_clause`] records raw variable indices
    /// ([`VariableId::to_index`]); [`Self::find_witness`] rewrites them in
    /// place to dense witness indices.
    literals: Vec<IndexedLiteral>,
    /// End offset of each pushed clause in `literals`.
    clause_ends: Vec<usize>,
    /// Per variable index: the variable's dense witness index plus one, or 0
    /// for variables that occur in no pushed clause. Rebuilt on every
    /// [`Self::find_witness`] call.
    variable_index: Vec<u32>,
    /// The constrained variables in ascending [`VariableId`] order; a
    /// variable's dense witness index is its position here.
    variables: Vec<VariableId>,
}

impl WitnessScratch {
    /// Discards the previously pushed clauses, keeping the allocations.
    fn clear(&mut self) {
        self.literals.clear();
        self.clause_ends.clear();
    }

    /// Appends one clause to the formula.
    fn push_clause(&mut self, literals: &[Literal]) {
        self.literals.extend(
            literals
                .iter()
                .map(|literal| (literal.variable().to_index(), literal.negate())),
        );
        self.clause_ends.push(self.literals.len());
    }

    /// Searches for an assignment of solver variables satisfying all pushed
    /// clauses (see [`find_witness_indexed`] for the search itself).
    ///
    /// The search scope is exactly the variables that occur in the clauses;
    /// unconstrained variables are left out of the witness, mirroring how
    /// cells record only load-bearing literals. Dense witness indices are
    /// assigned in ascending `VariableId` order — each variable's rank among
    /// the distinct constrained variables, exactly the numbering the
    /// previous sort + dedup + binary-search construction produced — so the
    /// search's variable visit order, and with it the canonical
    /// lexicographically-smallest witness, is unchanged.
    fn find_witness(&mut self) -> Option<Vec<(VariableId, bool)>> {
        self.variable_index.clear();
        self.variables.clear();
        if let Some(max_index) = self.literals.iter().map(|&(index, _)| index).max() {
            // Mark the occurring variables, then assign dense indices with
            // one ascending scan (yielding sorted order by construction),
            // then rewrite the arena in place through the O(1) lookup table.
            self.variable_index.resize(max_index + 1, 0);
            for &(index, _) in &self.literals {
                self.variable_index[index] = 1;
            }
            for index in 0..=max_index {
                if self.variable_index[index] != 0 {
                    self.variables.push(VariableId::from_index(index));
                    self.variable_index[index] = self.variables.len() as u32;
                }
            }
            for literal in &mut self.literals {
                literal.0 = self.variable_index[literal.0] as usize - 1;
            }
        }

        let assignment = find_witness_indexed(
            self.variables.len(),
            IndexedClauses {
                literals: &self.literals,
                clause_ends: &self.clause_ends,
            },
        )?;
        Some(self.variables.iter().copied().zip(assignment).collect())
    }
}

/// A dedicated backtracking search for an assignment satisfying all the
/// given clauses, generic over a dense variable index so that it is reusable
/// both over solver `VariableId`s (the coverage-termination check of the
/// enumeration loop, see [`find_witness`]) and over public [`EnvLiteral`]s
/// (the post-hoc verifier, see [`UniversalSolution::verify`]).
///
/// The number of environment literals is small, so a simple exhaustive
/// search with clause-violation pruning is sufficient; the main CDCL
/// machinery is deliberately not reused here (design doc 5.5).
///
/// Variables are assigned in ascending index order and `false` is tried
/// first, matching the split policy (environment literals default to false),
/// so the witness stays as close to the baseline machine as possible and the
/// search is deterministic. Returns one value per variable index, or `None`
/// when no assignment satisfies all clauses.
fn find_witness_indexed(variable_count: usize, clauses: IndexedClauses<'_>) -> Option<Vec<bool>> {
    debug_assert!(
        clauses
            .literals
            .iter()
            .all(|&(index, _)| index < variable_count),
        "every clause literal must reference a variable below `variable_count`"
    );

    // First decide satisfiability with a most-constrained-first decision
    // order (most clause occurrences first, index as tie break). The
    // refutation case is the common one (every successful universal solve
    // ends with exactly one refuted witness search proving coverage), its
    // result is order independent, and deciding frequently-occurring
    // variables first prunes the hundreds of accumulated blocking clauses
    // orders of magnitude faster than ascending index order.
    let mut occurrences = vec![0usize; variable_count];
    for &(index, _) in clauses.literals {
        occurrences[index] += 1;
    }
    let mut order: Vec<usize> = (0..variable_count).collect();
    order.sort_by_key(|&index| (std::cmp::Reverse(occurrences[index]), index));

    let mut assignment: Vec<Option<bool>> = vec![None; variable_count];
    if !search_indexed(clauses, &mut assignment, &order) {
        return None;
    }

    // A witness exists. Re-run in ascending index order to return the
    // canonical lexicographically smallest witness (false first), keeping
    // the reported cell of an unsolvable region deterministic and as close
    // to the baseline machine as before. Witness-producing searches happen
    // on failure paths where few blocking clauses have accumulated, so the
    // cost of the second search is negligible.
    let identity: Vec<usize> = (0..variable_count).collect();
    let mut assignment: Vec<Option<bool>> = vec![None; variable_count];
    let found = search_indexed(clauses, &mut assignment, &identity);
    debug_assert!(found, "a satisfiable formula stays satisfiable");
    if !found {
        return None;
    }
    Some(
        assignment
            .into_iter()
            .map(|value| value.expect("the search assigns every variable"))
            .collect(),
    )
}

/// Recursive helper of [`find_witness_indexed`]: tries to extend the partial
/// `assignment` to a satisfying total assignment. Returns true when one was
/// found (left in `assignment`).
///
/// The search interleaves decisions (`false` first, variables in the given
/// decision `order`) with unit propagation. Propagation only assigns values
/// entailed by the current partial assignment, so with the identity order
/// the first satisfying assignment found is the lexicographically smallest
/// one, exactly as the original propagation-free exhaustive search
/// returned. Propagation and the caller-chosen decision order are what keep
/// the coverage check tractable when hundreds of blocking clauses have
/// accumulated (a high-cell-count solve used to spend minutes here, orders
/// of magnitude longer than the enumeration itself).
fn search_indexed(
    clauses: IndexedClauses<'_>,
    assignment: &mut [Option<bool>],
    order: &[usize],
) -> bool {
    // Propagate the consequences of the current assignment, recording what
    // was assigned so it can be undone on backtrack.
    let mut propagated: Vec<usize> = Vec::new();
    if !propagate_indexed(clauses, assignment, &mut propagated) {
        for index in propagated {
            assignment[index] = None;
        }
        return false;
    }

    let Some(&unassigned) = order.iter().find(|&&index| assignment[index].is_none()) else {
        return true;
    };

    for value in [false, true] {
        assignment[unassigned] = Some(value);
        if search_indexed(clauses, assignment, order) {
            return true;
        }
    }
    assignment[unassigned] = None;
    for index in propagated {
        assignment[index] = None;
    }
    false
}

/// Unit propagation for [`search_indexed`]: repeatedly assigns the last
/// unassigned literal of any clause whose other literals are all false,
/// pushing every assigned index onto `propagated`. Returns false when a
/// clause is violated (all literals assigned and false).
fn propagate_indexed(
    clauses: IndexedClauses<'_>,
    assignment: &mut [Option<bool>],
    propagated: &mut Vec<usize>,
) -> bool {
    loop {
        let mut changed = false;
        for clause in clauses.iter() {
            let mut unit: Option<IndexedLiteral> = None;
            let mut unassigned = 0usize;
            let mut satisfied = false;
            for &(index, negate) in clause {
                match assignment[index] {
                    None => {
                        unassigned += 1;
                        unit = Some((index, negate));
                        if unassigned > 1 {
                            break;
                        }
                    }
                    Some(value) => {
                        if value != negate {
                            satisfied = true;
                            break;
                        }
                    }
                }
            }
            if satisfied || unassigned > 1 {
                continue;
            }
            match unit {
                None => return false,
                Some((index, negate)) => {
                    assignment[index] = Some(!negate);
                    propagated.push(index);
                    changed = true;
                }
            }
        }
        if !changed {
            return true;
        }
    }
}

/// Flattens nested clause vectors into the flat arena form and runs
/// [`find_witness_indexed`]. Used by the post-hoc verifier, which assembles
/// its clauses ad hoc; the hot coverage check assembles the flat form
/// directly in [`WitnessScratch`].
fn find_witness_indexed_nested(
    variable_count: usize,
    clauses: &[Vec<IndexedLiteral>],
) -> Option<Vec<bool>> {
    let mut literals = Vec::new();
    let mut clause_ends = Vec::with_capacity(clauses.len());
    for clause in clauses {
        literals.extend_from_slice(clause);
        clause_ends.push(literals.len());
    }
    find_witness_indexed(
        variable_count,
        IndexedClauses {
            literals: &literals,
            clause_ends: &clause_ends,
        },
    )
}

/// Searches for an assignment of solver variables satisfying all the given
/// clauses: a test-facing convenience wrapper around [`WitnessScratch`]
/// (which `find_environment_witness` drives directly with reused buffers).
#[cfg(test)]
fn find_witness(clauses: &[Vec<Literal>]) -> Option<Vec<(VariableId, bool)>> {
    let mut scratch = WitnessScratch::default();
    for clause in clauses {
        scratch.push_clause(clause);
    }
    scratch.find_witness()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{Interner, solver::env_test_provider::EnvTestProvider};

    fn literal(variable: usize, negate: bool) -> Literal {
        Literal::new(VariableId::from_index(variable), negate)
    }

    /// Formats the cells of a [`UniversalSolution`] for inline snapshots.
    fn cells_to_string(solver: &Solver<EnvTestProvider>, solution: &UniversalSolution) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for cell in solution.cells() {
            let solvables = cell
                .solvables()
                .iter()
                .map(|&s| solver.provider().display_solvable(s).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                out,
                "{} -> [{}]",
                cell.condition().display(solver.provider()),
                solvables
            )
            .unwrap();
        }
        out
    }

    /// Disjointness repair, append case: the second cell generalizes to just
    /// `glibc in 217..1000`, which overlaps the first cell
    /// (`glibc in 228..1000`, a subset). The repair must append the
    /// distinguishing literal `not (glibc in 228..1000)` from the first
    /// cell's blocking clause to restore provable disjointness.
    #[test]
    fn test_repair_appends_distinguishing_literal() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("glibc", false);
        let glibc_217 = provider.version_set("glibc", 217, 1000);
        let glibc_228 = provider.version_set("glibc", 228, 1000);
        let pkg_2 = provider.add_package("pkg", 2);
        let pkg_1 = provider.add_package("pkg", 1);
        provider.set_dependencies(pkg_2, vec![glibc_228.into()], vec![]);
        provider.set_dependencies(pkg_1, vec![glibc_217.into()], vec![]);
        let pkg_any = provider.version_set("pkg", 0, 3);

        let mut solver = Solver::new(provider);
        let problem = UniversalProblem::new()
            .requirements(vec![pkg_any.into()])
            .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
                SignedEnvLiteral::new(EnvLiteral::Matches(glibc_217), true),
            ])]));
        let solution = solver.solve_universal(problem).expect("solvable");

        insta::assert_snapshot!(cells_to_string(&solver, &solution), @r"
        glibc in 228..1000 -> [pkg=2]
        glibc in 217..1000 AND not (glibc in 228..1000) -> [pkg=1]
        ");
    }

    /// Disjointness repair, no-append case: a true absent literal and a true
    /// matches literal of the same package are provably disjoint, so the
    /// second cell keeps exactly one literal.
    #[test]
    fn test_repair_skips_provably_disjoint_cells() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("cuda", true);
        let cuda_11 = provider.version_set("cuda", 11, 100);
        let cuda_name = provider.pool.intern_package_name("cuda");
        let a = provider.add_package("a", 1);
        provider.set_dependencies(a, vec![], vec![cuda_11]);
        let a_any = provider.version_set("a", 0, 2);

        let mut solver = Solver::new(provider);
        let problem = UniversalProblem::new()
            .requirements(vec![a_any.into()])
            .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
                SignedEnvLiteral::new(EnvLiteral::Absent(cuda_name), true),
                SignedEnvLiteral::new(EnvLiteral::Matches(cuda_11), true),
            ])]));
        let solution = solver.solve_universal(problem).expect("solvable");

        insta::assert_snapshot!(cells_to_string(&solver, &solution), @r"
        cuda absent -> [a=1]
        cuda in 11..100 -> [a=1]
        ");
    }

    /// The enumerated solution of a real solve passes the independent
    /// verifier: the cells are provably disjoint (the repair step appended a
    /// distinguishing literal) and together cover the model.
    #[test]
    fn test_verify_accepts_enumerated_solution() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("glibc", false);
        let glibc_217 = provider.version_set("glibc", 217, 1000);
        let glibc_228 = provider.version_set("glibc", 228, 1000);
        let pkg_2 = provider.add_package("pkg", 2);
        let pkg_1 = provider.add_package("pkg", 1);
        provider.set_dependencies(pkg_2, vec![glibc_228.into()], vec![]);
        provider.set_dependencies(pkg_1, vec![glibc_217.into()], vec![]);
        let pkg_any = provider.version_set("pkg", 0, 3);

        let mut solver = Solver::new(provider);
        let problem = UniversalProblem::new()
            .requirements(vec![pkg_any.into()])
            .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
                SignedEnvLiteral::new(EnvLiteral::Matches(glibc_217), true),
            ])]));
        let solution = solver.solve_universal(problem).expect("solvable");

        assert_eq!(solution.verify(solver.provider()), Ok(()));
    }

    /// A hand-built solution that covers only the `absent` half of an
    /// `absent OR matches` model: the verifier reports the uncovered
    /// region. The region assignment is deterministic (literals in
    /// model-then-cells order, false tried first): `matches` must be true
    /// (the model and the missing cell leave only that region) and `absent`
    /// false (oracle exclusion).
    #[test]
    fn test_verify_reports_uncovered_region() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("cuda", true);
        let cuda_11 = provider.version_set("cuda", 11, 100);
        let cuda_name = provider.pool.intern_package_name("cuda");
        let a = provider.add_package("a", 1);
        let matches_lit = EnvLiteral::Matches(cuda_11);
        let absent_lit = EnvLiteral::Absent(cuda_name);

        let solution: UniversalSolution = UniversalSolution::from_cells(
            vec![Cell::new(
                CellCondition::new(vec![SignedEnvLiteral::new(absent_lit, true)]).unwrap(),
                vec![a],
                vec![],
            )],
            EnvironmentModel::new(vec![EnvClause::new(vec![
                SignedEnvLiteral::new(matches_lit, true),
                SignedEnvLiteral::new(absent_lit, true),
            ])]),
        )
        .unwrap();

        assert_eq!(
            solution.verify(&provider),
            Err(vec![Violation::UncoveredRegion(
                CellCondition::from_literals_unchecked(vec![
                    SignedEnvLiteral::new(matches_lit, true),
                    SignedEnvLiteral::new(absent_lit, false),
                ])
            )])
        );
    }

    /// Two cells with different solvable sets whose conditions genuinely
    /// overlap (an empty condition overlaps everything) and involve no
    /// Unknown oracle answer: OverlappingCells. Coverage is fine because the
    /// empty cell condition covers every environment.
    #[test]
    fn test_verify_reports_overlapping_cells() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("cuda", true);
        let cuda_11 = provider.version_set("cuda", 11, 100);
        let a1 = provider.add_package("a", 1);
        let a2 = provider.add_package("a", 2);

        let solution: UniversalSolution = UniversalSolution::from_cells(
            vec![
                Cell::new(CellCondition::default(), vec![a1], vec![]),
                Cell::new(
                    CellCondition::new(vec![SignedEnvLiteral::new(
                        EnvLiteral::Matches(cuda_11),
                        true,
                    )])
                    .unwrap(),
                    vec![a2],
                    vec![],
                ),
            ],
            EnvironmentModel::default(),
        )
        .unwrap();

        assert_eq!(
            solution.verify(&provider),
            Err(vec![Violation::OverlappingCells {
                first: 0,
                second: 1
            }])
        );
    }

    /// Two cells with different solvable sets whose only same-package literal
    /// pair gets an Unknown oracle answer (partially overlapping ranges):
    /// UnprovenDisjointness, which callers may treat as a warning. The model
    /// makes coverage pass so the unproven pair is the only violation.
    #[test]
    fn test_verify_reports_unproven_disjointness() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("cuda", true);
        let cuda_0_5 = provider.version_set("cuda", 0, 5);
        let cuda_3_8 = provider.version_set("cuda", 3, 8);
        let a1 = provider.add_package("a", 1);
        let a2 = provider.add_package("a", 2);
        let lit_0_5 = EnvLiteral::Matches(cuda_0_5);
        let lit_3_8 = EnvLiteral::Matches(cuda_3_8);

        let solution: UniversalSolution = UniversalSolution::from_cells(
            vec![
                Cell::new(
                    CellCondition::new(vec![SignedEnvLiteral::new(lit_0_5, true)]).unwrap(),
                    vec![a1],
                    vec![],
                ),
                Cell::new(
                    CellCondition::new(vec![SignedEnvLiteral::new(lit_3_8, true)]).unwrap(),
                    vec![a2],
                    vec![],
                ),
            ],
            EnvironmentModel::new(vec![EnvClause::new(vec![
                SignedEnvLiteral::new(lit_0_5, true),
                SignedEnvLiteral::new(lit_3_8, true),
            ])]),
        )
        .unwrap();

        assert_eq!(
            solution.verify(&provider),
            Err(vec![Violation::UnprovenDisjointness {
                first: 0,
                second: 1
            }])
        );
    }

    /// Overlapping cells with IDENTICAL solvable sets are harmless (the
    /// merge step ORs them) and not reported.
    #[test]
    fn test_verify_accepts_identical_solvable_overlap() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("cuda", true);
        let cuda_11 = provider.version_set("cuda", 11, 100);
        let a = provider.add_package("a", 1);

        let solution: UniversalSolution = UniversalSolution::from_cells(
            vec![
                Cell::new(CellCondition::default(), vec![a], vec![]),
                Cell::new(
                    CellCondition::new(vec![SignedEnvLiteral::new(
                        EnvLiteral::Matches(cuda_11),
                        true,
                    )])
                    .unwrap(),
                    vec![a],
                    vec![],
                ),
            ],
            EnvironmentModel::default(),
        )
        .unwrap();

        assert_eq!(solution.verify(&provider), Ok(()));
    }

    /// Cells distinguished only by two different positive matches literals
    /// are accepted when the oracle proves the version sets disjoint, and
    /// coverage holds because the model restricts the space to the union of
    /// the two ranges.
    #[test]
    fn test_verify_accepts_oracle_disjoint_cells() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("cuda", true);
        let cuda_0_2 = provider.version_set("cuda", 0, 2);
        let cuda_5_9 = provider.version_set("cuda", 5, 9);
        let a1 = provider.add_package("a", 1);
        let a2 = provider.add_package("a", 2);
        let lit_0_2 = EnvLiteral::Matches(cuda_0_2);
        let lit_5_9 = EnvLiteral::Matches(cuda_5_9);

        let solution: UniversalSolution = UniversalSolution::from_cells(
            vec![
                Cell::new(
                    CellCondition::new(vec![SignedEnvLiteral::new(lit_0_2, true)]).unwrap(),
                    vec![a1],
                    vec![],
                ),
                Cell::new(
                    CellCondition::new(vec![SignedEnvLiteral::new(lit_5_9, true)]).unwrap(),
                    vec![a2],
                    vec![],
                ),
            ],
            EnvironmentModel::new(vec![EnvClause::new(vec![
                SignedEnvLiteral::new(lit_0_2, true),
                SignedEnvLiteral::new(lit_5_9, true),
            ])]),
        )
        .unwrap();

        assert_eq!(solution.verify(&provider), Ok(()));
    }

    /// project() picks the unique cell whose condition holds under the
    /// evaluation closure: a glibc 230 machine satisfies both range literals
    /// and lands in the first cell, a glibc 220 machine fails the `>=228`
    /// literal and lands in the second.
    #[test]
    fn test_project_selects_unique_cell() {
        let mut provider = EnvTestProvider::default();
        provider.add_env_package("glibc", false);
        let glibc_217 = provider.version_set("glibc", 217, 1000);
        let glibc_228 = provider.version_set("glibc", 228, 1000);
        let pkg_2 = provider.add_package("pkg", 2);
        let pkg_1 = provider.add_package("pkg", 1);
        provider.set_dependencies(pkg_2, vec![glibc_228.into()], vec![]);
        provider.set_dependencies(pkg_1, vec![glibc_217.into()], vec![]);
        let pkg_any = provider.version_set("pkg", 0, 3);

        let mut solver = Solver::new(provider);
        let problem = UniversalProblem::new()
            .requirements(vec![pkg_any.into()])
            .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
                SignedEnvLiteral::new(EnvLiteral::Matches(glibc_217), true),
            ])]));
        let solution = solver.solve_universal(problem).expect("solvable");

        let provider = solver.provider();
        let eval_for = |glibc_version: u32| {
            move |literal: &EnvLiteral<NameId>| match *literal {
                EnvLiteral::Matches(version_set) => provider
                    .pool
                    .resolve_version_set(version_set)
                    .contains(glibc_version),
                EnvLiteral::Absent(_) => false,
            }
        };

        assert_eq!(solution.project(eval_for(230)), Some(&[pkg_2][..]));
        assert_eq!(solution.project(eval_for(220)), Some(&[pkg_1][..]));
        // Outside the model (glibc < 217) no cell matches.
        assert_eq!(solution.project(eval_for(100)), None);
    }

    /// Two cells matching the same environment is a broken invariant:
    /// project() debug-asserts (and returns the first match in release).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "multiple cells")]
    fn test_project_panics_on_overlapping_cells_in_debug() {
        let mut provider = EnvTestProvider::default();
        let a1 = provider.add_package("a", 1);
        let a2 = provider.add_package("a", 2);

        let solution: UniversalSolution = UniversalSolution::from_cells(
            vec![
                Cell::new(CellCondition::default(), vec![a1], vec![]),
                Cell::new(CellCondition::default(), vec![a2], vec![]),
            ],
            EnvironmentModel::default(),
        )
        .unwrap();

        let _ = solution.project(|_| false);
    }

    /// Helper for the simplification unit tests: a matches literal for
    /// version set index `vs` of a single shared package.
    fn env_lit(vs: usize) -> EnvLiteral<NameId> {
        EnvLiteral::Matches(VersionSetId::from_index(vs))
    }

    /// Helper for the simplification unit tests: a conjunction of signed
    /// literals given as `(version set index, sign)` pairs.
    fn conj(literals: &[(usize, bool)]) -> CellCondition<NameId> {
        CellCondition::from_literals_unchecked(
            literals
                .iter()
                .map(|&(vs, sign)| SignedEnvLiteral::new(env_lit(vs), sign))
                .collect(),
        )
    }

    /// Two disjuncts that are identical except for one literal appearing
    /// with opposite signs merge by dropping that literal.
    #[test]
    fn test_simplify_merges_opposite_literal() {
        let disjuncts = vec![
            conj(&[(0, true), (1, true)]),
            conj(&[(0, true), (1, false)]),
        ];
        assert_eq!(simplify_disjuncts(disjuncts), vec![conj(&[(0, true)])]);
    }

    /// Literal order within a conjunction does not matter for merging.
    #[test]
    fn test_simplify_merges_reordered_literals() {
        let disjuncts = vec![
            conj(&[(0, true), (1, true)]),
            conj(&[(1, false), (0, true)]),
        ];
        assert_eq!(simplify_disjuncts(disjuncts), vec![conj(&[(0, true)])]);
    }

    /// All four sign combinations of two literals collapse to the
    /// always-true presence (a single empty conjunction) at the fixpoint.
    #[test]
    fn test_simplify_fixpoint_collapses_to_all_environments() {
        let disjuncts = vec![
            conj(&[(0, true), (1, true)]),
            conj(&[(0, true), (1, false)]),
            conj(&[(0, false), (1, true)]),
            conj(&[(0, false), (1, false)]),
        ];
        assert_eq!(simplify_disjuncts(disjuncts), vec![conj(&[])]);
    }

    /// Disjuncts over different literals or of different lengths are not
    /// merged.
    #[test]
    fn test_simplify_keeps_unmergeable_disjuncts() {
        let disjuncts = vec![conj(&[(0, true)]), conj(&[(1, true)])];
        assert_eq!(simplify_disjuncts(disjuncts.clone()), disjuncts);

        let disjuncts = vec![conj(&[(0, true)]), conj(&[(0, false), (1, true)])];
        assert_eq!(simplify_disjuncts(disjuncts.clone()), disjuncts);
    }

    /// Disjuncts differing in more than one sign are not merged.
    #[test]
    fn test_simplify_keeps_doubly_differing_disjuncts() {
        let disjuncts = vec![
            conj(&[(0, true), (1, true)]),
            conj(&[(0, false), (1, false)]),
        ];
        assert_eq!(simplify_disjuncts(disjuncts.clone()), disjuncts);
    }

    /// Identical disjuncts are deduplicated.
    #[test]
    fn test_simplify_drops_duplicate_disjuncts() {
        let disjuncts = vec![conj(&[(0, true)]), conj(&[(0, true)])];
        assert_eq!(simplify_disjuncts(disjuncts), vec![conj(&[(0, true)])]);
    }

    /// No clauses at all: the (empty) assignment vacuously satisfies
    /// everything, so a witness exists and is empty. This is what makes a
    /// plain unsolvable problem (no model, no blocking clauses) fail with
    /// the "<all environments>" cell.
    #[test]
    fn test_find_witness_no_clauses() {
        assert_eq!(find_witness(&[]), Some(vec![]));
    }

    /// `(x)` and `(not x)` are contradictory: no witness.
    #[test]
    fn test_find_witness_contradiction() {
        let clauses = vec![vec![literal(1, false)], vec![literal(1, true)]];
        assert_eq!(find_witness(&clauses), None);
    }

    /// `(x or y) and (not x)`: the search tries false first, so the witness
    /// is `x = false, y = true`.
    #[test]
    fn test_find_witness_false_first() {
        let clauses = vec![
            vec![literal(1, false), literal(2, false)],
            vec![literal(1, true)],
        ];
        assert_eq!(
            find_witness(&clauses),
            Some(vec![
                (VariableId::from_index(1), false),
                (VariableId::from_index(2), true),
            ])
        );
    }

    /// All four sign combinations of two variables blocked: no witness.
    #[test]
    fn test_find_witness_exhausted_space() {
        let clauses = vec![
            vec![literal(1, false), literal(2, false)],
            vec![literal(1, true), literal(2, false)],
            vec![literal(1, false), literal(2, true)],
            vec![literal(1, true), literal(2, true)],
        ];
        assert_eq!(find_witness(&clauses), None);
    }

    /// An unconstrained variable does not appear in the witness.
    #[test]
    fn test_find_witness_scope_is_clause_variables_only() {
        let clauses = vec![vec![literal(3, false)]];
        assert_eq!(
            find_witness(&clauses),
            Some(vec![(VariableId::from_index(3), true)])
        );
    }

    /// The indexed search tries `false` first: for `(x0 or x1)` the witness
    /// is `x0 = false, x1 = true`.
    #[test]
    fn test_find_witness_indexed_false_first() {
        let clauses = vec![vec![(0, false), (1, false)]];
        assert_eq!(
            find_witness_indexed_nested(2, &clauses),
            Some(vec![false, true])
        );
    }

    /// The indexed search assigns every variable, including ones that occur
    /// in no clause (they default to false).
    #[test]
    fn test_find_witness_indexed_assigns_unconstrained_variables() {
        let clauses = vec![vec![(1, true)]];
        assert_eq!(
            find_witness_indexed_nested(2, &clauses),
            Some(vec![false, false])
        );
    }

    /// An empty clause is unsatisfiable: no witness.
    #[test]
    fn test_find_witness_indexed_empty_clause() {
        assert_eq!(find_witness_indexed_nested(2, &[vec![]]), None);
    }

    /// Regression test for the unit-propagation search: deciding `x0 =
    /// false` propagates `x1 = true` and `x2 = true` into a conflict, and
    /// the propagated values must be unassigned on backtrack or the
    /// satisfiable `x0 = true` branch (with `x1 = false`) would wrongly
    /// fail. The result is the lexicographically smallest model, exactly
    /// what the propagation-free search returned.
    #[test]
    fn test_find_witness_indexed_propagation_undone_on_backtrack() {
        let clauses = vec![
            // (x0 or x1)
            vec![(0, false), (1, false)],
            // (not x1 or x2)
            vec![(1, true), (2, false)],
            // (not x2 or not x1)
            vec![(2, true), (1, true)],
        ];
        assert_eq!(
            find_witness_indexed_nested(3, &clauses),
            Some(vec![true, false, false])
        );
    }
}
