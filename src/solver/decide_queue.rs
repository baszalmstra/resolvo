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
//!   queue is a superset of the eligible items: items are only removed
//!   when an inspection proves them ineligible, and every state change
//!   that could make an item eligible again re-inserts it.
//! - Wake-ups are driven by the assignment trail. The queue keeps a mirror
//!   of the trail and catches up lazily at the start of each `decide()`
//!   call (see [`DecisionTracker::take_sync_floor`]), routing each changed
//!   variable through occurrence lists to the affected items.
//! - The candidate walk of a requirement (first undecided candidate,
//!   candidate count, satisfying candidate) is cached per *requirement*
//!   and shared by all clauses with that requirement. A cached
//!   `Satisfied` result is revalidated in O(1) on wake-up (is the
//!   satisfying candidate still true?), which makes the frequent case,
//!   propagation forbidding candidates of already-satisfied clauses, a
//!   no-op.
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

/// The first possible key of a parent's items in a segment.
pub(crate) fn requires_segment_start(parent_pos: usize) -> u64 {
    pack_key(SEGMENT_REQUIRES, parent_pos, 0)
}

/// The last possible key of a parent's items in the requires segment.
pub(crate) fn requires_segment_end(parent_pos: usize) -> u64 {
    pack_key(SEGMENT_REQUIRES, parent_pos, (1 << CLAUSE_BITS) - 1)
}

/// The first possible key of a parent's items in the env-constrains segment.
pub(crate) fn env_segment_start(parent_pos: usize) -> u64 {
    pack_key(SEGMENT_ENV_CONSTRAINS, parent_pos, 0)
}

/// The last possible key of a parent's items in the env-constrains segment.
pub(crate) fn env_segment_end(parent_pos: usize) -> u64 {
    pack_key(SEGMENT_ENV_CONSTRAINS, parent_pos, (1 << CLAUSE_BITS) - 1)
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

/// Incremental queue of `decide()` items. See the module docs.
#[derive(Default)]
pub(crate) struct DecideQueue {
    items: Vec<DecideItem>,
    /// Items that may be eligible, ordered by scan position.
    queue: BTreeSet<(u64, ItemId)>,
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

fn enqueue(items: &mut [DecideItem], queue: &mut BTreeSet<(u64, ItemId)>, id: ItemId) {
    let item = &mut items[id as usize];
    if !item.queued {
        item.queued = true;
        queue.insert((item.key, id));
    }
}

impl DecideQueue {
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
        sorted_candidates: &[Vec<VariableId>],
        clause_id: ClauseId,
    ) {
        let id: ItemId = self.items.len().try_into().expect("decide item overflow");
        let key = pack_key(SEGMENT_REQUIRES, parent_pos, clause_pos);
        self.items.push(DecideItem {
            key,
            parent,
            clause_id,
            kind: DecideItemKind::Requires {
                requirement,
                condition,
            },
            queued: false,
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
        // their parent was selected), so the item starts queued.
        enqueue(&mut self.items, &mut self.queue, id);
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
        clause_id: ClauseId,
    ) {
        let id: ItemId = self.items.len().try_into().expect("decide item overflow");
        let key = pack_key(SEGMENT_ENV_CONSTRAINS, parent_pos, clause_pos);
        self.items.push(DecideItem {
            key,
            parent,
            clause_id,
            kind: DecideItemKind::EnvConstrains { env_constrains_id },
            queued: false,
        });

        occ_push(&mut self.parent_occ, parent, id);
        if let Some(absent_var) = absent_var {
            occ_push(&mut self.env_occ, absent_var, id);
        }
        occ_push(&mut self.env_occ, matches_var, id);

        enqueue(&mut self.items, &mut self.queue, id);
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
            if let ReqState::Satisfied { by } = entry.state {
                // A clause satisfied by a candidate that is still true
                // stays satisfied (and ineligible) regardless of the other
                // candidates; skip the wake-up entirely. This keeps the
                // common case, propagation forbidding candidates of
                // already-satisfied clauses, O(1).
                if !entry.dirty && tracker.assigned_value(by) == Some(true) {
                    continue;
                }
            }
            entry.dirty = true;
            for &item in &entry.items {
                enqueue(items, queue, item);
            }
        }
        for &item in occ_get(parent_occ, variable) {
            enqueue(items, queue, item);
        }
        for &item in occ_get(cond_occ, variable) {
            enqueue(items, queue, item);
        }
        for &item in occ_get(env_occ, variable) {
            enqueue(items, queue, item);
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
        entry.state = state;
        entry.dirty = false;
        state
    }
}
