use std::{
    fmt::{Debug, Display, Formatter},
    iter,
    num::NonZeroU32,
    ops::ControlFlow,
};

use crate::requirement::RequirementMap;
use crate::solver::conditions::Disjunction;
use crate::{
    DenseIndex, Interner, Requirement, SolverId, StringId, VariableId, VersionSetId,
    internal::{
        arena::Arena,
        id::{ClauseId, EnvClauseId, EnvConstrainsId, LearntClauseId},
    },
    solver::{
        conditions::DisjunctionId, decision_map::DecisionMap, decision_tracker::DecisionTracker,
        variable_map::VariableMap,
    },
};

/// Represents a single clause in the SAT problem
///
/// # SAT terminology
///
/// Clauses consist of disjunctions of literals (i.e. a non-empty list of
/// variables, potentially negated, joined by the logical "or" operator). Here
/// are some examples:
///
/// - (¬A ∨ ¬B)
/// - (¬A ∨ ¬B ∨ ¬C ∨ ¬D)
/// - (¬A ∨ B ∨ C)
/// - (root)
///
/// For additional clarity: if `(¬A ∨ ¬B)` is a clause, `¬A` and `¬B` are its
/// literals, and `A` and `B` are variables. In our implementation, variables
/// are represented by [`VariableId`], and assignments are tracked in
/// the [`DecisionMap`].
///
/// The solver will attempt to assign values to the variables involved in the
/// problem in such a way that all clauses become true. If that turns out to be
/// impossible, the problem is unsatisfiable.
///
/// Since we are not interested in general-purpose SAT solving, but are
/// targeting the specific use-case of dependency resolution, we only support a
/// limited set of clauses. There are thousands of clauses for a particular
/// dependency resolution problem, and we try to keep the [`Clause`] enum small.
/// A naive implementation would store a `Vec<Literal>`.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Clause<N> {
    /// An assertion that the root solvable must be installed
    ///
    /// In SAT terms: (root)
    InstallRoot,
    /// Makes the solvable require the candidates associated with the
    /// [`Requirement`].
    ///
    /// Optionally the requirement can be associated with a condition in the
    /// form of a disjunction.
    ///
    /// In SAT terms: (¬A ∨ ¬D1 v ¬D2 .. v ¬D99 v B1 ∨ B2 ∨ ... ∨ B99), where D1
    /// to D99 represent the candidates of the disjunction and B1 to B99
    /// represent the possible candidates for the provided [`Requirement`].
    Requires(VariableId, Option<DisjunctionId>, Requirement),
    /// Ensures only a single version of a package is installed
    ///
    /// Usage: generate one [`Clause::ForbidMultipleInstances`] clause for each
    /// possible combination of packages under the same name. The clause
    /// itself forbids two solvables from being installed at the same time.
    ///
    /// In SAT terms: (¬A ∨ ¬B)
    ForbidMultipleInstances(VariableId, Literal, N),
    /// Forbids packages that do not satisfy a solvable's constrains
    ///
    /// Usage: for each constrains relationship in a package, determine all the
    /// candidates that do _not_ satisfy it, and create one
    /// [`Clause::Constrains`]. The clause itself forbids two solvables from
    /// being installed at the same time, just as
    /// [`Clause::ForbidMultipleInstances`], but it pays off to have a
    /// separate variant for user-friendly error messages.
    ///
    /// In SAT terms: (¬A ∨ ¬B)
    Constrains(VariableId, VariableId, VersionSetId),
    /// Forbids the package on the right-hand side
    ///
    /// Note that the package on the left-hand side is not part of the clause,
    /// but just context to know which exact package was locked (necessary
    /// for user-friendly error messages)
    ///
    /// In SAT terms: (¬root ∨ ¬B). Note that we could encode this as an
    /// assertion (¬B), but that would require additional logic in the
    /// solver.
    Lock(VariableId, VariableId),
    /// A clause learnt during solving
    ///
    /// The learnt clause id can be used to retrieve the clause's literals,
    /// which are stored elsewhere to prevent the size of [`Clause`] from
    /// blowing up
    Learnt(LearntClauseId),

    /// A clause that forbids a package from being installed for an external
    /// reason.
    Excluded(VariableId, StringId),

    /// A clause that indicates that any version of a package C is selected.
    /// In SAT terms: (C_selected v ¬Cj)
    AnyOf(VariableId, VariableId),

    /// A clause encoding a constraint on an environment package.
    ///
    /// Semantics: if the parent solvable `A` is installed, the environment
    /// must either be absent (if `Ab_p` is given) or match the version set.
    ///
    /// In SAT terms (when absent is possible): `(¬A ∨ Ab_p ∨ L_S)`.
    /// In SAT terms (when absent is not possible): `(¬A ∨ L_S)`.
    ///
    /// The payload ([`EnvConstrainsClause`]) is stored in a side arena, like
    /// the literals of a [`Clause::Learnt`] clause, to keep [`Clause`] small.
    EnvConstrains(EnvConstrainsId),

    /// A binary oracle-consistency clause between two environment literals.
    ///
    /// These clauses are tautologies within the modeled environment space.
    /// They are emitted when a new env-matches literal is interned for a
    /// package that already has other interned literals, based on the answer
    /// from `DependencyProvider::environment_version_set_relation`.
    ///
    /// They participate in propagation and watching like any binary clause,
    /// but never appear in conflict chains (they are tautologies).
    EnvOracleConsistency(Literal, Literal),

    /// A disjunction of signed environment literals, used for the clauses of
    /// the environment model supplied to `solve_universal` and for the
    /// blocking clauses added after each enumerated cell.
    ///
    /// The literals (and whether the clause is a model or a blocking clause)
    /// are stored in a side arena ([`EnvClause`]) to keep [`Clause`] small,
    /// following the [`Clause::Learnt`] pattern. A clause with a single
    /// literal is an assertion and is applied during propagation like a
    /// single-literal learnt clause.
    ///
    /// Model clauses are tautologies over the modeled environment space and
    /// blocking clauses are handled by the disjointness repair step, so
    /// neither ever contributes to cell support.
    //
    // The name intentionally matches `EnvClauseId`/`EnvClause`.
    #[allow(clippy::enum_variant_names)]
    EnvClause(EnvClauseId),
}

/// The payload of a [`Clause::EnvClause`] clause, stored in a side arena
/// indexed by [`EnvClauseId`].
#[derive(Clone, Debug)]
pub(crate) struct EnvClause {
    /// The literals of the disjunction. Never empty.
    pub literals: Vec<Literal>,
    /// Whether this is an environment model clause or a blocking clause.
    pub kind: EnvClauseKind,
}

/// Distinguishes the two kinds of [`EnvClause`] payloads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnvClauseKind {
    /// A clause of the environment model CNF supplied by the caller.
    Model,
    /// A blocking clause excluding a previously enumerated cell. `decide()`
    /// actively makes progress on these (see `Solver::decide`) so that the
    /// undecided-counts-as-false completion of every solution satisfies them.
    Blocking,
}

/// The payload of a [`Clause::EnvConstrains`] clause, stored in a side arena
/// indexed by [`EnvConstrainsId`].
///
/// `absent_var` is `Some` iff `can_be_absent` was true when the environment
/// package was declared. `version_set` is the constraint's version set,
/// retained for display and for package-activity lookups in `decide()`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct EnvConstrainsClause {
    pub parent: VariableId,
    pub absent_var: Option<VariableId>,
    pub matches_var: VariableId,
    pub version_set: VersionSetId,
}

impl<N> Clause<N> {
    /// Returns `true` if the clause has exactly two literals that can never
    /// change. The propagation loop relies on this to skip the
    /// `next_unwatched_literal` scan, which can never succeed for such
    /// clauses.
    #[inline]
    pub fn is_binary(&self) -> bool {
        match self {
            Clause::Constrains(..)
            | Clause::ForbidMultipleInstances(..)
            | Clause::Lock(..)
            | Clause::AnyOf(..) => true,
            // EnvConstrains is binary only when there is no absent literal,
            // but determining that requires the side arena. Conservatively
            // report false; the `next_unwatched_literal` scan handles the
            // binary case correctly (it simply finds no unwatched literal).
            Clause::EnvConstrains(_) => false,
            // Oracle consistency clauses are always binary.
            Clause::EnvOracleConsistency(..) => true,
            // Env model/blocking clauses have a variable number of literals.
            Clause::EnvClause(_) => false,
            _ => false,
        }
    }
}

impl<N: SolverId> Clause<N> {
    /// Returns the building blocks needed for a new [WatchedLiterals] of the
    /// [Clause::Requires] kind.
    ///
    /// These building blocks are:
    ///
    /// - The [Clause] itself;
    /// - The ids of the solvables that will be watched (unless there are no
    ///   candidates, i.e. the clause is actually an assertion);
    /// - A boolean indicating whether the clause conflicts with existing
    ///   decisions. This should always be false when adding clauses before
    ///   starting the solving process, but can be true for clauses that are
    ///   added dynamically.
    fn requires(
        parent: VariableId,
        requirement: Requirement,
        candidates: impl IntoIterator<Item = VariableId>,
        condition: Option<(DisjunctionId, &[Literal])>,
        decision_tracker: &DecisionTracker,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        // It only makes sense to introduce a requires clause when the parent solvable
        // is undecided or going to be installed
        assert_ne!(decision_tracker.assigned_value(parent), Some(false));

        let kind = Clause::Requires(parent, condition.map(|d| d.0), requirement);

        // Construct literals to watch
        let condition_literals = condition
            .into_iter()
            .flat_map(|(_, candidates)| candidates)
            .copied()
            .peekable();
        let candidate_literals = candidates
            .into_iter()
            .map(|candidate| candidate.positive())
            .peekable();

        let mut literals = condition_literals.chain(candidate_literals).peekable();
        let Some(&first_literal) = literals.peek() else {
            // If there are no candidates and there is no condition, then this clause is an
            // assertion.
            // There is also no conflict because we asserted above that the parent is not
            // assigned to false yet.
            return (kind, None, false);
        };

        match literals.find(|&c| c.eval(decision_tracker.map()) != Some(false)) {
            // Watch any candidate that is not assigned to false
            Some(watched_candidate) => (kind, Some([parent.negative(), watched_candidate]), false),

            // All candidates are assigned to false! Therefore, the clause conflicts with the
            // current decisions. There are no valid watches for it at the moment, but we will
            // assign default ones nevertheless, because they will become valid after the solver
            // restarts.
            None => {
                // Try to find a condition that is not assigned to false.
                (kind, Some([parent.negative(), first_literal]), true)
            }
        }
    }

    /// Returns the building blocks needed for a new [WatchedLiterals] of the
    /// [Clause::Constrains] kind.
    ///
    /// These building blocks are:
    ///
    /// - The [Clause] itself;
    /// - The ids of the solvables that will be watched;
    /// - A boolean indicating whether the clause conflicts with existing
    ///   decisions. This should always be false when adding clauses before
    ///   starting the solving process, but can be true for clauses that are
    ///   added dynamically.
    fn constrains(
        parent: VariableId,
        forbidden_solvable: VariableId,
        via: VersionSetId,
        decision_tracker: &DecisionTracker,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        // It only makes sense to introduce a constrains clause when the parent solvable
        // is undecided or going to be installed
        assert_ne!(decision_tracker.assigned_value(parent), Some(false));

        // If the forbidden solvable is already assigned to true, that means that there
        // is a conflict with current decisions, because it implies the parent
        // solvable would be false (and we just asserted that it is not)
        let conflict = decision_tracker.assigned_value(forbidden_solvable) == Some(true);

        (
            Clause::Constrains(parent, forbidden_solvable, via),
            Some([parent.negative(), forbidden_solvable.negative()]),
            conflict,
        )
    }

    /// Returns the ids of the solvables that will be watched as well as the
    /// clause itself.
    fn forbid_multiple(
        candidate: VariableId,
        constrained_candidate: Literal,
        name: N,
    ) -> (Self, Option<[Literal; 2]>) {
        (
            Clause::ForbidMultipleInstances(candidate, constrained_candidate, name),
            Some([candidate.negative(), constrained_candidate]),
        )
    }

    fn root() -> (Self, Option<[Literal; 2]>) {
        (Clause::InstallRoot, None)
    }

    fn exclude(candidate: VariableId, reason: StringId) -> (Self, Option<[Literal; 2]>) {
        (Clause::Excluded(candidate, reason), None)
    }

    fn lock(
        locked_candidate: VariableId,
        other_candidate: VariableId,
    ) -> (Self, Option<[Literal; 2]>) {
        (
            Clause::Lock(locked_candidate, other_candidate),
            Some([VariableId::root().negative(), other_candidate.negative()]),
        )
    }

    fn learnt(
        learnt_clause_id: LearntClauseId,
        literals: &[Literal],
    ) -> (Self, Option<[Literal; 2]>) {
        debug_assert!(!literals.is_empty());
        (
            Clause::Learnt(learnt_clause_id),
            if literals.len() == 1 {
                // No need for watches, since we learned an assertion
                None
            } else {
                Some([*literals.first().unwrap(), *literals.last().unwrap()])
            },
        )
    }

    fn any_of(selected_var: VariableId, other_var: VariableId) -> (Self, Option<[Literal; 2]>) {
        let kind = Clause::AnyOf(selected_var, other_var);
        (kind, Some([selected_var.positive(), other_var.negative()]))
    }

    /// Build an env model/blocking clause from a payload that has already
    /// been allocated in the side arena. Mirrors [`Clause::learnt`]: a
    /// single-literal clause is an assertion and gets no watches.
    fn env_clause(
        env_clause_id: EnvClauseId,
        literals: &[Literal],
    ) -> (Self, Option<[Literal; 2]>) {
        debug_assert!(!literals.is_empty());
        (
            Clause::EnvClause(env_clause_id),
            if literals.len() == 1 {
                // No need for watches, this is an assertion.
                None
            } else {
                Some([*literals.first().unwrap(), *literals.last().unwrap()])
            },
        )
    }

    /// Build an `EnvConstrains` clause from a payload that has already been
    /// allocated in the side arena.
    ///
    /// Returns `(clause, watched_literals, conflict)`.
    ///
    /// When `absent_var` is `None` the clause is binary `(¬parent ∨ matches)`.
    /// When `absent_var` is `Some(ab)` the clause is ternary
    /// `(¬parent ∨ ab ∨ matches)` and behaves like a requires clause with
    /// ordered candidates `[ab, matches]`.
    fn env_constrains(
        env_constrains_id: EnvConstrainsId,
        payload: &EnvConstrainsClause,
        decision_tracker: &DecisionTracker,
    ) -> (Self, Option<[Literal; 2]>, bool) {
        let EnvConstrainsClause {
            parent,
            absent_var,
            matches_var,
            ..
        } = *payload;
        assert_ne!(decision_tracker.assigned_value(parent), Some(false));

        let kind = Clause::EnvConstrains(env_constrains_id);

        match absent_var {
            None => {
                // Binary clause: (not parent or matches)
                let conflict = decision_tracker.assigned_value(matches_var) == Some(false);
                (
                    kind,
                    Some([parent.negative(), matches_var.positive()]),
                    conflict,
                )
            }
            Some(ab) => {
                // Ternary clause: (not parent or ab or matches).
                // Ordered candidates: absent first (per split policy), then matches.
                // Watch parent.negative() and the first non-false candidate.
                let ab_val = decision_tracker.assigned_value(ab);
                let matches_val = decision_tracker.assigned_value(matches_var);

                // Find a candidate that is not assigned false.
                let watched_candidate = if ab_val != Some(false) {
                    ab.positive()
                } else if matches_val != Some(false) {
                    matches_var.positive()
                } else {
                    // Both false -- conflict.
                    return (kind, Some([parent.negative(), ab.positive()]), true);
                };

                let conflict = ab_val == Some(false) && matches_val == Some(false);
                (kind, Some([parent.negative(), watched_candidate]), conflict)
            }
        }
    }

    /// Tries to fold over all the literals in the clause.
    ///
    /// This function is useful to iterate, find, or filter the literals in a
    /// clause.
    #[allow(clippy::too_many_arguments)]
    pub fn try_fold_literals<B, C, F>(
        &self,
        learnt_clauses: &Arena<LearntClauseId, Vec<Literal>>,
        requirements_to_sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        disjunction_to_candidates: &Arena<DisjunctionId, Disjunction>,
        env_constrains_clauses: &Arena<EnvConstrainsId, EnvConstrainsClause>,
        env_clauses: &Arena<EnvClauseId, EnvClause>,
        init: C,
        mut visit: F,
    ) -> ControlFlow<B, C>
    where
        F: FnMut(C, Literal) -> ControlFlow<B, C>,
    {
        match *self {
            Clause::InstallRoot => unreachable!(),
            Clause::Excluded(solvable, _) => visit(init, solvable.negative()),
            Clause::Learnt(learnt_id) => learnt_clauses[learnt_id]
                .iter()
                .copied()
                .try_fold(init, visit),
            Clause::Requires(solvable_id, disjunction, match_spec_id) => {
                iter::once(solvable_id.negative())
                    .chain(
                        disjunction
                            .into_iter()
                            .flat_map(|d| disjunction_to_candidates[d].literals.iter())
                            .copied(),
                    )
                    .chain(
                        requirements_to_sorted_candidates[match_spec_id]
                            .iter()
                            .flatten()
                            .map(|&s| s.positive()),
                    )
                    .try_fold(init, visit)
            }
            Clause::Constrains(s1, s2, _) => [s1.negative(), s2.negative()]
                .into_iter()
                .try_fold(init, visit),
            Clause::ForbidMultipleInstances(s1, s2, _) => {
                [s1.negative(), s2].into_iter().try_fold(init, visit)
            }
            Clause::Lock(_, s) => [s.negative(), VariableId::root().negative()]
                .into_iter()
                .try_fold(init, visit),
            Clause::AnyOf(selected, variable) => [selected.positive(), variable.negative()]
                .into_iter()
                .try_fold(init, visit),
            Clause::EnvConstrains(env_constrains_id) => {
                let payload = &env_constrains_clauses[env_constrains_id];
                iter::once(payload.parent.negative())
                    .chain(payload.absent_var.map(|ab| ab.positive()))
                    .chain(iter::once(payload.matches_var.positive()))
                    .try_fold(init, visit)
            }
            Clause::EnvOracleConsistency(lit_a, lit_b) => {
                [lit_a, lit_b].into_iter().try_fold(init, visit)
            }
            Clause::EnvClause(env_clause_id) => env_clauses[env_clause_id]
                .literals
                .iter()
                .copied()
                .try_fold(init, visit),
        }
    }

    /// Visits each literal in the clause.
    ///
    /// If you need to exit early or return a value, use [`try_fold_literals`].
    pub fn visit_literals(
        &self,
        learnt_clauses: &Arena<LearntClauseId, Vec<Literal>>,
        requirements_to_sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        disjunction_to_candidates: &Arena<DisjunctionId, Disjunction>,
        env_constrains_clauses: &Arena<EnvConstrainsId, EnvConstrainsClause>,
        env_clauses: &Arena<EnvClauseId, EnvClause>,
        mut visit: impl FnMut(Literal),
    ) {
        let _ = self.try_fold_literals(
            learnt_clauses,
            requirements_to_sorted_candidates,
            disjunction_to_candidates,
            env_constrains_clauses,
            env_clauses,
            (),
            |_, lit| {
                visit(lit);
                ControlFlow::<()>::Continue(())
            },
        );
    }

    /// Construct a [`ClauseDisplay`] to display the clause.
    pub fn display<'i, I: Interner<NameId = N>>(
        &self,
        variable_map: &'i VariableMap<I::NameId, I::SolvableId>,
        interner: &'i I,
    ) -> ClauseDisplay<'i, I> {
        ClauseDisplay {
            kind: *self,
            variable_map,
            interner,
        }
    }
}

/// Keeps track of the literals watched by a [`Clause`] and the state associated
/// to two linked lists this clause is part of
///
/// In our SAT implementation, each clause tracks two literals present in its
/// clause, to be notified when the value assigned to the variable has changed
/// (this technique is known as _watches_). Clauses that are tracking the same
/// variable are grouped together in a linked list, so it becomes easy to notify
/// them all.
#[derive(Clone)]
pub(crate) struct WatchedLiterals {
    /// The ids of the literals this clause is watching. A clause that is
    /// watching literals is always watching two literals, no more, no less.
    pub watched_literals: [Literal; 2],
    /// The ids of the next clause in each linked list that this clause is part
    /// of. If either of these or `None` then there is no next clause.
    pub(crate) next_watches: [Option<ClauseId>; 2],
}

impl WatchedLiterals {
    /// Shorthand method to construct a [`Clause::InstallRoot`] without
    /// requiring complicated arguments.
    pub fn root<N: SolverId>() -> (Option<Self>, Clause<N>) {
        let (kind, watched_literals) = Clause::root();
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    /// Shorthand method to construct a [Clause::Requires] without requiring
    /// complicated arguments.
    ///
    /// The returned boolean value is true when adding the clause resulted in a
    /// conflict.
    pub fn requires<N: SolverId>(
        candidate: VariableId,
        requirement: Requirement,
        matching_candidates: impl IntoIterator<Item = VariableId>,
        condition: Option<(DisjunctionId, &[Literal])>,
        decision_tracker: &DecisionTracker,
    ) -> (Option<Self>, bool, Clause<N>) {
        let (kind, watched_literals, conflict) = Clause::requires(
            candidate,
            requirement,
            matching_candidates,
            condition,
            decision_tracker,
        );

        (
            Self::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    /// Shorthand method to construct a [Clause::Constrains] without requiring
    /// complicated arguments.
    ///
    /// The returned boolean value is true when adding the clause resulted in a
    /// conflict.
    pub fn constrains<N: SolverId>(
        candidate: VariableId,
        constrained_package: VariableId,
        requirement: VersionSetId,
        decision_tracker: &DecisionTracker,
    ) -> (Option<Self>, bool, Clause<N>) {
        let (kind, watched_literals, conflict) = Clause::constrains(
            candidate,
            constrained_package,
            requirement,
            decision_tracker,
        );

        (
            Self::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    pub fn lock<N: SolverId>(
        locked_candidate: VariableId,
        other_candidate: VariableId,
    ) -> (Option<Self>, Clause<N>) {
        let (kind, watched_literals) = Clause::lock(locked_candidate, other_candidate);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    pub fn forbid_multiple<N: SolverId>(
        candidate: VariableId,
        other_candidate: Literal,
        name: N,
    ) -> (Option<Self>, Clause<N>) {
        let (kind, watched_literals) = Clause::forbid_multiple(candidate, other_candidate, name);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    pub fn learnt<N: SolverId>(
        learnt_clause_id: LearntClauseId,
        literals: &[Literal],
    ) -> (Option<Self>, Clause<N>) {
        let (kind, watched_literals) = Clause::learnt(learnt_clause_id, literals);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    pub fn exclude<N: SolverId>(
        candidate: VariableId,
        reason: StringId,
    ) -> (Option<Self>, Clause<N>) {
        let (kind, watched_literals) = Clause::exclude(candidate, reason);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    pub fn any_of<N: SolverId>(
        selected_var: VariableId,
        other_var: VariableId,
    ) -> (Option<Self>, Clause<N>) {
        let (kind, watched_literals) = Clause::any_of(selected_var, other_var);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    /// Construct an `EnvOracleConsistency` binary clause from two arbitrary
    /// literals `(lit_a ∨ lit_b)`.
    pub fn oracle_consistency<N: SolverId>(
        lit_a: Literal,
        lit_b: Literal,
    ) -> (Option<Self>, Clause<N>) {
        let kind = Clause::EnvOracleConsistency(lit_a, lit_b);
        (
            Some(WatchedLiterals {
                watched_literals: [lit_a, lit_b],
                next_watches: [None, None],
            }),
            kind,
        )
    }

    /// Construct an env model/blocking clause ([`Clause::EnvClause`]) from a
    /// payload that has already been allocated in the side arena.
    ///
    /// A single-literal clause is an assertion: it gets no watches and must
    /// be applied during propagation (see `Solver::decide_env_assertions`),
    /// exactly like a single-literal learnt clause.
    pub fn env_clause<N: SolverId>(
        env_clause_id: EnvClauseId,
        literals: &[Literal],
    ) -> (Option<Self>, Clause<N>) {
        let (kind, watched_literals) = Clause::env_clause(env_clause_id, literals);
        (Self::from_kind_and_initial_watches(watched_literals), kind)
    }

    /// Construct an `EnvConstrains` clause from a payload that has already
    /// been allocated in the side arena. The returned boolean is true when
    /// the clause conflicts with existing decisions.
    pub fn env_constrains<N: SolverId>(
        env_constrains_id: EnvConstrainsId,
        payload: &EnvConstrainsClause,
        decision_tracker: &DecisionTracker,
    ) -> (Option<Self>, bool, Clause<N>) {
        let (kind, watched_literals, conflict) =
            Clause::env_constrains(env_constrains_id, payload, decision_tracker);
        (
            Self::from_kind_and_initial_watches(watched_literals),
            conflict,
            kind,
        )
    }

    fn from_kind_and_initial_watches(watched_literals: Option<[Literal; 2]>) -> Option<Self> {
        let watched_literals = watched_literals?;
        debug_assert!(watched_literals[0] != watched_literals[1]);
        Some(Self {
            watched_literals,
            next_watches: [None, None],
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn next_unwatched_literal<N: SolverId>(
        &self,
        clause: &Clause<N>,
        learnt_clauses: &Arena<LearntClauseId, Vec<Literal>>,
        requirement_to_sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        disjunction_to_candidates: &Arena<DisjunctionId, Disjunction>,
        env_constrains_clauses: &Arena<EnvConstrainsId, EnvConstrainsClause>,
        env_clauses: &Arena<EnvClauseId, EnvClause>,
        decision_map: &DecisionMap,
        for_watch_index: usize,
    ) -> Option<Literal> {
        let other_watch_index = 1 - for_watch_index;

        match clause {
            Clause::InstallRoot => unreachable!(),
            Clause::Excluded(_, _) => unreachable!(),
            _ if clause.is_binary() => None,
            clause => {
                let next = clause.try_fold_literals(
                    learnt_clauses,
                    requirement_to_sorted_candidates,
                    disjunction_to_candidates,
                    env_constrains_clauses,
                    env_clauses,
                    (),
                    |_, lit| {
                        // The next unwatched variable (if available), is a variable that is:
                        // * Not already being watched
                        // * Not yet decided, or decided in such a way that the literal yields true
                        if self.watched_literals[other_watch_index] != lit
                            && lit.eval(decision_map).unwrap_or(true)
                        {
                            ControlFlow::Break(lit)
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                );
                match next {
                    ControlFlow::Break(lit) => Some(lit),
                    ControlFlow::Continue(_) => None,
                }
            }
        }
    }
}

/// Represents a literal in a SAT clause, a literal holds a variable and
/// indicates whether it should be positive or negative (i.e. either A or ¬A).
///
/// A [`Literal`] stores a [`NonZeroU32`] which ensures that the size of an
/// `Option<Literal>` is the same as a `Literal`.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct Literal(NonZeroU32);

impl Literal {
    /// Constructs a new [`Literal`] from a [`VariableId`] and a boolean
    /// indicating whether the literal should be negated.
    #[inline]
    pub fn new(variable: VariableId, negate: bool) -> Self {
        let variable_idx = variable.to_index();
        let encoded_literal = variable_idx << 1 | negate as usize;
        Self::from_index(encoded_literal)
    }
}

impl DenseIndex for Literal {
    #[inline]
    fn from_index(x: usize) -> Self {
        let idx: u32 = (x + 1).try_into().expect("watched literal id too big");
        // SAFETY: This is safe because we are adding 1 to the index
        unsafe { Literal(NonZeroU32::new_unchecked(idx)) }
    }

    #[inline]
    fn to_index(self) -> usize {
        self.0.get() as usize - 1
    }
}

impl Literal {
    #[inline]
    pub fn negate(&self) -> bool {
        (self.0.get() & 1) == 0
    }

    /// Returns the value that would make the literal evaluate to true if
    /// assigned to the literal's solvable
    #[inline]
    pub(crate) fn satisfying_value(self) -> bool {
        !self.negate()
    }

    /// Returns the value that would make the literal evaluate to true if
    /// assigned to the literal's solvable
    #[inline]
    pub(crate) fn variable(self) -> VariableId {
        VariableId::from_index(self.to_index() >> 1)
    }

    /// Evaluates the literal, or returns `None` if no value has been assigned
    /// to the solvable
    #[inline(always)]
    pub(crate) fn eval(self, decision_map: &DecisionMap) -> Option<bool> {
        decision_map
            .value(self.variable())
            .map(|value| value != self.negate())
    }
}

impl VariableId {
    /// Constructs a [`Literal`] that indicates this solvable should be assigned
    /// a positive value.
    #[inline]
    pub(crate) fn positive(self) -> Literal {
        Literal::new(self, false)
    }

    /// Constructs a [`Literal`] that indicates this solvable should be assigned
    /// a negative value.
    #[inline]
    pub(crate) fn negative(self) -> Literal {
        Literal::new(self, true)
    }
}

/// A representation of a clause that implements [`Debug`]
pub(crate) struct ClauseDisplay<'i, I: Interner> {
    kind: Clause<I::NameId>,
    interner: &'i I,
    variable_map: &'i VariableMap<I::NameId, I::SolvableId>,
}

impl<I: Interner> Display for ClauseDisplay<'_, I> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            Clause::InstallRoot => write!(f, "InstallRoot"),
            Clause::Excluded(variable, reason) => {
                write!(
                    f,
                    "Excluded({}({:?}), {})",
                    variable.display(self.variable_map, self.interner),
                    variable,
                    self.interner.display_string(reason)
                )
            }
            Clause::Learnt(learnt_id) => write!(f, "Learnt({learnt_id:?})"),
            Clause::Requires(variable, condition, requirement) => {
                write!(
                    f,
                    "Requires({}({:?}), {}, condition={:?})",
                    variable.display(self.variable_map, self.interner),
                    variable,
                    requirement.display(self.interner),
                    condition
                )
            }
            Clause::Constrains(v1, v2, version_set_id) => {
                write!(
                    f,
                    "Constrains({}({:?}), {}({:?}), {})",
                    v1.display(self.variable_map, self.interner),
                    v1,
                    v2.display(self.variable_map, self.interner),
                    v2,
                    self.interner.display_version_set(version_set_id)
                )
            }
            Clause::ForbidMultipleInstances(v1, v2, name) => {
                write!(
                    f,
                    "ForbidMultipleInstances({}({:?}), {}({:?}), {})",
                    v1.display(self.variable_map, self.interner),
                    v1,
                    v2.variable().display(self.variable_map, self.interner),
                    v2,
                    self.interner.display_name(name)
                )
            }
            Clause::Lock(locked, other) => {
                write!(
                    f,
                    "Lock({}({:?}), {}({:?}))",
                    locked.display(self.variable_map, self.interner),
                    locked,
                    other.display(self.variable_map, self.interner),
                    other,
                )
            }
            Clause::AnyOf(variable, other) => {
                write!(
                    f,
                    "AnyOf({}({:?}), {}({:?}))",
                    variable.display(self.variable_map, self.interner),
                    variable,
                    other.display(self.variable_map, self.interner),
                    other,
                )
            }
            // Like `Learnt`, the payload lives in a side arena that is not
            // available here; display just the id.
            Clause::EnvConstrains(env_constrains_id) => {
                write!(f, "EnvConstrains({env_constrains_id:?})")
            }
            Clause::EnvOracleConsistency(lit_a, lit_b) => {
                write!(
                    f,
                    "EnvOracleConsistency({}{:?}, {}{:?})",
                    if lit_a.negate() { "not " } else { "" },
                    lit_a.variable(),
                    if lit_b.negate() { "not " } else { "" },
                    lit_b.variable(),
                )
            }
            // Like `Learnt`, the literals live in a side arena that is not
            // available here; display just the id.
            Clause::EnvClause(env_clause_id) => {
                write!(f, "EnvClause({env_clause_id:?})")
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{DenseIndex, solver::decision::Decision};

    #[test]
    #[allow(clippy::bool_assert_comparison)]
    fn test_literal_satisfying_value() {
        let lit = VariableId::root().negative();
        assert_eq!(lit.satisfying_value(), false);

        let lit = VariableId::root().positive();
        assert_eq!(lit.satisfying_value(), true);
    }

    #[test]
    fn test_literal_eval() {
        let mut decision_map = DecisionMap::default();

        let literal = VariableId::root().positive();
        let negated_literal = VariableId::root().negative();

        // Undecided
        assert_eq!(literal.eval(&decision_map), None);
        assert_eq!(negated_literal.eval(&decision_map), None);

        // Decided
        decision_map.set(VariableId::root(), true, 1);
        assert_eq!(literal.eval(&decision_map), Some(true));
        assert_eq!(negated_literal.eval(&decision_map), Some(false));

        decision_map.set(VariableId::root(), false, 1);
        assert_eq!(literal.eval(&decision_map), Some(false));
        assert_eq!(negated_literal.eval(&decision_map), Some(true));
    }

    #[test]
    fn test_requires_with_and_without_conflict() {
        let mut decisions = DecisionTracker::default();

        let parent = VariableId::from_index(1);
        let candidate1 = VariableId::from_index(2);
        let candidate2 = VariableId::from_index(3);

        // No conflict, all candidates available
        let (clause, conflict, _kind) = WatchedLiterals::requires::<crate::NameId>(
            parent,
            VersionSetId::from_index(0).into(),
            [candidate1, candidate2],
            None,
            &decisions,
        );
        assert!(!conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(clause.unwrap().watched_literals[1].variable(), candidate1);

        // No conflict, still one candidate available
        decisions
            .try_add_decision(Decision::new(candidate1, false, ClauseId::from_index(0)), 1)
            .unwrap();
        let (clause, conflict, _kind) = WatchedLiterals::requires::<crate::NameId>(
            parent,
            VersionSetId::from_index(0).into(),
            [candidate1, candidate2],
            None,
            &decisions,
        );
        assert!(!conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            candidate2
        );

        // Conflict, no candidates available
        decisions
            .try_add_decision(
                Decision::new(candidate2, false, ClauseId::install_root()),
                1,
            )
            .unwrap();
        let (clause, conflict, _kind) = WatchedLiterals::requires::<crate::NameId>(
            parent,
            VersionSetId::from_index(0).into(),
            [candidate1, candidate2],
            None,
            &decisions,
        );
        assert!(conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            candidate1
        );

        // Panic
        decisions
            .try_add_decision(Decision::new(parent, false, ClauseId::install_root()), 1)
            .unwrap();
        let panicked = std::panic::catch_unwind(|| {
            WatchedLiterals::requires::<crate::NameId>(
                parent,
                VersionSetId::from_index(0).into(),
                [candidate1, candidate2],
                None,
                &decisions,
            )
        })
        .is_err();
        assert!(panicked);
    }

    #[test]
    fn test_constrains_with_and_without_conflict() {
        let mut decisions = DecisionTracker::default();

        let parent = VariableId::from_index(1);
        let forbidden = VariableId::from_index(2);

        // No conflict, forbidden package not installed
        let (clause, conflict, _kind) = WatchedLiterals::constrains::<crate::NameId>(
            parent,
            forbidden,
            VersionSetId::from_index(0),
            &decisions,
        );
        assert!(!conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            forbidden
        );

        // Conflict, forbidden package installed
        decisions
            .try_add_decision(Decision::new(forbidden, true, ClauseId::install_root()), 1)
            .unwrap();
        let (clause, conflict, _kind) = WatchedLiterals::constrains::<crate::NameId>(
            parent,
            forbidden,
            VersionSetId::from_index(0),
            &decisions,
        );
        assert!(conflict);
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[0].variable(),
            parent
        );
        assert_eq!(
            clause.as_ref().unwrap().watched_literals[1].variable(),
            forbidden
        );

        // Panic
        decisions
            .try_add_decision(Decision::new(parent, false, ClauseId::install_root()), 1)
            .unwrap();
        let panicked = std::panic::catch_unwind(|| {
            WatchedLiterals::constrains::<crate::NameId>(
                parent,
                forbidden,
                VersionSetId::from_index(0),
                &decisions,
            )
        })
        .is_err();
        assert!(panicked);
    }

    #[test]
    fn test_watched_literals_size() {
        // This test is here to ensure we don't increase the size of `WatchedLiterals`
        // by accident, as we are creating thousands of instances.
        // libsolv: 24 bytes
        assert_eq!(std::mem::size_of::<WatchedLiterals>(), 16);
    }

    #[test]
    fn test_literal_size() {
        assert_eq!(std::mem::size_of::<Literal>(), 4);
        assert_eq!(
            std::mem::size_of::<Literal>(),
            std::mem::size_of::<Option<Literal>>()
        );
        assert_eq!(
            std::mem::size_of::<Literal>() * 2,
            std::mem::size_of::<[Literal; 2]>()
        );
        assert_eq!(
            std::mem::size_of::<Literal>() * 2,
            std::mem::size_of::<[Option<Literal>; 2]>()
        );
        assert_eq!(
            std::mem::size_of::<Literal>() * 2,
            std::mem::size_of::<Option<[Literal; 2]>>()
        );
    }

    #[test]
    fn test_watched_literal_size() {
        assert_eq!(std::mem::size_of::<WatchedLiterals>(), 16);
        assert_eq!(
            std::mem::size_of::<Option<WatchedLiterals>>(),
            std::mem::size_of::<WatchedLiterals>()
        );
    }

    #[test]
    fn test_clause_size() {
        // The Clause enum should be kept small since we create thousands of instances.
        // Asserted exactly to catch both growth (worse cache) and silent shrinkage
        // (which would mean a variant got smaller and the bound is now loose).
        // The largest payload is Requires(VariableId, Option<DisjunctionId>,
        // Requirement) at 12 bytes; with the discriminant that gives 16 bytes.
        // Variants with larger payloads (Learnt, EnvConstrains) store their
        // payload in a side arena and carry only the arena id.
        assert_eq!(std::mem::size_of::<Clause<crate::NameId>>(), 16);
    }

    #[test]
    fn test_key_type_sizes() {
        use crate::internal::id::*;
        use crate::{NameId, SolvableId};
        eprintln!("=== Key type sizes ===");
        eprintln!("VariableId: {} bytes", std::mem::size_of::<VariableId>());
        eprintln!("ClauseId: {} bytes", std::mem::size_of::<ClauseId>());
        eprintln!(
            "Option<ClauseId>: {} bytes",
            std::mem::size_of::<Option<ClauseId>>()
        );
        eprintln!("SolvableId: {} bytes", std::mem::size_of::<SolvableId>());
        eprintln!("NameId: {} bytes", std::mem::size_of::<NameId>());
        eprintln!(
            "Requirement: {} bytes",
            std::mem::size_of::<crate::Requirement>()
        );
        eprintln!(
            "Decision: {} bytes",
            std::mem::size_of::<super::super::decision::Decision>()
        );
        eprintln!(
            "DecisionAndLevel (in DecisionMap): {} bytes",
            std::mem::size_of::<i32>()
        );
        eprintln!(
            "Clause + WatchedLiterals per clause: {} bytes",
            std::mem::size_of::<Clause<crate::NameId>>()
                + std::mem::size_of::<Option<WatchedLiterals>>()
        );
    }
}
