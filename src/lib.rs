//! Implements a SAT solver for dependency resolution based on the CDCL
//! algorithm (conflict-driven clause learning)
//!
//! The CDCL algorithm is masterly explained in [An Extensible
//! SAT-solver](http://minisat.se/downloads/MiniSat.pdf). Regarding the data structures used, we
//! mostly follow the approach taken by [libsolv](https://github.com/openSUSE/libsolv). The code of
//! libsolv is, however, very low level C, so if you are looking for an
//! introduction to CDCL, you are encouraged to look at the paper instead or to
//! keep reading through this codebase and its comments.
//!
//! # Universal solving
//!
//! Besides classic single-environment resolution ([`Solver::solve`]), resolvo
//! supports *universal* multi-environment resolution via
//! [`Solver::solve_universal`]: a single solve whose result is valid for a
//! whole family of environments (for example all glibc versions in a range,
//! or machines with and without CUDA). Properties of the target environment
//! are modeled as *environment packages*: packages whose value is unknown at
//! solve time, classified by
//! [`UniversalDependencyProvider::environment_package`] and related through the
//! [`UniversalDependencyProvider::environment_version_set_relation`] oracle. A
//! [`UniversalProblem`] bounds the environment space with an explicit
//! [`EnvironmentModel`]; the solver partitions that space into disjoint
//! cells, each paired with the solvables valid throughout the cell, returned
//! as a [`UniversalSolution`] (or a [`UniversalFailure`] carrying a conflict
//! scoped to the unsolvable region). See the `solver::universal` module
//! documentation in the source for a worked end-to-end example.

#![deny(missing_docs)]
#![deny(unnameable_types)]

mod conditional_requirement;
pub mod conflict;
pub mod id;
pub(crate) mod internal;
mod requirement;
pub mod runtime;
pub mod snapshot;
mod solver;
pub mod solver_id;
pub mod utils;

use std::{
    any::Any,
    fmt::{Debug, Display},
};

pub use conditional_requirement::{Condition, ConditionalRequirement, LogicalOperator};
pub use id::{
    ConditionId, DenseIndex, NameId, NameTag, SolvableId, SolvableTag, StringId, VariableId,
    VersionSetId, VersionSetUnionId,
};
use itertools::Itertools;
pub use requirement::Requirement;
pub use solver::{
    Cell, CellEdge, EmptySolvables, EnvInputSource, EnvironmentModel, InvalidUniversalInput,
    Problem, Solver, SolverCache, UniversalFailure, UniversalProblem, UniversalSolution,
    UnsolvableOrCancelled, Violation,
};
#[cfg(feature = "diagnostics")]
pub use solver::{CellPinCounts, CellRetract};
pub use solver_id::{DenseId, IdMap, IdSet, SolverId, SparseId};
pub use utils::{IndexedSet, Mapping, MappingIter};

/// The relation between two version sets that refer to the same environment
/// package.
///
/// Soundness contract: answers other than `Unknown` must be correct. When in
/// doubt return `Unknown`. A wrong `Disjoint` or `Subset` produces broken
/// lockfiles; `Unknown` merely risks describing environment regions no real
/// machine has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum VersionSetRelation {
    /// No value matches both version sets.
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

/// Describes an environment package: a package whose value is unknown at solve
/// time. Returned by [`UniversalDependencyProvider::environment_package`].
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EnvironmentPackage {
    /// Whether the environment may lack this package entirely. Controls
    /// creation of the absent literal.
    pub can_be_absent: bool,
}

/// Internal cache representation of a package: either a normal package with
/// concrete candidate solvables, or an environment package whose value is
/// unknown at solve time.
///
/// This type never appears on the public [`DependencyProvider`] surface:
/// [`DependencyProvider::get_candidates`] only ever produces the
/// [`Candidates`](PackageCandidates::Candidates) case, and the environment
/// classification is layered on by [`Solver::solve_universal`] via
/// [`UniversalDependencyProvider::environment_package`]. It exists only so the
/// solver cache can carry either kind uniformly.
#[derive(Clone, Debug)]
pub(crate) enum PackageCandidates<S = SolvableId> {
    /// A normal package with concrete candidate solvables.
    Candidates(Candidates<S>),
    /// An environment package whose value is unknown at solve time.
    Environment(EnvironmentPackage),
}

/// A reference to a specific value of an environment package: either the
/// environment's value for the package matches a version set, or the package
/// is absent from the environment.
///
/// Invalid combinations that a flat `{ package, version_set }` pair could
/// express — most importantly a package paired with a version set that belongs
/// to a *different* package — are unrepresentable here: a
/// [`Matches`](EnvLiteral::Matches) literal derives its package from the
/// version set via [`Interner::version_set_name`], and only an
/// [`Absent`](EnvLiteral::Absent) literal carries a package name of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EnvLiteral<N> {
    /// The environment's value for the package exists and matches this version
    /// set. The package the literal refers to is the version set's package,
    /// [`Interner::version_set_name`].
    Matches(VersionSetId),
    /// The named package is absent from the environment.
    Absent(N),
}

impl<N> EnvLiteral<N> {
    /// The environment package this literal refers to. A
    /// [`Matches`](EnvLiteral::Matches) literal derives its package from the
    /// version set via the interner; an [`Absent`](EnvLiteral::Absent) literal
    /// returns the name it stores directly.
    pub fn package<I: Interner<NameId = N>>(&self, interner: &I) -> N
    where
        N: Copy,
    {
        match self {
            EnvLiteral::Matches(version_set) => interner.version_set_name(*version_set),
            EnvLiteral::Absent(name) => *name,
        }
    }
}

/// A signed environment literal: an [`EnvLiteral`] paired with the truth value
/// it must take.
///
/// `positive` follows the public convention **`true` = the literal holds**: a
/// positive [`Matches`](EnvLiteral::Matches) requires the environment to match
/// the version set, a negative one requires it not to. The solver's internal
/// negation convention is confined to its own literal types and never surfaces
/// on this public boundary.
///
/// The same signed-literal type is shared by [`EnvClause`] (a disjunction) and
/// by [`CellCondition`] (a conjunction), so the two differ only in how their
/// literals combine, never in the literal representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SignedEnvLiteral<N> {
    /// The environment literal.
    pub literal: EnvLiteral<N>,
    /// Whether the literal must hold (`true`) or must not hold (`false`).
    pub positive: bool,
}

impl<N> SignedEnvLiteral<N> {
    /// Pairs a literal with the truth value it must take (`true` = the literal
    /// holds).
    pub fn new(literal: EnvLiteral<N>, positive: bool) -> Self {
        Self { literal, positive }
    }
}

impl<N> From<(EnvLiteral<N>, bool)> for SignedEnvLiteral<N> {
    /// Converts a `(literal, positive)` pair, preserving the `true` = holds
    /// convention so existing tuple call sites stay ergonomic.
    fn from((literal, positive): (EnvLiteral<N>, bool)) -> Self {
        Self { literal, positive }
    }
}

/// A disjunction (logical OR) of [`SignedEnvLiteral`]s: one clause of an
/// [`EnvironmentModel`] CNF.
///
/// A clause is satisfied by an environment when at least one of its signed
/// literals holds; an empty clause is unsatisfiable. This is deliberately a
/// *different* type from [`CellCondition`], which is a *conjunction* of the
/// same literals: a disjunction and a conjunction are not interchangeable even
/// though both range over signed environment literals.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EnvClause<N>(Vec<SignedEnvLiteral<N>>);

impl<N> Default for EnvClause<N> {
    /// The empty disjunction, which no environment satisfies.
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<N> EnvClause<N> {
    /// Creates a clause (a disjunction) from its signed literals.
    pub fn new(literals: Vec<SignedEnvLiteral<N>>) -> Self {
        Self(literals)
    }

    /// Returns an iterator over the signed literals of the disjunction, in
    /// order. The clause holds when at least one of them holds.
    pub fn literals(&self) -> impl ExactSizeIterator<Item = &SignedEnvLiteral<N>> + '_ {
        self.0.iter()
    }

    /// The number of literals in the disjunction.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the disjunction has no literals. An empty disjunction is
    /// unsatisfiable.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<N> FromIterator<SignedEnvLiteral<N>> for EnvClause<N> {
    fn from_iter<T: IntoIterator<Item = SignedEnvLiteral<N>>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<N> FromIterator<(EnvLiteral<N>, bool)> for EnvClause<N> {
    fn from_iter<T: IntoIterator<Item = (EnvLiteral<N>, bool)>>(iter: T) -> Self {
        Self(iter.into_iter().map(SignedEnvLiteral::from).collect())
    }
}

/// A conjunction of signed environment literals.
///
/// An empty conjunction means "all environments". Every environment literal
/// appears at most once: the [normalizing constructor](CellCondition::new)
/// deduplicates repeated literals and rejects a literal supplied with both
/// signs, so a `CellCondition` can never encode a self-contradiction.
///
/// This is a *conjunction*; the disjunction counterpart is [`EnvClause`]. The
/// two are intentionally distinct types and cannot be substituted for one
/// another.
///
/// Only [`Serialize`](serde::Serialize) is derived (behind the `serde`
/// feature), not [`Deserialize`](serde::Deserialize): reconstructing from
/// serialized data must go through [`CellCondition::new`], which re-establishes
/// the deduplication/no-contradiction invariant that a derived `Deserialize`
/// would bypass.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CellCondition<N>(Vec<SignedEnvLiteral<N>>);

impl<N> Default for CellCondition<N> {
    /// The empty conjunction, which holds in every environment.
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<N> CellCondition<N> {
    /// Returns an object that formats the cell condition in a human readable
    /// way, e.g. `cuda in >=11, <100 AND not (glibc in >=228, <1000)`. The
    /// empty conjunction is formatted as `<all environments>`.
    pub fn display<'a, I: Interner<NameId = N>>(
        &'a self,
        interner: &'a I,
    ) -> CellConditionDisplay<'a, I> {
        CellConditionDisplay {
            condition: self,
            interner,
        }
    }

    /// Returns an iterator over the signed environment literals of the
    /// conjunction, in order. Each literal must hold with its stored sign
    /// ([`SignedEnvLiteral::positive`], `true` = holds) for the condition to be
    /// satisfied.
    pub fn literals(&self) -> impl ExactSizeIterator<Item = &SignedEnvLiteral<N>> + '_ {
        self.0.iter()
    }

    /// The number of literals in the conjunction.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the conjunction is empty, i.e. holds in every environment.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Wraps literals already known to be normalized (each environment literal
    /// at most once, no sign contradiction). The enumerator produces such
    /// literals directly; external callers must use [`CellCondition::new`],
    /// which enforces the invariant.
    pub(crate) fn from_literals_unchecked(literals: Vec<SignedEnvLiteral<N>>) -> Self {
        Self(literals)
    }
}

impl<N: PartialEq> CellCondition<N> {
    /// Creates a condition from a list of signed environment literals,
    /// normalizing it: exact duplicate signed literals are dropped and a
    /// literal that occurs with both signs is rejected as
    /// [`ContradictoryLiteral`], because no environment can satisfy it.
    ///
    /// This is the entry point for reconstructing conditions from serialized
    /// data (e.g. a lockfile): deserialize the raw [`SignedEnvLiteral`]s, then
    /// re-normalize through this constructor rather than deriving
    /// `Deserialize` on `CellCondition` itself (which would bypass the
    /// invariant). The returned condition upholds the invariant that every
    /// environment literal appears at most once.
    pub fn new(literals: Vec<SignedEnvLiteral<N>>) -> Result<Self, ContradictoryLiteral<N>> {
        let mut normalized: Vec<SignedEnvLiteral<N>> = Vec::with_capacity(literals.len());
        for signed in literals {
            if let Some(existing) = normalized
                .iter()
                .find(|known| known.literal == signed.literal)
            {
                if existing.positive != signed.positive {
                    return Err(ContradictoryLiteral {
                        literal: signed.literal,
                    });
                }
                // Exact duplicate: keep the single existing entry.
            } else {
                normalized.push(signed);
            }
        }
        Ok(Self(normalized))
    }
}

/// A helper struct that implements [`Display`] for a [`CellCondition`]. See
/// [`CellCondition::display`].
pub struct CellConditionDisplay<'a, I: Interner> {
    condition: &'a CellCondition<I::NameId>,
    interner: &'a I,
}

impl<I: Interner> Display for CellConditionDisplay<'_, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.condition.0.is_empty() {
            return write!(f, "<all environments>");
        }
        for (idx, signed) in self.condition.0.iter().enumerate() {
            if idx > 0 {
                write!(f, " AND ")?;
            }
            if !signed.positive {
                write!(f, "not (")?;
            }
            match signed.literal {
                EnvLiteral::Matches(version_set) => write!(
                    f,
                    "{} in {}",
                    self.interner
                        .display_name(self.interner.version_set_name(version_set)),
                    self.interner.display_version_set(version_set)
                )?,
                EnvLiteral::Absent(package) => {
                    write!(f, "{} absent", self.interner.display_name(package))?
                }
            }
            if !signed.positive {
                write!(f, ")")?;
            }
        }
        Ok(())
    }
}

/// The error returned by [`CellCondition::new`] when the same environment
/// literal is supplied with both signs. Such a conjunction is unsatisfiable —
/// no environment can make a literal simultaneously true and false — so it is
/// rejected rather than silently normalized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContradictoryLiteral<N> {
    /// The literal that appeared with two different signs.
    pub literal: EnvLiteral<N>,
}

impl<N: Debug> Display for ContradictoryLiteral<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the environment literal {:?} appears with both signs, which no \
             environment can satisfy",
            self.literal,
        )
    }
}

impl<N: Debug> std::error::Error for ContradictoryLiteral<N> {}

/// A presence condition: a disjunction (logical OR) of [`CellCondition`]
/// conjunctions, i.e. a formula in disjunctive normal form over signed
/// environment literals.
///
/// A presence condition holds in a concrete environment when at least one of
/// its disjuncts holds. A disjunct that is the empty conjunction holds in
/// every environment, making the whole presence condition always true; an
/// empty disjunction never holds.
///
/// Produced by [`UniversalSolution::merged`] and [`UniversalSolution::edges`],
/// which OR together the conditions of the cells a solvable (or edge) appears
/// in, simplified within the bounds of the environment model.
///
/// Only [`Serialize`](serde::Serialize) is derived (behind the `serde`
/// feature), not [`Deserialize`](serde::Deserialize): reconstructing from
/// serialized data must go through [`Presence::new`], which re-establishes the
/// duplicate-disjunct normalization that a derived `Deserialize` would bypass.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Presence<N>(Vec<CellCondition<N>>);

impl<N> Presence<N> {
    /// Returns an object that formats the presence condition in a human
    /// readable way, e.g. `(cuda in >=11, <100 AND rocm in >=5, <10) OR
    /// cuda absent`. The always-true presence is formatted as
    /// `<all environments>` and the empty disjunction as
    /// `<no environments>`.
    pub fn display<'a, I: Interner<NameId = N>>(
        &'a self,
        interner: &'a I,
    ) -> PresenceDisplay<'a, I> {
        PresenceDisplay {
            presence: self,
            interner,
        }
    }

    /// Returns an iterator over the disjuncts (the OR-ed [`CellCondition`]s).
    /// The presence holds in an environment when at least one disjunct does.
    pub fn disjuncts(&self) -> impl ExactSizeIterator<Item = &CellCondition<N>> + '_ {
        self.0.iter()
    }

    /// The number of disjuncts.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the presence has no disjuncts, i.e. holds in no environment.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Wraps disjuncts already known to be normalized. Used by the solver,
    /// which produces simplified disjuncts directly.
    pub(crate) fn from_disjuncts_unchecked(disjuncts: Vec<CellCondition<N>>) -> Self {
        Self(disjuncts)
    }
}

impl<N: PartialEq> Presence<N> {
    /// Creates a presence from a list of disjuncts, dropping exact duplicate
    /// disjuncts. Intended for reconstructing a presence from serialized data.
    pub fn new(disjuncts: Vec<CellCondition<N>>) -> Self {
        let mut normalized: Vec<CellCondition<N>> = Vec::with_capacity(disjuncts.len());
        for disjunct in disjuncts {
            if !normalized.contains(&disjunct) {
                normalized.push(disjunct);
            }
        }
        Self(normalized)
    }
}

/// A helper struct that implements [`Display`] for a [`Presence`]. See
/// [`Presence::display`].
pub struct PresenceDisplay<'a, I: Interner> {
    presence: &'a Presence<I::NameId>,
    interner: &'a I,
}

impl<I: Interner> Display for PresenceDisplay<'_, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let disjuncts = &self.presence.0;
        if disjuncts.is_empty() {
            return write!(f, "<no environments>");
        }
        for (idx, disjunct) in disjuncts.iter().enumerate() {
            if idx > 0 {
                write!(f, " OR ")?;
            }
            // Parenthesize multi-literal conjunctions when there are
            // multiple disjuncts, so OR/AND precedence stays unambiguous.
            if disjuncts.len() > 1 && disjunct.0.len() > 1 {
                write!(f, "({})", disjunct.display(self.interner))?;
            } else {
                write!(f, "{}", disjunct.display(self.interner))?;
            }
        }
        Ok(())
    }
}

/// An object that is used by the solver to query certain properties of
/// different internalized objects.
pub trait Interner {
    /// The package-name ID type used by this interner.
    type NameId: SolverId;

    /// The solvable ID type used by this interner.
    type SolvableId: SolverId;

    /// Returns an object that can be used to display the given solvable in a
    /// user-friendly way.
    ///
    /// When formatting the solvable, it should it include both the name of
    /// the package and any other identifying properties.
    fn display_solvable(&self, solvable: Self::SolvableId) -> impl Display + '_;

    /// Returns an object that can be used to display the name of a solvable in
    /// a user-friendly way.
    fn display_solvable_name(&self, solvable: Self::SolvableId) -> impl Display + '_ {
        self.display_name(self.solvable_name(solvable))
    }

    /// Returns an object that can be used to display multiple solvables in a
    /// user-friendly way. For example the conda provider should only display
    /// the versions (not build strings etc.) and merges multiple solvables
    /// into one line.
    ///
    /// When formatting the solvables, both the name of the package and any
    /// other identifying properties should be displayed.
    fn display_merged_solvables(&self, solvables: &[Self::SolvableId]) -> impl Display + '_ {
        if solvables.is_empty() {
            return String::new();
        }

        let versions = solvables
            .iter()
            .map(|&id| self.display_solvable(id).to_string())
            .sorted()
            .unique()
            .format(" | ");

        let name = self.display_solvable_name(solvables[0]);
        format!("{name} {versions}")
    }

    /// Returns an object that can be used to display the given name in a
    /// user-friendly way.
    fn display_name(&self, name: Self::NameId) -> impl Display + '_;

    /// Returns an object that can be used to display the given version set in a
    /// user-friendly way.
    ///
    /// The name of the package should *not* be included in the display. Where
    /// appropriate, this information is added.
    fn display_version_set(&self, version_set: VersionSetId) -> impl Display + '_;

    /// Displays the string with the given id.
    fn display_string(&self, string_id: StringId) -> impl Display + '_;

    /// Returns the name of the package that the specified version set is
    /// associated with.
    fn version_set_name(&self, version_set: VersionSetId) -> Self::NameId;

    /// Returns the name of the package for the given solvable.
    fn solvable_name(&self, solvable: Self::SolvableId) -> Self::NameId;

    /// Returns the version sets comprising the given union.
    ///
    /// The implementor must take care that the order in which the version sets
    /// are returned is deterministic.
    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId>;

    /// Resolves how a condition should be represented in the solver.
    ///
    /// Internally, the solver uses `ConditionId` to represent conditions. This
    /// allows implementers to have a custom representation for conditions that
    /// differ from the representation of the solver.
    fn resolve_condition(&self, condition: ConditionId) -> Condition;
}

/// Defines implementation specific behavior for the solver and a way for the
/// solver to access the packages that are available in the system.
#[allow(async_fn_in_trait)]
pub trait DependencyProvider: Sized + Interner {
    /// Given a set of solvables, return the candidates that match the given
    /// version set or if `inverse` is true, the candidates that do *not* match
    /// the version set.
    async fn filter_candidates(
        &self,
        candidates: &[Self::SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<Self::SolvableId>;

    /// Obtains a list of solvables that should be considered when a package
    /// with the given name is requested.
    ///
    /// Return `None` to indicate that the package name is unknown, or the
    /// [`Candidates`] for a normal package otherwise.
    ///
    /// This method describes only *concrete* packages. Environment packages
    /// (whose value is unknown at solve time) are not visible here: they are
    /// classified separately by
    /// [`UniversalDependencyProvider::environment_package`], which is consulted
    /// only for universal solving (see [`Solver::solve_universal`]). This
    /// signature enforces at the type level that a package classified as an
    /// environment package is never described through `get_candidates`.
    async fn get_candidates(&self, name: Self::NameId) -> Option<Candidates<Self::SolvableId>>;

    /// Sort the specified solvables based on which solvable to try first. The
    /// solver will iteratively try to select the highest version. If a
    /// conflict is found with the highest version the next version is
    /// tried. This continues until a solution is found.
    async fn sort_candidates(&self, solver: &SolverCache<Self>, solvables: &mut [Self::SolvableId]);

    /// Returns the dependencies for the specified solvable.
    async fn get_dependencies(&self, solvable: Self::SolvableId) -> Dependencies;

    /// Whether the solver should stop the dependency resolution algorithm.
    ///
    /// This method gets called at the beginning of each unit propagation round
    /// and before potentially blocking operations (like
    /// [Self::get_dependencies] and [Self::get_candidates]). If it returns
    /// `Some(...)`, the solver will stop and return
    /// [UnsolvableOrCancelled::Cancelled].
    fn should_cancel_with_value(&self) -> Option<Box<dyn Any>> {
        None
    }
}

/// A [`DependencyProvider`] that additionally classifies package names as
/// *environment packages* for universal solving (see
/// [`Solver::solve_universal`]).
///
/// The methods of this trait are consulted only for universal solving: a
/// solver used exclusively through the plain [`Solver::solve`] never calls
/// them, so purely concrete solving cannot observe environment packages. This
/// is enforced by construction: the shared solver internals classify a package
/// as an environment package only after [`Solver::solve_universal`] has
/// installed the classification on that solver. Once installed it persists
/// (alongside the cached candidates) for the solver's lifetime, so packages
/// keep a consistent classification across any subsequent solves on the same
/// solver.
pub trait UniversalDependencyProvider: DependencyProvider {
    /// Declares `name` as an environment package whose value is unknown at
    /// solve time. Consulted (and cached) before
    /// [`get_candidates`](DependencyProvider::get_candidates) during universal
    /// solves; `None` means a normal concrete package.
    ///
    /// Sync by design: this is metadata classification, not a fetch. A package
    /// classified as an environment package is never passed to
    /// [`get_candidates`](DependencyProvider::get_candidates).
    fn environment_package(&self, name: Self::NameId) -> Option<EnvironmentPackage>;

    /// Returns the relation between two version sets that refer to the same
    /// environment package.
    ///
    /// Only called for version sets whose
    /// [`version_set_name`](Interner::version_set_name) is an environment
    /// package.
    ///
    /// Soundness contract: answers other than
    /// [`Unknown`](VersionSetRelation::Unknown) must be correct; when in doubt
    /// return `Unknown`. A wrong `Disjoint` or `Subset` answer produces broken
    /// lockfiles; `Unknown` merely risks describing environment regions no real
    /// machine has.
    fn environment_version_set_relation(
        &self,
        a: VersionSetId,
        b: VersionSetId,
    ) -> VersionSetRelation;
}

/// A list of candidate solvables for a specific package. This is returned from
/// [`DependencyProvider::get_candidates`].
#[derive(Clone, Debug)]
pub struct Candidates<S = SolvableId> {
    /// A list of all solvables for the package.
    pub candidates: Vec<S>,

    /// Optionally the id of the solvable that is favored over other solvables.
    /// The solver will first attempt to solve for the specified solvable
    /// but will fall back to other candidates if no solution could be found
    /// otherwise.
    ///
    /// The same behavior can be achieved by sorting this candidate to the top
    /// using the [`DependencyProvider::sort_candidates`] function but using
    /// this method provides better error messages to the user.
    pub favored: Option<S>,

    /// If specified this is the Id of the only solvable that can be selected.
    /// Although it would also be possible to simply return a single
    /// candidate using this field provides better error messages to the
    /// user.
    pub locked: Option<S>,

    /// A hint to the solver that the dependencies of some of the solvables are
    /// also directly available. This allows the solver to request the
    /// dependencies of these solvables immediately. Having the dependency
    /// information available might make the solver much faster because it
    /// has more information available up-front which provides the solver with a
    /// more complete picture of the entire problem space. However, it might
    /// also be the case that the solver doesnt actually need this
    /// information to form a solution. In general though, if the
    /// dependencies can easily be provided one should provide them up-front.
    pub hint_dependencies_available: HintDependenciesAvailable<S>,

    /// A list of solvables that are available but have been excluded from the
    /// solver. For example, a package might be excluded from the solver
    /// because it is not compatible with the runtime. The solver will not
    /// consider these solvables when forming a solution but will use
    /// them in the error message if no solution could be found.
    pub excluded: Vec<(S, StringId)>,
}

impl<S> Default for Candidates<S> {
    fn default() -> Self {
        Self {
            candidates: Vec::new(),
            favored: None,
            locked: None,
            hint_dependencies_available: HintDependenciesAvailable::None,
            excluded: Vec::new(),
        }
    }
}

/// Defines for which candidates dependencies are available without the
/// [`DependencyProvider`] having to perform extra work, e.g. it's cheap to
/// request them.
#[derive(Default, Clone, Debug)]
pub enum HintDependenciesAvailable<S = SolvableId> {
    /// None of the dependencies are available up-front. The dependency provide
    /// will have to do work to find the dependencies.
    #[default]
    None,

    /// All the dependencies are available up-front. Querying them is cheap.
    All,

    /// Only the dependencies for the specified solvables are available.
    /// Querying the dependencies for these solvables is cheap. Querying
    /// dependencies for other solvables is expensive.
    Some(Vec<S>),
}

/// Holds information about the dependencies of a package.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Dependencies {
    /// The dependencies are known.
    Known(KnownDependencies),
    /// The dependencies are unknown, so the parent solvable should be excluded
    /// from the solution.
    ///
    /// The string provides more information about why the dependencies are
    /// unknown (e.g. an error message).
    Unknown(StringId),
}

/// Holds information about the dependencies of a package when they are known.
#[derive(Default, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct KnownDependencies {
    /// Defines which packages should be installed alongside the depending
    /// package and the constraints applied to the package.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub requirements: Vec<ConditionalRequirement>,

    /// Defines additional constraints on packages that may or may not be part
    /// of the solution. Different from `requirements`, packages in this set
    /// are not necessarily included in the solution. Only when one or more
    /// packages list the package in their `requirements` is the
    /// package also added to the solution.
    ///
    /// This is often useful to use for optional dependencies.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub constrains: Vec<VersionSetId>,
}
