//! An incremental index over the blocking clauses for [`super::Solver::decide`].
//!
//! Universal enumeration appends one blocking clause per recorded non-empty
//! cell. When nothing else is left to decide, `decide()` must find the first
//! registered multi-literal blocking clause that is not satisfied under the
//! undecided-counts-as-false completion, and decide that clause's first
//! undecided positive literal (see the blocking-completion step of
//! [`super::Solver::decide`]). The clause list grows monotonically with the
//! cell count, so rescanning every clause and literal on every query
//! approaches quadratic work in high-cell solves. This module keeps the
//! per-clause completion result cached and invalidates it through occurrence
//! lists, exactly like [`super::decide_queue`] does for requires clauses.
//!
//! - Every multi-literal blocking clause is registered as a
//!   [`BlockingEntry`], identified by its [`BlockingEntryId`]: the position
//!   in registration order. Single-literal blocking clauses are assertions
//!   applied during propagation and are never indexed.
//! - [`BlockingCompletionIndex::active`] maps the entry id of every clause
//!   that is *not* satisfied under the completion to its
//!   [`CompletionAction`]. The first completion result is the lowest active
//!   entry id, which reproduces the reference scan's first-clause winner;
//!   the per-entry action caches the first-literal winner.
//! - Assignment changes reach the index lazily: a dedicated trail floor
//!   ([`super::decision_tracker::DecisionTracker::take_blocking_sync_floor`])
//!   plus a trail mirror route every net-changed variable through the
//!   occurrence lists immediately before a query, marking the affected
//!   entries dirty; dirty entries are recomputed against the current
//!   decision map before the winner is read.
//!
//! The index is exactly behavior preserving. Debug builds re-run the full
//! reference scan at every query and assert that the complete result tuple
//! matches (see `blocking_completion_reference` in [`super`]).

use std::collections::BTreeMap;

use ahash::HashMap;

use crate::{
    VariableId,
    internal::id::{ClauseId, EnvClauseId},
};

use super::{clause::Literal, decision::Decision, decision_map::DecisionMap};

/// Index of a [`BlockingEntry`] in [`BlockingCompletionIndex::entries`]:
/// the registration order of the multi-literal blocking clauses, which is
/// their winner-selection order.
pub(crate) type BlockingEntryId = u32;

/// One registered multi-literal blocking clause.
struct BlockingEntry {
    env_clause_id: EnvClauseId,
    clause_id: ClauseId,
    /// The clause's literals in [`super::SolverState::add_env_clause`] order
    /// (after its stable duplicate removal). Kept inline so recomputation
    /// and the unit tests need no access to the env-clause arena.
    literals: Vec<Literal>,
}

/// The cached completion result of one unsatisfied blocking clause.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum CompletionAction {
    /// The clause's first undecided positive literal: deciding this variable
    /// to true is the completion progress `decide()` makes.
    Decide(VariableId),
    /// The clause has no undecided positive literal: every literal is false
    /// under the current assignment. This is an internal invariant failure
    /// (a fully-false clause must have conflicted during propagation), kept
    /// in the index so an earlier broken clause is never skipped in favor of
    /// a later intact one — dropping it would hide the propagation bug.
    FullyFalse,
}

/// Work counters of the index, for diagnostics only.
#[cfg(feature = "diagnostics")]
#[derive(Default)]
pub(crate) struct BlockingCompletionCounters {
    /// Completion queries answered.
    pub queries: u64,
    /// Multi-literal blocking clauses registered.
    pub clauses_registered: u64,
    /// Trail variables routed through the occurrence lists during sync.
    pub trail_routed: u64,
    /// Occurrence-list entries visited while routing.
    pub occurrence_visits: u64,
    /// Dirty entries recomputed.
    pub recomputes: u64,
    /// Literals evaluated during recomputation.
    pub literal_visits: u64,
    /// Queries that returned a completion action.
    pub hits: u64,
    /// Recomputations that cached [`CompletionAction::FullyFalse`].
    pub fully_false: u64,
    /// Largest observed size of the active set.
    pub max_active: u64,
}

/// See the module docs.
#[derive(Default)]
pub(crate) struct BlockingCompletionIndex {
    /// All registered clauses, in registration order.
    entries: Vec<BlockingEntry>,
    /// For each variable occurring in a registered clause, the entries whose
    /// cached result the variable's assignment can change. Deduplicated per
    /// clause: opposite-sign literals of one variable share the entry.
    occurrences: HashMap<VariableId, Vec<BlockingEntryId>>,
    /// Every entry not satisfied under the completion, with its cached
    /// action. Ordered by entry id so the winner is the first key.
    active: BTreeMap<BlockingEntryId, CompletionAction>,
    /// Entries whose cached state may be stale, pending recomputation at the
    /// next query. Deduplicated through `dirty_bits`.
    dirty: Vec<BlockingEntryId>,
    /// Dense dirty markers, parallel to `entries`.
    dirty_bits: Vec<bool>,
    /// The trail variables as of the previous sync (see
    /// [`Self::sync`]).
    mirror: Vec<VariableId>,
    #[cfg(feature = "diagnostics")]
    pub(crate) counters: BlockingCompletionCounters,
}

impl BlockingCompletionIndex {
    /// Registers a multi-literal blocking clause. `literals` must be the
    /// deduplicated literal list of the env clause, in order.
    ///
    /// The new entry starts dirty unconditionally: registration can happen
    /// under a retained trail prefix that the mirror has never seen, and a
    /// suffix of that trail can be popped again before the next query
    /// without ever reaching the mirror. Deferring the first computation to
    /// the next query makes it correct whether the trail is unchanged,
    /// extended, or retracted in between.
    pub(crate) fn register(
        &mut self,
        env_clause_id: EnvClauseId,
        clause_id: ClauseId,
        literals: &[Literal],
    ) {
        debug_assert!(
            literals.len() > 1,
            "single-literal blocking clauses are assertions and are not indexed"
        );
        let id = BlockingEntryId::try_from(self.entries.len())
            .expect("more blocking clauses than fit a u32");
        for (position, literal) in literals.iter().enumerate() {
            let variable = literal.variable();
            // One occurrence per distinct variable: opposite-sign literals
            // of the same variable need only one invalidation.
            if literals[..position]
                .iter()
                .any(|earlier| earlier.variable() == variable)
            {
                continue;
            }
            self.occurrences.entry(variable).or_default().push(id);
        }
        self.entries.push(BlockingEntry {
            env_clause_id,
            clause_id,
            literals: literals.to_vec(),
        });
        self.dirty_bits.push(false);
        self.mark_dirty(id);
        #[cfg(feature = "diagnostics")]
        {
            self.counters.clauses_registered += 1;
        }
    }

    fn mark_dirty(&mut self, id: BlockingEntryId) {
        if !std::mem::replace(&mut self.dirty_bits[id as usize], true) {
            self.dirty.push(id);
        }
    }

    /// Brings the index up to date with all assignment changes since the
    /// previous call, dirtying every entry whose cached result may have
    /// changed.
    ///
    /// `trail` is the solver's chronological assignment log and `floor`
    /// (from [`super::decision_tracker::DecisionTracker::take_blocking_sync_floor`])
    /// is the lowest trail length reached since the previous call, so
    /// `mirror[floor..]` was popped at some point and `trail[floor..]` was
    /// pushed since; variables in either range cover every net assignment
    /// change (see [`super::decide_queue::DecideQueue::sync`], which this
    /// mirrors). An assignment both pushed and popped in between has no net
    /// effect and is correctly never observed.
    pub(crate) fn sync(&mut self, floor: usize, trail: &[Decision]) {
        for position in floor..self.mirror.len() {
            let variable = self.mirror[position];
            self.touch(variable);
        }
        self.mirror.truncate(floor);
        for decision in &trail[floor..] {
            self.mirror.push(decision.variable);
            self.touch(decision.variable);
        }
    }

    /// Dirties every entry in `variable`'s occurrence list.
    fn touch(&mut self, variable: VariableId) {
        #[cfg(feature = "diagnostics")]
        {
            self.counters.trail_routed += 1;
        }
        let Self {
            occurrences,
            dirty,
            dirty_bits,
            ..
        } = self;
        let Some(ids) = occurrences.get(&variable) else {
            return;
        };
        for &id in ids {
            #[cfg(feature = "diagnostics")]
            {
                self.counters.occurrence_visits += 1;
            }
            if !std::mem::replace(&mut dirty_bits[id as usize], true) {
                dirty.push(id);
            }
        }
    }

    /// Recomputes the dirty entries against `map` and returns the first
    /// registered clause unsatisfied under the undecided-counts-as-false
    /// completion, with its cached action. [`Self::sync`] must run first.
    pub(crate) fn first_unsatisfied(
        &mut self,
        map: &DecisionMap,
    ) -> Option<(EnvClauseId, ClauseId, CompletionAction)> {
        // Processing order is not observable: winner selection below uses
        // only the (registration-ordered) key order of `active`.
        while let Some(id) = self.dirty.pop() {
            self.dirty_bits[id as usize] = false;
            self.recompute(id, map);
        }
        #[cfg(feature = "diagnostics")]
        {
            self.counters.queries += 1;
            self.counters.max_active = self.counters.max_active.max(self.active.len() as u64);
        }
        let result = self.active.first_key_value().map(|(&id, &action)| {
            let entry = &self.entries[id as usize];
            (entry.env_clause_id, entry.clause_id, action)
        });
        #[cfg(feature = "diagnostics")]
        if result.is_some() {
            self.counters.hits += 1;
        }
        result
    }

    /// Recomputes one entry's completion state: scan the clause in literal
    /// order, drop it from `active` as soon as one literal is true under the
    /// completion, otherwise cache the first undecided positive literal (or
    /// [`CompletionAction::FullyFalse`] when none exists).
    fn recompute(&mut self, id: BlockingEntryId, map: &DecisionMap) {
        #[cfg(feature = "diagnostics")]
        {
            self.counters.recomputes += 1;
        }
        let entry = &self.entries[id as usize];
        let mut first_undecided_positive = None;
        for &literal in &entry.literals {
            #[cfg(feature = "diagnostics")]
            {
                self.counters.literal_visits += 1;
            }
            let assigned = map.value(literal.variable());
            // The literal's value under the undecided-counts-as-false
            // completion: an undecided variable evaluates positive literals
            // to false and negative literals to true.
            let completed = assigned.map_or(literal.negate(), |value| value != literal.negate());
            if completed {
                self.active.remove(&id);
                return;
            }
            if !literal.negate() && assigned.is_none() && first_undecided_positive.is_none() {
                first_undecided_positive = Some(literal.variable());
            }
        }
        let action = match first_undecided_positive {
            Some(variable) => CompletionAction::Decide(variable),
            None => {
                #[cfg(feature = "diagnostics")]
                {
                    self.counters.fully_false += 1;
                }
                CompletionAction::FullyFalse
            }
        };
        self.active.insert(id, action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DenseIndex, internal::id::ClauseId};

    fn variable(index: usize) -> VariableId {
        VariableId::from_index(index)
    }

    fn positive(index: usize) -> Literal {
        Literal::new(variable(index), false)
    }

    fn negative(index: usize) -> Literal {
        Literal::new(variable(index), true)
    }

    fn env_clause_id(index: usize) -> EnvClauseId {
        EnvClauseId::from_index(index)
    }

    fn clause_id(index: usize) -> ClauseId {
        ClauseId::from_index(index)
    }

    /// A miniature stand-in for the solver's trail + map pair: pushing
    /// assigns, popping unassigns, and the index is synchronized through the
    /// same floor discipline `decide()` uses.
    #[derive(Default)]
    struct Harness {
        map: DecisionMap,
        trail: Vec<Decision>,
        floor: usize,
    }

    impl Harness {
        fn push(&mut self, index: usize, value: bool) {
            self.map.set(variable(index), value, 1);
            self.trail
                .push(Decision::new(variable(index), value, clause_id(0)));
        }

        fn pop(&mut self) {
            let decision = self.trail.pop().expect("trail is not empty");
            self.map.reset(decision.variable);
            self.floor = self.floor.min(self.trail.len());
        }

        fn query(
            &mut self,
            index: &mut BlockingCompletionIndex,
        ) -> Option<(EnvClauseId, ClauseId, CompletionAction)> {
            let floor = std::mem::replace(&mut self.floor, self.trail.len());
            index.sync(floor, &self.trail);
            index.first_unsatisfied(&self.map)
        }
    }

    #[test]
    fn earlier_unsatisfied_clause_beats_a_later_one() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        index.register(env_clause_id(1), clause_id(11), &[positive(3), positive(4)]);
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::Decide(variable(1))
            ))
        );
    }

    #[test]
    fn first_undecided_positive_literal_wins() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(
            env_clause_id(0),
            clause_id(10),
            &[positive(1), positive(2), positive(3)],
        );
        // Assigning the first positive literal false moves the frontier to
        // the second.
        harness.push(1, false);
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::Decide(variable(2))
            ))
        );
    }

    #[test]
    fn undecided_negative_literal_satisfies_the_clause() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[negative(1), positive(2)]);
        // Variable 1 is undecided, so the negative literal is true under the
        // completion: the clause is satisfied and there is nothing to do.
        assert_eq!(harness.query(&mut index), None);
    }

    #[test]
    fn assigning_the_negative_variable_true_makes_the_clause_eligible() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[negative(1), positive(2)]);
        assert_eq!(harness.query(&mut index), None);
        harness.push(1, true);
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::Decide(variable(2))
            ))
        );
    }

    #[test]
    fn assigning_a_positive_variable_true_removes_the_clause_from_active() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        assert!(harness.query(&mut index).is_some());
        harness.push(2, true);
        assert_eq!(harness.query(&mut index), None);
    }

    #[test]
    fn backtracking_restores_the_previous_frontier() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        harness.push(1, false);
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::Decide(variable(2))
            ))
        );
        harness.pop();
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::Decide(variable(1))
            ))
        );
    }

    #[test]
    fn pop_and_reassign_above_the_floor_dirties_all_occurrences() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        index.register(env_clause_id(1), clause_id(11), &[positive(1), positive(3)]);
        harness.push(1, true);
        // Both clauses are satisfied: variable 1 occurs in both.
        assert_eq!(harness.query(&mut index), None);
        // Pop and reassign the variable false at a different trail position:
        // both entries must be invalidated and become active again.
        harness.pop();
        harness.push(4, true);
        harness.push(1, false);
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::Decide(variable(2))
            ))
        );
        harness.pop();
        harness.pop();
        harness.push(1, true);
        assert_eq!(harness.query(&mut index), None);
    }

    #[test]
    fn net_push_and_pop_between_queries_changes_nothing() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        let before = harness.query(&mut index);
        // A push and pop between queries has no net effect; the floor
        // discipline never routes the variable, and the result is unchanged.
        harness.push(2, true);
        harness.pop();
        assert_eq!(harness.query(&mut index), before);
    }

    #[test]
    fn append_under_an_existing_trail_is_dirty_until_its_first_query() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        harness.push(1, true);
        // Sync the mirror to the current trail first, then register: the
        // registration itself must leave the entry dirty so the next query
        // computes it against the existing assignment.
        harness.query(&mut index);
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        assert_eq!(harness.query(&mut index), None);
    }

    #[test]
    fn append_under_a_retained_suffix_popped_before_the_first_query() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        // The suffix (variable 1 = true) never reaches the mirror: it is
        // pushed and popped entirely between queries, with the registration
        // in between. The unconditional dirty marking must still yield the
        // post-backtrack result.
        harness.push(1, true);
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        harness.pop();
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::Decide(variable(1))
            ))
        );
    }

    #[test]
    fn duplicate_and_opposite_sign_variable_occurrences_invalidate_correctly() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        // Variable 1 occurs with both signs; the entry registers a single
        // occurrence for it and both literals still evaluate correctly.
        index.register(
            env_clause_id(0),
            clause_id(10),
            &[negative(1), positive(1), positive(2)],
        );
        // Undecided: the negative literal is true under the completion.
        assert_eq!(harness.query(&mut index), None);
        // Assigned true: the positive literal satisfies the clause.
        harness.push(1, true);
        assert_eq!(harness.query(&mut index), None);
        // Assigned false: the negative literal satisfies the clause.
        harness.pop();
        harness.push(1, false);
        assert_eq!(harness.query(&mut index), None);
        // With a purely positive clause the same flip goes both ways.
        index.register(env_clause_id(1), clause_id(11), &[positive(1), positive(3)]);
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(1),
                clause_id(11),
                CompletionAction::Decide(variable(3))
            ))
        );
        harness.pop();
        harness.push(1, true);
        assert_eq!(harness.query(&mut index), None);
    }

    #[test]
    fn a_fully_false_earlier_clause_is_not_skipped() {
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        index.register(env_clause_id(0), clause_id(10), &[positive(1), positive(2)]);
        index.register(env_clause_id(1), clause_id(11), &[positive(3), positive(4)]);
        harness.push(1, false);
        harness.push(2, false);
        // The earlier clause is fully false (the invariant failure case);
        // it must still win over the later intact clause so the breakage is
        // observed, not skipped.
        assert_eq!(
            harness.query(&mut index),
            Some((
                env_clause_id(0),
                clause_id(10),
                CompletionAction::FullyFalse
            ))
        );
    }

    #[test]
    fn alternating_satisfaction_across_backtracks_tracks_the_reference() {
        // Three clauses whose assignments alternately satisfy and invalidate
        // earlier entries across backtracks; every query is checked against
        // a straight reimplementation of the reference scan.
        let clauses: Vec<Vec<Literal>> = vec![
            vec![positive(1), negative(2), positive(3)],
            vec![positive(2), positive(4)],
            vec![negative(1), positive(5), positive(2)],
        ];
        let mut index = BlockingCompletionIndex::default();
        let mut harness = Harness::default();
        for (position, literals) in clauses.iter().enumerate() {
            index.register(env_clause_id(position), clause_id(10 + position), literals);
        }
        let reference = |map: &DecisionMap| -> Option<(usize, CompletionAction)> {
            for (position, literals) in clauses.iter().enumerate() {
                let mut first_undecided_positive = None;
                let mut satisfied = false;
                for &literal in literals {
                    let assigned = map.value(literal.variable());
                    let completed =
                        assigned.map_or(literal.negate(), |value| value != literal.negate());
                    if completed {
                        satisfied = true;
                        break;
                    }
                    if !literal.negate() && assigned.is_none() && first_undecided_positive.is_none()
                    {
                        first_undecided_positive = Some(literal.variable());
                    }
                }
                if !satisfied {
                    return Some((
                        position,
                        first_undecided_positive
                            .map_or(CompletionAction::FullyFalse, CompletionAction::Decide),
                    ));
                }
            }
            None
        };
        let steps: Vec<(&str, usize, bool)> = vec![
            ("push", 2, true),
            ("push", 1, false),
            ("pop", 0, false),
            ("push", 4, true),
            ("push", 1, true),
            ("pop", 0, false),
            ("pop", 0, false),
            ("push", 2, false),
            ("push", 5, true),
            ("pop", 0, false),
            ("pop", 0, false),
        ];
        // Check the initial state and after every step.
        for (kind, variable_index, value) in std::iter::once(("noop", 0, false)).chain(steps) {
            match kind {
                "push" => harness.push(variable_index, value),
                "pop" => harness.pop(),
                _ => {}
            }
            let expected = reference(&harness.map).map(|(position, action)| {
                (env_clause_id(position), clause_id(10 + position), action)
            });
            assert_eq!(harness.query(&mut index), expected, "after {kind}");
        }
    }
}
