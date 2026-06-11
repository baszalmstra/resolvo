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
//!     Candidates, DependencyProvider, EnvLiteral, EnvLiteralKind, EnvironmentPackage,
//!     PackageCandidates, Solver, UniversalProblem, VersionSetRelation,
//! };
//!
//! // 1. Declare an environment package: a package whose value is unknown at
//! //    solve time (e.g. `cuda`). Instead of concrete candidate solvables,
//! //    `get_candidates` returns `PackageCandidates::Environment`.
//! impl DependencyProvider for MyProvider {
//!     async fn get_candidates(&self, name: NameId) -> Option<PackageCandidates> {
//!         if self.is_environment_package(name) {
//!             return Some(PackageCandidates::Environment(EnvironmentPackage {
//!                 // Machines without CUDA exist, so the absent literal is
//!                 // part of the environment space.
//!                 can_be_absent: true,
//!             }));
//!         }
//!         Some(Candidates { candidates: self.candidates_for(name), ..Default::default() }.into())
//!     }
//!
//!     // 2. Implement the relation oracle for version sets of environment
//!     //    packages. Soundness contract: answers other than `Unknown` must
//!     //    be correct; when in doubt return `Unknown`. A wrong `Disjoint`
//!     //    or `Subset` answer produces broken solutions, `Unknown` merely
//!     //    risks describing environment regions no real machine has.
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
//!
//!     // ... the remaining DependencyProvider methods as usual ...
//! }
//!
//! // 3. Build the problem: requirements as usual, plus an environment model
//! //    bounding the environment space the solution must cover. The model is
//! //    a CNF over signed environment literals; this one says "cuda is
//! //    absent, or cuda matches >=11". An empty model means "all
//! //    environments".
//! let model = vec![vec![
//!     (EnvLiteral { package: cuda_name, kind: EnvLiteralKind::Absent }, true),
//!     (EnvLiteral { package: cuda_name, kind: EnvLiteralKind::Matches(cuda_ge_11) }, true),
//! ]];
//! let problem = UniversalProblem::new()
//!     .requirements(requirements)
//!     .environment_model(model.clone());
//!
//! let mut solver = Solver::new(provider);
//! let solution = solver.solve_universal(problem)?;
//!
//! // 4. Consume the solution. `cells` partitions the environment space:
//! //    each cell pairs a region (a conjunction of signed environment
//! //    literals) with the solvables valid throughout that region.
//! for (condition, solvables) in &solution.cells {
//!     println!("{}: {solvables:?}", condition.display(solver.provider()));
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
//!     .seed_partition(solution.cells.iter().map(|(c, _)| c.clone()).collect());
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

use std::{any::Any, marker::PhantomData};

use crate::{
    CellCondition, ConditionalRequirement, DenseIndex, Dependencies, DependencyProvider,
    EnvLiteral, EnvLiteralKind, EnvironmentPackage, Interner, KnownDependencies, NameId,
    PackageCandidates, Presence, Requirement, SolvableId, VariableId, VersionSetId,
    VersionSetRelation,
    conflict::Conflict,
    internal::{id::ClauseId, solver_id::SolvableIdOrRoot},
    runtime::AsyncRuntime,
    solver::{
        Solver, SolverState, UnsolvableOrCancelled,
        clause::{Clause, EnvClauseKind, Literal, WatchedLiterals},
        decision::Decision,
        variable_map::VariableOrigin,
    },
    solver_id::IdMap,
};

/// Returns true when the variable represents an environment literal (either
/// a matches or an absent literal).
fn is_env_variable<D: DependencyProvider>(state: &SolverState<D>, variable: VariableId) -> bool {
    matches!(
        state.variable_map.origin(variable),
        VariableOrigin::EnvMatches(_) | VariableOrigin::EnvAbsent(_)
    )
}

/// An environment model: a CNF over signed environment literals. Each inner
/// `Vec` is a disjunction of `(literal, positive)` pairs; the conjunction of
/// all disjunctions bounds the environment space a universal solve must
/// cover. An empty CNF means "all environments".
pub type EnvironmentModel<N = NameId> = Vec<Vec<(EnvLiteral<N>, bool)>>;

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
    pub fn new() -> Self {
        Self {
            requirements: Vec::new(),
            constraints: Vec::new(),
            environment_model: Vec::new(),
            seed_partition: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Sets the requirements that _must_ have one candidate solvable included
    /// in the solution of every cell.
    pub fn requirements(self, requirements: Vec<ConditionalRequirement>) -> Self {
        Self {
            requirements,
            ..self
        }
    }

    /// Sets the additional constraints imposed on individual packages that
    /// the solvable (if any) chosen for that package _must_ adhere to.
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
    /// Seeds may only reference environment packages; an absent literal for
    /// a package declared with `can_be_absent: false` is also a caller error.
    /// Both panic with a clear message.
    pub fn seed_partition(self, seed_partition: Vec<CellCondition<N>>) -> Self {
        Self {
            seed_partition,
            ..self
        }
    }
}

/// The result of a successful [`Solver::solve_universal`] call.
#[derive(Debug)]
pub struct UniversalSolution<Id = SolvableId, N = NameId> {
    /// The enumerated cells: each pairs the conjunction of environment
    /// literals describing a region of the environment space with the
    /// solvables chosen for that region.
    ///
    /// The cells are pairwise disjoint, listed in deterministic enumeration
    /// order (the baseline cell first), and together cover the environment
    /// model.
    pub cells: Vec<(CellCondition<N>, Vec<Id>)>,

    /// The environment model the solve was bounded by. Stored so that
    /// [`UniversalSolution::verify`] can re-check model coverage without any
    /// solver state (e.g. on a solution reconstructed from a lockfile).
    pub environment_model: EnvironmentModel<N>,

    /// The dependency edges active in each cell, parallel to
    /// [`UniversalSolution::cells`] (entry `i` holds the edges of cell `i`).
    /// Captured at solve time because the solver state is reset by the next
    /// solve. Use [`UniversalSolution::edges`] for the aggregated view.
    pub cell_edges: Vec<Vec<CellEdge<Id>>>,
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
        for (_, solvables) in &self.cells {
            for &solvable in solvables {
                if !order.contains(&solvable) {
                    order.push(solvable);
                }
            }
        }
        order
            .into_iter()
            .map(|solvable| {
                let presence =
                    self.presence_for_cells(|index| self.cells[index].1.contains(&solvable));
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
        debug_assert_eq!(
            self.cell_edges.len(),
            self.cells.len(),
            "cell_edges must be parallel to cells"
        );
        let mut order: Vec<CellEdge<Id>> = Vec::new();
        for edges in &self.cell_edges {
            for &edge in edges {
                if !order.contains(&edge) {
                    order.push(edge);
                }
            }
        }
        order
            .into_iter()
            .map(|edge| {
                let presence =
                    self.presence_for_cells(|index| self.cell_edges[index].contains(&edge));
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
        D: DependencyProvider + Interner<SolvableId = Id, NameId = N>,
    {
        let mut violations = Vec::new();

        // Pairwise disjointness for cells whose solvable sets differ.
        for first in 0..self.cells.len() {
            for second in first + 1..self.cells.len() {
                if same_solvable_set(&self.cells[first].1, &self.cells[second].1) {
                    continue;
                }
                match prove_env_disjoint(provider, &self.cells[first].0, &self.cells[second].0) {
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
        D: DependencyProvider + Interner<NameId = N>,
    {
        // Collect the distinct environment literals of the model and the
        // cells, in deterministic first-occurrence order. Every collected
        // literal occurs in at least one clause below.
        let mut literals: Vec<EnvLiteral<N>> = Vec::new();
        let cell_literals = self
            .cells
            .iter()
            .flat_map(|(condition, _)| condition.0.iter());
        for (literal, _) in self.environment_model.iter().flatten().chain(cell_literals) {
            if !literals.contains(literal) {
                literals.push(literal.clone());
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
                if literals[i].package != literals[j].package {
                    continue;
                }
                match (&literals[i].kind, &literals[j].kind) {
                    (EnvLiteralKind::Matches(vs_i), EnvLiteralKind::Matches(vs_j)) => {
                        match provider.environment_version_set_relation(*vs_i, *vs_j) {
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
                    (EnvLiteralKind::Matches(_), EnvLiteralKind::Absent)
                    | (EnvLiteralKind::Absent, EnvLiteralKind::Matches(_)) => {
                        clauses.push(vec![(i, true), (j, true)]);
                    }
                    (EnvLiteralKind::Absent, EnvLiteralKind::Absent) => {
                        unreachable!("two absent literals of the same package are equal")
                    }
                }
            }
        }

        // The model clauses.
        for disjunction in &self.environment_model {
            clauses.push(
                disjunction
                    .iter()
                    .map(|(literal, positive)| (index_of(literal), !positive))
                    .collect(),
            );
        }

        // The negation of every cell condition: at least one of the cell's
        // literals must evaluate opposite. A cell with the empty condition
        // yields the empty (unsatisfiable) clause: it covers everything.
        for (condition, _) in &self.cells {
            clauses.push(
                condition
                    .0
                    .iter()
                    .map(|(literal, sign)| (index_of(literal), *sign))
                    .collect(),
            );
        }

        let assignment = find_witness_indexed(literals.len(), &clauses)?;
        Some(CellCondition(
            literals.into_iter().zip(assignment).collect(),
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
        let mut found: Option<&[Id]> = None;
        for (condition, solvables) in &self.cells {
            let matches = condition
                .0
                .iter()
                .all(|(literal, sign)| eval(literal) == *sign);
            if !matches {
                continue;
            }
            debug_assert!(
                found.is_none(),
                "broken invariant: multiple cells match the same environment"
            );
            if found.is_none() {
                found = Some(solvables);
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
            return Presence(vec![CellCondition(Vec::new())]);
        }
        let disjuncts = (0..self.cells.len())
            .filter(|&index| member(index))
            .map(|index| self.cells[index].0.clone())
            .collect();
        Presence(simplify_disjuncts(disjuncts))
    }
}

/// The errors of an unsuccessful [`Solver::solve_universal`] call.
#[derive(Debug)]
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
    /// The solving process was cancelled.
    Cancelled(Box<dyn Any>),
}

impl<D: DependencyProvider, RT: AsyncRuntime> Solver<D, RT> {
    /// Solves the given [`UniversalProblem`], producing a partition of the
    /// environment model into cells, each with the solvables valid throughout
    /// that cell.
    ///
    /// See the module documentation and the universal-solve design document
    /// for the underlying algorithm.
    #[allow(clippy::type_complexity)]
    pub fn solve_universal(
        &mut self,
        problem: UniversalProblem<D::SolvableId, D::NameId>,
    ) -> Result<UniversalSolution<D::SolvableId, D::NameId>, UniversalFailure<D::NameId>> {
        // Re-initialize the solver state, like `solve` does. One state is
        // shared across the whole enumeration loop: the formula only grows,
        // so learnt clauses and interned variables stay valid across cells.
        self.state = SolverState::default();

        let root_dependencies = Dependencies::Known(KnownDependencies {
            requirements: problem.requirements,
            constrains: problem.constraints,
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
        let environment_model = problem.environment_model;
        self.encode_environment_model(&environment_model)?;

        let mut cells = Vec::new();
        // The dependency edges of each recorded cell, captured while the
        // cell's assignment is still on the decision stack.
        let mut cell_edges = Vec::new();
        // The same cells in solver variable space, used for disjointness
        // checks against new cells.
        let mut cell_assignments: Vec<Vec<(VariableId, bool)>> = Vec::new();

        // Cells from a previous solve are solved first, in order, each under
        // its condition pushed as assumption decisions (design doc 5.7). When
        // the seeds run out the loop continues as free enumeration.
        let mut pending_seeds = problem.seed_partition.into_iter();

        loop {
            // Set up the assumptions of the next viable seed, if any remain.
            // Seeds that contribute no assumptions are skipped: an empty
            // condition describes the whole environment space (which free
            // enumeration covers anyway) and a self-contradictory condition
            // describes no environment at all.
            let mut seeded = false;
            while !seeded {
                let Some(seed) = pending_seeds.next() else {
                    break;
                };
                seeded = self.push_seed_assumptions(&seed)?;
            }

            match self.run_sat(SolvableIdOrRoot::root(), &root_dependencies) {
                Ok(true) => {
                    // Extract the load-bearing environment literal
                    // assignments and restore provable disjointness against
                    // all previously recorded cells. For a seeded cell the
                    // extraction may produce a condition that is MORE GENERAL
                    // than the seed: the recorded cell reflects what the new
                    // solution actually depends on, so a stale over-specific
                    // seed heals (the repair step still re-specializes where
                    // needed to keep cells disjoint).
                    let mut cell = self.extract_cell();
                    self.repair_disjointness(&mut cell, &cell_assignments);

                    let condition = self.cell_to_condition(&cell);
                    let solvables = self.chosen_solvables_canonical();
                    let cell_is_empty = cell.is_empty();
                    cells.push((condition, solvables));
                    cell_edges.push(self.capture_cell_edges());
                    cell_assignments.push(cell.clone());

                    // Retract all decisions (assumptions included) before
                    // adding the blocking clause so that its watch
                    // initialization sees no decisions.
                    self.state.assumption_levels = 0;
                    self.state.decision_tracker.undo_until(0);

                    if cell_is_empty {
                        // No environment literal was load bearing: the
                        // solution is valid in every environment, so coverage
                        // is complete. (An empty cell can only be the first
                        // cell; the repair step extends any later cell.)
                        break;
                    }

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
                            let scoped_conflict =
                                self.run_scoped_conflict(&witness, conflict, &root_dependencies);
                            return Err(UniversalFailure::Unsolvable {
                                cell: self.cell_to_condition(&witness),
                                conflict: scoped_conflict,
                            });
                        }
                    }
                }
            }
        }

        Ok(UniversalSolution {
            cells,
            environment_model,
            cell_edges,
        })
    }

    /// Performs a scoped re-solve to produce a conflict that is tightly
    /// bound to the witness region identified by `witness`.
    ///
    /// The formula is already UNSAT (the `fallback` conflict was just
    /// produced). Adding unit [`EnvClauseKind::Model`] clauses that pin each
    /// witness literal to its witnessed value keeps it UNSAT while scoping
    /// the CDCL to the specific region. The resulting conflict graph will
    /// only reference clauses that matter in that region.
    ///
    /// If for any reason the re-solve produces `Ok` (which should not happen
    /// for a well-formed witness), the `fallback` conflict is returned
    /// unchanged.
    fn run_scoped_conflict(
        &mut self,
        witness: &[(VariableId, bool)],
        fallback: Conflict,
        root_dependencies: &Dependencies,
    ) -> Conflict {
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
        match self.run_sat(SolvableIdOrRoot::root(), root_dependencies) {
            Err(UnsolvableOrCancelled::Unsolvable(scoped)) => scoped,
            // Unexpected: fall back to the unscoped conflict.
            _ => fallback,
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

    /// Captures the dependency edges that are active in the current solution
    /// (design doc 5.8, "conditional edges"). Must be called while the
    /// solution's decisions are still on the decision stack.
    ///
    /// An edge exists for every requires clause whose parent (a solvable or
    /// the root) is installed and whose condition holds, where "holds" means
    /// every condition complement literal evaluates to false under the
    /// undecided-counts-as-false completion. The edge's target is the first
    /// installed candidate of the requirement, or `None` when the requirement
    /// is on an environment package (its candidate is an environment literal,
    /// not a solvable).
    fn capture_cell_edges(&self) -> Vec<CellEdge<D::SolvableId>> {
        let state = &self.state;
        let decision_map = state.decision_tracker.map();
        let mut edges: Vec<CellEdge<D::SolvableId>> = Vec::new();

        for (&parent_var, requirements) in state.requires_clauses.iter() {
            if state.decision_tracker.assigned_value(parent_var) != Some(true) {
                continue;
            }
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
                // disjunction evaluates to false under the
                // undecided-counts-as-false completion.
                if let Some(disjunction) = *disjunction {
                    let condition_holds = state.disjunctions[disjunction]
                        .literals
                        .iter()
                        .all(|literal| !literal.eval(decision_map).unwrap_or(literal.negate()));
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
                if !edges.contains(&edge) {
                    edges.push(edge);
                }
            }
        }

        edges
    }

    /// Encodes the environment model CNF as [`Clause::EnvClause`] clauses.
    ///
    /// For every package referenced by a model literal the candidates are
    /// resolved and required to be [`PackageCandidates::Environment`]; the
    /// literal variables are interned together with their oracle consistency
    /// clauses.
    fn encode_environment_model(
        &mut self,
        model: &EnvironmentModel<D::NameId>,
    ) -> Result<(), UniversalFailure<D::NameId>> {
        for disjunction in model {
            assert!(
                !disjunction.is_empty(),
                "the environment model contains an empty disjunction, which makes the model \
                 unsatisfiable"
            );

            let mut literals = Vec::with_capacity(disjunction.len());
            for (env_literal, positive) in disjunction {
                let variable = match env_literal.kind {
                    EnvLiteralKind::Matches(version_set) => {
                        let package_name = self.cache.provider().version_set_name(version_set);
                        assert!(
                            package_name == env_literal.package,
                            "environment model literal for package '{}' references version set \
                             '{}' which belongs to package '{}'",
                            self.provider().display_name(env_literal.package),
                            self.provider().display_version_set(version_set),
                            self.provider().display_name(package_name),
                        );
                        self.declare_environment_package(package_name, "environment model")?;
                        self.state.intern_env_matches_with_oracle_clauses(
                            self.cache.provider(),
                            version_set,
                            package_name,
                        )
                    }
                    EnvLiteralKind::Absent => {
                        let package_name = env_literal.package;
                        let env_pkg =
                            self.declare_environment_package(package_name, "environment model")?;
                        assert!(
                            env_pkg.can_be_absent,
                            "the environment model contains an absent literal for package '{}' \
                             which was declared with `can_be_absent: false`",
                            self.provider().display_name(package_name),
                        );
                        self.state
                            .intern_env_absent_with_oracle_clauses(package_name)
                    }
                };
                // A positive model literal asserts the variable, a negative
                // one its negation.
                literals.push(Literal::new(variable, !positive));
            }
            self.state.add_env_clause(literals, EnvClauseKind::Model);
        }
        Ok(())
    }

    /// Resolves the candidates for `package_name` and requires the package to
    /// be an environment package. Records its metadata in the solver state
    /// (mirroring what the encoder does in `on_candidates_available`).
    ///
    /// `context` names the input that referenced the package (the
    /// environment model or the seed partition) for the panic message.
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
            PackageCandidates::Candidates(_) => panic!(
                "the {context} references package '{}' which is not an environment package; only \
                 packages declared via `PackageCandidates::Environment` can appear in the \
                 {context}",
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
    /// # Panics
    ///
    /// Panics when the seed references a package that is not an environment
    /// package, contains an absent literal for a package declared with
    /// `can_be_absent: false`, or pairs a package with a version set that
    /// belongs to a different package. These are caller errors, mirroring
    /// the environment model validation.
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
        let mut assumptions = Vec::with_capacity(seed.0.len());
        for (env_literal, sign) in &seed.0 {
            let variable = match env_literal.kind {
                EnvLiteralKind::Matches(version_set) => {
                    let package_name = self.cache.provider().version_set_name(version_set);
                    assert!(
                        package_name == env_literal.package,
                        "seed partition literal for package '{}' references version set '{}' \
                         which belongs to package '{}'",
                        self.provider().display_name(env_literal.package),
                        self.provider().display_version_set(version_set),
                        self.provider().display_name(package_name),
                    );
                    self.declare_environment_package(package_name, "seed partition")?;
                    self.state.intern_env_matches_with_oracle_clauses(
                        self.cache.provider(),
                        version_set,
                        package_name,
                    )
                }
                EnvLiteralKind::Absent => {
                    let package_name = env_literal.package;
                    let env_pkg =
                        self.declare_environment_package(package_name, "seed partition")?;
                    assert!(
                        env_pkg.can_be_absent,
                        "the seed partition contains an absent literal for package '{}' which \
                         was declared with `can_be_absent: false`",
                        self.provider().display_name(package_name),
                    );
                    self.state
                        .intern_env_absent_with_oracle_clauses(package_name)
                }
            };
            assumptions.push((variable, *sign));
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
    /// Scans only the indexed support clauses
    /// (`SolverState::env_support_clauses`). Oracle consistency, model and
    /// blocking clauses never contribute support: the former two are
    /// tautologies over the modeled environment space and the latter are
    /// handled by the disjointness repair. Learnt clauses are implied by the
    /// other clauses and never contribute either.
    fn extract_cell(&self) -> Vec<(VariableId, bool)> {
        let state = &self.state;
        let decision_map = state.decision_tracker.map();
        let mut cell: Vec<(VariableId, bool)> = Vec::new();

        let record =
            |cell: &mut Vec<(VariableId, bool)>, variable: VariableId, value: bool| match cell
                .iter()
                .find(|&&(v, _)| v == variable)
            {
                Some(&(_, existing)) => debug_assert_eq!(
                    existing, value,
                    "bug: a cell literal was recorded with both signs"
                ),
                None => cell.push((variable, value)),
            };

        for &clause_id in &state.env_support_clauses {
            match state.clauses.kinds[clause_id.to_index()] {
                Clause::Requires(parent, disjunction, requirement) => {
                    // Parent not installed: the clause is satisfied by
                    // `not parent` in every environment, no support needed.
                    if state.decision_tracker.assigned_value(parent) != Some(true) {
                        continue;
                    }

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
                            ) {
                                record(&mut cell, candidate, true);
                            }
                            satisfied = true;
                            break;
                        }
                    }
                    if satisfied {
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

                    // If a concrete complement literal holds under the
                    // undecided-counts-as-false completion, the clause is
                    // satisfied in every environment of the cell.
                    let satisfied_by_concrete = literals.iter().any(|literal| {
                        !is_env_variable(state, literal.variable())
                            && literal.eval(decision_map).unwrap_or(literal.negate())
                    });
                    if satisfied_by_concrete {
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
                    // Env literals that are assigned TRUE must not be
                    // recorded: their complement is false and contributes
                    // nothing (e.g. an AND condition where another clause
                    // forces one of the literals true).
                    let supporting = literals.iter().find(|literal| {
                        is_env_variable(state, literal.variable())
                            && state.decision_tracker.assigned_value(literal.variable())
                                != Some(true)
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
                    record(&mut cell, literal.variable(), false);
                }
                Clause::EnvConstrains(env_constrains_id) => {
                    let payload = state.env_constrains[env_constrains_id];
                    if state.decision_tracker.assigned_value(payload.parent) != Some(true) {
                        continue;
                    }
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
                        record(
                            &mut cell,
                            payload.absent_var.expect("checked by absent_true"),
                            true,
                        );
                    } else if matches_true {
                        record(&mut cell, payload.matches_var, true);
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

        cell.sort_by_key(|&(variable, _)| variable.to_index());
        cell
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
    fn repair_disjointness(
        &self,
        cell: &mut Vec<(VariableId, bool)>,
        earlier_cells: &[Vec<(VariableId, bool)>],
    ) {
        for earlier in earlier_cells {
            if self.provably_disjoint(cell, earlier) {
                continue;
            }

            let distinguishing = earlier.iter().find(|&&(variable, value)| {
                let current = self.state.decision_tracker.assigned_value(variable) == Some(true);
                current != value
            });
            let &(variable, value) = distinguishing.expect(
                "bug: the earlier cell's blocking clause is satisfied by the current \
                 assignment, so a distinguishing literal must exist",
            );
            // The current assignment's value for the variable is the opposite
            // of the earlier cell's sign.
            cell.push((variable, !value));
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
                        // sets of the same package.
                        if provider.version_set_name(vs_a) == provider.version_set_name(vs_b)
                            && provider.environment_version_set_relation(vs_a, vs_b)
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
        CellCondition(
            cell.iter()
                .map(|&(variable, value)| {
                    let literal = match self.state.variable_map.origin(variable) {
                        VariableOrigin::EnvMatches(version_set) => EnvLiteral {
                            package: self.provider().version_set_name(version_set),
                            kind: EnvLiteralKind::Matches(version_set),
                        },
                        VariableOrigin::EnvAbsent(package) => EnvLiteral {
                            package,
                            kind: EnvLiteralKind::Absent,
                        },
                        _ => unreachable!("cell literals are always environment literal variables"),
                    };
                    (literal, value)
                })
                .collect(),
        )
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
    fn find_environment_witness(&self) -> Option<Vec<(VariableId, bool)>> {
        let mut clauses: Vec<Vec<Literal>> = Vec::new();
        for kind in &self.state.clauses.kinds {
            match *kind {
                Clause::EnvOracleConsistency(lit_a, lit_b) => clauses.push(vec![lit_a, lit_b]),
                Clause::EnvClause(env_clause_id) => {
                    clauses.push(self.state.env_clauses[env_clause_id].literals.clone());
                }
                _ => {}
            }
        }
        find_witness(&clauses)
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
    D: DependencyProvider + Interner<NameId = N>,
{
    let mut unknown_involved = false;
    for (lit_a, sign_a) in &a.0 {
        for (lit_b, sign_b) in &b.0 {
            if lit_a == lit_b {
                if sign_a != sign_b {
                    return Disjointness::Disjoint;
                }
                continue;
            }
            if !(*sign_a && *sign_b) || lit_a.package != lit_b.package {
                continue;
            }
            match (&lit_a.kind, &lit_b.kind) {
                (EnvLiteralKind::Matches(vs_a), EnvLiteralKind::Matches(vs_b)) => {
                    match provider.environment_version_set_relation(*vs_a, *vs_b) {
                        VersionSetRelation::Disjoint => return Disjointness::Disjoint,
                        VersionSetRelation::Unknown => unknown_involved = true,
                        VersionSetRelation::Subset
                        | VersionSetRelation::Superset
                        | VersionSetRelation::Equal => {}
                    }
                }
                (EnvLiteralKind::Matches(_), EnvLiteralKind::Absent)
                | (EnvLiteralKind::Absent, EnvLiteralKind::Matches(_)) => {
                    return Disjointness::Disjoint;
                }
                (EnvLiteralKind::Absent, EnvLiteralKind::Absent) => {
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
                if merged.0.is_empty() {
                    // The merged disjunct holds in every environment, which
                    // makes every other disjunct redundant.
                    return vec![CellCondition(Vec::new())];
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
    if a.0.len() != b.0.len() {
        return None;
    }
    let mut differing = None;
    for (index, (literal, sign)) in a.0.iter().enumerate() {
        let (_, b_sign) = b.0.iter().find(|(b_literal, _)| b_literal == literal)?;
        if sign != b_sign {
            if differing.is_some() {
                return None;
            }
            differing = Some(index);
        }
    }
    // Equal lengths and every literal of `a` found in `b`: with no duplicate
    // literals this is a bijection, so the literal sets are identical.
    let merged = match differing {
        None => a.0.clone(),
        Some(drop_index) => {
            a.0.iter()
                .enumerate()
                .filter(|&(index, _)| index != drop_index)
                .map(|(_, literal)| literal.clone())
                .collect()
        }
    };
    Some(CellCondition(merged))
}

/// A signed literal over a dense witness-search variable index: `(index,
/// negate)`. The literal evaluates to true when the variable at `index` is
/// assigned the opposite of `negate`.
type IndexedLiteral = (usize, bool);

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
fn find_witness_indexed(
    variable_count: usize,
    clauses: &[Vec<IndexedLiteral>],
) -> Option<Vec<bool>> {
    debug_assert!(
        clauses
            .iter()
            .flatten()
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
    for &(index, _) in clauses.iter().flatten() {
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
    clauses: &[Vec<IndexedLiteral>],
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
    clauses: &[Vec<IndexedLiteral>],
    assignment: &mut [Option<bool>],
    propagated: &mut Vec<usize>,
) -> bool {
    loop {
        let mut changed = false;
        for clause in clauses {
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

/// Searches for an assignment of solver variables satisfying all the given
/// clauses by mapping the variables onto a dense index for
/// [`find_witness_indexed`].
///
/// The search scope is exactly the variables that occur in the clauses;
/// unconstrained variables are left out of the witness, mirroring how cells
/// record only load-bearing literals.
fn find_witness(clauses: &[Vec<Literal>]) -> Option<Vec<(VariableId, bool)>> {
    let mut variables: Vec<VariableId> = clauses
        .iter()
        .flatten()
        .map(|literal| literal.variable())
        .collect();
    variables.sort_by_key(|variable| variable.to_index());
    variables.dedup();

    let index_of = |variable: VariableId| {
        variables
            .binary_search_by_key(&variable.to_index(), |v| v.to_index())
            .expect("every clause variable is in `variables`")
    };
    let indexed_clauses: Vec<Vec<IndexedLiteral>> = clauses
        .iter()
        .map(|clause| {
            clause
                .iter()
                .map(|literal| (index_of(literal.variable()), literal.negate()))
                .collect()
        })
        .collect();

    let assignment = find_witness_indexed(variables.len(), &indexed_clauses)?;
    Some(variables.into_iter().zip(assignment).collect())
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
        for (condition, solvables) in &solution.cells {
            let solvables = solvables
                .iter()
                .map(|&s| solver.provider().display_solvable(s).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                out,
                "{} -> [{}]",
                condition.display(solver.provider()),
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
        let glibc_name = provider.pool.intern_package_name("glibc");
        let pkg_2 = provider.add_package("pkg", 2);
        let pkg_1 = provider.add_package("pkg", 1);
        provider.set_dependencies(pkg_2, vec![glibc_228.into()], vec![]);
        provider.set_dependencies(pkg_1, vec![glibc_217.into()], vec![]);
        let pkg_any = provider.version_set("pkg", 0, 3);

        let mut solver = Solver::new(provider);
        let problem = UniversalProblem::new()
            .requirements(vec![pkg_any.into()])
            .environment_model(vec![vec![(
                EnvLiteral {
                    package: glibc_name,
                    kind: EnvLiteralKind::Matches(glibc_217),
                },
                true,
            )]]);
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
            .environment_model(vec![vec![
                (
                    EnvLiteral {
                        package: cuda_name,
                        kind: EnvLiteralKind::Absent,
                    },
                    true,
                ),
                (
                    EnvLiteral {
                        package: cuda_name,
                        kind: EnvLiteralKind::Matches(cuda_11),
                    },
                    true,
                ),
            ]]);
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
        let glibc_name = provider.pool.intern_package_name("glibc");
        let pkg_2 = provider.add_package("pkg", 2);
        let pkg_1 = provider.add_package("pkg", 1);
        provider.set_dependencies(pkg_2, vec![glibc_228.into()], vec![]);
        provider.set_dependencies(pkg_1, vec![glibc_217.into()], vec![]);
        let pkg_any = provider.version_set("pkg", 0, 3);

        let mut solver = Solver::new(provider);
        let problem = UniversalProblem::new()
            .requirements(vec![pkg_any.into()])
            .environment_model(vec![vec![(
                EnvLiteral {
                    package: glibc_name,
                    kind: EnvLiteralKind::Matches(glibc_217),
                },
                true,
            )]]);
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
        let matches_lit = EnvLiteral {
            package: cuda_name,
            kind: EnvLiteralKind::Matches(cuda_11),
        };
        let absent_lit = EnvLiteral {
            package: cuda_name,
            kind: EnvLiteralKind::Absent,
        };

        let solution: UniversalSolution = UniversalSolution {
            cells: vec![(CellCondition(vec![(absent_lit.clone(), true)]), vec![a])],
            environment_model: vec![vec![
                (matches_lit.clone(), true),
                (absent_lit.clone(), true),
            ]],
            cell_edges: vec![vec![]],
        };

        assert_eq!(
            solution.verify(&provider),
            Err(vec![Violation::UncoveredRegion(CellCondition(vec![
                (matches_lit, true),
                (absent_lit, false),
            ]))])
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
        let cuda_name = provider.pool.intern_package_name("cuda");
        let a1 = provider.add_package("a", 1);
        let a2 = provider.add_package("a", 2);

        let solution: UniversalSolution = UniversalSolution {
            cells: vec![
                (CellCondition(vec![]), vec![a1]),
                (
                    CellCondition(vec![(
                        EnvLiteral {
                            package: cuda_name,
                            kind: EnvLiteralKind::Matches(cuda_11),
                        },
                        true,
                    )]),
                    vec![a2],
                ),
            ],
            environment_model: vec![],
            cell_edges: vec![vec![], vec![]],
        };

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
        let cuda_name = provider.pool.intern_package_name("cuda");
        let a1 = provider.add_package("a", 1);
        let a2 = provider.add_package("a", 2);
        let lit_0_5 = EnvLiteral {
            package: cuda_name,
            kind: EnvLiteralKind::Matches(cuda_0_5),
        };
        let lit_3_8 = EnvLiteral {
            package: cuda_name,
            kind: EnvLiteralKind::Matches(cuda_3_8),
        };

        let solution: UniversalSolution = UniversalSolution {
            cells: vec![
                (CellCondition(vec![(lit_0_5.clone(), true)]), vec![a1]),
                (CellCondition(vec![(lit_3_8.clone(), true)]), vec![a2]),
            ],
            environment_model: vec![vec![(lit_0_5, true), (lit_3_8, true)]],
            cell_edges: vec![vec![], vec![]],
        };

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
        let cuda_name = provider.pool.intern_package_name("cuda");
        let a = provider.add_package("a", 1);

        let solution: UniversalSolution = UniversalSolution {
            cells: vec![
                (CellCondition(vec![]), vec![a]),
                (
                    CellCondition(vec![(
                        EnvLiteral {
                            package: cuda_name,
                            kind: EnvLiteralKind::Matches(cuda_11),
                        },
                        true,
                    )]),
                    vec![a],
                ),
            ],
            environment_model: vec![],
            cell_edges: vec![vec![], vec![]],
        };

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
        let cuda_name = provider.pool.intern_package_name("cuda");
        let a1 = provider.add_package("a", 1);
        let a2 = provider.add_package("a", 2);
        let lit_0_2 = EnvLiteral {
            package: cuda_name,
            kind: EnvLiteralKind::Matches(cuda_0_2),
        };
        let lit_5_9 = EnvLiteral {
            package: cuda_name,
            kind: EnvLiteralKind::Matches(cuda_5_9),
        };

        let solution: UniversalSolution = UniversalSolution {
            cells: vec![
                (CellCondition(vec![(lit_0_2.clone(), true)]), vec![a1]),
                (CellCondition(vec![(lit_5_9.clone(), true)]), vec![a2]),
            ],
            environment_model: vec![vec![(lit_0_2, true), (lit_5_9, true)]],
            cell_edges: vec![vec![], vec![]],
        };

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
        let glibc_name = provider.pool.intern_package_name("glibc");
        let pkg_2 = provider.add_package("pkg", 2);
        let pkg_1 = provider.add_package("pkg", 1);
        provider.set_dependencies(pkg_2, vec![glibc_228.into()], vec![]);
        provider.set_dependencies(pkg_1, vec![glibc_217.into()], vec![]);
        let pkg_any = provider.version_set("pkg", 0, 3);

        let mut solver = Solver::new(provider);
        let problem = UniversalProblem::new()
            .requirements(vec![pkg_any.into()])
            .environment_model(vec![vec![(
                EnvLiteral {
                    package: glibc_name,
                    kind: EnvLiteralKind::Matches(glibc_217),
                },
                true,
            )]]);
        let solution = solver.solve_universal(problem).expect("solvable");

        let provider = solver.provider();
        let eval_for = |glibc_version: u32| {
            move |literal: &EnvLiteral<NameId>| match literal.kind {
                EnvLiteralKind::Matches(version_set) => provider
                    .pool
                    .resolve_version_set(version_set)
                    .contains(glibc_version),
                EnvLiteralKind::Absent => false,
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

        let solution: UniversalSolution = UniversalSolution {
            cells: vec![
                (CellCondition(vec![]), vec![a1]),
                (CellCondition(vec![]), vec![a2]),
            ],
            environment_model: vec![],
            cell_edges: vec![vec![], vec![]],
        };

        let _ = solution.project(|_| false);
    }

    /// Helper for the simplification unit tests: a matches literal for
    /// version set index `vs` of a single shared package.
    fn env_lit(vs: usize) -> EnvLiteral<NameId> {
        EnvLiteral {
            package: NameId::from_index(0),
            kind: EnvLiteralKind::Matches(VersionSetId::from_index(vs)),
        }
    }

    /// Helper for the simplification unit tests: a conjunction of signed
    /// literals given as `(version set index, sign)` pairs.
    fn conj(literals: &[(usize, bool)]) -> CellCondition<NameId> {
        CellCondition(
            literals
                .iter()
                .map(|&(vs, sign)| (env_lit(vs), sign))
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
        assert_eq!(find_witness_indexed(2, &clauses), Some(vec![false, true]));
    }

    /// The indexed search assigns every variable, including ones that occur
    /// in no clause (they default to false).
    #[test]
    fn test_find_witness_indexed_assigns_unconstrained_variables() {
        let clauses = vec![vec![(1, true)]];
        assert_eq!(find_witness_indexed(2, &clauses), Some(vec![false, false]));
    }

    /// An empty clause is unsatisfiable: no witness.
    #[test]
    fn test_find_witness_indexed_empty_clause() {
        assert_eq!(find_witness_indexed(2, &[vec![]]), None);
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
            find_witness_indexed(3, &clauses),
            Some(vec![true, false, false])
        );
    }
}
