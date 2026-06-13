use std::{any::Any, fmt::Display};

use ahash::{HashMap, HashSet};
use assertion_scans::AssertionScans;
pub use cache::SolverCache;
use clause::{Clause, EnvClause, EnvClauseKind, EnvConstrainsClause, Literal, WatchedLiterals};
use conditions::{DeferredRequirement, Disjunction, DisjunctionId, condition_disjunct_holds};
use decide_queue::{ClausePosition, TrackedClauseId};
use decision::Decision;
use decision_tracker::DecisionTracker;
use encoding::Encoder;
use indexmap::IndexMap;
use itertools::Itertools;
use variable_map::VariableMap;

use watch_map::WatchMap;

use crate::{
    ConditionId, ConditionalRequirement, DenseIndex, Dependencies, DependencyProvider,
    EnvironmentPackage, KnownDependencies, Requirement, SolvableId, VariableId, VersionSetId,
    VersionSetRelation,
    conflict::Conflict,
    internal::{
        arena::Arena,
        id::{ClauseId, EnvClauseId, EnvConstrainsId, LearntClauseId},
        solver_id::{SolvableIdOrRoot, WithRootSet},
    },
    requirement::RequirementMap,
    runtime::{AsyncRuntime, NowOrNeverRuntime},
    solver::binary_encoding::AtMostOnceTracker,
    solver_id::{IdMap, IdSet, SolverId},
    utils::{IndexedSet, Mapping},
};

mod assertion_scans;
mod binary_encoding;
mod cache;
pub(crate) mod clause;
mod conditions;
mod decide_queue;
mod decision;
mod decision_map;
mod decision_tracker;
#[cfg(feature = "diagnostics")]
mod diagnostics;
mod encoding;
#[cfg(test)]
pub(crate) mod env_test_provider;
mod universal;
#[cfg(test)]
mod universal_prop;
pub(crate) mod variable_map;
mod watch_map;

pub use universal::{
    CellEdge, EnvironmentModel, UniversalFailure, UniversalProblem, UniversalSolution, Violation,
};

#[cfg(feature = "diagnostics")]
pub use universal::CellPinCounts;

/// Describes the problem that is to be solved by the solver.
///
/// This struct is generic over the type `S` of the collection of soft
/// requirements passed to the solver, typically expected to be a type
/// implementing [`IntoIterator`].
///
/// This struct follows the builder pattern and can have its fields set by one
/// of the available setter methods.
pub struct Problem<Id = SolvableId, S = EmptySolvables<Id>> {
    requirements: Vec<ConditionalRequirement>,
    constraints: Vec<VersionSetId>,
    soft_requirements: S,
    _marker: std::marker::PhantomData<fn(Id) -> Id>,
}

/// Empty soft-requirements iterator for a [`Problem`] parameterized by an ID type.
pub struct EmptySolvables<Id>(pub std::marker::PhantomData<fn(Id) -> Id>);

impl<Id> Default for EmptySolvables<Id> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<Id> Iterator for EmptySolvables<Id> {
    type Item = Id;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

impl<Id> Default for Problem<Id, EmptySolvables<Id>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id> Problem<Id, EmptySolvables<Id>> {
    /// Creates a new empty [`Problem`]. Use the setter methods to build the
    /// problem before passing it to the solver to be solved.
    pub fn new() -> Self {
        Self {
            requirements: Default::default(),
            constraints: Default::default(),
            soft_requirements: Default::default(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<Id, S: IntoIterator<Item = Id>> Problem<Id, S> {
    /// Sets the requirements that _must_ have one candidate solvable be
    /// included in the solution.
    ///
    /// Returns the [`Problem`] for further mutation or to pass to
    /// [`Solver::solve`].
    pub fn requirements(self, requirements: Vec<ConditionalRequirement>) -> Self {
        Self {
            requirements,
            ..self
        }
    }

    /// Sets the additional constraints imposed on individual packages that the
    /// solvable (if any) chosen for that package _must_ adhere to.
    ///
    /// Returns the [`Problem`] for further mutation or to pass to
    /// [`Solver::solve`].
    pub fn constraints(self, constraints: Vec<VersionSetId>) -> Self {
        Self {
            constraints,
            ..self
        }
    }

    /// Sets the additional requirements that the solver should _try_ and
    /// fulfill once it has found a solution to the main problem.
    ///
    /// An unsatisfiable soft requirement does not cause a conflict; the solver
    /// will try and fulfill as many soft requirements as possible and skip
    /// the unsatisfiable ones.
    ///
    /// Soft requirements are currently only specified as individual solvables
    /// to be included in the solution, however in the future they will be
    /// able to be specified as version sets.
    ///
    /// # Returns
    ///
    /// Returns the [`Problem`] for further mutation or to pass to
    /// [`Solver::solve`].
    pub fn soft_requirements<I: IntoIterator<Item = Id>>(
        self,
        soft_requirements: I,
    ) -> Problem<Id, I> {
        Problem {
            requirements: self.requirements,
            constraints: self.constraints,
            soft_requirements,
            _marker: std::marker::PhantomData,
        }
    }
}

pub(crate) struct Clauses<N> {
    pub(crate) kinds: Vec<Clause<N>>,
    watched_literals: Vec<Option<WatchedLiterals>>,
}

impl<N> Default for Clauses<N> {
    fn default() -> Self {
        Self {
            kinds: Vec::new(),
            watched_literals: Vec::new(),
        }
    }
}

impl<N> Clauses<N> {
    pub fn alloc(
        &mut self,
        watched_literals: Option<WatchedLiterals>,
        kind: Clause<N>,
    ) -> ClauseId {
        let id = ClauseId::from_index(self.kinds.len());
        debug_assert_ne!(
            id,
            ClauseId::assumption(),
            "the clause arena grew into the reserved assumption sentinel id"
        );
        self.kinds.push(kind);
        self.watched_literals.push(watched_literals);
        id
    }
}

type RequirementCandidateVariables = Vec<Vec<VariableId>>;

/// Drives the SAT solving process.
pub struct Solver<D: DependencyProvider, RT: AsyncRuntime = NowOrNeverRuntime> {
    /// The runtime to use for async operations.
    pub(crate) async_runtime: RT,

    /// A cache that stores request to the dependency provider.
    pub(crate) cache: SolverCache<D>,

    /// Holds the current state of the solver.
    pub(crate) state: SolverState<D>,

    /// The activity add factor. This is a value that is added to the activity
    /// score of each package that is part of a conflict.
    activity_add: f32,

    /// The activity decay factor. This is a value between 0 and 1 with which
    /// the activity scores of each package are multiplied when a conflict is
    /// detected.
    activity_decay: f32,

    /// Conflicts a single `run_sat` call may accumulate before the
    /// env-literals-last ordering is suspended for the rest of that run (see
    /// [`SolverState::env_ordering_suspended`]). Defaults to
    /// [`ENV_ORDERING_CONFLICT_LIMIT`]; overridable for tests and
    /// experiments through `Solver::set_env_ordering_conflict_limit`.
    env_ordering_conflict_limit: u64,

    /// Work multiple of the refutation switch's work-based co-trigger (see
    /// [`ENV_ORDERING_WORK_FACTOR`]); overridable for experiments.
    env_ordering_work_factor: u64,

    /// Floor of the work-based co-trigger (see
    /// [`ENV_ORDERING_WORK_FLOOR`]); overridable for experiments.
    env_ordering_work_floor: u64,
}

type RequiresClause = (Requirement, Option<DisjunctionId>, ClauseId);

/// The running best of the `decide()` fold (see [`Solver::decide`]).
struct PossibleDecision {
    /// The activity of the package that is selected.
    package_activity: f32,

    /// If this decision is based on a requirement that is explicitly
    /// requested by the user.
    is_explicit_requirement: bool,

    /// The total number of possible candidates that are available for
    /// this requirement.
    candidate_count: u32,

    /// The decision to make.
    decision: (VariableId, VariableId, ClauseId),
}

/// The env-literals-last deferral class of an eligible decision (see
/// [`Solver::decision_class`]). Classes are resolved in declaration order:
/// ordinary decisions first, then installs that would assign environment
/// literals, then the environment literals themselves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DecisionClass {
    Ordinary,
    EnvParent,
    EnvLiteral,
}

/// Applies the decision-selection heuristics (explicit requirements first,
/// then package activity, then fewest candidates) to one candidate decision
/// within its class slot. This is the exact replacement rule of the
/// reference scan, shared by the deferral-class accumulators of
/// [`Solver::decide`].
fn consider(slot: &mut Option<PossibleDecision>, new: PossibleDecision) {
    match slot {
        None => *slot = Some(new),
        Some(best) => {
            // Prefer decisions on explicit requirements over non-explicit
            // requirements. This optimizes direct dependencies over
            // transitive dependencies.
            if best.is_explicit_requirement && !new.is_explicit_requirement {
                return;
            }

            // Prefer decisions with a higher package activity score to
            // root out conflicts faster.
            if best.package_activity >= new.package_activity {
                return;
            }

            if best.candidate_count <= new.candidate_count {
                return;
            }

            *slot = Some(new);
        }
    }
}

pub(crate) struct SolverState<D: DependencyProvider> {
    pub(crate) clauses: Clauses<D::NameId>,
    requires_clauses: IndexMap<VariableId, Vec<RequiresClause>, ahash::RandomState>,

    /// The payloads of `Clause::EnvConstrains` clauses, stored out-of-line
    /// (like `learnt_clauses`) to keep the `Clause` enum small.
    pub(crate) env_constrains: Arena<EnvConstrainsId, EnvConstrainsClause>,

    /// The payloads of `Clause::EnvClause` clauses (environment model and
    /// blocking clauses), stored out-of-line to keep the `Clause` enum small.
    /// Only populated by `solve_universal`.
    pub(crate) env_clauses: Arena<EnvClauseId, EnvClause>,

    /// The clause ids of all `Clause::EnvClause` clauses, iterated during
    /// propagation to apply single-literal clauses as assertions (mirrors
    /// `unit_learnt_clause_ids`, except env clauses are few enough that the
    /// multi-literal ones are filtered during the scan).
    env_clause_ids: Vec<ClauseId>,

    /// The subset of `env_clauses` that are blocking clauses, iterated in
    /// `decide()` to make sure every solution's undecided-counts-as-false
    /// completion satisfies them.
    blocking_clauses: Vec<(EnvClauseId, ClauseId)>,

    /// Index of clause ids that can contribute to a cell's support during
    /// universal solving: `Requires` clauses whose requirement candidates are
    /// environment literals or whose condition disjunction contains
    /// environment literals, and `EnvConstrains` clauses. Populated at clause
    /// creation in the encoder. Oracle consistency, model and blocking
    /// clauses are never support.
    env_support_clauses: Vec<ClauseId>,

    /// For every solvable variable whose install adds or activates clauses
    /// that assign environment literals, the environment literal variables
    /// those clauses assign: the env candidates of its `Requires` clauses
    /// and the absent/matches literals of its `EnvConstrains` clauses.
    /// Populated at clause creation in the encoder and persistent across the
    /// cells of a universal enumeration, which is what gives the
    /// env-literals-last ordering cross-cell knowledge: a candidate's
    /// clauses are encoded only after its first install, so the first cell
    /// treats it like any other candidate, and every later cell defers it
    /// while one of its literals is unassigned (see
    /// [`SolverState::env_install_pending`] and
    /// docs/design/universal-env-literals-last.md).
    env_sensitive_parents: HashMap<VariableId, Vec<VariableId>>,

    /// Whether any environment literal variable has been interned. Gates the
    /// env-literals-last classification in `decide()`: a plain concrete
    /// solve never interns one, so its decision order is untouched.
    env_ordering_active: bool,

    /// Whether the solver is running a universal solve. Set by
    /// `solve_universal` before any encoding. The encoder uses it to resolve
    /// conditional requirements *eagerly* (in one fetch) rather than via the
    /// lazy deferred path: universal cell enumeration depends on the order in
    /// which requires clauses are registered, and the deferred path's extra
    /// fetch round-trip reorders them, which breaks the reseed fixed-point.
    /// A plain solve keeps the lazy path (it has no such ordering
    /// requirement).
    universal_mode: bool,

    /// Conflicts handled by the current `run_sat` call (reset on entry).
    /// Drives the refutation switch below.
    run_conflicts: u64,

    /// Whether the env-literals-last ordering is suspended for the rest of
    /// the current `run_sat` call. The ordering is optimal for FINDING a
    /// solution (env literals land on top of the trail, so cell transitions
    /// retract almost nothing), but anti-optimal for REFUTATIONS: deferring
    /// env-sensitive installs makes the search exhaust concrete subtrees
    /// before it can reach the env contradictions that prune them. The
    /// refutation switch (see
    /// docs/design/universal-refutation-ordering.md) flips this flag in
    /// two stages:
    ///
    /// - Stage 1 ([`Solver::learn_from_conflict`]): a run that accumulated
    ///   [`ENV_ORDERING_CONFLICT_LIMIT`] conflicts falls back to the plain
    ///   decision order but keeps its trail (conflict-rich runs can still
    ///   be marching to a solution).
    /// - Stage 2 ([`Solver::propagate_impl`] via
    ///   [`Self::env_ordering_work_deadline`]): a run that also burnt a
    ///   real propagation budget after its first conflict is refuting;
    ///   it suspends AND restarts with an activity reset
    ///   ([`Self::env_ordering_restart_pending`]).
    ///
    /// Reset by `run_sat` on entry; never set while
    /// [`Self::env_ordering_active`] is false, and irrelevant then
    /// (`env_ordering_enabled` is gated on both).
    env_ordering_suspended: bool,

    /// Set by both stages of the refutation switch; makes
    /// [`Solver::propagate_and_learn`] retract the trail to
    /// [`Self::run_root_level`] (a classic CDCL restart: learnt clauses
    /// and the root install survive, search decisions do not). Suspension
    /// alone only changes FUTURE decisions, while the trail at trip time
    /// is still shaped by the deferral (env literals undecided underneath
    /// a deep concrete subtree); measurements show the rest of the
    /// refutation pays for that shape long after the switch flipped. At
    /// most two restarts per run (one per stage; each stage fires once).
    env_ordering_restart_pending: bool,

    /// Set by stage 2 only: the restart also resets the package activity
    /// scores. The bumps accumulated under the deferral reflect conflicts
    /// of an ordering the run has abandoned, and a long neutral refutation
    /// otherwise keeps chasing them (measured 3x on the worst corpus
    /// refutation outlier). Stage 1 keeps the scores: resetting them on
    /// every conflict-limit trip wrecked an otherwise healthy mechanical
    /// enumeration by 5x (problem 95), so the reset needs the stronger
    /// work-deadline evidence.
    env_ordering_reset_pending: bool,

    /// The work-based co-trigger of the refutation switch: the value of
    /// [`Self::propagated_total`] at which the current run suspends the
    /// ordering even without crossing the conflict limit. Armed by the
    /// FIRST conflict of a run at `max(ENV_ORDERING_WORK_FACTOR *
    /// fresh_solve_cost, ENV_ORDERING_WORK_FLOOR)` propagated decisions
    /// ahead, and deliberately never at run entry: a conflict-free run is
    /// marching to a solution over a kept trail prefix and must not be
    /// restarted (measured: tripping conflict-free runs reverts
    /// refutation-heavy problems to their pre-trail-reuse cost). This
    /// catches propagation-heavy, conflict-light refutations: under the
    /// deferral, refuting a dead environment region means exhausting whole
    /// concrete subtrees per conflict, so a run can burn orders of
    /// magnitude more work than a fresh solve while staying under any
    /// reasonable conflict limit. `None` when disarmed (no conflict yet,
    /// no env literals, or already suspended).
    env_ordering_work_deadline: Option<u64>,

    /// The decision level of the root install of the current `run_sat`
    /// call (`starting_level + 1`): everything at or below it is the
    /// preserved prior state plus the root install (assumption decisions
    /// of a seeded cell, a preserved prior solution under a soft
    /// requirement). This is the shared retract target of BOTH restart
    /// triggers, the Luby pacing ([`Solver::maybe_restart`]) and the
    /// refutation switch ([`Solver::take_env_ordering_restart`]); see
    /// [`Solver::restart_to_run_root`].
    run_root_level: u32,

    /// The stage-1 conflict limit in effect for the current run. Normally
    /// the configured limit; the run directly after a stage-1 trip gets
    /// [`ENV_ORDERING_COOLDOWN_FACTOR`] times that. A tripped run records
    /// its cell with a neutral-shaped trail, so the next run re-derives
    /// that trail with warm-up-like conflicts; without the cooldown the
    /// rebuild itself re-trips at the limit and the enumeration locks into
    /// a trip-rebuild-trip loop where every transition costs a fresh solve
    /// (measured: 21 consecutive trips and a 2.5x regression on problem
    /// 491).
    run_conflict_limit: u64,

    /// Set by a stage-1 trip; makes the next `run_sat` call raise its
    /// conflict limit (see [`Self::run_conflict_limit`]). Cleared at every
    /// run entry after being applied, so the cooldown lasts exactly one
    /// run unless that run trips again.
    env_ordering_cooldown: bool,

    watches: WatchMap,

    /// A mapping from requirements to the variables that represent the
    /// candidates.
    requirement_to_sorted_candidates: RequirementMap<RequirementCandidateVariables>,

    pub(crate) variable_map: VariableMap<D::NameId, D::SolvableId>,

    negative_assertions: Vec<(VariableId, ClauseId)>,

    learnt_clauses: Arena<LearntClauseId, Vec<Literal>>,
    learnt_why: Mapping<LearntClauseId, Vec<ClauseId>>,

    /// The clause ids of single-literal learnt clauses. They have no
    /// watches, so `decide_learned` (re-)applies their assertions on every
    /// propagation round; the scan length is a direct per-`propagate()`
    /// cost, and multi-literal learnt clauses propagate through their
    /// watches, so only the units belong here.
    unit_learnt_clause_ids: Vec<ClauseId>,

    disjunctions: Arena<DisjunctionId, Disjunction>,

    clauses_added_for_package: <D::NameId as SolverId>::Set,
    clauses_added_for_solvable: WithRootSet<D::SolvableId>,
    at_most_one_trackers: HashMap<D::NameId, AtMostOnceTracker<VariableId>>,

    /// Keeps track of auxiliary variables that are used to encode at-least-one
    /// solvable for a package.
    at_least_one_tracker: <D::NameId as SolverId>::Map<Option<VariableId>>,

    /// The auxiliary variable of the shared constrains encoding per version
    /// set. The [`Clause::ConstrainsExcluded`] clauses are emitted exactly
    /// once, when the variable is allocated.
    constrains_aux_vars: HashMap<VersionSetId, VariableId>,

    /// Records environment package metadata (particularly `can_be_absent`)
    /// for packages declared via `PackageCandidates::Environment`. Populated
    /// in `on_candidates_available`.
    pub(crate) env_packages: <D::NameId as SolverId>::Map<Option<EnvironmentPackage>>,
    pub(crate) decision_tracker: DecisionTracker,

    /// The number of assumption decision levels currently active: decisions
    /// at levels `1..=assumption_levels` are seeded-cell assumptions pushed
    /// by `solve_universal`, derived from [`ClauseId::assumption`]. Zero
    /// outside seeded solves. While non-zero, conflict analysis must never
    /// backtrack below this boundary (see [`Solver::learn_from_conflict`]
    /// and [`Solver::analyze`]); a conflict at or below it means the seeded
    /// cell is unsolvable as seeded, not that the problem is unsolvable.
    pub(crate) assumption_levels: u32,

    /// Total decisions propagated by this state, ever. Drives the kept-prefix
    /// work budget; counted unconditionally (one increment per propagated
    /// decision, negligible next to the watchlist traversal it pays for).
    propagated_total: u64,

    /// While the current `run_sat` started from a kept trail prefix
    /// (trail-prefix preservation in `solve_universal`'s free phase), the
    /// value of [`Self::propagated_total`] at which the run must give up.
    /// Reuse pays off when the new region is a small edit of the previous
    /// solution; a prefix-started run that needs real search performs far
    /// worse than a restart-from-scratch enumeration (the inherited trail
    /// and the solver state shaped by earlier reused transitions steer it
    /// badly), and the damage is not repairable mid-run. Once the run has
    /// propagated several times the cost of a from-scratch solve it aborts
    /// with [`PrefixBudgetExhausted`] so that `solve_universal` can rebuild
    /// and re-enumerate with trail reuse disabled, seeded by the cells found
    /// so far. `None` when no prefix budget is armed.
    prefix_budget_deadline: Option<u64>,

    /// The propagation cost of the most recent from-scratch solve of the
    /// current universal enumeration (the first cell), used to calibrate
    /// [`Self::prefix_budget_deadline`]. Zero until the first cell is
    /// recorded.
    fresh_solve_cost: u64,

    /// Decisions propagated by prefix-started runs in the current universal
    /// enumeration. Complements the per-run deadline: a reuse attempt can
    /// also fail by bleeding moderate overhead on every transition without
    /// any single run crossing its deadline. Compared against
    /// [`Self::prefix_cumulative_budget`].
    prefix_spent: u64,

    /// Cumulative work budget for all prefix-started runs, maintained by
    /// `solve_universal` as the enumeration records cells: trail reuse must
    /// on average cost less than one from-scratch solve per cell or it is
    /// not paying for itself.
    prefix_cumulative_budget: u64,

    /// Conflicts learnt since the last restart (of either trigger) or
    /// since entry of the current `run_sat` call. Compared against
    /// [`Self::restart_limit`]; reset by both the Luby restart and a
    /// consumed refutation-switch restart (a new descent begins either
    /// way), while the Luby index advances only with the Luby trigger.
    conflicts_since_restart: u64,

    /// The number of Luby restarts performed by the current `run_sat`
    /// call; indexes the Luby sequence that paces [`Self::restart_limit`].
    /// Refutation-switch restarts do not advance it.
    restarts_performed: u64,

    /// The conflict count at which the next Luby restart fires (see
    /// [`Solver::maybe_restart`]). Both restart triggers retract to
    /// [`Self::run_root_level`].
    restart_limit: u64,

    /// Activity score per package.
    name_activity: <D::NameId as SolverId>::Map<f32>,

    /// The largest activity stored in `name_activity`, maintained bit-exactly
    /// next to the bumps and decays. Used by the decision fold to stop early
    /// when no remaining item can have a strictly higher activity.
    max_activity: f32,

    /// Incremental work queue that tracks which requires and env-constrains
    /// clauses are eligible for the next decision. The encoder registers each
    /// clause as it is encoded; the queue is kept in lockstep with the
    /// assignment trail by lazy sync. See [`decide_queue`].
    decide_queue: decide_queue::DecideQueue<D>,

    /// Conditional requirements whose condition did not hold at the moment the
    /// encoder reached them, and whose requirement candidates have therefore
    /// not been fetched. Keyed by `ConditionId`; each entry is a list of
    /// per-disjunct deferred requirements that share that condition.
    ///
    /// Invariant: an entry remains in this map only as long as the
    /// corresponding requires clause has *not* been encoded. The first time a
    /// deferred entry's disjunct fires the entry is drained and the encoder
    /// builds its requires clause exactly as the eager path would have. Once
    /// encoded the entry is removed; condition firings later in the solve
    /// (after backtracking or otherwise) do not re-encode it.
    deferred_requirements:
        IndexMap<ConditionId, Vec<DeferredRequirement<D::SolvableId>>, ahash::RandomState>,

    /// Incremental scan state for the per-propagate assertion scans over
    /// [`Self::negative_assertions`], [`Self::unit_learnt_clause_ids`] and
    /// [`Self::env_clause_ids`]: only entries appended since the last
    /// propagation round or invalidated by backtracking are visited.
    assertion_scans: AssertionScans,
    #[cfg(feature = "diagnostics")]
    propagation_counters: PropagationCounters,
}

impl<D: DependencyProvider> Default for SolverState<D> {
    fn default() -> Self {
        Self {
            clauses: Default::default(),
            requires_clauses: Default::default(),
            env_constrains: Default::default(),
            env_clauses: Default::default(),
            env_clause_ids: Default::default(),
            blocking_clauses: Default::default(),
            env_support_clauses: Default::default(),
            env_sensitive_parents: Default::default(),
            env_ordering_active: false,
            universal_mode: false,
            run_conflicts: 0,
            env_ordering_suspended: false,
            env_ordering_restart_pending: false,
            env_ordering_reset_pending: false,
            env_ordering_work_deadline: None,
            run_root_level: 0,
            run_conflict_limit: ENV_ORDERING_CONFLICT_LIMIT,
            env_ordering_cooldown: false,
            watches: Default::default(),
            requirement_to_sorted_candidates: Default::default(),
            variable_map: Default::default(),
            negative_assertions: Default::default(),
            learnt_clauses: Default::default(),
            learnt_why: Default::default(),
            unit_learnt_clause_ids: Default::default(),
            disjunctions: Default::default(),
            clauses_added_for_package: Default::default(),
            clauses_added_for_solvable: Default::default(),
            at_most_one_trackers: Default::default(),
            at_least_one_tracker: Default::default(),
            constrains_aux_vars: Default::default(),
            env_packages: Default::default(),
            decision_tracker: Default::default(),
            assumption_levels: 0,
            propagated_total: 0,
            prefix_budget_deadline: None,
            fresh_solve_cost: 0,
            prefix_spent: 0,
            prefix_cumulative_budget: 0,
            conflicts_since_restart: 0,
            restarts_performed: 0,
            restart_limit: u64::MAX,
            name_activity: Default::default(),
            max_activity: 0.0,
            decide_queue: Default::default(),
            deferred_requirements: Default::default(),
            assertion_scans: Default::default(),
            #[cfg(feature = "diagnostics")]
            propagation_counters: Default::default(),
        }
    }
}

/// Work budget multiple for a `run_sat` that starts from a kept trail
/// prefix, in units of the from-scratch solve cost (see
/// [`SolverState::prefix_budget_deadline`]). Mechanical cell-to-cell hops
/// cost a fraction of a fresh solve and the heaviest legitimate transitions
/// observed in the benchmark corpus stay around ten; the pathological runs
/// this budget exists for cost hundreds to thousands of fresh solves.
const PREFIX_BUDGET_FACTOR: u64 = 16;

/// Lower bound for the kept-prefix work budget, so that trivial first cells
/// do not abort transitions that are cheap in absolute terms.
const PREFIX_BUDGET_FLOOR: u64 = 10_000;

/// Conflicts a single `run_sat` call may accumulate before the
/// env-literals-last ordering is suspended for the rest of that run (see
/// [`SolverState::env_ordering_suspended`] and
/// docs/design/universal-refutation-ordering.md).
///
/// THE SWITCH IS DISABLED BY DEFAULT. It was designed and tuned before
/// the Luby restarts landed; the corrected full-corpus A/B (night-2
/// acceptance in the benchmark report; the first pair was invalidated
/// by a tool build mistake) shows that with restarts present the
/// switch is a net negative: off recovers four capped problems (265,
/// 433, 704, 750) and the mechanical-enumeration slowdowns (problem
/// 106 runs 10x faster) against one loss (577, which completes at
/// ~102 s), with a lower total wall. The machinery stays for research
/// through `Solver::set_env_ordering_conflict_limit` and
/// `Solver::set_env_ordering_work_budget`.
const ENV_ORDERING_CONFLICT_LIMIT: u64 = u64::MAX;

/// Work multiple for the refutation switch's work-based co-trigger, in
/// units of the from-scratch solve cost (see
/// [`SolverState::env_ordering_work_deadline`]). Zero together with a
/// zero floor means the co-trigger is disabled, which is the default
/// (see [`ENV_ORDERING_CONFLICT_LIMIT`] for the corpus rationale).
const ENV_ORDERING_WORK_FACTOR: u64 = 0;

/// Lower bound for the work-based co-trigger, used directly while no
/// fresh-solve cost has been recorded yet (the first run of an
/// enumeration) and as a floor for trivially cheap first cells. Zero
/// together with a zero factor means the co-trigger is disabled.
const ENV_ORDERING_WORK_FLOOR: u64 = 0;

/// Upper bound for the work-based co-trigger (clamped after the
/// fresh-cost scaling, but never below the floor override): on problems
/// whose first cell is itself expensive, `factor * fresh_solve_cost`
/// grows past any per-run cost ever observed on a healthy run, and the
/// trigger would go deaf exactly where it is needed (the measured
/// refutation outliers all have expensive fresh solves).
const ENV_ORDERING_WORK_CAP: u64 = 8_000_000;

/// Conflict-limit multiplier for the run directly after a stage-1 trip
/// (see [`SolverState::run_conflict_limit`]): the rebuild of a
/// neutral-shaped trail must be able to finish ordered, or the trip
/// chains into a permanent trip-rebuild loop.
const ENV_ORDERING_COOLDOWN_FACTOR: u64 = 4;

/// Cancellation sentinel raised by [`Solver::learn_from_conflict`] when a
/// prefix-started run exhausts its propagation-work budget (see
/// [`PREFIX_BUDGET_FACTOR`] and [`PREFIX_BUDGET_FLOOR`]). `solve_universal`
/// intercepts it before it can escape to the caller.
pub(crate) struct PrefixBudgetExhausted;

/// Base interval, in learnt conflicts, of the Luby restart sequence: the
/// n-th restart of a `run_sat` call fires after `luby(n) *
/// RESTART_BASE_INTERVAL` conflicts. Classic CDCL restarts keep all learnt
/// clauses and activity scores but abandon the current partial assignment,
/// so a search that walked into a hopeless subtree under stale heuristics
/// re-descends under the post-conflict ones.
const RESTART_BASE_INTERVAL: u64 = 128;

/// The Luby sequence (1, 1, 2, 1, 1, 2, 4, 1, ...) for `x >= 0`, the
/// textbook universally-optimal restart pacing: frequent short intervals
/// with geometrically rarer long ones, so neither short- nor long-tailed
/// conflict distributions degenerate.
fn luby(mut x: u64) -> u64 {
    let (mut size, mut seq) = (1u64, 0u32);
    while size < x + 1 {
        seq += 1;
        size = 2 * size + 1;
    }
    while size - 1 != x {
        size = (size - 1) >> 1;
        seq -= 1;
        x %= size;
    }
    1u64 << seq
}

/// Counters that track propagation loop behavior for performance analysis.
#[cfg(feature = "diagnostics")]
#[derive(Default)]
pub(crate) struct PropagationCounters {
    pub decisions_propagated: u64,
    /// Total number of clause visits during watch traversal.
    pub clause_visits: u64,
    /// Number of times other_watched was already true (early skip).
    pub early_skips: u64,
    /// Number of times we found a new unwatched literal (watch moved).
    pub watch_moves: u64,
    /// Number of times we had to unit-propagate (no unwatched literal found).
    pub unit_propagations: u64,
    /// Breakdown of [`Self::clause_visits`] by clause type.
    pub visits_by_type: PropagationVisitsByType,
    /// Breakdown of `next_unwatched_literal` calls by clause type.
    pub unwatched_calls_by_type: PropagationVisitsByType,
    pub propagate_calls: u64,
    pub conflicts: u64,
    /// Times the Luby restart policy fired (see [`Solver::maybe_restart`]).
    pub restarts: u64,
    /// For each recorded cell of a universal solve, the number of decisions
    /// propagated while solving that cell (the delta of
    /// [`Self::decisions_propagated`] between cell recordings). Empty for a
    /// plain solve.
    pub cell_decisions: Vec<u64>,
    /// For each recorded free-phase cell of a universal solve with trail
    /// reuse, the retract target chosen before adding the cell's blocking
    /// clause (clamped to the trail depth; an unfalsified blocking clause
    /// needs no retraction) and the trail depth at that point. Empty for a
    /// plain solve and for seeded cells.
    pub cell_retracts: Vec<(u32, u32)>,
    /// For each recorded cell of a universal solve, the number of cell
    /// literals each pinning rule contributed (load-bearing extraction
    /// versus disjointness-repair appends). Empty for a plain solve.
    pub cell_pins: Vec<universal::CellPinCounts>,
    /// Times the kept-prefix conflict budget aborted a trail-reuse attempt.
    pub prefix_budget_aborts: u64,
    /// Times the per-run conflict limit suspended the env-literals-last
    /// ordering (stage 1 of the refutation switch, see
    /// docs/design/universal-refutation-ordering.md).
    pub env_ordering_suspensions: u64,
    /// Times the work deadline escalated a run to a restart with an
    /// activity reset (stage 2 of the refutation switch).
    pub env_ordering_restarts: u64,
    /// Time spent adding clauses from the dependency provider.
    pub encoding_duration: std::time::Duration,
    pub propagation_duration: std::time::Duration,
    pub decide_duration: std::time::Duration,
    /// Time spent in [`Self::analyze`] / `learn_from_conflict`.
    pub learn_duration: std::time::Duration,
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
pub(crate) struct PropagationVisitsByType {
    pub requires: u64,
    pub constrains: u64,
    pub forbid_multiple: u64,
    pub lock: u64,
    pub learnt: u64,
    pub any_of: u64,
    pub other: u64,
}

#[cfg(feature = "diagnostics")]
impl PropagationVisitsByType {
    fn count<N>(&mut self, clause: &Clause<N>) {
        match clause {
            Clause::Requires(..) => self.requires += 1,
            Clause::Constrains(..)
            | Clause::ConstrainsExcluded(..)
            | Clause::ConstrainsParent(..) => self.constrains += 1,
            Clause::ForbidMultipleInstances(..) => self.forbid_multiple += 1,
            Clause::Lock(..) => self.lock += 1,
            Clause::Learnt(..) => self.learnt += 1,
            Clause::AnyOf(..) => self.any_of += 1,
            _ => self.other += 1,
        }
    }
}

impl<D: DependencyProvider> Solver<D, NowOrNeverRuntime> {
    /// Creates a single threaded block solver, using the provided
    /// [`DependencyProvider`].
    pub fn new(provider: D) -> Self {
        Self {
            cache: SolverCache::new(provider),
            async_runtime: NowOrNeverRuntime,
            state: SolverState::default(),
            activity_add: 1.0,
            activity_decay: 0.95,
            env_ordering_conflict_limit: ENV_ORDERING_CONFLICT_LIMIT,
            env_ordering_work_factor: ENV_ORDERING_WORK_FACTOR,
            env_ordering_work_floor: ENV_ORDERING_WORK_FLOOR,
        }
    }
}

/// The root cause of a solver error.
#[derive(Debug)]
pub enum UnsolvableOrCancelled {
    /// The problem was unsolvable.
    Unsolvable(Conflict),
    /// The solving process was cancelled.
    Cancelled(Box<dyn Any>),
}

impl From<Conflict> for UnsolvableOrCancelled {
    fn from(value: Conflict) -> Self {
        UnsolvableOrCancelled::Unsolvable(value)
    }
}

impl From<Box<dyn Any>> for UnsolvableOrCancelled {
    fn from(value: Box<dyn Any>) -> Self {
        UnsolvableOrCancelled::Cancelled(value)
    }
}

/// The error of the inner CDCL chain ([`Solver::resolve_dependencies`],
/// [`Solver::set_propagate_learn`], [`Solver::propagate_and_learn`],
/// [`Solver::learn_from_conflict`]).
///
/// Compared to [`UnsolvableOrCancelled`] there is one extra outcome: a
/// conflict at or below the assumption boundary, which [`Solver::run_sat`]
/// translates into "unsolvable under the current assumptions" (`Ok(false)`)
/// instead of a global conflict.
#[derive(Debug)]
enum ResolveError {
    /// The problem is unsolvable, regardless of any assumptions.
    Unsolvable(Conflict),
    /// The conflict is forced at or below the assumption boundary: the
    /// problem is unsolvable under the active assumption decisions. The
    /// assumptions only steer the search and are not part of the problem,
    /// so this says nothing about global solvability.
    AssumptionConflict,
    /// The solving process was cancelled.
    Cancelled(Box<dyn Any>),
}

impl From<Conflict> for ResolveError {
    fn from(value: Conflict) -> Self {
        ResolveError::Unsolvable(value)
    }
}

/// An error during the propagation step
#[derive(Debug)]
pub(crate) enum PropagationError {
    Conflict(VariableId, bool, ClauseId),
    Cancelled(Box<dyn Any>),
}

impl Display for PropagationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PropagationError::Conflict(solvable, value, clause) => {
                write!(
                    f,
                    "conflict while propagating solvable {:?}, value {} caused by clause {:?}",
                    solvable, value, clause
                )
            }
            PropagationError::Cancelled(_) => {
                write!(f, "propagation was cancelled")
            }
        }
    }
}

impl<D: DependencyProvider, RT: AsyncRuntime> Solver<D, RT> {
    /// Returns the dependency provider used by this instance.
    pub fn provider(&self) -> &D {
        self.cache.provider()
    }

    /// Returns the number of clauses in the solver after solving.
    pub fn clause_count(&self) -> usize {
        self.state.clauses.kinds.len()
    }

    /// Total number of deferred per-disjunct conditional requirements still
    /// waiting for their condition to fire. Useful to verify the "remove on
    /// first fire" invariant of the lazy-conditional-candidates path.
    pub fn deferred_requirements_count(&self) -> usize {
        self.state.deferred_requirements_count()
    }

    /// Overrides the per-run conflict limit of the refutation switch (see
    /// [`ENV_ORDERING_CONFLICT_LIMIT`]). Diagnostics-only: tests use a low
    /// limit to exercise the switch on small universes, and benchmark
    /// builds sweep candidate values.
    #[cfg(feature = "diagnostics")]
    pub fn set_env_ordering_conflict_limit(&mut self, limit: u64) {
        self.env_ordering_conflict_limit = limit;
    }

    /// Overrides the work-based co-trigger of the refutation switch (see
    /// [`ENV_ORDERING_WORK_FACTOR`] and [`ENV_ORDERING_WORK_FLOOR`]).
    /// Passing zero for BOTH disables the co-trigger, which is the
    /// default. Diagnostics-only, for tests and benchmark sweeps.
    #[cfg(feature = "diagnostics")]
    pub fn set_env_ordering_work_budget(&mut self, factor: u64, floor: u64) {
        self.env_ordering_work_factor = factor;
        self.env_ordering_work_floor = floor;
    }

    /// The number of times the refutation switch suspended the
    /// env-literals-last ordering for the rest of a `run_sat` call
    /// (stage 1, the per-run conflict limit).
    #[cfg(feature = "diagnostics")]
    pub fn env_ordering_suspensions(&self) -> u64 {
        self.state.propagation_counters.env_ordering_suspensions
    }

    /// The number of times the refutation switch escalated a run to a
    /// restart with an activity reset (stage 2, the work deadline).
    #[cfg(feature = "diagnostics")]
    pub fn env_ordering_restarts(&self) -> u64 {
        self.state.propagation_counters.env_ordering_restarts
    }

    /// The number of times the Luby restart policy fired (distinct from
    /// the refutation-switch restarts above; see [`Solver::maybe_restart`]).
    #[cfg(feature = "diagnostics")]
    pub fn restart_count(&self) -> u64 {
        self.state.propagation_counters.restarts
    }

    /// Total conflicts handled by this solver, ever.
    #[cfg(feature = "diagnostics")]
    pub fn conflict_count(&self) -> u64 {
        self.state.propagation_counters.conflicts
    }

    /// Total decisions propagated by this solver, ever.
    #[cfg(feature = "diagnostics")]
    pub fn decisions_propagated(&self) -> u64 {
        self.state.propagation_counters.decisions_propagated
    }

    /// The number of times the kept-prefix work budget aborted a trail-reuse
    /// attempt.
    #[cfg(feature = "diagnostics")]
    pub fn prefix_budget_aborts(&self) -> u64 {
        self.state.propagation_counters.prefix_budget_aborts
    }

    /// Set the runtime of the solver to `runtime`.
    #[must_use]
    pub fn with_runtime<RT2: AsyncRuntime>(self, runtime: RT2) -> Solver<D, RT2> {
        Solver {
            async_runtime: runtime,
            cache: self.cache,
            state: self.state,
            activity_decay: self.activity_decay,
            activity_add: self.activity_add,
            env_ordering_conflict_limit: self.env_ordering_conflict_limit,
            env_ordering_work_factor: self.env_ordering_work_factor,
            env_ordering_work_floor: self.env_ordering_work_floor,
        }
    }

    /// Configure activity andd and decay parameters. This enables tweaking
    /// these parameters.
    #[must_use]
    pub fn with_activity_params(self, add: f32, decay: f32) -> Self {
        Self {
            activity_add: add,
            activity_decay: decay,
            ..self
        }
    }

    /// Solves the given [`Problem`].
    ///
    /// The solver first solves for the root requirements and constraints, and
    /// then tries to include in the solution as many of the soft
    /// requirements as it can. Each soft requirement is subject to all the
    /// clauses and decisions introduced for all the previously decided
    /// solvables in the solution.
    ///
    /// Unless the corresponding package has been requested by a version set in
    /// another solvable's clauses, each soft requirement is _not_ subject
    /// to the package-level clauses introduced in
    /// [`DependencyProvider::get_candidates`] since the solvables have been
    /// requested specifically (not through a version set) in the solution.
    ///
    /// # Returns
    ///
    /// If a solution was found, returns a [`Vec`] of the solvables included in
    /// the solution.
    ///
    /// If no solution to the _root_ requirements and constraints was found,
    /// returns a [`Conflict`] wrapped in a
    /// [`UnsolvableOrCancelled::Unsolvable`], which provides ways to
    /// inspect the causes and report them to the user. If a soft requirement is
    /// unsolvable, it is simply not included in the solution.
    ///
    /// If the solution process is cancelled (see
    /// [`DependencyProvider::should_cancel_with_value`]), returns an
    /// [`UnsolvableOrCancelled::Cancelled`] containing the cancellation value.
    pub fn solve(
        &mut self,
        problem: Problem<D::SolvableId, impl IntoIterator<Item = D::SolvableId>>,
    ) -> Result<Vec<D::SolvableId>, UnsolvableOrCancelled> {
        let result = self.solve_impl(problem);
        // Report after every outcome: unsolvable and cancelled solves are
        // exactly the ones whose counters matter when hunting pathological
        // search behavior.
        #[cfg(feature = "diagnostics")]
        self.report_diagnostics();
        result
    }

    fn solve_impl(
        &mut self,
        problem: Problem<D::SolvableId, impl IntoIterator<Item = D::SolvableId>>,
    ) -> Result<Vec<D::SolvableId>, UnsolvableOrCancelled> {
        // Re-initialize the solver state.
        self.state = SolverState::default();

        // The decision fold's activity-based shortcuts are only sound when
        // activities can never become negative and cold packages stay at
        // exactly zero.
        self.state
            .decide_queue
            .set_standard_activity_params(self.activity_add > 0.0 && self.activity_decay >= 0.0);

        // Construct the root dependencies from the problem
        let root_dependencies = Dependencies::Known(KnownDependencies {
            requirements: problem.requirements,
            constrains: problem.constraints,
        });

        // The first clause will always be the install root clause. Here we verify that
        // this is indeed the case.
        let root_clause = {
            let (state, kind) = WatchedLiterals::root();
            self.state.add_clause(state, kind)
        };
        assert_eq!(root_clause, ClauseId::install_root());

        assert!(
            self.run_sat(SolvableIdOrRoot::root(), &root_dependencies, 0)?,
            "bug: Since root is the first requested solvable, \
                  should have returned Err instead of Ok(false) if root is unsolvable"
        );

        for additional in problem.soft_requirements {
            let additional_var = self.state.variable_map.intern_solvable(additional);

            if self
                .state
                .decision_tracker
                .assigned_value(additional_var)
                .is_none()
            {
                // The solution found so far is prior state: an unsolvable
                // soft requirement reports `Ok(false)` and is left out.
                let starting_level = self.state.decision_tracker.deepest_level();
                self.run_sat(additional.into(), &root_dependencies, starting_level)?;
            }
        }

        Ok(self.state.chosen_solvables().collect())
    }

    /// Run the CDCL algorithm to solve the SAT problem
    ///
    /// The CDCL algorithm's job is to find a valid assignment to the variables
    /// involved in the provided clauses. It works in the following steps:
    ///
    /// 1. __Set__: Assign a value to a variable that hasn't been assigned yet.
    ///    An assignment in this step starts a new "level" (the first one being
    ///    level 1). If all variables have been assigned, then we are done.
    /// 2. __Propagate__: Perform [unit propagation](https://en.wikipedia.org/wiki/Unit_propagation).
    ///    Assignments in this step are associated to the same "level" as the
    ///    decision that triggered them. This "level" metadata is useful when it
    ///    comes to handling conflicts. See [`Solver::propagate`] for the
    ///    implementation of this step.
    /// 3. __Learn__: If propagation finishes without conflicts, go back to 1.
    ///    Otherwise find the combination of assignments that caused the
    ///    conflict and add a new clause to the solver to forbid that
    ///    combination of assignments (i.e. learn from this mistake so it is not
    ///    repeated in the future). Then backtrack and go back to step 1 or, if
    ///    the learnt clause is in conflict with existing clauses, declare the
    ///    problem to be unsolvable. See [`Solver::analyze`] for the
    ///    implementation of this step.
    ///
    /// The solver loop can be found in [`Solver::resolve_dependencies`].
    ///
    /// `starting_level` declares which part of the pre-existing decision
    /// stack the caller wants preserved: everything at levels
    /// `1..=starting_level` is prior state that the search must not destroy
    /// (the previous solution under a soft requirement, or the assumption
    /// decisions of a seeded universal cell), while decisions above it are
    /// restartable scratch state (a trail prefix kept from a previous
    /// universal cell, see `docs/design/universal-trail-reuse.md`) that
    /// conflicts and resets may undo.
    ///
    /// Returns `Ok(true)` if a solution was found for `solvable`. If a solution
    /// was not found, returns `Ok(false)` if `starting_level > 0` (the
    /// problem is unsolvable on top of the preserved prior decisions).
    /// Otherwise, returns [`UnsolvableOrCancelled::Unsolvable`] as an `Err`
    /// on no solution.
    ///
    /// If the solution process is cancelled (see
    /// [`DependencyProvider::should_cancel_with_value`]),
    /// returns [`UnsolvableOrCancelled::Cancelled`] as an `Err`.
    fn run_sat(
        &mut self,
        root_solvable: SolvableIdOrRoot<D::SolvableId>,
        root_deps: &Dependencies,
        starting_level: u32,
    ) -> Result<bool, UnsolvableOrCancelled> {
        let mut level = self.state.decision_tracker.deepest_level();
        debug_assert!(
            level >= starting_level,
            "bug: the preserved decisions must be on the stack when entering run_sat"
        );
        let mut new_solvables: Vec<(VariableId, ClauseId)> = Vec::new();
        let mut solvable_ids: Vec<SolvableIdOrRoot<D::SolvableId>> = Vec::new();

        // Arm the Luby restart policy for this run: `run_root_level` (set
        // below) pins everything the caller wants preserved, and the Luby
        // sequence restarts from its first interval. Each `run_sat` call
        // is its own search episode; cross-cell knowledge transfers
        // through the learnt clauses and activity scores, not the pacing.
        self.state.conflicts_since_restart = 0;
        self.state.restarts_performed = 0;
        self.state.restart_limit = RESTART_BASE_INTERVAL * luby(0);

        // Re-arm the env-literals-last ordering for this run. The ordering
        // is the right default (it is what collapses cell transitions), but
        // a run that accumulates many conflicts is refuting, and refutation
        // wants env conflicts EARLY; `learn_from_conflict` suspends the
        // ordering for the rest of the run once the per-run conflict count
        // crosses the limit.
        self.state.run_conflicts = 0;
        self.state.env_ordering_suspended = false;
        self.state.env_ordering_restart_pending = false;
        self.state.env_ordering_reset_pending = false;
        self.state.run_root_level = starting_level + 1;
        // The run after a stage-1 trip gets a raised conflict limit: it
        // must be able to re-derive the neutral-shaped trail the tripped
        // run left behind without immediately re-tripping (see
        // `SolverState::run_conflict_limit`).
        self.state.run_conflict_limit = if self.state.env_ordering_cooldown {
            self.env_ordering_conflict_limit
                .saturating_mul(ENV_ORDERING_COOLDOWN_FACTOR)
        } else {
            self.env_ordering_conflict_limit
        };
        self.state.env_ordering_cooldown = false;
        // The work-based co-trigger is armed by the FIRST conflict of the
        // run (see `learn_from_conflict`), never at entry: a run without
        // conflicts is marching to a solution over a kept trail prefix,
        // and restarting it would throw the prefix away (measured: tripping
        // conflict-free runs reverts refutation-heavy problems to their
        // pre-trail-reuse cost).
        self.state.env_ordering_work_deadline = None;

        // Arm the kept-prefix work budget (see the field docs). Only a free
        // universal solve enters with restartable decisions above the
        // starting level; all other entries leave this disarmed.
        self.state.prefix_budget_deadline = if level > starting_level {
            let budget =
                (self.state.fresh_solve_cost * PREFIX_BUDGET_FACTOR).max(PREFIX_BUDGET_FLOOR);
            Some(self.state.propagated_total + budget)
        } else {
            None
        };

        // A kept trail prefix (restartable decisions above `starting_level`)
        // may contradict clauses added since the retraction: the blocking
        // clause's unit assertion cascades through oracle consistency
        // clauses into env literals assigned below the retract target.
        // Propagate and LEARN before entering the decision loop so the
        // conflict backjumps precisely through the prefix; the in-loop
        // conflict handling below would instead restart from scratch and
        // forfeit the prefix on every such cascade.
        if level > starting_level {
            level = match self.propagate_and_learn(level) {
                Ok(level) => level,
                Err(ResolveError::Unsolvable(conflict)) => {
                    return Err(UnsolvableOrCancelled::Unsolvable(conflict));
                }
                Err(ResolveError::Cancelled(value)) => {
                    return Err(UnsolvableOrCancelled::Cancelled(value));
                }
                Err(ResolveError::AssumptionConflict) => {
                    // Only the free phase of a universal solve enters with a
                    // prefix, and it holds no assumptions; mirror the
                    // resolve_dependencies handling anyway.
                    debug_assert!(false, "bug: a kept prefix never coexists with assumptions");
                    self.state.decision_tracker.undo_until(starting_level);
                    return Ok(false);
                }
            };
        }

        loop {
            if level == starting_level {
                tracing::trace!("Level {starting_level}: Resetting the decision loop");
            } else {
                tracing::trace!("Level {}: Starting the decision loop", level);
            }

            // A level of starting_level means the decision loop has been completely reset
            // because a partial solution was invalidated by newly added clauses.
            if level == starting_level {
                // Level starting_level + 1 is the initial decision level
                level = starting_level + 1;

                // Assign `true` to the root solvable. This must be installed to satisfy the
                // solution. The root solvable contains the dependencies that
                // were injected when calling `Solver::solve`. If we can find a
                // solution were the root is installable we found a
                // solution that satisfies the user requirements.
                tracing::trace!(
                    "╤══ Install {} at level {level}",
                    root_solvable.display(self.provider())
                );
                self.state
                    .decision_tracker
                    .try_add_decision(
                        Decision::new(
                            self.state
                                .variable_map
                                .intern_solvable_or_root(root_solvable),
                            true,
                            ClauseId::install_root(),
                        ),
                        level,
                    )
                    .expect("already decided");

                // Add the clauses for the root solvable.
                #[cfg(feature = "diagnostics")]
                let encoding_start = std::time::Instant::now();
                let conflicting_clauses = self.async_runtime.block_on(
                    Encoder::new(&mut self.state, &self.cache, root_deps, level)
                        .encode([root_solvable]),
                )?;
                #[cfg(feature = "diagnostics")]
                {
                    self.state.propagation_counters.encoding_duration += encoding_start.elapsed();
                }

                if let Some(clause_id) = conflicting_clauses.into_iter().next() {
                    return self.run_sat_process_unsolvable(
                        root_solvable,
                        starting_level,
                        clause_id,
                    );
                }
            }

            tracing::trace!("Level {}: Propagating", level);

            // Propagate decisions from assignments above
            let propagate_result = self.propagate(level);

            tracing::trace!("Propagate result: {:?}", propagate_result);

            // Handle propagation errors
            match propagate_result {
                Ok(()) => {}
                Err(PropagationError::Conflict(_, _, clause_id)) => {
                    if level == starting_level + 1 {
                        return self.run_sat_process_unsolvable(
                            root_solvable,
                            starting_level,
                            clause_id,
                        );
                    } else {
                        // The conflict was caused because new clauses have been added dynamically.
                        // We need to start over.
                        tracing::debug!(
                            "├─ added clause {clause} introduces a conflict which invalidates the partial solution",
                            clause = self.state.clauses.kinds[clause_id.to_index()]
                                .display(&self.state.variable_map, self.provider())
                        );
                        level = starting_level;
                        self.state.decision_tracker.undo_until(starting_level);
                        continue;
                    }
                }
                Err(PropagationError::Cancelled(value)) => {
                    // Propagation was cancelled
                    return Err(UnsolvableOrCancelled::Cancelled(value));
                }
            }

            // Enter the solver loop, return immediately if no new assignments have been
            // made.
            tracing::trace!("Level {}: Resolving dependencies", level);
            level = match self.resolve_dependencies(level) {
                Ok(new_level) => new_level,
                Err(ResolveError::AssumptionConflict) => {
                    // The problem is unsolvable under the active assumption
                    // decisions (a seeded cell of a universal solve). Mirror
                    // the `run_sat_process_unsolvable` contract for callers
                    // with prior decisions: retract everything above the
                    // starting level (which keeps the assumptions, exactly
                    // the levels `1..=starting_level`) and report `Ok(false)`
                    // so the caller can retract the assumptions and drop the
                    // seed.
                    debug_assert!(
                        self.state.assumption_levels > 0,
                        "bug: an assumption conflict was reported without active assumptions"
                    );
                    self.state.decision_tracker.undo_until(starting_level);
                    return Ok(false);
                }
                Err(ResolveError::Unsolvable(conflict)) => {
                    return Err(UnsolvableOrCancelled::Unsolvable(conflict));
                }
                Err(ResolveError::Cancelled(value)) => {
                    return Err(UnsolvableOrCancelled::Cancelled(value));
                }
            };
            tracing::trace!("Level {}: Done resolving dependencies", level);

            // We have a partial solution. E.g. there is a solution that satisfies all the
            // clauses that have been added so far.

            // Determine which solvables are part of the solution for which we did not yet
            // get any dependencies. If we find any such solvable it means we
            // did not arrive at the full solution yet.
            new_solvables.clear();
            new_solvables.extend(
                self.state
                    .decision_tracker
                    .stack()
                    // Filter only decisions that led to a positive assignment
                    .filter(|d| d.value)
                    // Select solvables for which we do not yet have dependencies
                    .filter(|d| {
                        let Some(solvable_or_root) =
                            d.variable.as_solvable_or_root(&self.state.variable_map)
                        else {
                            return false;
                        };
                        !self
                            .state
                            .clauses_added_for_solvable
                            .contains(solvable_or_root)
                    })
                    .map(|d| (d.variable, d.derived_from)),
            );

            // Also detect deferred conditional requirements whose condition
            // has just started to hold. These have to be encoded before we can
            // conclude the solution is complete.
            let fired_conditions = self.state.fired_deferred_conditions();

            if new_solvables.is_empty() && fired_conditions.is_empty() {
                // If no new literals were selected and no deferred conditions
                // fired, this solution is complete and we can return.
                tracing::trace!(
                    "Level {}: No new solvables or fired conditions, solution is complete",
                    level
                );
                return Ok(true);
            }

            if !new_solvables.is_empty() {
                tracing::debug!("==== Found newly selected solvables");
                tracing::debug!(
                    " - {}",
                    new_solvables
                        .iter()
                        .copied()
                        .format_with("\n- ", |(id, derived_from), f| f(&format_args!(
                            "{} (derived from {})",
                            id.display(&self.state.variable_map, self.provider()),
                            self.state.clauses.kinds[derived_from.to_index()]
                                .display(&self.state.variable_map, self.provider()),
                        )))
                        .to_string()
                );
                tracing::debug!("====");
            }

            if !fired_conditions.is_empty() {
                tracing::debug!(
                    "==== Found {} deferred condition(s) that just fired",
                    fired_conditions.len()
                );
            }

            solvable_ids.clear();
            solvable_ids.extend(new_solvables.iter().filter_map(|(variable, _)| {
                self.state
                    .variable_map
                    .origin(*variable)
                    .as_solvable()
                    .map(SolvableIdOrRoot::from)
            }));

            // Drain the fired deferred requirements.
            let mut deferred_to_encode = Vec::new();
            for condition in fired_conditions {
                deferred_to_encode.extend(self.state.drain_fired_deferred(condition));
            }

            #[cfg(feature = "diagnostics")]
            let encoding_start = std::time::Instant::now();
            let conflicting_clauses = self.async_runtime.block_on(
                Encoder::new(&mut self.state, &self.cache, root_deps, level)
                    .encode_with_deferred(solvable_ids.iter().copied(), deferred_to_encode),
            )?;
            #[cfg(feature = "diagnostics")]
            {
                self.state.propagation_counters.encoding_duration += encoding_start.elapsed();
            }

            // Serially process the outputs, to reduce the need for synchronization
            for &clause_id in &conflicting_clauses {
                tracing::debug!(
                    "├─ Added clause {clause} introduces a conflict which invalidates the partial solution",
                    clause = self.state.clauses.kinds[clause_id.to_index()]
                        .display(&self.state.variable_map, self.provider())
                );
            }

            if let Some(_first_conflicting_clause_id) = conflicting_clauses.into_iter().next() {
                self.state.decision_tracker.undo_until(starting_level);
                level = starting_level;
            }
        }
    }

    /// Decides how to terminate the solver algorithm when the given `solvable`
    /// was deemed unsolvable by [`Solver::run_sat`].
    ///
    /// Returns an `Err` value of [`UnsolvableOrCancelled::Unsolvable`] only if
    /// `solvable` is the very first solvable we are solving for. Otherwise,
    /// undoes all the decisions made when trying to solve for `solvable`,
    /// sets it to `false` and returns `Ok(false)`.
    fn run_sat_process_unsolvable(
        &mut self,
        solvable_or_root: SolvableIdOrRoot<D::SolvableId>,
        starting_level: u32,
        clause_id: ClauseId,
    ) -> Result<bool, UnsolvableOrCancelled> {
        if starting_level == 0 {
            tracing::trace!(
                "Unsolvable: {}",
                self.state.clauses.kinds[clause_id.to_index()]
                    .display(&self.state.variable_map, self.provider(),)
            );
            Err(UnsolvableOrCancelled::Unsolvable(
                self.analyze_unsolvable(clause_id),
            ))
        } else {
            self.state.decision_tracker.undo_until(starting_level);
            self.state
                .decision_tracker
                .try_add_decision(
                    Decision::new(
                        self.state
                            .variable_map
                            .intern_solvable_or_root(solvable_or_root),
                        false,
                        ClauseId::install_root(),
                    ),
                    starting_level + 1,
                )
                .expect("bug: already decided - decision should have been undone");
            Ok(false)
        }
    }

    /// Resolves all dependencies
    ///
    /// Repeatedly chooses the next variable to assign, and calls
    /// [`Solver::set_propagate_learn`] to drive the solving process (as you
    /// can see from the name, the method executes the set, propagate and
    /// learn steps described in the [`Solver::run_sat`] docs).
    ///
    /// The next variable to assign is obtained by finding the next dependency
    /// for which no concrete package has been picked yet. Then we pick the
    /// highest possible version for that package, or the favored version if
    /// it was provided by the user, and set its value to true.
    fn resolve_dependencies(&mut self, mut level: u32) -> Result<u32, ResolveError> {
        loop {
            // Make a decision. If no decision could be made it means the problem is
            // satisfyable.
            #[cfg(feature = "diagnostics")]
            let decide_start = std::time::Instant::now();
            let Some((candidate, required_by, clause_id)) = self.decide() else {
                #[cfg(feature = "diagnostics")]
                {
                    self.state.propagation_counters.decide_duration += decide_start.elapsed();
                }
                break;
            };
            #[cfg(feature = "diagnostics")]
            {
                self.state.propagation_counters.decide_duration += decide_start.elapsed();
            }

            tracing::debug!(
                "╒══ Install {} at level {level} (derived from {})",
                candidate.display(&self.state.variable_map, self.provider()),
                self.state.clauses.kinds[clause_id.to_index()]
                    .display(&self.state.variable_map, self.provider())
            );

            // Propagate the decision
            match self.set_propagate_learn(level, candidate, required_by, clause_id) {
                Ok(new_level) => {
                    level = new_level;
                    tracing::debug!("╘══ Propagation completed");
                }
                Err(ResolveError::Cancelled(value)) => {
                    tracing::debug!("╘══ Propagation cancelled");
                    return Err(ResolveError::Cancelled(value));
                }
                Err(ResolveError::Unsolvable(conflict)) => {
                    tracing::debug!("╘══ Propagation resulted in a conflict");
                    return Err(ResolveError::Unsolvable(conflict));
                }
                Err(ResolveError::AssumptionConflict) => {
                    tracing::debug!("╘══ Propagation conflicted at the assumption boundary");
                    return Err(ResolveError::AssumptionConflict);
                }
            }
        }

        // We just went through all clauses and there are no choices left to be made
        Ok(level)
    }

    /// Classifies an eligible decision into its env-literals-last deferral
    /// class: environment literals themselves are decided last, candidates
    /// whose install would assign a still unassigned environment literal come
    /// before them, and everything else is ordinary. Only meaningful while
    /// [`SolverState::env_ordering_active`] is set; the test is dynamic
    /// (it reads the current assignment), so it must run at fold time and
    /// cannot be baked into the queue order key.
    fn decision_class(state: &SolverState<D>, candidate: VariableId) -> DecisionClass {
        if matches!(
            state.variable_map.origin(candidate),
            variable_map::VariableOrigin::EnvMatches(_)
                | variable_map::VariableOrigin::EnvAbsent(_)
        ) {
            DecisionClass::EnvLiteral
        } else if state.env_install_pending(candidate) {
            DecisionClass::EnvParent
        } else {
            DecisionClass::Ordinary
        }
    }

    /// Inspects one queued item: if it is eligible (parent installed,
    /// condition met, clause unsatisfied) its heuristic tuple is returned;
    /// otherwise the item leaves the queue (the wake-up rules re-insert it
    /// when any input of the inspection changes) and `None` is returned.
    fn evaluate_queued_item(
        state: &mut SolverState<D>,
        provider: &D,
        item_id: TrackedClauseId,
    ) -> Option<PossibleDecision> {
        let item = state.decide_queue.item(item_id);
        let is_explicit_requirement = item.parent == VariableId::root();
        let clause_id = item.clause_id;

        let SolverState {
            decide_queue,
            decision_tracker,
            requirement_to_sorted_candidates,
            disjunctions,
            name_activity,
            ..
        } = &mut *state;

        // The eligibility walk (parent installed, condition met, clause
        // unsatisfied) and the heuristic frontier are computed by the queue,
        // uniformly for requires and env-constrains clauses. An ineligible
        // clause leaves the queue; the wake-up rules re-insert it when any
        // input of the inspection changes.
        match decide_queue.inspect(
            item,
            decision_tracker.map(),
            requirement_to_sorted_candidates,
            disjunctions,
            provider,
        ) {
            None => {
                decide_queue.unqueue(item_id);
                None
            }
            Some((candidate, version_set, candidate_count)) => Some(PossibleDecision {
                package_activity: name_activity.get(provider.version_set_name(version_set)),
                is_explicit_requirement,
                candidate_count,
                decision: (candidate, item.parent, clause_id),
            }),
        }
    }

    /// Selects the next solvable to assign after all logical decisions have
    /// been propagated. When propagation runs dry we must guess to make
    /// progress; this picks the best guess by several heuristics, tuned to
    /// find a solution that also maximizes the user's requirements.
    ///
    /// The heuristics are (in order of importance):
    /// 1. Prefer decisions on explicit requirements over non-explicit
    ///    requirements, so direct dependencies are maximized over transitive
    ///    ones.
    /// 2. Prefer decisions with a higher "package activity score" (bumped
    ///    whenever a package is involved in a conflict, decayed each conflict;
    ///    the VSIDS idea), to root out conflicts faster.
    /// 3. Prefer decisions with the fewest remaining candidates, which needs
    ///    less backtracking to determine unsatisfiability.
    ///
    /// The selection is incremental: the
    /// [`DecideQueue`](decide_queue::DecideQueue) tracks the eligible clauses
    /// (parent installed, condition met, clause unsatisfied) and the fold
    /// visits the first eligible item and then only the eligible *hot* items
    /// after it (see the [`decide_queue`] module docs for why cold items can
    /// never replace the running best), stopping as soon as no remaining item
    /// could replace the best. The plain path uses
    /// [`next_decision`](decide_queue::DecideQueue::next_decision); the
    /// env-ordering path below drives the queue's cursor directly.
    ///
    /// Under the env-literals-last ordering (universal solving, see
    /// docs/design/universal-env-literals-last.md) the fold maintains one best
    /// accumulator per deferral class. Class membership is dynamic (it depends
    /// on the current assignment through [`SolverState::env_install_pending`]),
    /// so each eligible item is classified at fold time rather than in its
    /// order key. A finished ordinary best ends the fold; while the ordinary
    /// class is empty the fold walks every eligible item (cold items can never
    /// replace a non-empty class slot, so no hot filtering is needed on that
    /// path).
    fn decide(&mut self) -> Option<(VariableId, VariableId, ClauseId)> {
        let provider = self.cache.provider();
        {
            let state = &mut self.state;
            let sync_floor = state.decision_tracker.take_sync_floor();
            state.decide_queue.sync(
                sync_floor,
                state.decision_tracker.assignments(),
                state.decision_tracker.map(),
            );
        }

        // Plain path: main's incremental selection, with the activity-based
        // shortcut guard ([`DecideQueue::set_standard_activity_params`]). The
        // env-literals-last class machinery below only applies to universal
        // solves while the ordering is active; a plain solve never interns an
        // environment literal, so its decision order is untouched.
        if !self.state.env_ordering_enabled() {
            let queue_decision = {
                let SolverState {
                    decide_queue,
                    decision_tracker,
                    requirement_to_sorted_candidates,
                    disjunctions,
                    name_activity,
                    max_activity,
                    ..
                } = &mut self.state;
                let decision = decide_queue.next_decision(
                    decision_tracker.map(),
                    requirement_to_sorted_candidates,
                    disjunctions,
                    name_activity,
                    *max_activity,
                    provider,
                );
                #[cfg(debug_assertions)]
                decide_queue.debug_assert_invariants(
                    decision_tracker.map(),
                    requirement_to_sorted_candidates,
                    disjunctions,
                    name_activity,
                    *max_activity,
                    provider,
                );
                decision
            };

            if let Some(queue_decision) = &queue_decision {
                tracing::trace!(
                    "deciding to assign {}, ({}, {} activity score, {} possible candidates)",
                    queue_decision
                        .candidate
                        .display(&self.state.variable_map, self.provider()),
                    self.state.clauses.kinds[queue_decision.clause_id.to_index()]
                        .display(&self.state.variable_map, self.provider()),
                    queue_decision.package_activity,
                    queue_decision.candidate_count,
                );
            }

            return queue_decision.map(|queue_decision| {
                (
                    queue_decision.candidate,
                    queue_decision.required_by,
                    queue_decision.clause_id,
                )
            });
        }

        let state = &mut self.state;
        // The plain path returned above via `next_decision`; reaching here
        // means the env-literals-last ordering is active.
        debug_assert!(state.env_ordering_enabled());

        // Phase 1: the first eligible *ordinary* item in scan order is the
        // initial best. Items reached on the way are proven ineligible and
        // leave the queue until a wake-up re-inserts them, so this scan is
        // amortized by queue insertions; eligible deferred items stay queued
        // and fold into their class slots.
        let mut best_decision: Option<PossibleDecision> = None;
        let mut best_env_parent: Option<PossibleDecision> = None;
        let mut best_env_literal: Option<PossibleDecision> = None;
        let mut cursor: Option<ClausePosition> = None;
        loop {
            let Some((key, item_id)) = state.decide_queue.next_after(cursor) else {
                break;
            };
            cursor = Some(key);
            let Some(found) = Self::evaluate_queued_item(state, provider, item_id) else {
                continue;
            };
            match Self::decision_class(state, found.decision.0) {
                DecisionClass::Ordinary => {
                    best_decision = Some(found);
                    break;
                }
                DecisionClass::EnvParent => consider(&mut best_env_parent, found),
                DecisionClass::EnvLiteral => consider(&mut best_env_literal, found),
            }
        }

        // Phase 2: fold the replacement rule over the eligible *hot* items
        // after the initial best, in scan order. A replacement needs a
        // strictly higher package activity, activities are non-negative,
        // and a package that was never involved in a conflict has activity
        // zero, so only hot items can ever replace the running best;
        // skipping the cold ones cannot change the fold result. Stop as
        // soon as replacement becomes impossible: when the best's
        // candidate count is 1 (a replacement needs strictly fewer
        // candidates) or its activity equals the global maximum (strictly
        // higher is impossible).
        if let Some(best_key) = cursor.filter(|_| best_decision.is_some()) {
            let mut hot_cursor = best_key;
            loop {
                // Stop once nothing can beat the best: a replacement needs
                // strictly fewer candidates (so a count of one is unbeatable)
                // and, with standard activity parameters, strictly higher
                // activity (so the global maximum is unbeatable).
                {
                    let best = best_decision.as_ref().expect("set in phase 1");
                    if best.candidate_count <= 1 || best.package_activity >= state.max_activity {
                        break;
                    }
                }
                let Some((key, item_id)) = state.decide_queue.next_hot_after(hot_cursor) else {
                    break;
                };
                hot_cursor = key;
                let Some(found) = Self::evaluate_queued_item(state, provider, item_id) else {
                    continue;
                };
                // Deferred items cannot replace an ordinary best: class
                // precedence makes the ordinary result final.
                if Self::decision_class(state, found.decision.0) != DecisionClass::Ordinary {
                    continue;
                }
                // Apply the shared replacement rule (explicit-first, then
                // strictly-higher activity, then strictly-fewer candidates) —
                // the same `consider` used to fold the deferral-class slots.
                consider(&mut best_decision, found);
            }
        }

        // Deferred decisions (and blocking-clause completions below) are
        // only taken when the encoder has nothing pending: clauses are
        // encoded in batches when the decision loop runs dry, so a batch
        // boundary can leave only deferred candidates visible even though
        // the not-yet-encoded solvables will reveal more ordinary
        // decisions. Returning no decision hands control back to `run_sat`,
        // which encodes the pending solvables and re-enters the loop;
        // without this, the encode cadence would force environment literals
        // onto the trail mid-solve, defeating the env-literals-last
        // ordering.
        if best_decision.is_none()
            && (best_env_parent.is_some()
                || best_env_literal.is_some()
                || !self.state.blocking_clauses.is_empty())
            && self.has_pending_clause_encodes()
        {
            return None;
        }

        // Resolve the classes in order: ordinary decisions first, then
        // candidates whose install would assign environment literals, then
        // the environment literals themselves.
        let mut best_decision = best_decision.or(best_env_parent).or(best_env_literal);

        // Finally, make progress on blocking clauses (universal solving
        // only; the list is empty in a plain solve). A blocking clause is a
        // disjunction of signed environment literals. Watches alone do not
        // guarantee that the undecided-counts-as-false completion of a
        // solution satisfies a clause with two or more positive literals
        // (all of them can simply stay undecided), which would let the
        // enumeration loop rediscover an already-blocked cell and break the
        // disjointness-repair invariant. Whenever nothing else is left to
        // decide and a blocking clause is not yet satisfied under the
        // completion, decide its first undecided positive literal to true.
        if best_decision.is_none() {
            'blocking: for &(env_clause_id, clause_id) in &self.state.blocking_clauses {
                debug_assert_eq!(
                    self.state.env_clauses[env_clause_id].kind,
                    EnvClauseKind::Blocking,
                    "only blocking clauses are registered for decide()"
                );
                let literals = &self.state.env_clauses[env_clause_id].literals;
                if literals.len() <= 1 {
                    // Single-literal blocking clauses are assertions, applied
                    // during propagation.
                    continue;
                }

                let mut first_undecided_positive = None;
                for &literal in literals {
                    let assigned = self
                        .state
                        .decision_tracker
                        .assigned_value(literal.variable());
                    // The literal's value under the undecided-counts-as-false
                    // completion: an undecided variable evaluates positive
                    // literals to false and negative literals to true.
                    let completed =
                        assigned.map_or(literal.negate(), |value| value != literal.negate());
                    if completed {
                        // The clause is already satisfied under completion.
                        continue 'blocking;
                    }
                    if !literal.negate() && assigned.is_none() && first_undecided_positive.is_none()
                    {
                        first_undecided_positive = Some(literal.variable());
                    }
                }

                let candidate = first_undecided_positive.expect(
                    "an unsatisfied blocking clause must have an undecided positive literal; \
                     a fully-false clause would have conflicted during propagation",
                );
                best_decision = Some(PossibleDecision {
                    is_explicit_requirement: false,
                    package_activity: 0.0,
                    candidate_count: 1,
                    decision: (candidate, VariableId::root(), clause_id),
                });
                break;
            }
        }

        if let Some(PossibleDecision {
            candidate_count,
            package_activity,
            decision: (candidate, _solvable_id, clause_id),
            ..
        }) = &best_decision
        {
            tracing::trace!(
                "deciding to assign {}, ({}, {} activity score, {} possible candidates)",
                candidate.display(&self.state.variable_map, self.provider()),
                self.state.clauses.kinds[clause_id.to_index()]
                    .display(&self.state.variable_map, self.provider()),
                package_activity,
                candidate_count,
            );
        }

        best_decision.map(
            |PossibleDecision {
                 decision: (candidate, required_by, via),
                 ..
             }| { (candidate, required_by, via) },
        )
    }

    /// Returns true when a solvable chosen by the current assignment still
    /// has its clauses pending encoding (the `new_solvables` scan of
    /// [`Solver::run_sat`] would find it). Used by [`Solver::decide`] to
    /// postpone deferred env decisions until the formula for every chosen
    /// solvable is present.
    fn has_pending_clause_encodes(&self) -> bool {
        self.state.decision_tracker.stack().any(|d| {
            d.value
                && d.variable
                    .as_solvable_or_root(&self.state.variable_map)
                    .is_some_and(|solvable_or_root| {
                        !self
                            .state
                            .clauses_added_for_solvable
                            .contains(solvable_or_root)
                    })
        })
    }

    /// Executes one iteration of the CDCL loop
    ///
    /// A set-propagate-learn round is always initiated by a requirement clause
    /// (i.e. [`Clause::Requires`]). The parameters include the variable
    /// associated to the candidate for the dependency (`solvable`), the
    /// package that originates the dependency (`required_by`), and the
    /// id of the requires clause (`clause_id`).
    ///
    /// Refer to the documentation of [`Solver::run_sat`] for details on the
    /// CDCL algorithm.
    ///
    /// Returns the new level after this set-propagate-learn round, or a
    /// [`Conflict`] if we discovered that the requested jobs are
    /// unsatisfiable.
    fn set_propagate_learn(
        &mut self,
        mut level: u32,
        solvable: VariableId,
        _required_by: VariableId,
        clause_id: ClauseId,
    ) -> Result<u32, ResolveError> {
        level += 1;

        self.state
            .decision_tracker
            .try_add_decision(Decision::new(solvable, true, clause_id), level)
            .expect("bug: solvable was already decided!");

        self.propagate_and_learn(level)
    }

    fn propagate_and_learn(&mut self, mut level: u32) -> Result<u32, ResolveError> {
        loop {
            match self.propagate(level) {
                Ok(()) => {
                    // The work-based co-trigger can fire mid-propagation
                    // without a conflict; consume the restart before
                    // handing the level back to the decision loop.
                    if self.take_env_ordering_restart(level) {
                        level = self.state.run_root_level;
                        continue;
                    }
                    return Ok(level);
                }
                Err(PropagationError::Cancelled(value)) => {
                    return Err(ResolveError::Cancelled(value));
                }
                Err(PropagationError::Conflict(
                    conflicting_solvable,
                    attempted_value,
                    conflicting_clause,
                )) => {
                    level = self.learn_from_conflict(
                        level,
                        conflicting_solvable,
                        attempted_value,
                        conflicting_clause,
                    )?;
                    // Two restart triggers coexist: the refutation
                    // switch's stage restarts and the Luby pacing. A
                    // pending switch restart wins (it already retracts to
                    // the run root, the same target as a Luby restart, and
                    // resets the Luby conflict counter on consumption: a
                    // new descent begins, so the pre-trip conflicts must
                    // not also be billed to the next Luby interval). The
                    // Luby index advances only when the Luby trigger
                    // itself fires, keeping the two counters and their
                    // diagnostics distinguishable.
                    if self.take_env_ordering_restart(level) {
                        level = self.state.run_root_level;
                    } else {
                        level = self.maybe_restart(level);
                    }
                }
            }
        }
    }

    /// Consumes a pending refutation-switch restart (see
    /// [`SolverState::env_ordering_restart_pending`]): suspension only
    /// steers future decisions, but the trail at trip time is still shaped
    /// by the deferral (a deep concrete subtree with the env literals
    /// undecided at the bottom), and the rest of the run would keep paying
    /// for that shape. Retracts to the root install; learnt clauses carry
    /// over, exactly like a classic CDCL restart. Any tripping conflict
    /// was analyzed and learnt before this runs, so knowledge grows
    /// monotonically, and each stage trips at most once per run, which
    /// bounds the restarts.
    ///
    /// A stage-2 trip additionally resets the package activity scores
    /// (see [`SolverState::env_ordering_reset_pending`]). Zeroing is safe:
    /// activity is a heuristic read fresh at every decision, the hot-set
    /// tracking of the decide queue stays a conservative superset, and
    /// the tracked maximum scales to zero with the scores.
    ///
    /// Returns true when the trail was retracted to
    /// [`SolverState::run_root_level`].
    fn take_env_ordering_restart(&mut self, level: u32) -> bool {
        if !self.state.env_ordering_restart_pending {
            return false;
        }
        self.state.env_ordering_restart_pending = false;
        // A switch restart begins a new descent: the conflicts that led to
        // the trip must not also count toward the next Luby restart. The
        // Luby index itself is untouched (only the Luby trigger advances
        // it), so the pacing sequence stays the documented one.
        self.state.conflicts_since_restart = 0;
        let reset = std::mem::take(&mut self.state.env_ordering_reset_pending);
        if reset {
            self.state.name_activity.for_each_mut(|activity| {
                *activity = 0.0;
            });
            self.state.max_activity = 0.0;
        }
        if level <= self.state.run_root_level {
            return false;
        }
        tracing::debug!(
            "├ Restarting at level {} after suspending the env-literals-last ordering",
            self.state.run_root_level
        );
        self.restart_to_run_root(level);
        true
    }

    fn learn_from_conflict(
        &mut self,
        mut level: u32,
        conflicting_solvable: VariableId,
        attempted_value: bool,
        conflicting_clause: ClauseId,
    ) -> Result<u32, ResolveError> {
        #[cfg(feature = "diagnostics")]
        let learn_start = std::time::Instant::now();

        // Stage 1 of the refutation switch: once this run has conflicted
        // often enough, it is probably proving unsolvability rather than
        // finding a solution, and the env-literals-last deferral only
        // delays the env contradictions a refutation needs early. Suspend
        // the ordering for the rest of the run and restart it (without a
        // restart the search keeps standing in the deferral-shaped trail;
        // measured 2-20x on the refutation outliers). Stage 2 (the work
        // deadline, armed below and consumed in `propagate_impl`)
        // escalates with an activity reset once the run has also burnt
        // real propagation work. The flags only ever flip here and in
        // `propagate_impl`, between `decide()` calls, so the incremental
        // queue and the debug-build reference scan always read the same
        // value within one selection.
        self.state.run_conflicts += 1;
        if self.state.run_conflicts == 1
            && self.state.env_ordering_active
            && !self.state.env_ordering_suspended
            && !(self.env_ordering_work_factor == 0 && self.env_ordering_work_floor == 0)
        {
            // First conflict of the run: refutation work may be starting.
            // Arm the work-based stage-2 trigger; from here the run has a
            // bounded propagation budget to complete before the switch
            // escalates to a restart (see
            // `SolverState::env_ordering_work_deadline`). Never armed at
            // run entry: a conflict-free run is marching to a solution
            // over a kept trail prefix and must not be restarted.
            let cap = self.env_ordering_work_floor.max(ENV_ORDERING_WORK_CAP);
            let budget = (self.state.fresh_solve_cost * self.env_ordering_work_factor)
                .max(self.env_ordering_work_floor)
                .min(cap);
            self.state.env_ordering_work_deadline = Some(self.state.propagated_total + budget);
        }
        if self.state.env_ordering_active
            && !self.state.env_ordering_suspended
            && self.state.run_conflicts >= self.state.run_conflict_limit
        {
            tracing::debug!(
                "├ {} conflicts in this run: suspending the env-literals-last ordering \
                 for the rest of it",
                self.state.run_conflicts
            );
            self.state.env_ordering_suspended = true;
            self.state.env_ordering_restart_pending = true;
            self.state.env_ordering_cooldown = true;
            #[cfg(feature = "diagnostics")]
            {
                self.state.propagation_counters.env_ordering_suspensions += 1;
            }
        }

        {
            tracing::debug!(
                "├┬ Propagation conflicted: could not set {solvable} to {attempted_value}",
                solvable = conflicting_solvable.display(&self.state.variable_map, self.provider()),
            );
            tracing::debug!(
                "││ During unit propagation for clause: {}",
                self.state.clauses.kinds[conflicting_clause.to_index()]
                    .display(&self.state.variable_map, self.provider())
            );

            // The conflicting variable may be an assumption decision, whose
            // `derived_from` is the assumption sentinel, not a real clause.
            let derived_from = self
                .state
                .decision_tracker
                .find_clause_for_assignment(conflicting_solvable)
                .unwrap();
            if derived_from == ClauseId::assumption() {
                tracing::debug!(
                    "││ Previously decided value: {}. Derived from an assumption",
                    !attempted_value,
                );
            } else {
                tracing::debug!(
                    "││ Previously decided value: {}. Derived from: {}",
                    !attempted_value,
                    self.state.clauses.kinds[derived_from.to_index()]
                        .display(&self.state.variable_map, self.provider()),
                );
            }
        }

        // A conflict at or below the root level cannot be fixed by
        // backtracking the search. The root level is 1 when solving freely;
        // when solving under assumptions the assumptions occupy levels
        // `1..=n` and the root install sits directly above them at `n + 1`.
        let root_level = self.state.assumption_levels + 1;
        if level <= root_level {
            if self.state.assumption_levels > 0 {
                // Everything at or below the root level is forced: the
                // assumptions themselves plus unit propagation from them and
                // the root install (which must be installed regardless).
                // Hence the conflict proves the problem unsolvable UNDER THE
                // ASSUMPTIONS; it is not a global conflict and must not go
                // through `analyze_unsolvable` (which builds a conflict for
                // the unconditional problem and cannot represent assumption
                // decisions). Any clause learnt earlier in this solve stays
                // valid: learnt clauses are resolvents of real clauses only.
                tracing::debug!(
                    "│└ Conflict at level {level} is at or below the assumption boundary \
                     (root level {root_level}); the problem is unsolvable as seeded"
                );
                #[cfg(feature = "diagnostics")]
                {
                    self.state.propagation_counters.learn_duration += learn_start.elapsed();
                }
                return Err(ResolveError::AssumptionConflict);
            }

            for decision in self.state.decision_tracker.stack() {
                let clause_id = decision.derived_from;
                let clause = self.state.clauses.kinds[clause_id.to_index()];
                let level = self.state.decision_tracker.level(decision.variable);
                let action = if decision.value { "install" } else { "forbid" };

                if let Clause::ForbidMultipleInstances(..) = clause {
                    // Skip forbids clauses, to reduce noise
                    continue;
                }

                tracing::debug!(
                    "* ({level}) {action} {}. Reason: {}",
                    decision
                        .variable
                        .display(&self.state.variable_map, self.provider()),
                    self.state.clauses.kinds[decision.derived_from.to_index()]
                        .display(&self.state.variable_map, self.provider()),
                );
            }

            #[cfg(feature = "diagnostics")]
            {
                self.state.propagation_counters.learn_duration += learn_start.elapsed();
            }
            return Err(self.analyze_unsolvable(conflicting_clause).into());
        }

        let (new_level, learned_clause_id, literal) =
            self.analyze(level, conflicting_solvable, conflicting_clause);
        self.state.conflicts_since_restart += 1;
        let old_level = level;
        level = new_level;

        // Optimization: propagate right now, since we know that the clause is a unit
        // clause
        let decision = literal.satisfying_value();
        self.state
            .decision_tracker
            .try_add_decision(
                Decision::new(literal.variable(), decision, learned_clause_id),
                level,
            )
            .expect("bug: solvable was already decided!");
        tracing::trace!(
            "│├ Propagate after learn: {} = {decision}",
            literal
                .variable()
                .display(&self.state.variable_map, self.provider()),
        );

        tracing::debug!("│└ Backtracked from {old_level} -> {level}");

        #[cfg(feature = "diagnostics")]
        {
            self.state.propagation_counters.learn_duration += learn_start.elapsed();
        }

        Ok(level)
    }

    /// Fires a classic CDCL restart when the current `run_sat` call has
    /// accumulated enough conflicts since the previous one (Luby pacing,
    /// see [`RESTART_BASE_INTERVAL`]).
    ///
    /// A restart undoes every decision above the run's root install level
    /// while keeping all learnt clauses and activity scores: the search
    /// re-descends guided by what the conflicts taught it instead of
    /// staying committed to early decisions that predate that knowledge.
    /// See [`Solver::restart_to_run_root`] for the shared retract
    /// primitive and its soundness argument.
    fn maybe_restart(&mut self, level: u32) -> u32 {
        if self.state.conflicts_since_restart < self.state.restart_limit {
            return level;
        }

        self.state.conflicts_since_restart = 0;
        self.state.restarts_performed += 1;
        self.state.restart_limit = RESTART_BASE_INTERVAL * luby(self.state.restarts_performed);
        #[cfg(feature = "diagnostics")]
        {
            self.state.propagation_counters.restarts += 1;
        }

        if level > self.state.run_root_level {
            tracing::debug!(
                "├─ Restart {}: backtracking from level {level} to {}",
                self.state.restarts_performed,
                self.state.run_root_level
            );
        }
        self.restart_to_run_root(level)
    }

    /// The shared restart primitive of the two restart triggers (the Luby
    /// pacing of [`Solver::maybe_restart`] and the refutation switch of
    /// [`Solver::take_env_ordering_restart`]): retracts the trail to
    /// [`SolverState::run_root_level`], the root install level of the
    /// current run.
    ///
    /// Everything at or below the root level (assumption decisions of a
    /// seeded universal cell, a preserved prior solution under a soft
    /// requirement) survives by construction; a kept trail prefix of the
    /// universal free phase sits above it and is restartable scratch
    /// state, so discarding it is sound. Learnt clauses and activity
    /// scores are untouched. Backtracking is the only effect, which the
    /// incremental decide queue absorbs through its sync floor and the
    /// assertion scans through their assert floor; assertions of
    /// single-literal learnt clauses are re-applied by `decide_learned`
    /// on the next propagation round.
    fn restart_to_run_root(&mut self, level: u32) -> u32 {
        let root_level = self.state.run_root_level;
        if level <= root_level {
            // Already at (or below) the restart target.
            return level;
        }
        self.state.decision_tracker.undo_until(root_level);
        root_level
    }

    /// The propagate step of the CDCL algorithm
    ///
    /// Propagation is implemented by means of watches: each clause that has two
    /// or more literals is "subscribed" to changes in the values of two
    /// solvables that appear in the clause. When a value is assigned to a
    /// solvable, each of the clauses tracking that solvable will be notified.
    /// That way, the clause can check whether the literal that is using the
    /// solvable has become false, in which case it picks a new solvable to
    /// watch (if available) or triggers an assignment.
    fn propagate(&mut self, level: u32) -> Result<(), PropagationError> {
        #[cfg(feature = "diagnostics")]
        let propagation_start = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        {
            self.state.propagation_counters.propagate_calls += 1;
        }

        let result = self.propagate_impl(level);

        #[cfg(feature = "diagnostics")]
        {
            self.state.propagation_counters.propagation_duration += propagation_start.elapsed();
        }

        result
    }

    fn propagate_impl(&mut self, level: u32) -> Result<(), PropagationError> {
        if let Some(value) = self.provider().should_cancel_with_value() {
            return Err(PropagationError::Cancelled(value));
        };

        // Catch the assertion scans up with any backtracking that
        // happened since the previous propagation round: assertions whose
        // verifying assignment was popped become pending again. Together
        // with the per-list cursors over the (append-only) assertion lists
        // this makes the scans below visit only the entries on which the
        // historical full rescan would not have been a no-op.
        let assert_floor = self.state.decision_tracker.take_assert_floor();
        self.state.assertion_scans.sync(assert_floor);

        // Add decisions from assertions and learned clauses. If any of these cause a
        // conflict, we will return an error.
        self.decide_assertions(level)?;
        self.decide_learned(level)?;
        self.decide_env_assertions(level)?;

        // For each decision that has not been propagated yet, we propagate the
        // decision.
        //
        // Propagation entails iterating through the linked list of clauses that watch
        // the literal that the decision caused to turn false. If a clause can only be
        // satisfied if one of the literals involved is assigned a value, we also make a
        // decision on that literal to ensure that the clause is satisfied.
        //
        // Any new decision is also propagated. If by making a decision on one of the
        // remaining literals of a clause we cause a conflict, propagation is halted and
        // an error is returned.

        let interner = self.cache.provider();
        let clause_kinds = &self.state.clauses.kinds;

        while let Some(decision) = self.state.decision_tracker.next_unpropagated() {
            let watched_literal = Literal::new(decision.variable, decision.value);

            self.state.propagated_total += 1;
            #[cfg(feature = "diagnostics")]
            {
                self.state.propagation_counters.decisions_propagated += 1;
            }

            // A prefix-started run that needs real work performs far worse
            // than a restart-from-scratch enumeration: once a single run has
            // cost several fresh solves, or all prefix-started runs together
            // average about one fresh solve per recorded cell, abort the
            // trail-reuse attempt so that `solve_universal` can rebuild and
            // re-enumerate without it.
            if let Some(deadline) = self.state.prefix_budget_deadline {
                self.state.prefix_spent += 1;
                if self.state.propagated_total > deadline
                    || self.state.prefix_spent > self.state.prefix_cumulative_budget
                {
                    tracing::debug!(
                        "The kept trail prefix exceeded its work budget; abandoning the \
                         trail-reuse attempt"
                    );
                    #[cfg(feature = "diagnostics")]
                    {
                        self.state.propagation_counters.prefix_budget_aborts += 1;
                    }
                    return Err(PropagationError::Cancelled(Box::new(PrefixBudgetExhausted)));
                }
            }

            // Stage 2 of the refutation switch (see
            // `SolverState::env_ordering_work_deadline`): a run that has
            // both conflicted and burnt a real propagation budget since
            // its first conflict is refuting, even when its conflict count
            // stays low. Suspend the ordering (if stage 1 has not already)
            // and request the one-shot restart; propagation itself
            // finishes normally (the restart is consumed by
            // `propagate_and_learn`).
            if self
                .state
                .env_ordering_work_deadline
                .is_some_and(|deadline| self.state.propagated_total > deadline)
            {
                tracing::debug!(
                    "├ The run crossed its work budget without completing: restarting \
                     without the env-literals-last ordering"
                );
                self.state.env_ordering_work_deadline = None;
                self.state.env_ordering_suspended = true;
                self.state.env_ordering_restart_pending = true;
                self.state.env_ordering_reset_pending = true;
                #[cfg(feature = "diagnostics")]
                {
                    self.state.propagation_counters.env_ordering_restarts += 1;
                }
            }

            debug_assert!(
                watched_literal.eval(self.state.decision_tracker.map()) == Some(false),
                "we are only watching literals that are turning false"
            );

            // Propagate, iterating through the linked list of clauses that
            // watch this solvable.
            let mut next_cursor = self
                .state
                .watches
                .cursor(&mut self.state.clauses.watched_literals, watched_literal);
            while let Some(cursor) = next_cursor.take() {
                let clause_id = cursor.clause_id();
                let clause = &clause_kinds[clause_id.to_index()];
                let watch_index = cursor.watch_index();

                #[cfg(feature = "diagnostics")]
                {
                    self.state.propagation_counters.clause_visits += 1;
                    self.state.propagation_counters.visits_by_type.count(clause);
                }

                // If the other literal the current clause is watching is already true, we can
                // skip this clause. Its is already satisfied.
                let watched_literals = cursor.watched_literals();
                // Prefetch the next clause's `WatchedLiterals` to overlap the
                // pointer-chasing latency with this iteration's work. The
                // inner BCP loop is memory-bound on this linked-list walk.
                cursor.prefetch_next();
                let other_watched_literal =
                    watched_literals.watched_literals[1 - cursor.watch_index()];
                if other_watched_literal.eval(self.state.decision_tracker.map()) == Some(true) {
                    #[cfg(feature = "diagnostics")]
                    {
                        self.state.propagation_counters.early_skips += 1;
                    }
                    // Continue with the next clause in the linked list.
                    next_cursor = cursor.next();
                } else if let Some(literal) = if clause.is_binary() {
                    // Binary clauses can never move their watches; skip the
                    // `next_unwatched_literal` scan entirely.
                    None
                } else {
                    #[cfg(feature = "diagnostics")]
                    {
                        self.state
                            .propagation_counters
                            .unwatched_calls_by_type
                            .count(clause);
                    }
                    watched_literals.next_unwatched_literal(
                        clause,
                        &self.state.learnt_clauses,
                        &self.state.requirement_to_sorted_candidates,
                        &self.state.disjunctions,
                        &self.state.env_constrains,
                        &self.state.env_clauses,
                        self.state.decision_tracker.map(),
                        watch_index,
                    )
                } {
                    #[cfg(feature = "diagnostics")]
                    {
                        self.state.propagation_counters.watch_moves += 1;
                    }
                    // Update the watch to point to the new literal
                    next_cursor = cursor.update(literal);
                } else if self.state.env_ordering_active
                    // The refutation switch also stops the unit-propagation
                    // deferral: a suspended run must install forced variant
                    // parents immediately again (inlined
                    // `SolverState::env_ordering_enabled`, see there).
                    && !self.state.env_ordering_suspended
                    && matches!(clause, Clause::Requires(..))
                    && other_watched_literal.satisfying_value()
                    && self
                        .state
                        .decision_tracker
                        .assigned_value(other_watched_literal.variable())
                        .is_none()
                    // Inlined `SolverState::env_install_pending` (a method
                    // call would borrow the whole state struct, which the
                    // watch cursor partially borrows mutably).
                    && self
                        .state
                        .env_sensitive_parents
                        .get(&other_watched_literal.variable())
                        .is_some_and(|literals| {
                            literals.iter().any(|&literal| {
                                self.state
                                    .decision_tracker
                                    .assigned_value(literal)
                                    .is_none()
                            })
                        })
                {
                    // Env-literals-last: a `Requires` clause that became
                    // unit on a solvable whose install would assign a still
                    // unassigned environment literal (a variant parent such
                    // as a microarch-level or sysroot build) is NOT
                    // propagated here. Propagating it would install the
                    // parent, and with it the environment literal, at the
                    // level of whatever package required it, mid-trail;
                    // `decide()` installs it instead, after every ordinary
                    // decision (the deferred classes), so the literal lands
                    // at the top of the trail. Soundness: the clause stays
                    // watched, so if the skipped candidate is later
                    // assigned false by another clause, this clause's watch
                    // fires and reports the conflict through the (already
                    // false) other watched literal; and `decide()` is
                    // guaranteed to see the clause because `Requires`
                    // clauses are indexed by their installed parent.
                    // Skipping a forced assignment merely turns it into a
                    // later decision, which conflict analysis treats like
                    // any other decision.
                    next_cursor = cursor.next();
                } else {
                    #[cfg(feature = "diagnostics")]
                    {
                        self.state.propagation_counters.unit_propagations += 1;
                    }
                    // We could not find another literal to watch, which means the remaining
                    // watched literal must be set to true.
                    let decided = self
                        .state
                        .decision_tracker
                        .try_add_decision(
                            Decision::new(
                                other_watched_literal.variable(),
                                other_watched_literal.satisfying_value(),
                                clause_id,
                            ),
                            level,
                        )
                        .map_err(|_| {
                            #[cfg(feature = "diagnostics")]
                            {
                                self.state.propagation_counters.conflicts += 1;
                            }
                            PropagationError::Conflict(
                                other_watched_literal.variable(),
                                true,
                                clause_id,
                            )
                        })?;

                    if decided {
                        match clause {
                            // Skip logging for ForbidMultipleInstances, which is so noisy
                            Clause::ForbidMultipleInstances(..) => {}
                            _ => {
                                tracing::debug!(
                                    "├ Propagate {} = {}. {}",
                                    other_watched_literal
                                        .variable()
                                        .display(&self.state.variable_map, interner),
                                    other_watched_literal.satisfying_value(),
                                    clause.display(&self.state.variable_map, interner)
                                );
                            }
                        }
                    }

                    // Skip to the next clause in the linked list.
                    next_cursor = cursor.next();
                }
            }
        }

        Ok(())
    }

    /// Add decisions for negative assertions derived from other rules
    /// (assertions are clauses that consist of a single literal, and
    /// therefore do not have watches).
    ///
    /// Only entries flagged by the [`assertion_scans`] state are visited:
    /// when a truncation may have popped a verifying assignment, every
    /// inspected entry is re-applied in index order (the historical scan
    /// order), followed by the assertions appended since the previous scan.
    /// Every skipped entry still holds its asserted value, so the historical
    /// full rescan would have no-opped on it.
    fn decide_assertions(&mut self, level: u32) -> Result<(), PropagationError> {
        // Re-verify every inspected entry when a truncation may have popped
        // a verifying assignment. A conflict (`?`) leaves the list dirty for
        // the rescan after the conflict is handled.
        if self.state.assertion_scans.negative.needs_rescan() {
            for index in 0..self.state.assertion_scans.negative.cursor() {
                let position = self.apply_negative_assertion(index, level)?;
                self.state.assertion_scans.negative.record(position);
            }
            self.state.assertion_scans.negative.mark_clean();
        }
        // Apply the entries appended since the previous scan.
        while self.state.assertion_scans.negative.cursor() < self.state.negative_assertions.len() {
            let index = self.state.assertion_scans.negative.cursor();
            let position = self.apply_negative_assertion(index, level)?;
            self.state.assertion_scans.negative.record(position);
            self.state.assertion_scans.negative.advance();
        }
        Ok(())
    }

    /// Apply the negative assertion at `index`, exactly as the historical
    /// full scan did per entry. Returns the trail position at (or above)
    /// the assignment that satisfies the assertion.
    fn apply_negative_assertion(
        &mut self,
        index: usize,
        level: u32,
    ) -> Result<usize, PropagationError> {
        let (solvable_id, clause_id) = self.state.negative_assertions[index];
        let value = false;
        let decided = self
            .state
            .decision_tracker
            .try_add_decision(Decision::new(solvable_id, value, clause_id), level)
            .map_err(|_| PropagationError::Conflict(solvable_id, value, clause_id))?;

        if decided {
            tracing::trace!(
                "Negative assertions derived from other rules: Propagate assertion {} = {}",
                solvable_id.display(&self.state.variable_map, self.provider()),
                value
            );
        }
        // The variable is assigned (a fresh decision sits on top of the
        // stack; an existing assignment sits at or below the top), so the
        // stack is non-empty and `len - 1` bounds the assignment position.
        Ok(self.state.decision_tracker.stack_len() - 1)
    }

    /// Add decisions derived from single-literal learnt clauses (which
    /// have no watches; multi-literal learnt clauses propagate through
    /// theirs and are never registered for this scan). Visits only the
    /// entries flagged by the [`assertion_scans`] state (see
    /// [`Self::decide_assertions`]). The scan runs over the already
    /// unit-only list: the filter removes exactly the entries on which
    /// the historical full scan never acted, so the composition stays
    /// behavior-preserving.
    fn decide_learned(&mut self, level: u32) -> Result<(), PropagationError> {
        // `unit_learnt_clause_ids` is single-literal by construction, so a
        // rescan walks `0..cursor` directly (no multi-literal filtering).
        if self.state.assertion_scans.learnt.needs_rescan() {
            for index in 0..self.state.assertion_scans.learnt.cursor() {
                let position = self.apply_learnt_assertion(index, level)?;
                self.state.assertion_scans.learnt.record(position);
            }
            self.state.assertion_scans.learnt.mark_clean();
        }
        while self.state.assertion_scans.learnt.cursor() < self.state.unit_learnt_clause_ids.len() {
            let index = self.state.assertion_scans.learnt.cursor();
            let position = self.apply_learnt_assertion(index, level)?;
            self.state.assertion_scans.learnt.record(position);
            self.state.assertion_scans.learnt.advance();
        }

        Ok(())
    }

    /// Apply the single-literal learnt clause at `index` of
    /// `unit_learnt_clause_ids`, exactly as the historical full scan did
    /// per entry. Returns the trail position at (or above) the satisfying
    /// assignment.
    fn apply_learnt_assertion(
        &mut self,
        index: usize,
        level: u32,
    ) -> Result<usize, PropagationError> {
        let clause_id = self.state.unit_learnt_clause_ids[index];
        let clause = self.state.clauses.kinds[clause_id.to_index()];
        let Clause::Learnt(learnt_index) = clause else {
            unreachable!();
        };

        let literals = &self.state.learnt_clauses[learnt_index];
        debug_assert_eq!(
            literals.len(),
            1,
            "only single-literal learnt clauses are registered for decide_learned"
        );

        let literal = literals[0];
        let decision = literal.satisfying_value();

        let decided = self
            .state
            .decision_tracker
            .try_add_decision(
                Decision::new(literal.variable(), decision, clause_id),
                level,
            )
            .map_err(|_| PropagationError::Conflict(literal.variable(), decision, clause_id))?;

        if decided {
            tracing::trace!(
                "├─ Propagate assertion {} = {}",
                literal
                    .variable()
                    .display(&self.state.variable_map, self.provider()),
                decision
            );
        }

        Ok(self.state.decision_tracker.stack_len() - 1)
    }

    /// Add decisions derived from single-literal environment model/blocking
    /// clauses. Such clauses have no watches (like single-literal learnt
    /// clauses) so their assertions must hold on every propagation round.
    /// Visits only the entries flagged by the [`assertion_scans`] state (see
    /// [`Self::decide_assertions`]). A re-applied assertion records the
    /// current level, exactly as the historical full rescan did.
    fn decide_env_assertions(&mut self, level: u32) -> Result<(), PropagationError> {
        // `env_clause_ids` also holds the multi-literal model/blocking
        // clauses (handled by watches); only the single-literal entries
        // collected in `env_single` are re-inspected on a rescan.
        if self.state.assertion_scans.env.needs_rescan() {
            for k in 0..self.state.assertion_scans.env_single.len() {
                let index = self.state.assertion_scans.env_single[k] as usize;
                let position = self
                    .apply_env_assertion(index, level)?
                    .expect("a multi-literal env clause is never in env_single");
                self.state.assertion_scans.env.record(position);
            }
            self.state.assertion_scans.env.mark_clean();
        }
        while self.state.assertion_scans.env.cursor() < self.state.env_clause_ids.len() {
            let index = self.state.assertion_scans.env.cursor();
            if let Some(position) = self.apply_env_assertion(index, level)? {
                self.state.assertion_scans.env.record(position);
                self.state.assertion_scans.env_single.push(index as u32);
            }
            self.state.assertion_scans.env.advance();
        }

        Ok(())
    }

    /// Apply the single-literal environment clause at `index` of
    /// `env_clause_ids`, exactly as the historical full scan did per
    /// entry. Returns the trail position at (or above) the satisfying
    /// assignment, or `None` for a multi-literal clause.
    fn apply_env_assertion(
        &mut self,
        index: usize,
        level: u32,
    ) -> Result<Option<usize>, PropagationError> {
        let clause_id = self.state.env_clause_ids[index];
        let clause = self.state.clauses.kinds[clause_id.to_index()];
        let Clause::EnvClause(env_clause_id) = clause else {
            unreachable!();
        };

        let literals = &self.state.env_clauses[env_clause_id].literals;
        if literals.len() > 1 {
            return Ok(None);
        }

        debug_assert!(!literals.is_empty());

        let literal = literals[0];
        let decision = literal.satisfying_value();

        let decided = self
            .state
            .decision_tracker
            .try_add_decision(
                Decision::new(literal.variable(), decision, clause_id),
                level,
            )
            .map_err(|_| PropagationError::Conflict(literal.variable(), decision, clause_id))?;

        if decided {
            tracing::trace!(
                "├─ Propagate env assertion {} = {}",
                literal
                    .variable()
                    .display(&self.state.variable_map, self.provider()),
                decision
            );
        }

        Ok(Some(self.state.decision_tracker.stack_len() - 1))
    }

    /// Adds the clause with `clause_id` to the current [`Conflict`]
    ///
    /// Because learnt clauses are not relevant for the user, they are not added
    /// to the [`Conflict`]. Instead, we report the clauses that caused them.
    fn analyze_unsolvable_clause(
        clauses: &[Clause<D::NameId>],
        learnt_why: &Mapping<LearntClauseId, Vec<ClauseId>>,
        clause_id: ClauseId,
        conflict: &mut Conflict,
        seen: &mut IndexedSet<ClauseId>,
    ) {
        let clause = &clauses[clause_id.to_index()];
        match clause {
            Clause::Learnt(learnt_clause_id) => {
                if !seen.insert(clause_id) {
                    return;
                }

                for &cause in learnt_why
                    .get(*learnt_clause_id)
                    .expect("no cause for learnt clause available")
                {
                    Self::analyze_unsolvable_clause(clauses, learnt_why, cause, conflict, seen);
                }
            }
            _ => conflict.add_clause(clause_id),
        }
    }

    /// Create a [`Conflict`] based on the id of the clause that triggered an
    /// unrecoverable conflict
    fn analyze_unsolvable(&mut self, clause_id: ClauseId) -> Conflict {
        debug_assert_eq!(
            self.state.assumption_levels, 0,
            "bug: a conflict under assumptions must not be analyzed as a global conflict"
        );
        let last_decision = self.state.decision_tracker.stack().last().unwrap();
        let highest_level = self.state.decision_tracker.level(last_decision.variable);
        debug_assert_eq!(highest_level, 1);

        let mut conflict = Conflict::default();

        tracing::debug!("=== ANALYZE UNSOLVABLE");

        let mut involved = HashSet::default();
        self.state.clauses.kinds[clause_id.to_index()].visit_literals(
            &self.state.learnt_clauses,
            &self.state.requirement_to_sorted_candidates,
            &self.state.disjunctions,
            &self.state.env_constrains,
            &self.state.env_clauses,
            |literal| {
                involved.insert(literal.variable());
            },
        );

        let mut seen = IndexedSet::default();
        Self::analyze_unsolvable_clause(
            &self.state.clauses.kinds,
            &self.state.learnt_why,
            clause_id,
            &mut conflict,
            &mut seen,
        );

        for decision in self.state.decision_tracker.stack().rev() {
            if decision.variable.is_root() {
                continue;
            }

            let why = decision.derived_from;

            if !involved.contains(&decision.variable) {
                continue;
            }

            assert_ne!(why, ClauseId::install_root());

            Self::analyze_unsolvable_clause(
                &self.state.clauses.kinds,
                &self.state.learnt_why,
                why,
                &mut conflict,
                &mut seen,
            );

            self.state.clauses.kinds[why.to_index()].visit_literals(
                &self.state.learnt_clauses,
                &self.state.requirement_to_sorted_candidates,
                &self.state.disjunctions,
                &self.state.env_constrains,
                &self.state.env_clauses,
                |literal| {
                    if literal.eval(self.state.decision_tracker.map()) == Some(true) {
                        assert_eq!(literal.variable(), decision.variable);
                    } else {
                        involved.insert(literal.variable());
                    }
                },
            );
        }

        conflict
    }

    /// Analyze the causes of the conflict and learn from it
    ///
    /// This function finds the combination of assignments that caused the
    /// conflict and adds a new clause to the solver to forbid that
    /// combination of assignments (i.e. learn from this mistake
    /// so it is not repeated in the future). It corresponds to the
    /// `Solver.analyze` function from the MiniSAT paper.
    ///
    /// Returns the level to which we should backtrack, the id of the learnt
    /// clause and the literal that should be assigned (by definition, when
    /// we learn a clause, all its literals except one evaluate to false, so
    /// the value of the remaining literal must be assigned to make the clause
    /// become true)
    fn analyze(
        &mut self,
        mut current_level: u32,
        mut conflicting_solvable: VariableId,
        mut clause_id: ClauseId,
    ) -> (u32, ClauseId, Literal) {
        let mut seen = HashSet::default();
        let mut causes_at_current_level = 0u32;
        let mut learnt = Vec::new();
        let mut back_track_to = 0;

        let mut s_value;
        let mut learnt_why = Vec::new();
        let mut first_iteration = true;
        let clause_kinds = &self.state.clauses.kinds;
        loop {
            learnt_why.push(clause_id);

            clause_kinds[clause_id.to_index()].visit_literals(
                &self.state.learnt_clauses,
                &self.state.requirement_to_sorted_candidates,
                &self.state.disjunctions,
                &self.state.env_constrains,
                &self.state.env_clauses,
                |literal| {
                    if !first_iteration && literal.variable() == conflicting_solvable {
                        // We are only interested in the causes of the conflict, so we ignore the
                        // solvable whose value was propagated
                        return;
                    }

                    if !seen.insert(literal.variable()) {
                        // Skip literals we have already seen
                        return;
                    }

                    let decision_level = self.state.decision_tracker.level(literal.variable());
                    if decision_level == current_level {
                        causes_at_current_level += 1;
                    } else if current_level > 1 {
                        let learnt_literal = Literal::new(
                            literal.variable(),
                            self.state
                                .decision_tracker
                                .assigned_value(literal.variable())
                                .unwrap(),
                        );
                        learnt.push(learnt_literal);
                        back_track_to = back_track_to.max(decision_level);
                    } else {
                        unreachable!();
                    }
                },
            );

            first_iteration = false;

            // Select next literal to look at
            loop {
                let (last_decision, last_decision_level) = self.state.decision_tracker.undo_last();

                // Assumption decisions are never popped during analysis: the
                // conflict level is above the assumption boundary
                // (`learn_from_conflict` ends the solve for conflicts at or
                // below it) and the analysis consumes only decisions at the
                // conflict level, which all sit above the assumptions on the
                // stack. Resolving on an assumption would dereference the
                // sentinel "clause" and derive an unsound learnt clause.
                debug_assert_ne!(
                    last_decision.derived_from,
                    ClauseId::assumption(),
                    "bug: conflict analysis popped an assumption decision"
                );

                conflicting_solvable = last_decision.variable;
                s_value = last_decision.value;
                clause_id = last_decision.derived_from;

                current_level = last_decision_level;

                // We are interested in the first literal we come across that caused the
                // conflicting assignment
                if seen.contains(&last_decision.variable) {
                    break;
                }
            }

            causes_at_current_level = causes_at_current_level.saturating_sub(1);
            if causes_at_current_level == 0 {
                break;
            }
        }

        let last_literal = Literal::new(conflicting_solvable, s_value);
        learnt.push(last_literal);

        // Increase the activity of the packages in the learned clause
        for literal in &learnt {
            let name_id = literal
                .variable()
                .as_solvable(&self.state.variable_map)
                .map(|s| self.provider().solvable_name(s));
            if let Some(name_id) = name_id {
                let activity = self.state.name_activity.get(name_id) + self.activity_add;
                self.state.name_activity.set(name_id, activity);
                // Keep the global maximum in lockstep so that decide() can
                // stop scanning early (see `SolverState::max_activity`),
                // and mark the name hot so its items take part in the
                // replacement fold.
                self.state.max_activity = self.state.max_activity.max(activity);
                self.state.decide_queue.mark_name_hot(name_id);
            }
        }

        // Add the clause
        let learnt_id = self.state.learnt_clauses.alloc(learnt);
        self.state.learnt_why.insert(learnt_id, learnt_why);

        let (watched_literals, kind) =
            WatchedLiterals::learnt(learnt_id, &self.state.learnt_clauses[learnt_id]);
        let clause_id = self.state.add_clause(watched_literals, kind);
        if self.state.learnt_clauses[learnt_id].len() == 1 {
            self.state.unit_learnt_clause_ids.push(clause_id);
        }

        tracing::debug!("│├ Learnt disjunction:",);
        for lit in &self.state.learnt_clauses[learnt_id] {
            tracing::debug!(
                "││ - {}{}",
                if lit.negate() { "NOT " } else { "" },
                lit.variable()
                    .display(&self.state.variable_map, self.provider()),
            );
        }

        // Should revert at most to the root level: level 1 when solving
        // freely, or the level directly above the assumption prefix when
        // solving under assumptions. A raw backjump target inside the
        // assumption prefix (possible when every non-UIP literal of the
        // learnt clause sits at an assumption level) would pop the root
        // install decision, and nothing would reinstall it. Clamping is
        // sound: the literals that make the learnt clause unit live at
        // levels at or below the clamped target either way, so asserting
        // the UIP literal at the root level is a valid (merely later than
        // strictly necessary) unit propagation.
        let target_level = back_track_to.max(self.state.assumption_levels + 1);
        self.state.decision_tracker.undo_until(target_level);

        self.decay_activity_scores();

        (target_level, clause_id, last_literal)
    }

    /// Decays the activity scores of all packages in the solver. This function
    /// is caleld after each conflict.
    fn decay_activity_scores(&mut self) {
        self.state.name_activity.for_each_mut(|activity| {
            *activity *= self.activity_decay;
        });
        // The decay is uniform, so the maximum scales by the same factor.
        // The multiplication is the identical f32 operation applied to the
        // identical value, so the tracked maximum stays bit-exact with the
        // largest stored activity.
        self.state.max_activity *= self.activity_decay;
    }
}

impl<D: DependencyProvider> SolverState<D> {
    /// Registers a newly encoded requires clause with the incremental decide
    /// queue.
    pub(crate) fn add_requires_clause(
        &mut self,
        parent: VariableId,
        requirement: Requirement,
        condition: Option<DisjunctionId>,
        clause_id: ClauseId,
        names: impl IntoIterator<Item = D::NameId>,
    ) {
        self.decide_queue.register_clause(
            parent,
            requirement,
            condition,
            clause_id,
            names,
            &self.disjunctions,
            self.decision_tracker.assigned_value(parent),
        );
    }

    /// Registers a newly encoded env-constrains clause with the incremental
    /// decide queue (universal solving).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_env_constrains_clause_to_queue(
        &mut self,
        parent: VariableId,
        package_name: D::NameId,
        absent_var: Option<VariableId>,
        matches_var: VariableId,
        version_set: VersionSetId,
        clause_id: ClauseId,
    ) {
        self.decide_queue.register_env_constrains_clause(
            parent,
            package_name,
            absent_var,
            matches_var,
            version_set,
            clause_id,
            self.decision_tracker.assigned_value(parent),
        );
    }

    /// Returns true when installing `variable` would assign at least one
    /// currently unassigned environment literal (through unit propagation of
    /// its `Requires` clauses on environment packages or a `decide()` pick
    /// on its `EnvConstrains` clauses).
    ///
    /// This is the deferral test of the env-literals-last ordering, and it
    /// is deliberately dynamic: a parent whose env literals are all assigned
    /// is ordinary (installing it assigns nothing), which keeps the ordering
    /// selective on corpora where a baseline literal such as
    /// `__glibc >=2.17` is required by nearly every package. Once the first
    /// deferred parent assigns that literal, the rest of its cone stops
    /// being deferred; only parents of literals that still discriminate
    /// between environment regions stay at the top of the trail.
    /// Returns true while the env-literals-last ordering steers the search:
    /// environment literals were interned (a universal solve) AND the
    /// refutation switch has not suspended the ordering for the current
    /// `run_sat` call (see [`Self::env_ordering_suspended`]). All decision
    /// deferral reads this; `propagate_impl` reads the two flags directly
    /// because the watch cursor partially borrows the state.
    fn env_ordering_enabled(&self) -> bool {
        self.env_ordering_active && !self.env_ordering_suspended
    }

    fn env_install_pending(&self, variable: VariableId) -> bool {
        self.env_sensitive_parents
            .get(&variable)
            .is_some_and(|literals| {
                literals
                    .iter()
                    .any(|&literal| self.decision_tracker.assigned_value(literal).is_none())
            })
    }

    /// Allocate a clause and, if it has watched literals, register them in
    /// the [`WatchMap`].
    pub(crate) fn add_clause(
        &mut self,
        watched_literals: Option<WatchedLiterals>,
        kind: Clause<D::NameId>,
    ) -> ClauseId {
        let clause_id = self.clauses.alloc(watched_literals, kind);
        let Some(wl) = self.clauses.watched_literals[clause_id.to_index()].as_mut() else {
            return clause_id;
        };

        self.watches.start_watching(wl, clause_id);

        clause_id
    }

    /// Records the propagation cost of the first recorded cell of a universal
    /// enumeration as the calibration constant for the kept-prefix work
    /// budget (see [`Self::prefix_budget_deadline`]).
    pub(crate) fn record_fresh_solve_cost(&mut self) {
        self.fresh_solve_cost = self.propagated_total;
    }

    /// Extends the cumulative kept-prefix work budget after a cell was
    /// recorded (see [`Self::prefix_cumulative_budget`]): one from-scratch
    /// solve per recorded cell plus a fixed slack for the first transitions.
    pub(crate) fn extend_prefix_budget(&mut self, cells_recorded: usize) {
        self.prefix_cumulative_budget = self
            .fresh_solve_cost
            .saturating_mul(cells_recorded as u64 + 32)
            .max(PREFIX_BUDGET_FLOOR);
    }

    /// Allocate an environment model or blocking clause (a plain disjunction
    /// of signed environment literals) and register it for propagation.
    ///
    /// A clause with two or more literals participates in watching like any
    /// other clause. The clause may be added under a partial trail (a
    /// blocking clause after trail-prefix retraction): the watches are
    /// chosen among the literals that are not false under the current
    /// assignment, and a clause that is unit under the trail propagates its
    /// remaining literal immediately, mirroring clause learning. The caller
    /// must retract the trail far enough that at least one literal is not
    /// false. A single-literal clause is an assertion that is (re-)applied
    /// on every propagation round, mirroring single-literal learnt clauses.
    pub(crate) fn add_env_clause(
        &mut self,
        mut literals: Vec<Literal>,
        kind: EnvClauseKind,
    ) -> ClauseId {
        assert!(
            !literals.is_empty(),
            "an environment clause must contain at least one literal; an empty \
             disjunction is unsatisfiable"
        );

        // Defensively drop exact duplicate literals (a caller-supplied model
        // disjunction may repeat a literal) while preserving order; the watch
        // initialization below assumes the watched literals differ.
        let mut deduped = Vec::with_capacity(literals.len());
        for literal in literals.drain(..) {
            if !deduped.contains(&literal) {
                deduped.push(literal);
            }
        }

        let env_clause_id = self.env_clauses.alloc(EnvClause {
            literals: deduped,
            kind,
        });
        let (watched_literals, clause_kind, assertion) = WatchedLiterals::env_clause::<D::NameId>(
            env_clause_id,
            &self.env_clauses[env_clause_id].literals,
            &self.decision_tracker,
        );
        let clause_id = self.add_clause(watched_literals, clause_kind);
        self.env_clause_ids.push(clause_id);
        if kind == EnvClauseKind::Blocking {
            self.blocking_clauses.push((env_clause_id, clause_id));
        }

        // Propagate a clause that is unit under the current trail right away
        // (the next propagation round picks the decision up from the stack).
        // The assignment must succeed: the asserted literal is unassigned by
        // construction.
        if let Some(literal) = assertion {
            let level = self.decision_tracker.deepest_level();
            self.decision_tracker
                .try_add_decision(
                    Decision::new(literal.variable(), literal.satisfying_value(), clause_id),
                    level,
                )
                .expect("bug: the unit literal of an environment clause is unassigned");
        }

        clause_id
    }

    /// Intern the "absent" literal `Ab_p` for the environment package named
    /// `package_name`. On first interning, emits the absent-vs-matches
    /// exclusion clause `(not Ab_p or not L_S)` for every matches literal of
    /// the package that already exists. Idempotent: repeat calls return the
    /// existing variable without emitting anything.
    ///
    /// This is the only place that interns absent literals, so each exclusion
    /// clause is emitted exactly once: either here (matches literal interned
    /// first) or in [`Self::intern_env_matches_with_oracle_clauses`] (absent
    /// literal interned first).
    pub(crate) fn intern_env_absent_with_oracle_clauses(
        &mut self,
        package_name: D::NameId,
    ) -> VariableId {
        self.env_ordering_active = true;
        let (ab_var, is_new, prior_matches) = self.variable_map.intern_env_absent(package_name);
        if is_new {
            for prior_vs in &prior_matches {
                let matches_var = self
                    .variable_map
                    .get_env_matches(*prior_vs)
                    .expect("variable for a previously interned version set must exist");
                let (wl, kind) = WatchedLiterals::oracle_consistency::<D::NameId>(
                    ab_var.negative(),
                    matches_var.negative(),
                );
                self.add_clause(wl, kind);
            }
        }
        ab_var
    }

    /// Intern an env-matches literal `L_S` for `version_set_id` (which belongs
    /// to an environment package named `package_name`) and emit oracle
    /// consistency clauses for all previously interned literals of the same
    /// package.
    ///
    /// Returns the variable id of `L_S`.
    pub(crate) fn intern_env_matches_with_oracle_clauses(
        &mut self,
        provider: &D,
        version_set_id: VersionSetId,
        package_name: D::NameId,
    ) -> VariableId {
        self.env_ordering_active = true;
        let (new_var, prior_vsets, absent_var) = self
            .variable_map
            .intern_env_matches(version_set_id, package_name);

        // If the literal was already interned, nothing to do.
        if prior_vsets.is_empty() && absent_var.is_none() {
            // Either repeat call, or first literal for this package with no
            // absent literal yet. Either way no consistency clauses to emit.
            return new_var;
        }

        // Emit absent-vs-matches exclusion: `(not Ab_p or not L_new)`.
        if let Some(ab_var) = absent_var {
            let (wl, kind) = WatchedLiterals::oracle_consistency::<D::NameId>(
                ab_var.negative(),
                new_var.negative(),
            );
            self.add_clause(wl, kind);
        }

        // Emit consistency clauses against every previously interned matches
        // literal of the same package.
        for prior_vs in &prior_vsets {
            let prior_var = self
                .variable_map
                .get_env_matches(*prior_vs)
                .expect("variable for prior version set must exist");

            // Query the oracle with `a` = the literal being interned and
            // `b` = the previously interned literal; the match arms below
            // depend on this argument order.
            let relation = provider.environment_version_set_relation(version_set_id, *prior_vs);

            match relation {
                VersionSetRelation::Disjoint => {
                    // (not L_new or not L_prior)
                    let (wl, kind) = WatchedLiterals::oracle_consistency::<D::NameId>(
                        new_var.negative(),
                        prior_var.negative(),
                    );
                    self.add_clause(wl, kind);
                }
                VersionSetRelation::Subset => {
                    // a is new, b is prior: every value matching a also matches b.
                    // Clause: (not L_new or L_prior)
                    let (wl, kind) = WatchedLiterals::oracle_consistency::<D::NameId>(
                        new_var.negative(),
                        prior_var.positive(),
                    );
                    self.add_clause(wl, kind);
                }
                VersionSetRelation::Superset => {
                    // a is new, b is prior: every value matching b also matches a.
                    // Clause: (not L_prior or L_new)
                    let (wl, kind) = WatchedLiterals::oracle_consistency::<D::NameId>(
                        prior_var.negative(),
                        new_var.positive(),
                    );
                    self.add_clause(wl, kind);
                }
                VersionSetRelation::Equal => {
                    // Both implications:
                    // (not L_new or L_prior) and (not L_prior or L_new)
                    let (wl, kind) = WatchedLiterals::oracle_consistency::<D::NameId>(
                        new_var.negative(),
                        prior_var.positive(),
                    );
                    self.add_clause(wl, kind);
                    let (wl, kind) = WatchedLiterals::oracle_consistency::<D::NameId>(
                        prior_var.negative(),
                        new_var.positive(),
                    );
                    self.add_clause(wl, kind);
                }
                VersionSetRelation::Unknown => {
                    // No consistency clause needed.
                }
            }
        }

        new_var
    }

    /// Returns the solvables that the solver has chosen to include in the
    /// solution so far.
    fn chosen_solvables(&self) -> impl Iterator<Item = D::SolvableId> + '_ {
        self.decision_tracker.stack().filter_map(|d| {
            if d.value {
                d.variable.as_solvable(&self.variable_map)
            } else {
                // Ignore things that are set to false
                None
            }
        })
    }

    /// Returns true if `solvable_id` has been assigned the value `true` in the
    /// current decision state. A solvable that has never been interned as a
    /// variable is treated as not decided.
    pub(crate) fn is_positively_decided(&self, solvable_id: D::SolvableId) -> bool {
        match self.variable_map.lookup_solvable(solvable_id) {
            Some(variable) => self.decision_tracker.assigned_value(variable) == Some(true),
            None => false,
        }
    }

    /// Returns the `ConditionId`s of deferred requirements whose gating
    /// disjunct currently holds. Entries are reported in insertion order so
    /// that drain order is deterministic.
    pub(crate) fn fired_deferred_conditions(&self) -> Vec<ConditionId> {
        self.deferred_requirements
            .iter()
            .filter(|(_, entries)| {
                entries.iter().any(|entry| {
                    condition_disjunct_holds(&entry.disjunct, |s| self.is_positively_decided(s))
                })
            })
            .map(|(condition, _)| *condition)
            .collect()
    }

    /// Drain deferred requirements for `condition` whose disjunct currently
    /// holds. Removed entries are returned. If, after draining, no entries
    /// remain for `condition`, the key is removed from the map entirely.
    pub(crate) fn drain_fired_deferred(
        &mut self,
        condition: ConditionId,
    ) -> Vec<DeferredRequirement<D::SolvableId>> {
        // Decide first which indices fire while only holding immutable
        // borrows of the state, then mutate the deferred map. The two-pass
        // structure avoids overlapping mutable and immutable borrows of
        // `self`.
        let fire_mask: Vec<bool> = match self.deferred_requirements.get(&condition) {
            Some(entries) => entries
                .iter()
                .map(|entry| {
                    condition_disjunct_holds(&entry.disjunct, |s| self.is_positively_decided(s))
                })
                .collect(),
            None => return Vec::new(),
        };

        let entries = self
            .deferred_requirements
            .get_mut(&condition)
            .expect("entries existed in the immutable phase");
        let mut fired = Vec::new();
        // Use `remove` instead of `swap_remove`: it preserves the relative
        // order of the entries that stay in the deferred map, so that any
        // subsequent drain visits them in the same order regardless of how
        // many drains have happened before. Walk indices in reverse so each
        // `remove` does not shift the indices we still need to inspect.
        for (i, _) in fire_mask
            .iter()
            .enumerate()
            .rev()
            .filter(|&(_, &did_fire)| did_fire)
        {
            fired.push(entries.remove(i));
        }

        if entries.is_empty() {
            self.deferred_requirements.swap_remove(&condition);
        }

        // We pushed in reverse order; restore insertion order for determinism.
        fired.reverse();
        fired
    }

    /// Register a new deferred requirement. Used by the encoder when it
    /// detects that a conditional requirement's disjunct does not yet hold.
    pub(crate) fn push_deferred(&mut self, entry: DeferredRequirement<D::SolvableId>) {
        // Universal solving must encode conditional requirements eagerly so
        // that requires-clause registration order (hence the decide scan
        // order, hence the enumerated cells) is independent of runtime
        // decisions; the lazy deferral's extra encode round-trip would
        // reorder it and break the reseed fixed point. `universal_mode` gates
        // the eager path in `queue_conditional_requirement`, so nothing should
        // ever defer here in a universal solve. (See `Self::universal_mode`.)
        debug_assert!(
            !self.universal_mode,
            "universal solving must not defer conditional requirements: \
             deferral reorders clause registration and breaks enumeration determinism"
        );
        self.deferred_requirements
            .entry(entry.condition)
            .or_default()
            .push(entry);
    }

    /// Total number of deferred per-disjunct entries still waiting for their
    /// condition to fire.
    pub(crate) fn deferred_requirements_count(&self) -> usize {
        self.deferred_requirements
            .values()
            .map(|entries| entries.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::luby;

    #[test]
    fn luby_produces_the_textbook_sequence() {
        let observed: Vec<u64> = (0..15).map(luby).collect();
        assert_eq!(observed, vec![1, 1, 2, 1, 1, 2, 4, 1, 1, 2, 1, 1, 2, 4, 8]);
    }
}
