//! An incremental work queue for [`super::Solver::decide`].
//!
//! `decide()` must pick a candidate from a requires clause that is currently
//! *eligible*: its parent is assigned true, its condition (if any) holds, and
//! no candidate is assigned true yet. Only a small fraction of all requires
//! clauses is eligible at any point, so instead of inspecting every clause on
//! every call, this module tracks the eligible set incrementally and selects
//! the best decision from it.
//!
//! - Every requires clause is registered as a [`TrackedClause`], identified
//!   by its [`ClausePosition`]: a *segment* (requires clauses before
//!   env-constrains clauses, see below), then the parent's registration
//!   index, then the clause's position in the parent's list. All are
//!   append-only, so positions are stable, and iterating clauses by position
//!   visits them in a fixed, deterministic order.
//! - [`DecideQueue::queue`] maps the position of each possibly-eligible
//!   clause to its [`TrackedClauseId`]. It holds a *superset* of the
//!   eligible clauses, restricted to clauses whose parent is assigned true:
//!   clauses are removed only when an inspection in
//!   [`DecideQueue::next_decision`] proves them ineligible, and every
//!   assignment change that can make an unqueued clause eligible re-inserts
//!   it through occurrence lists (see [`DecideQueue::sync`]).
//! - [`DecideQueue::next_decision`] selects among the eligible clauses: it
//!   takes the first eligible clause by position as the initial best and
//!   then considers only the *hot* clauses after it. A clause replaces the
//!   best only with strictly higher package activity; activities are
//!   non-negative and a package that was never involved in a conflict has
//!   activity exactly zero, so only clauses naming a package whose activity
//!   was ever bumped (hot) can win. Hot clauses are mirrored in the much
//!   smaller [`DecideQueue::hot_queue`].
//!
//! The selection heuristic lives only in `next_decision`; the
//! heuristic-independent bookkeeping (wake-ups, caches, hot promotion) is
//! verified on every call in debug builds by
//! [`DecideQueue::debug_assert_invariants`], because a hole there does not
//! crash or change solutions, it just silently degrades the search.
//!
//! # Universal solving
//!
//! Universal solving (see [`crate::solver::universal`]) adds a second kind of
//! tracked clause, the *env-constrains* clause `(¬A v Ab_p v L_S)`, whose
//! ordered candidates are the absent literal then the matches literal. These
//! clauses live in the [`SEGMENT_ENV_CONSTRAINS`] segment, so they sort after
//! every requires clause; combined with the env-literals-last ordering in
//! `decide()` this keeps environment-literal decisions at the top of the
//! trail. Env-constrains clauses share all the queue bookkeeping with requires
//! clauses; only their eligibility walk differs (see [`DecideQueue::inspect`]).
//! For the env-literals-last selection `decide()` drives the queue directly
//! through the cursor accessors ([`DecideQueue::next_after`],
//! [`DecideQueue::next_hot_after`], [`DecideQueue::item`],
//! [`DecideQueue::inspect`] and [`DecideQueue::unqueue`]) instead of
//! [`DecideQueue::next_decision`].

use std::collections::BTreeMap;
use std::ops::Bound;

use ahash::HashMap;

use crate::{
    DependencyProvider, Requirement, VariableId, VersionSetId,
    internal::{arena::Arena, id::ClauseId, small_vec::SmallVec},
    requirement::RequirementMap,
    solver_id::{IdMap, IdSet, SolverId},
};

use super::{
    conditions::{Disjunction, DisjunctionId},
    decision::Decision,
    decision_map::DecisionMap,
};

/// Index of a [`TrackedClause`] in [`DecideQueue::clauses`].
pub(crate) type TrackedClauseId = u32;

/// Requires clauses are scanned before env-constrains clauses, mirroring the
/// two passes of the original `decide()` rescan and realizing the
/// env-literals-last ordering: a requires decision is always preferred over an
/// env-constrains decision.
const SEGMENT_REQUIRES: u64 = 0;
const SEGMENT_ENV_CONSTRAINS: u64 = 1;

const PARENT_BITS: u32 = 31;
const CLAUSE_BITS: u32 = 31;

/// The position of a tracked clause in the queue's registration order: the
/// segment in the high bits, then the parent's registration index, then the
/// clause's position in the parent's list. Comparing positions orders clauses
/// by segment first, parent second and list order last, which is the order in
/// which [`DecideQueue::next_decision`] considers them.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub(crate) struct ClausePosition(u64);

impl ClausePosition {
    fn new(segment: u64, parent_pos: usize, clause_pos: usize) -> Self {
        assert!(
            parent_pos < (1 << PARENT_BITS) && clause_pos < (1 << CLAUSE_BITS),
            "clause position exceeds the packed u64 layout"
        );
        Self(
            (segment << (PARENT_BITS + CLAUSE_BITS))
                | ((parent_pos as u64) << CLAUSE_BITS)
                | clause_pos as u64,
        )
    }
}

/// What kind of clause a [`TrackedClause`] represents, with the per-kind data
/// the eligibility walk needs.
#[derive(Copy, Clone)]
pub(crate) enum TrackedClauseKind {
    /// A requires clause `(¬A v B1 .. v Bn)`. Its candidates are the sorted
    /// candidate lists in `requirement_to_sorted_candidates`, walked through
    /// the per-requirement cache.
    Requires {
        requirement: Requirement,
        condition: Option<DisjunctionId>,
    },
    /// An env-constrains clause `(¬A v Ab_p v L_S)` (universal solving). Its
    /// ordered candidates are the absent literal (if the package can be
    /// absent) followed by the matches literal. `version_set` is the
    /// constraint's version set, used for the package-activity lookup.
    EnvConstrains {
        absent_var: Option<VariableId>,
        matches_var: VariableId,
        version_set: VersionSetId,
    },
}

/// A clause as tracked by the queue.
#[derive(Copy, Clone)]
pub(crate) struct TrackedClause {
    pub(crate) position: ClausePosition,
    pub(crate) parent: VariableId,
    pub(crate) clause_id: ClauseId,
    pub(crate) kind: TrackedClauseKind,
    /// Whether any package name in the clause has ever had its activity
    /// bumped. Only hot clauses can replace a running best in
    /// [`DecideQueue::next_decision`]; cold clauses have package activity
    /// exactly zero.
    hot: bool,
}

/// The cached result of walking a requirement's sorted candidate lists.
///
/// The walk depends only on the requirement and the current assignment, so it
/// is cached per requirement and shared by all clauses with that requirement.
/// Package activity is *not* cached; it is read fresh during selection.
#[derive(Copy, Clone)]
enum RequirementState {
    /// Unknown: not yet evaluated, or a candidate assignment changed since
    /// the last evaluation.
    Dirty,
    /// A candidate is assigned true, so every clause with this requirement is
    /// satisfied. `by` acts like a satisfying watch literal: as long as it
    /// stays true, changes to other candidates cannot break satisfaction.
    Satisfied { by: VariableId },
    /// No candidate is assigned true, and `candidate` is the first undecided
    /// candidate in walk order. `count` is the number of undecided candidates
    /// in the version set the first undecided candidate was found in.
    Frontier {
        candidate: VariableId,
        version_set: VersionSetId,
        count: u32,
    },
}

struct RequirementEntry {
    state: RequirementState,
    /// Candidate occurrences are registered on the first evaluation; most
    /// encoded requirements belong to solvables that are never installed and
    /// are never evaluated. Before registration the entry is permanently
    /// dirty, so a missed wake-up cannot lose information.
    occurrences_registered: bool,
}

/// The decision produced by [`DecideQueue::next_decision`], including the
/// heuristic inputs for tracing.
pub(crate) struct QueueDecision {
    pub candidate: VariableId,
    pub required_by: VariableId,
    pub clause_id: ClauseId,
    pub package_activity: f32,
    pub candidate_count: u32,
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
pub(crate) struct DecideQueueCounters {
    /// Variables routed through the occurrence lists during sync.
    pub sync_touches: u64,
    /// Clauses removed from the queue after an inspection proved them
    /// ineligible.
    pub dequeues: u64,
    /// Clause inspections during selection.
    pub selection_visits: u64,
    /// Inspections of hot clauses after the initial best.
    pub hot_visits: u64,
    /// Requirement walks actually evaluated (cache misses).
    pub walk_evals: u64,
}

pub(crate) struct DecideQueue<D: DependencyProvider> {
    /// All registered clauses, indexed by [`TrackedClauseId`].
    clauses: Vec<TrackedClause>,
    /// The registration index of each parent variable: the parent half of the
    /// [`ClausePosition`]s of its clauses, assigned on first registration.
    parent_positions: HashMap<VariableId, u32>,
    /// Clause ids per parent registration index.
    clauses_by_parent: Vec<Vec<TrackedClauseId>>,

    /// Position -> clause id for every possibly-eligible clause, ordered so
    /// that iteration visits clauses in their fixed selection order.
    queue: BTreeMap<ClausePosition, TrackedClauseId>,
    /// The subset of `queue` whose clauses are hot, maintained in lockstep.
    hot_queue: BTreeMap<ClausePosition, TrackedClauseId>,

    /// Names whose activity was ever bumped. Never shrinks: decay can bring
    /// an activity back to zero, which only makes the hot set a conservative
    /// superset.
    hot_names: <D::NameId as SolverId>::Set,
    /// Name -> clauses whose requirement (or env-constrains package) mentions
    /// that name, used to promote clauses when a name first becomes hot.
    clauses_by_name: HashMap<D::NameId, Vec<TrackedClauseId>>,

    /// Candidate variable -> requirements whose walk inspected it. Registered
    /// lazily on a requirement's first evaluation.
    requirements_by_candidate: HashMap<VariableId, SmallVec<Requirement>>,
    /// Condition variable -> clauses whose condition disjunction mentions it.
    clauses_by_condition_variable: HashMap<VariableId, Vec<TrackedClauseId>>,
    /// Environment-literal variable -> env-constrains clauses whose candidate
    /// pair contains it. Used to re-enqueue an env-constrains clause when the
    /// literal that satisfied it stops being true.
    clauses_by_env_var: HashMap<VariableId, Vec<TrackedClauseId>>,

    /// The walk cache, one entry per requirement.
    requirement_states: RequirementMap<RequirementEntry>,
    /// Requirement -> clauses with that requirement, woken when the
    /// requirement's satisfaction breaks.
    clauses_by_requirement: RequirementMap<Vec<TrackedClauseId>>,

    /// The variables of the solver's trail (the chronological assignment log
    /// in [`super::decision_tracker::DecisionTracker`]) as they were at the
    /// previous [`Self::sync`]. Comparing this snapshot against the current
    /// trail tells the queue which variables changed in between.
    mirror: Vec<VariableId>,

    /// When true (the default; requires `activity_add > 0` and
    /// `activity_decay >= 0`) the selection only visits hot clauses after the
    /// first eligible one and may stop early on activity grounds. With
    /// non-standard activity parameters the non-negativity argument breaks
    /// down, so the selection visits every eligible clause instead.
    hot_only: bool,

    #[cfg(feature = "diagnostics")]
    pub(crate) counters: DecideQueueCounters,
}

impl<D: DependencyProvider> Default for DecideQueue<D> {
    fn default() -> Self {
        Self {
            clauses: Vec::new(),
            parent_positions: HashMap::default(),
            clauses_by_parent: Vec::new(),
            queue: BTreeMap::new(),
            hot_queue: BTreeMap::new(),
            hot_names: Default::default(),
            clauses_by_name: HashMap::default(),
            requirements_by_candidate: HashMap::default(),
            clauses_by_condition_variable: HashMap::default(),
            clauses_by_env_var: HashMap::default(),
            requirement_states: RequirementMap::default(),
            clauses_by_requirement: RequirementMap::default(),
            mirror: Vec::new(),
            hot_only: true,
            #[cfg(feature = "diagnostics")]
            counters: DecideQueueCounters::default(),
        }
    }
}

/// Inserts a clause into the queue (and the hot queue if it is hot), unless
/// its parent is not currently assigned true. Filtering on the parent here
/// keeps the constant churn of candidate variables being forbidden from ever
/// touching the queue.
fn enqueue_clause(
    queue: &mut BTreeMap<ClausePosition, TrackedClauseId>,
    hot_queue: &mut BTreeMap<ClausePosition, TrackedClauseId>,
    clauses: &[TrackedClause],
    map: &DecisionMap,
    id: TrackedClauseId,
) {
    let clause = &clauses[id as usize];
    if map.value(clause.parent) != Some(true) {
        return;
    }
    queue.insert(clause.position, id);
    if clause.hot {
        hot_queue.insert(clause.position, id);
    }
}

impl<D: DependencyProvider> DecideQueue<D> {
    /// Configures whether the activity-based selection shortcuts are sound
    /// for the solver's activity parameters. See [`Self::hot_only`].
    pub(crate) fn set_standard_activity_params(&mut self, standard: bool) {
        self.hot_only = standard;
    }

    /// Allocates (or reuses) the parent registration index for `parent` and
    /// returns it together with the next clause position in the parent's list.
    fn allocate_position(&mut self, parent: VariableId) -> (usize, usize) {
        let parent_pos = *self.parent_positions.entry(parent).or_insert_with(|| {
            self.clauses_by_parent.push(Vec::new());
            (self.clauses_by_parent.len() - 1) as u32
        }) as usize;
        let clause_pos = self.clauses_by_parent[parent_pos].len();
        (parent_pos, clause_pos)
    }

    /// Registers a newly encoded requires clause. Must be called exactly once
    /// per requires clause, in encoding order.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_clause(
        &mut self,
        parent: VariableId,
        requirement: Requirement,
        condition: Option<DisjunctionId>,
        clause_id: ClauseId,
        names: impl IntoIterator<Item = D::NameId>,
        disjunctions: &Arena<DisjunctionId, Disjunction>,
        parent_value: Option<bool>,
    ) {
        let (parent_pos, clause_pos) = self.allocate_position(parent);
        let position = ClausePosition::new(SEGMENT_REQUIRES, parent_pos, clause_pos);
        let id = self.clauses.len() as TrackedClauseId;

        // A union can mention the same package in several version sets;
        // dedup so the occurrence lists hold each clause once.
        let mut hot = false;
        let mut seen_names: SmallVec<D::NameId> = SmallVec::empty();
        for name in names {
            if seen_names.as_slice().contains(&name) {
                continue;
            }
            seen_names.push(name);
            if self.hot_names.contains(name) {
                hot = true;
            }
            self.clauses_by_name.entry(name).or_default().push(id);
        }

        if let Some(condition) = condition {
            for literal in &disjunctions[condition].literals {
                self.clauses_by_condition_variable
                    .entry(literal.variable())
                    .or_default()
                    .push(id);
            }
        }

        self.requirement_states
            .get_or_insert_with(requirement, || RequirementEntry {
                state: RequirementState::Dirty,
                occurrences_registered: false,
            });
        self.clauses_by_requirement
            .get_or_insert_with(requirement, Vec::new)
            .push(id);

        self.clauses_by_parent[parent_pos].push(id);

        self.clauses.push(TrackedClause {
            position,
            parent,
            clause_id,
            kind: TrackedClauseKind::Requires {
                requirement,
                condition,
            },
            hot,
        });

        if parent_value == Some(true) {
            self.queue.insert(position, id);
            if hot {
                self.hot_queue.insert(position, id);
            }
        }
    }

    /// Registers a newly encoded env-constrains clause (universal solving).
    /// Must be called exactly once per env-constrains clause, in encoding
    /// order. The clause is placed in the [`SEGMENT_ENV_CONSTRAINS`] segment,
    /// so it sorts after every requires clause.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_env_constrains_clause(
        &mut self,
        parent: VariableId,
        package_name: D::NameId,
        absent_var: Option<VariableId>,
        matches_var: VariableId,
        version_set: VersionSetId,
        clause_id: ClauseId,
        parent_value: Option<bool>,
    ) {
        let (parent_pos, clause_pos) = self.allocate_position(parent);
        let position = ClausePosition::new(SEGMENT_ENV_CONSTRAINS, parent_pos, clause_pos);
        let id = self.clauses.len() as TrackedClauseId;

        let hot = self.hot_names.contains(package_name);
        self.clauses_by_name
            .entry(package_name)
            .or_default()
            .push(id);

        if let Some(absent_var) = absent_var {
            self.clauses_by_env_var
                .entry(absent_var)
                .or_default()
                .push(id);
        }
        self.clauses_by_env_var
            .entry(matches_var)
            .or_default()
            .push(id);

        self.clauses_by_parent[parent_pos].push(id);

        self.clauses.push(TrackedClause {
            position,
            parent,
            clause_id,
            kind: TrackedClauseKind::EnvConstrains {
                absent_var,
                matches_var,
                version_set,
            },
            hot,
        });

        if parent_value == Some(true) {
            self.queue.insert(position, id);
            if hot {
                self.hot_queue.insert(position, id);
            }
        }
    }

    /// Marks a package name as hot (its activity was bumped) and promotes the
    /// queued clauses that mention it into the hot queue. Names never become
    /// cold again.
    pub(crate) fn mark_name_hot(&mut self, name: D::NameId) {
        if !self.hot_names.insert(name) {
            return;
        }
        let Some(ids) = self.clauses_by_name.get(&name) else {
            return;
        };
        for &id in ids {
            let clause = &mut self.clauses[id as usize];
            if clause.hot {
                continue;
            }
            clause.hot = true;
            let position = clause.position;
            if let Some(&queued) = self.queue.get(&position) {
                self.hot_queue.insert(position, queued);
            }
        }
    }

    /// Brings the queue up to date with all assignment changes since the
    /// previous call, routing every changed variable through the occurrence
    /// lists.
    ///
    /// `trail` is the solver's chronological assignment log: propagation
    /// pushes onto it and backtracking pops from it, so it only ever changes
    /// at its end. [`Self::mirror`] holds the trail variables as of the
    /// previous sync, and `floor` (from
    /// [`super::decision_tracker::DecisionTracker::take_sync_floor`]) is the
    /// lowest trail length reached since then. That splits both snapshots in
    /// three:
    ///
    /// - `[..floor]` is identical in the mirror and the trail: untouched, no
    ///   work.
    /// - `mirror[floor..]` was popped at some point: each variable may be
    ///   unassigned now, or reassigned at a different trail position.
    /// - `trail[floor..]` was pushed since: newly assigned variables.
    ///
    /// Variables in the last two ranges are re-routed (a variable can appear
    /// in both; routing is idempotent). An assignment that was both pushed
    /// and popped between the two calls falls in neither range: it has no net
    /// effect on the assignment, and the queue only compares snapshots, so it
    /// is correct to never see it.
    pub(crate) fn sync(&mut self, floor: usize, trail: &[Decision], map: &DecisionMap) {
        for i in floor..self.mirror.len() {
            let variable = self.mirror[i];
            self.route_touched(variable, map);
        }
        self.mirror.truncate(floor);
        for decision in &trail[floor..] {
            self.mirror.push(decision.variable);
            self.route_touched(decision.variable, map);
        }
    }

    /// Routes one variable whose assignment may have changed through the
    /// occurrence lists, re-inserting clauses that may have become eligible
    /// and invalidating requirement walk caches.
    fn route_touched(&mut self, variable: VariableId, map: &DecisionMap) {
        #[cfg(feature = "diagnostics")]
        {
            self.counters.sync_touches += 1;
        }

        let Self {
            clauses,
            parent_positions,
            clauses_by_parent,
            queue,
            hot_queue,
            requirements_by_candidate,
            clauses_by_condition_variable,
            clauses_by_env_var,
            requirement_states,
            clauses_by_requirement,
            ..
        } = self;

        let value = map.value(variable);

        // Parent wake-up: a clause can only become eligible when its parent
        // is assigned true; any other value keeps it ineligible.
        if value == Some(true) {
            if let Some(&parent_pos) = parent_positions.get(&variable) {
                for &id in &clauses_by_parent[parent_pos as usize] {
                    enqueue_clause(queue, hot_queue, clauses, map, id);
                }
            }
        }

        // Candidate wake-up: invalidate the walk caches of the requirements
        // that inspected this variable, and wake clauses whose satisfaction
        // broke. Frontier entries are only dirtied: a frontier requirement's
        // eligible clauses are still queued (the selection only dequeues
        // clauses of satisfied requirements; parent and condition dequeues
        // are woken by their own lists).
        if let Some(requirements) = requirements_by_candidate.get(&variable) {
            for &requirement in requirements.as_slice() {
                let entry = requirement_states
                    .get_mut(requirement)
                    .expect("occurrence-registered requirement has a cache entry");
                match entry.state {
                    RequirementState::Dirty => {}
                    RequirementState::Frontier { .. } => entry.state = RequirementState::Dirty,
                    RequirementState::Satisfied { by } => {
                        if map.value(by) == Some(true) {
                            // The satisfying candidate is still true; the
                            // clause stays satisfied regardless of this
                            // variable.
                            continue;
                        }
                        entry.state = RequirementState::Dirty;
                        if let Some(woken) = clauses_by_requirement.get(requirement) {
                            for &id in woken {
                                enqueue_clause(queue, hot_queue, clauses, map, id);
                            }
                        }
                    }
                }
            }
        }

        // Condition wake-up: condition literals carry polarity, so any change
        // (in either direction) can complete an all-false condition.
        if let Some(woken) = clauses_by_condition_variable.get(&variable) {
            for &id in woken {
                enqueue_clause(queue, hot_queue, clauses, map, id);
            }
        }

        // Env-constrains wake-up: an env-constrains clause is satisfied by its
        // absent or matches literal being true and dequeued when it is. When
        // such a literal stops being true (undo, or a net true-to-false flip
        // between two syncs) the clause may be eligible again. A true
        // assignment can only satisfy, never unsatisfy, so it needs no wake-up
        // here (the parent wake-up already covers a freshly installed parent).
        if value != Some(true) {
            if let Some(woken) = clauses_by_env_var.get(&variable) {
                for &id in woken {
                    enqueue_clause(queue, hot_queue, clauses, map, id);
                }
            }
        }
    }

    /// Evaluates a requirement through the per-requirement cache by walking
    /// its sorted candidate lists: returns the satisfying candidate if one is
    /// assigned true, or the first undecided candidate, its version set, and
    /// the number of undecided candidates in that version set.
    fn eval_requirement(
        requirement_states: &mut RequirementMap<RequirementEntry>,
        requirements_by_candidate: &mut HashMap<VariableId, SmallVec<Requirement>>,
        requirement: Requirement,
        map: &DecisionMap,
        sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        provider: &D,
        #[cfg(feature = "diagnostics")] counters: &mut DecideQueueCounters,
    ) -> RequirementState {
        let entry = requirement_states
            .get_mut(requirement)
            .expect("every registered clause created a cache entry");
        if !matches!(entry.state, RequirementState::Dirty) {
            return entry.state;
        }

        #[cfg(feature = "diagnostics")]
        {
            counters.walk_evals += 1;
        }

        let version_set_candidates = &sorted_candidates[requirement];

        if !entry.occurrences_registered {
            entry.occurrences_registered = true;
            for &candidate in version_set_candidates.iter().flatten() {
                requirements_by_candidate
                    .entry(candidate)
                    .or_insert_with(SmallVec::empty)
                    .push(requirement);
            }
        }

        let mut first: Option<(VariableId, VersionSetId, u32)> = None;
        'walk: for (version_set, candidates) in requirement
            .version_sets(provider)
            .zip(version_set_candidates)
        {
            for &candidate in candidates {
                match map.value(candidate) {
                    Some(true) => {
                        entry.state = RequirementState::Satisfied { by: candidate };
                        break 'walk;
                    }
                    Some(false) => {}
                    None => match first.as_mut() {
                        Some((_, first_version_set, count)) => {
                            if *first_version_set == version_set {
                                *count += 1;
                            }
                        }
                        None => first = Some((candidate, version_set, 1)),
                    },
                }
            }
        }

        if matches!(entry.state, RequirementState::Dirty) {
            let Some((candidate, version_set, count)) = first else {
                unreachable!(
                    "when we get here it means that all candidates have been assigned false. This should not be able to happen at this point because during propagation the solvable should have been assigned false as well."
                )
            };
            entry.state = RequirementState::Frontier {
                candidate,
                version_set,
                count,
            };
        }
        entry.state
    }

    /// Checks whether a clause is eligible at the current assignment
    /// snapshot. Returns its decision frontier (the candidate to decide, the
    /// version set that supplies the package activity, and the number of
    /// undecided candidates) if it is, or `None` if the clause is ineligible
    /// (parent not installed, condition not met, or already satisfied).
    pub(crate) fn inspect(
        &mut self,
        clause: TrackedClause,
        map: &DecisionMap,
        sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        disjunctions: &Arena<DisjunctionId, Disjunction>,
        provider: &D,
    ) -> Option<(VariableId, VersionSetId, u32)> {
        #[cfg(feature = "diagnostics")]
        {
            self.counters.selection_visits += 1;
        }

        // Consider only clauses in which we have decided to install the
        // parent solvable.
        if map.value(clause.parent) != Some(true) {
            return None;
        }

        match clause.kind {
            TrackedClauseKind::Requires {
                requirement,
                condition,
            } => {
                // If the clause has a condition that is not yet satisfied we
                // need to skip it.
                if let Some(condition) = condition {
                    let literals = &disjunctions[condition].literals;
                    if !literals.iter().all(|c| c.eval(map) == Some(false)) {
                        return None;
                    }
                }

                match Self::eval_requirement(
                    &mut self.requirement_states,
                    &mut self.requirements_by_candidate,
                    requirement,
                    map,
                    sorted_candidates,
                    provider,
                    #[cfg(feature = "diagnostics")]
                    &mut self.counters,
                ) {
                    RequirementState::Satisfied { .. } => None,
                    RequirementState::Frontier {
                        candidate,
                        version_set,
                        count,
                    } => Some((candidate, version_set, count)),
                    RequirementState::Dirty => {
                        unreachable!("eval_requirement never leaves the entry dirty")
                    }
                }
            }
            TrackedClauseKind::EnvConstrains {
                absent_var,
                matches_var,
                version_set,
            } => {
                // Ordered candidates: absent first, then matches (the split
                // policy of the env-constrains encoding).
                let mut frontier: Option<(VariableId, u32)> = None;
                for candidate in absent_var.into_iter().chain([matches_var]) {
                    match map.value(candidate) {
                        Some(true) => return None,
                        Some(false) => {}
                        None => match frontier.as_mut() {
                            Some((_, count)) => *count += 1,
                            None => frontier = Some((candidate, 1)),
                        },
                    }
                }
                let (candidate, count) = frontier?;
                Some((candidate, version_set, count))
            }
        }
    }

    /// Selects the best decision among the eligible clauses, visiting them in
    /// position order: the first eligible clause is the initial best, and a
    /// later clause replaces it only with strictly higher package activity
    /// and strictly fewer remaining candidates (clauses of the root are
    /// always preferred over the rest). Ineligible clauses reached on the way
    /// are dequeued, which is what keeps the queue tight.
    ///
    /// Used for the env-ordering-off path (plain solves and universal solves
    /// once the env ordering is suspended); the env-literals-last path in
    /// `decide()` drives the queue through the cursor accessors instead.
    ///
    /// `max_activity` must be exactly the largest activity stored in
    /// `name_activity` (it is maintained next to the bumps and decays).
    pub(crate) fn next_decision(
        &mut self,
        map: &DecisionMap,
        sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        disjunctions: &Arena<DisjunctionId, Disjunction>,
        name_activity: &<D::NameId as SolverId>::Map<f32>,
        max_activity: f32,
        provider: &D,
    ) -> Option<QueueDecision> {
        struct Best {
            position: ClausePosition,
            explicit: bool,
            activity: f32,
            count: u32,
            decision: (VariableId, VariableId, ClauseId),
        }

        let hot_only = self.hot_only;
        // Replacement requires strictly fewer candidates than the best (so a
        // count of one is unbeatable) and, with standard activity parameters,
        // strictly higher activity (so the global maximum is unbeatable).
        let unbeatable =
            |best: &Best| best.count == 1 || (hot_only && best.activity == max_activity);

        // The first eligible clause in position order is the initial best.
        // Ineligible clauses reached on the way are dequeued (amortized
        // against their insertions).
        let mut best: Option<Best> = None;
        while let Some((&position, &id)) = self.queue.first_key_value() {
            let clause = self.clauses[id as usize];
            match self.inspect(clause, map, sorted_candidates, disjunctions, provider) {
                None => {
                    self.queue.pop_first();
                    self.hot_queue.remove(&position);
                    #[cfg(feature = "diagnostics")]
                    {
                        self.counters.dequeues += 1;
                    }
                }
                Some((candidate, version_set, count)) => {
                    let activity = name_activity.get(provider.version_set_name(version_set));
                    best = Some(Best {
                        position,
                        explicit: clause.parent == VariableId::root(),
                        activity,
                        count,
                        decision: (candidate, clause.parent, clause.clause_id),
                    });
                    break;
                }
            }
        }
        let mut best = best?;

        // Try to replace the best with an eligible clause after it. With
        // standard activity parameters only hot clauses can win, so only the
        // much smaller hot queue is visited.
        if !unbeatable(&best) {
            let mut cursor = best.position;
            loop {
                let next = if hot_only {
                    self.hot_queue
                        .range((Bound::Excluded(cursor), Bound::Unbounded))
                        .next()
                } else {
                    self.queue
                        .range((Bound::Excluded(cursor), Bound::Unbounded))
                        .next()
                }
                .map(|(&position, &id)| (position, id));
                let Some((position, id)) = next else {
                    break;
                };
                cursor = position;

                let clause = self.clauses[id as usize];
                let is_explicit = clause.parent == VariableId::root();

                // Decisions on explicit requirements (clauses of the root)
                // are preferred over non-explicit requirements; such clauses
                // are skipped without an eligibility inspection and stay
                // queued.
                if best.explicit && !is_explicit {
                    continue;
                }

                #[cfg(feature = "diagnostics")]
                {
                    self.counters.hot_visits += 1;
                }

                match self.inspect(clause, map, sorted_candidates, disjunctions, provider) {
                    None => {
                        self.queue.remove(&position);
                        self.hot_queue.remove(&position);
                        #[cfg(feature = "diagnostics")]
                        {
                            self.counters.dequeues += 1;
                        }
                    }
                    Some((candidate, version_set, count)) => {
                        let activity = name_activity.get(provider.version_set_name(version_set));

                        // Prefer a higher package activity score to root out
                        // conflicts faster, and fewer remaining candidates to
                        // reduce backtracking. Both must improve strictly.
                        if best.activity >= activity {
                            continue;
                        }
                        if best.count <= count {
                            continue;
                        }

                        best = Best {
                            position,
                            explicit: is_explicit,
                            activity,
                            count,
                            decision: (candidate, clause.parent, clause.clause_id),
                        };
                        if unbeatable(&best) {
                            break;
                        }
                    }
                }
            }
        }

        let (candidate, required_by, clause_id) = best.decision;
        Some(QueueDecision {
            candidate,
            required_by,
            clause_id,
            package_activity: best.activity,
            candidate_count: best.count,
        })
    }

    /// The clause tracked at `id`. Used by the env-literals-last selection in
    /// `decide()` to read a queued clause's parent, kind and clause id.
    pub(crate) fn item(&self, id: TrackedClauseId) -> TrackedClause {
        self.clauses[id as usize]
    }

    /// The first queued clause whose position is strictly greater than
    /// `cursor` (or the overall first when `cursor` is `None`). Used by the
    /// env-literals-last selection in `decide()`.
    pub(crate) fn next_after(
        &self,
        cursor: Option<ClausePosition>,
    ) -> Option<(ClausePosition, TrackedClauseId)> {
        let mut range = match cursor {
            None => self.queue.range(..),
            Some(position) => self
                .queue
                .range((Bound::Excluded(position), Bound::Unbounded)),
        };
        range.next().map(|(&position, &id)| (position, id))
    }

    /// The first queued *hot* clause whose position is strictly greater than
    /// `cursor`. Used by the env-literals-last selection in `decide()`.
    pub(crate) fn next_hot_after(
        &self,
        cursor: ClausePosition,
    ) -> Option<(ClausePosition, TrackedClauseId)> {
        self.hot_queue
            .range((Bound::Excluded(cursor), Bound::Unbounded))
            .next()
            .map(|(&position, &id)| (position, id))
    }

    /// Removes a clause from the queue after an inspection proved it
    /// ineligible. The wake-up rules re-insert it when any input of that
    /// inspection changes. Used by the env-literals-last selection in
    /// `decide()`; `next_decision` dequeues inline.
    pub(crate) fn unqueue(&mut self, id: TrackedClauseId) {
        let position = self.clauses[id as usize].position;
        self.queue.remove(&position);
        self.hot_queue.remove(&position);
        #[cfg(feature = "diagnostics")]
        {
            self.counters.dequeues += 1;
        }
    }

    /// Verifies the heuristic-independent queue invariants. Called on every
    /// `decide()` in debug builds; a violation means a wake-up, cache, or hot
    /// promotion hole, which would otherwise only show up as silently
    /// different decisions (the solver stays sound under any decision order).
    ///
    /// Must run after [`Self::sync`]; [`Self::next_decision`] only ever
    /// dequeues ineligible clauses, so the invariants hold before and after
    /// it.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_assert_invariants(
        &self,
        map: &DecisionMap,
        sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        disjunctions: &Arena<DisjunctionId, Disjunction>,
        name_activity: &<D::NameId as SolverId>::Map<f32>,
        max_activity: f32,
        provider: &D,
    ) {
        // Every eligible clause is queued. Eligibility is recomputed from
        // first principles, without the caches or the heuristic.
        for (id, clause) in self.clauses.iter().enumerate() {
            if map.value(clause.parent) != Some(true) {
                continue;
            }
            let eligible = match clause.kind {
                TrackedClauseKind::Requires {
                    requirement,
                    condition,
                } => {
                    if let Some(condition) = condition {
                        let literals = &disjunctions[condition].literals;
                        if !literals.iter().all(|c| c.eval(map) == Some(false)) {
                            continue;
                        }
                    }
                    !sorted_candidates[requirement]
                        .iter()
                        .flatten()
                        .any(|&candidate| map.value(candidate) == Some(true))
                }
                TrackedClauseKind::EnvConstrains {
                    absent_var,
                    matches_var,
                    ..
                } => !absent_var
                    .into_iter()
                    .chain([matches_var])
                    .any(|candidate| map.value(candidate) == Some(true)),
            };
            if !eligible {
                continue;
            }
            assert!(
                self.queue.contains_key(&clause.position),
                "eligible clause {id} is not queued"
            );
        }

        // Hot flags match the hot name set, and the hot queue mirrors the
        // queue for hot clauses.
        for (id, clause) in self.clauses.iter().enumerate() {
            let should_be_hot = match clause.kind {
                TrackedClauseKind::Requires { requirement, .. } => {
                    requirement.version_sets(provider).any(|version_set| {
                        self.hot_names
                            .contains(provider.version_set_name(version_set))
                    })
                }
                TrackedClauseKind::EnvConstrains { version_set, .. } => self
                    .hot_names
                    .contains(provider.version_set_name(version_set)),
            };
            assert_eq!(
                clause.hot, should_be_hot,
                "clause {id} hot flag out of sync with the hot name set"
            );
            if clause.hot {
                assert_eq!(
                    self.queue.contains_key(&clause.position),
                    self.hot_queue.contains_key(&clause.position),
                    "hot queue out of lockstep for clause {id}"
                );
            }
        }

        // The early stop relies on `max_activity` being exactly the largest
        // stored activity. Only meaningful with standard activity parameters
        // (the stop is disabled otherwise).
        if self.hot_only {
            let mut actual_max = 0.0f32;
            name_activity.for_each(|&activity| actual_max = actual_max.max(activity));
            assert_eq!(
                max_activity, actual_max,
                "max_activity diverged from the largest stored activity"
            );
        }
    }
}
