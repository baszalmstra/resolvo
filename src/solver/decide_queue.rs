//! An incremental work queue for [`super::Solver::decide`].
//!
//! `decide()` selects the next requirement to act on by folding a
//! best-decision accumulator over every requires clause of every installed
//! parent, in a fixed scan order (parent insertion order, then clause
//! order). Rescanning everything on every call is quadratic in practice:
//! large problems spend most of their time iterating parents whose clauses
//! are all satisfied.
//!
//! This module maintains the same fold incrementally:
//!
//! - Every requires / env-constrains clause is an *item* with a stable
//!   *order key* equal to its position in the original scan order.
//! - A sorted queue holds the items that may currently be *eligible*
//!   (parent installed, condition met, clause not yet satisfied). The
//!   queue is a superset of the eligible items whose parent is installed:
//!   items are only removed when an inspection proves them ineligible,
//!   and every state change that could make an item eligible re-inserts
//!   it.
//! - Wake-ups are driven by the assignment trail. The queue keeps a mirror
//!   of the trail and catches up lazily at the start of each `decide()`
//!   call (see `DecisionTracker::take_sync_floor`), routing each changed
//!   variable through occurrence lists to the affected items.
//! - The candidate walk of a requirement (first undecided candidate,
//!   candidate count, satisfying candidate) is cached per *requirement*
//!   and shared by all clauses with that requirement. A cached
//!   `Satisfied` result is revalidated in O(1) on wake-up (is the
//!   satisfying candidate still true?), which makes the frequent case,
//!   propagation forbidding candidates of already-satisfied clauses, a
//!   no-op.
//!
//! The fold itself exploits a property of the replacement rule: a later
//! clause only ever replaces the running best with a strictly *higher*
//! package activity, and activities are non-negative, so only clauses
//! whose package was ever involved in a conflict (a *hot* name) can
//! replace anything. The fold therefore visits the first eligible item
//! (the initial best) and then only the eligible *hot* items after it,
//! which the queue tracks in a second, much smaller sorted set. Names
//! become hot when [`DecideQueue::mark_name_hot`] is called from the
//! conflict-analysis activity bump and never become cold again (uniform
//! decay can underflow an activity back to zero, which only makes the hot
//! set a conservative superset).
//!
//! Because the queue iterates eligible items in exactly the original scan
//! order and the fold applies exactly the original replacement rule, the
//! selected decision is identical to the full rescan, bit for bit. In
//! debug builds [`super::Solver::decide`] asserts this against the
//! original implementation on every call.

use std::collections::BTreeSet;

use crate::{
    DenseIndex, Interner, Requirement, VariableId, VersionSetId,
    internal::id::{ClauseId, EnvConstrainsId},
    requirement::RequirementMap,
    solver_id::SolverId,
};

use super::{conditions::DisjunctionId, decision_tracker::DecisionTracker};

/// Identifies an item in [`DecideQueue::items`].
pub(crate) type ItemId = u32;

/// The requires pass precedes the env-constrains pass in the original scan
/// order, so requires items sort before env-constrains items.
const SEGMENT_REQUIRES: u64 = 0;
const SEGMENT_ENV_CONSTRAINS: u64 = 1;

const PARENT_BITS: u32 = 31;
const CLAUSE_BITS: u32 = 31;

/// Packs (segment, parent position, clause position) into a single key
/// that orders items exactly like the original nested scan.
fn pack_key(segment: u64, parent_pos: usize, clause_pos: usize) -> u64 {
    debug_assert!(parent_pos < (1 << PARENT_BITS), "parent position overflow");
    debug_assert!(clause_pos < (1 << CLAUSE_BITS), "clause position overflow");
    (segment << (PARENT_BITS + CLAUSE_BITS))
        | ((parent_pos as u64) << CLAUSE_BITS)
        | clause_pos as u64
}

/// One clause that `decide()` may act on.
pub(crate) struct DecideItem {
    /// The item's position in the original scan order.
    pub(crate) key: u64,
    /// The parent variable; the clause is relevant while this is true.
    pub(crate) parent: VariableId,
    /// The clause to report as the source of the decision.
    pub(crate) clause_id: ClauseId,
    pub(crate) kind: DecideItemKind,
    /// Whether the item is currently in [`DecideQueue::queue`].
    queued: bool,
    /// Whether any package name of the item's requirement was ever bumped
    /// (see [`DecideQueue::mark_name_hot`]). Hot queued items are mirrored
    /// in [`DecideQueue::hot_queue`].
    hot: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum DecideItemKind {
    Requires {
        requirement: Requirement,
        condition: Option<DisjunctionId>,
    },
    EnvConstrains {
        env_constrains_id: EnvConstrainsId,
    },
}

/// Cached candidate-walk state of a requirement, shared by all clauses
/// with the same requirement (the walk only depends on the requirement's
/// candidates and the current assignment).
struct ReqEntry {
    state: ReqState,
    /// Whether `state` must be recomputed before use.
    dirty: bool,
    /// The items whose clause carries this requirement.
    items: Vec<ItemId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReqState {
    /// Some candidate is assigned true: the clause is satisfied.
    Satisfied { by: VariableId },
    /// The first undecided candidate, its version set, and the number of
    /// undecided candidates in that version set (the original heuristic
    /// tuple of the rescan).
    Frontier {
        candidate: VariableId,
        version_set: VersionSetId,
        count: u32,
    },
    /// Every candidate is assigned false. For an eligible clause this is
    /// unreachable (propagation would have failed the parent first); the
    /// caller asserts on it.
    AllFalse,
}

/// Incremental queue of `decide()` items, generic over the provider's
/// package name id (used to track hot names). See the module docs.
pub(crate) struct DecideQueue<N: SolverId> {
    items: Vec<DecideItem>,
    /// Items that may be eligible, ordered by scan position.
    queue: BTreeSet<(u64, ItemId)>,
    /// The queued items whose requirement mentions a hot package name:
    /// the only items that can replace a non-trivial running best.
    hot_queue: BTreeSet<(u64, ItemId)>,
    /// Package names whose activity was ever bumped.
    hot_names: ahash::HashSet<N>,
    /// Package name -> items whose requirement mentions the name, for the
    /// cold-to-hot migration when the name is first bumped.
    name_items: ahash::HashMap<N, Vec<ItemId>>,
    /// Cached candidate walks, keyed by requirement.
    reqs: RequirementMap<ReqEntry>,
    /// Variable -> items whose clause parent is the variable.
    parent_occ: Vec<Vec<ItemId>>,
    /// Variable -> requirements containing the variable as a candidate.
    cand_occ: Vec<Vec<Requirement>>,
    /// Variable -> items whose condition disjunction mentions the variable.
    cond_occ: Vec<Vec<ItemId>>,
    /// Variable -> env-constrains items watching the variable.
    env_occ: Vec<Vec<ItemId>>,
    /// Mirror of the assignment trail at the last sync point.
    shadow: Vec<VariableId>,
    /// Reusable buffer for the undone trail suffix during sync.
    undone_scratch: Vec<VariableId>,
}

impl<N: SolverId> Default for DecideQueue<N> {
    fn default() -> Self {
        Self {
            items: Default::default(),
            queue: Default::default(),
            hot_queue: Default::default(),
            hot_names: Default::default(),
            name_items: Default::default(),
            reqs: Default::default(),
            parent_occ: Default::default(),
            cand_occ: Default::default(),
            cond_occ: Default::default(),
            env_occ: Default::default(),
            shadow: Default::default(),
            undone_scratch: Default::default(),
        }
    }
}

fn occ_push<T>(occ: &mut Vec<Vec<T>>, var: VariableId, value: T) {
    let index = var.to_index();
    if index >= occ.len() {
        occ.resize_with(index + 1, Vec::new);
    }
    occ[index].push(value);
}

fn occ_get<T>(occ: &[Vec<T>], var: VariableId) -> &[T] {
    occ.get(var.to_index()).map_or(&[], Vec::as_slice)
}

/// Queues an item unless its parent is not installed: an item is only
/// eligible while its parent is assigned true, and the parent-occurrence
/// wake-up re-attempts the insertion when the parent becomes true. The
/// filter keeps the constant churn of parent variables being assigned
/// false (forbid propagation) away from the queue.
fn enqueue(
    items: &mut [DecideItem],
    queue: &mut BTreeSet<(u64, ItemId)>,
    hot_queue: &mut BTreeSet<(u64, ItemId)>,
    tracker: &DecisionTracker,
    id: ItemId,
) {
    let item = &mut items[id as usize];
    if !item.queued && tracker.assigned_value(item.parent) == Some(true) {
        item.queued = true;
        queue.insert((item.key, id));
        if item.hot {
            hot_queue.insert((item.key, id));
        }
    }
}

impl<N: SolverId> DecideQueue<N> {
    /// Registers a requires clause. Must be called when the clause is added
    /// to `requires_clauses`, with the parent's map position and the
    /// clause's position in the parent's clause list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_requires_item(
        &mut self,
        parent_pos: usize,
        clause_pos: usize,
        parent: VariableId,
        requirement: Requirement,
        condition: Option<DisjunctionId>,
        condition_variables: impl Iterator<Item = VariableId>,
        package_names: impl Iterator<Item = N>,
        sorted_candidates: &[Vec<VariableId>],
        clause_id: ClauseId,
        tracker: &DecisionTracker,
    ) {
        let id: ItemId = self.items.len().try_into().expect("decide item overflow");
        let key = pack_key(SEGMENT_REQUIRES, parent_pos, clause_pos);

        let mut hot = false;
        for name in package_names {
            hot |= self.hot_names.contains(&name);
            self.name_items.entry(name).or_default().push(id);
        }

        self.items.push(DecideItem {
            key,
            parent,
            clause_id,
            kind: DecideItemKind::Requires {
                requirement,
                condition,
            },
            queued: false,
            hot,
        });

        occ_push(&mut self.parent_occ, parent, id);
        for variable in condition_variables {
            occ_push(&mut self.cond_occ, variable, id);
        }

        // Register the requirement's candidate occurrences exactly once;
        // all clauses with the same requirement share the cached walk.
        match self.reqs.get_mut(requirement) {
            Some(entry) => entry.items.push(id),
            None => {
                for &candidate in sorted_candidates.iter().flatten() {
                    occ_push(&mut self.cand_occ, candidate, requirement);
                }
                self.reqs.insert(
                    requirement,
                    ReqEntry {
                        // The state is recomputed on first use.
                        state: ReqState::AllFalse,
                        dirty: true,
                        items: vec![id],
                    },
                );
            }
        }

        // The parent may already be installed (clauses are encoded after
        // their parent was selected), so the item may start queued.
        enqueue(
            &mut self.items,
            &mut self.queue,
            &mut self.hot_queue,
            tracker,
            id,
        );
    }

    /// Registers an env-constrains clause, mirroring the original second
    /// pass of the rescan.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_env_constrains_item(
        &mut self,
        parent_pos: usize,
        clause_pos: usize,
        parent: VariableId,
        env_constrains_id: EnvConstrainsId,
        absent_var: Option<VariableId>,
        matches_var: VariableId,
        package_name: N,
        clause_id: ClauseId,
        tracker: &DecisionTracker,
    ) {
        let id: ItemId = self.items.len().try_into().expect("decide item overflow");
        let key = pack_key(SEGMENT_ENV_CONSTRAINS, parent_pos, clause_pos);

        let hot = self.hot_names.contains(&package_name);
        self.name_items.entry(package_name).or_default().push(id);

        self.items.push(DecideItem {
            key,
            parent,
            clause_id,
            kind: DecideItemKind::EnvConstrains { env_constrains_id },
            queued: false,
            hot,
        });

        occ_push(&mut self.parent_occ, parent, id);
        if let Some(absent_var) = absent_var {
            occ_push(&mut self.env_occ, absent_var, id);
        }
        occ_push(&mut self.env_occ, matches_var, id);

        enqueue(
            &mut self.items,
            &mut self.queue,
            &mut self.hot_queue,
            tracker,
            id,
        );
    }

    /// Marks a package name as hot because its activity was bumped. The
    /// queued items of the name migrate into the hot queue; future
    /// enqueues of its items insert into both queues.
    pub(crate) fn mark_name_hot(&mut self, name: N) {
        if !self.hot_names.insert(name) {
            return;
        }
        let Some(item_ids) = self.name_items.get(&name) else {
            return;
        };
        for &id in item_ids {
            let item = &mut self.items[id as usize];
            if !item.hot {
                item.hot = true;
                if item.queued {
                    self.hot_queue.insert((item.key, id));
                }
            }
        }
    }

    /// Catches the queue up with every assignment made or undone since the
    /// previous call, waking the affected items.
    pub(crate) fn sync(&mut self, tracker: &mut DecisionTracker) {
        let floor = tracker.take_sync_floor();
        let tracker = &*tracker;
        debug_assert!(
            floor <= self.shadow.len(),
            "the sync floor can never exceed the mirrored trail"
        );

        // Mirror entries at or beyond the floor were undone since the last
        // sync (entries re-assigned in the meantime reappear in the new
        // trail suffix below and are simply touched twice).
        let mut undone = std::mem::take(&mut self.undone_scratch);
        undone.clear();
        undone.extend(self.shadow.drain(floor..));
        for &variable in &undone {
            self.touch(variable, tracker);
        }
        self.undone_scratch = undone;

        // Trail entries at or beyond the floor are new assignments.
        for decision in tracker.stack_from(floor) {
            self.shadow.push(decision.variable);
            self.touch(decision.variable, tracker);
        }
    }

    /// Wakes every item whose eligibility or heuristic value may have
    /// changed because `variable`'s assignment changed.
    fn touch(&mut self, variable: VariableId, tracker: &DecisionTracker) {
        let DecideQueue {
            items,
            queue,
            hot_queue,
            reqs,
            parent_occ,
            cand_occ,
            cond_occ,
            env_occ,
            ..
        } = self;

        for &requirement in occ_get(cand_occ, variable) {
            let entry = reqs
                .get_mut(requirement)
                .expect("touched requirement is registered");
            // An already-dirty requirement was processed when it became
            // dirty; until the fold cleans it again there is nothing new
            // to do.
            if entry.dirty {
                continue;
            }
            match entry.state {
                ReqState::Satisfied { by } => {
                    // A clause satisfied by a candidate that is still true
                    // stays satisfied (and ineligible) regardless of the
                    // other candidates; skip the wake-up entirely. This
                    // keeps the common case, propagation forbidding
                    // candidates of already-satisfied clauses, O(1).
                    if tracker.assigned_value(by) == Some(true) {
                        continue;
                    }
                    // The satisfaction broke: items that were dequeued as
                    // satisfied may be eligible again and must re-enter
                    // the queue.
                    entry.dirty = true;
                    for &item in &entry.items {
                        enqueue(items, queue, hot_queue, tracker, item);
                    }
                }
                _ => {
                    // A frontier requirement's heuristic tuple changed,
                    // but its eligible items never left the queue (the
                    // fold only dequeues an item of a *satisfied*
                    // requirement, and parent/condition dequeues are woken
                    // by their own occurrence lists), so marking the entry
                    // for re-evaluation is enough.
                    entry.dirty = true;
                }
            }
        }
        // The remaining wake-ups are filtered by the variable's current
        // value: a wake-up is only needed when the new value could turn an
        // unqueued item eligible. Queued items never need waking, and
        // ineligible-making transitions are discovered lazily by the fold.
        let value = tracker.assigned_value(variable);
        if value == Some(true) {
            // A parent became installed: its items may now be eligible.
            // (Any other value keeps them ineligible.)
            for &item in occ_get(parent_occ, variable) {
                enqueue(items, queue, hot_queue, tracker, item);
            }
        }
        // Condition literals carry a polarity (a negated literal evaluates
        // to false when its variable is true), so any change of a
        // condition variable can complete the all-false condition of a
        // conditional requirement: wake unconditionally. Conditions are
        // rare, so this stays cheap.
        for &item in occ_get(cond_occ, variable) {
            enqueue(items, queue, hot_queue, tracker, item);
        }
        if value != Some(true) {
            // An env-constrains candidate stopped being true (undo, or a
            // net true-to-false flip between two syncs): an item that was
            // dequeued as satisfied by it may be eligible again. (A
            // true assignment can only satisfy, never unsatisfy.)
            for &item in occ_get(env_occ, variable) {
                enqueue(items, queue, hot_queue, tracker, item);
            }
        }
    }

    /// The first queued item whose key is strictly greater than `cursor`
    /// (or the overall first when `cursor` is `None`).
    pub(crate) fn next_after(&self, cursor: Option<u64>) -> Option<(u64, ItemId)> {
        use std::ops::Bound::{Excluded, Unbounded};
        match cursor {
            None => self.queue.iter().next().copied(),
            Some(key) => self
                .queue
                .range((Excluded((key, ItemId::MAX)), Unbounded))
                .next()
                .copied(),
        }
    }

    /// The first queued *hot* item whose key is strictly greater than
    /// `cursor`.
    pub(crate) fn next_hot_after(&self, cursor: u64) -> Option<(u64, ItemId)> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.hot_queue
            .range((Excluded((cursor, ItemId::MAX)), Unbounded))
            .next()
            .copied()
    }

    pub(crate) fn item(&self, id: ItemId) -> &DecideItem {
        &self.items[id as usize]
    }

    /// Removes an item from the queue after an inspection proved it
    /// ineligible. The wake-up rules re-insert it when any input of that
    /// inspection changes.
    pub(crate) fn unqueue(&mut self, id: ItemId) {
        let item = &mut self.items[id as usize];
        if item.queued {
            item.queued = false;
            self.queue.remove(&(item.key, id));
            if item.hot {
                self.hot_queue.remove(&(item.key, id));
            }
        }
    }

    /// Returns the requirement's cached candidate walk, recomputing it if
    /// any candidate assignment changed since it was cached. The walk is
    /// the exact walk of the original rescan: find the first version set
    /// with an undecided candidate, count the undecided candidates of that
    /// version set, and stop early when a candidate is already true.
    pub(crate) fn eval_requirement(
        &mut self,
        requirement: Requirement,
        sorted_candidates: &RequirementMap<Vec<Vec<VariableId>>>,
        interner: &impl Interner,
        tracker: &DecisionTracker,
    ) -> ReqState {
        let entry = self
            .reqs
            .get_mut(requirement)
            .expect("evaluated requirement is registered");
        if !entry.dirty {
            return entry.state;
        }

        let candidates_per_set = &sorted_candidates[requirement];
        let mut frontier: Option<(VariableId, VersionSetId, u32)> = None;
        let mut satisfied = None;
        'walk: for (version_set, candidates) in
            requirement.version_sets(interner).zip(candidates_per_set)
        {
            for &candidate in candidates {
                match tracker.assigned_value(candidate) {
                    Some(true) => {
                        satisfied = Some(ReqState::Satisfied { by: candidate });
                        break 'walk;
                    }
                    Some(false) => {}
                    None => match &mut frontier {
                        Some((_, frontier_set, count)) => {
                            if *frontier_set == version_set {
                                *count += 1;
                            }
                        }
                        None => frontier = Some((candidate, version_set, 1)),
                    },
                }
            }
        }

        let state = match (satisfied, frontier) {
            (Some(satisfied), _) => satisfied,
            (None, Some((candidate, version_set, count))) => ReqState::Frontier {
                candidate,
                version_set,
                count,
            },
            (None, None) => ReqState::AllFalse,
        };
        let entry = self
            .reqs
            .get_mut(requirement)
            .expect("evaluated requirement is registered");
        entry.state = state;
        entry.dirty = false;
        state
    }
}
