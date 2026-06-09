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

use std::{any::Any, marker::PhantomData};

use crate::{
    CellCondition, ConditionalRequirement, DenseIndex, Dependencies, DependencyProvider,
    EnvLiteral, EnvLiteralKind, EnvironmentPackage, KnownDependencies, NameId, PackageCandidates,
    SolvableId, VariableId, VersionSetId, VersionSetRelation,
    conflict::Conflict,
    internal::{id::ClauseId, solver_id::SolvableIdOrRoot},
    runtime::AsyncRuntime,
    solver::{
        Solver, SolverState, UnsolvableOrCancelled,
        clause::{Clause, EnvClauseKind, Literal, WatchedLiterals},
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
        /// The conflict produced by the failing solve.
        ///
        /// Note: in M2 the conflict is not yet scoped to the witness region;
        /// it is the conflict of the unconstrained re-solve. Precisely
        /// scoping it requires solving under assumptions (M4).
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
        self.encode_environment_model(&problem.environment_model)?;

        let mut cells = Vec::new();
        // The same cells in solver variable space, used for disjointness
        // checks against new cells.
        let mut cell_assignments: Vec<Vec<(VariableId, bool)>> = Vec::new();

        loop {
            match self.run_sat(SolvableIdOrRoot::root(), &root_dependencies) {
                Ok(success) => {
                    assert!(
                        success,
                        "bug: run_sat for the root either succeeds or returns an error"
                    );

                    // Extract the load-bearing environment literal
                    // assignments and restore provable disjointness against
                    // all previously recorded cells.
                    let mut cell = self.extract_cell();
                    self.repair_disjointness(&mut cell, &cell_assignments);

                    let condition = self.cell_to_condition(&cell);
                    let solvables = self.state.chosen_solvables().collect();
                    let cell_is_empty = cell.is_empty();
                    cells.push((condition, solvables));
                    cell_assignments.push(cell.clone());

                    // Retract all decisions before adding the blocking clause
                    // so that its watch initialization sees no decisions.
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
                    // it forbids the variable to take here.
                    let blocking = cell
                        .iter()
                        .map(|&(variable, value)| Literal::new(variable, value))
                        .collect();
                    self.state.add_env_clause(blocking, EnvClauseKind::Blocking);
                }
                Err(UnsolvableOrCancelled::Cancelled(value)) => {
                    return Err(UniversalFailure::Cancelled(value));
                }
                Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
                    // The formula is unsolvable. Check whether any region of
                    // the environment model remains uncovered.
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
                            return Err(UniversalFailure::Unsolvable {
                                cell: self.cell_to_condition(&witness),
                                conflict,
                            });
                        }
                    }
                }
            }
        }

        Ok(UniversalSolution { cells })
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
                        self.declare_environment_package(package_name)?;
                        self.state.intern_env_matches_with_oracle_clauses(
                            self.cache.provider(),
                            version_set,
                            package_name,
                        )
                    }
                    EnvLiteralKind::Absent => {
                        let package_name = env_literal.package;
                        let env_pkg = self.declare_environment_package(package_name)?;
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
    fn declare_environment_package(
        &mut self,
        package_name: D::NameId,
    ) -> Result<EnvironmentPackage, UniversalFailure<D::NameId>> {
        let package_candidates = self
            .async_runtime
            .block_on(self.cache.get_or_cache_candidates(package_name))
            .map_err(UniversalFailure::Cancelled)?;
        let env_pkg = match package_candidates {
            PackageCandidates::Environment(env_pkg) => *env_pkg,
            PackageCandidates::Candidates(_) => panic!(
                "the environment model references package '{}' which is not an environment \
                 package; only packages declared via `PackageCandidates::Environment` can appear \
                 in the environment model",
                self.provider().display_name(package_name),
            ),
        };
        self.state.env_packages.set(package_name, Some(env_pkg));
        Ok(env_pkg)
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

                    // Otherwise the environment condition literals carry the
                    // support: the clause is satisfied by `not L` being true,
                    // so record `L = false` for the env condition literals.
                    // `L` may be merely undecided; undecided counts as false
                    // and must still be recorded (soundness critical: a later
                    // environment where `L` is true would activate the
                    // requirement, which this solution does not satisfy).
                    let mut recorded_any = false;
                    for literal in literals {
                        if is_env_variable(state, literal.variable()) {
                            debug_assert!(
                                literal.negate(),
                                "bug: condition complements of env literals are negative literals"
                            );
                            debug_assert_ne!(
                                state.decision_tracker.assigned_value(literal.variable()),
                                Some(true),
                                "bug: the condition complement cannot support the clause if the \
                                 env literal is true"
                            );
                            record(&mut cell, literal.variable(), false);
                            recorded_any = true;
                        }
                    }
                    debug_assert!(
                        recorded_any,
                        "bug: requires clause with installed parent has neither an installed \
                         candidate nor a satisfying condition complement"
                    );
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

/// A dedicated backtracking search for an assignment satisfying all the
/// given clauses. The number of environment literals is small, so a simple
/// exhaustive search with clause-violation pruning is sufficient; the main
/// CDCL machinery is deliberately not reused here (design doc 5.5).
///
/// Variables are assigned in ascending variable-id order and `false` is
/// tried first, matching the split policy (environment literals default to
/// false), so the witness stays as close to the baseline machine as
/// possible and the search is deterministic.
fn find_witness(clauses: &[Vec<Literal>]) -> Option<Vec<(VariableId, bool)>> {
    let mut variables: Vec<VariableId> = clauses
        .iter()
        .flatten()
        .map(|literal| literal.variable())
        .collect();
    variables.sort_by_key(|variable| variable.to_index());
    variables.dedup();

    let mut assignment: Vec<Option<bool>> = vec![None; variables.len()];
    if search(clauses, &variables, &mut assignment, 0) {
        Some(
            variables
                .iter()
                .zip(assignment)
                .map(|(&variable, value)| {
                    (variable, value.expect("the search assigns every variable"))
                })
                .collect(),
        )
    } else {
        None
    }
}

/// Recursive helper of [`find_witness`]: tries to extend the partial
/// `assignment` from `depth` onwards. Returns true when a satisfying total
/// assignment was found (left in `assignment`).
fn search(
    clauses: &[Vec<Literal>],
    variables: &[VariableId],
    assignment: &mut [Option<bool>],
    depth: usize,
) -> bool {
    // Prune: a clause whose literals are all assigned and false can never
    // be satisfied by extending the assignment.
    let value_of = |variable: VariableId, assignment: &[Option<bool>]| {
        let index = variables
            .binary_search_by_key(&variable.to_index(), |v| v.to_index())
            .expect("every clause variable is in `variables`");
        assignment[index]
    };
    let violated = clauses.iter().any(|clause| {
        clause.iter().all(|literal| {
            match value_of(literal.variable(), assignment) {
                // The literal evaluates to false.
                Some(value) => value == literal.negate(),
                // Unassigned: the clause is not yet violated.
                None => false,
            }
        })
    });
    if violated {
        return false;
    }

    if depth == variables.len() {
        return true;
    }

    for value in [false, true] {
        assignment[depth] = Some(value);
        if search(clauses, variables, assignment, depth + 1) {
            return true;
        }
    }
    assignment[depth] = None;
    false
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
}
