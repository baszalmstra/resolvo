use crate::solver::{decision::Decision, decision_map::DecisionMap};
use crate::{VariableId, internal::id::ClauseId};

/// Tracks the assignments to solvables, keeping a log that can be used to backtrack, and a map that
/// can be used to query the current value assigned
#[derive(Default)]
pub(crate) struct DecisionTracker {
    map: DecisionMap,

    /// The *trail*: every assignment in the chronological order it was made,
    /// both decisions and propagated assignments. Assigning pushes onto it
    /// and backtracking pops from it, so the trail only ever changes at its
    /// end: two snapshots of the trail always share a common prefix up to
    /// the minimum length reached in between.
    stack: Vec<Decision>,

    propagate_index: usize,

    /// The minimum length of `stack` since the last call to
    /// [`Self::take_sync_floor`]: the length of the trail prefix that is
    /// guaranteed unchanged since then. [`crate::solver::decide_queue`] keeps
    /// a copy of the trail and uses the floor to find what changed: copied
    /// entries at or beyond the floor have been popped at some point (their
    /// variables may be unassigned now or reassigned elsewhere), and current
    /// `stack` entries at or beyond it were pushed since. An assignment that
    /// was both pushed and popped in between is in neither range; it has no
    /// net effect, so a consumer comparing snapshots never needs to see it.
    sync_floor: usize,

    /// The lowest stack length observed since the last call to
    /// [`Self::take_assert_floor`]. The assertion watermark uses this to
    /// find which verified assertions lost their assignment to
    /// backtracking: any assignment at a stack position at or beyond the
    /// floor has been popped since the previous observation. Unlike
    /// `sync_floor` this floor is armed to `usize::MAX` when taken (no
    /// truncation happened means nothing is invalidated); the `Default`
    /// of zero after `clear()` invalidates everything, as it must.
    assert_floor: usize,
}

impl DecisionTracker {
    pub(crate) fn clear(&mut self) {
        *self = Default::default();
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    #[inline(always)]
    pub(crate) fn assigned_value(&self, variable_id: VariableId) -> Option<bool> {
        self.map.value(variable_id)
    }

    #[inline]
    pub(crate) fn map(&self) -> &DecisionMap {
        &self.map
    }

    pub(crate) fn stack(&self) -> impl DoubleEndedIterator<Item = Decision> + '_ {
        self.stack.iter().copied()
    }

    /// Returns the current decision stack as a slice.
    pub(crate) fn assignments(&self) -> &[Decision] {
        &self.stack
    }

    /// Returns the minimum trail length reached since the previous call (or
    /// since construction) and resets the floor to the current trail length.
    ///
    /// The trail up to the returned floor is unchanged since the previous
    /// call; everything a snapshot-comparing consumer has to look at lies at
    /// or beyond it. See [`Self::sync_floor`].
    pub(crate) fn take_sync_floor(&mut self) -> usize {
        std::mem::replace(&mut self.sync_floor, self.stack.len())
    }

    pub(crate) fn level(&self, variable_id: VariableId) -> u32 {
        self.map.level(variable_id)
    }

    /// The decision level of the deepest decision on the stack, or 0 when no
    /// decisions have been made.
    pub(crate) fn deepest_level(&self) -> u32 {
        self.stack
            .last()
            .map(|decision| self.map.level(decision.variable))
            .unwrap_or(0)
    }

    // Find the clause that caused the assignment of the specified solvable. If no assignment has
    // been made to the solvable than `None` is returned.
    pub(crate) fn find_clause_for_assignment(&self, variable_id: VariableId) -> Option<ClauseId> {
        self.stack
            .iter()
            .find(|d| d.variable == variable_id)
            .map(|d| d.derived_from)
    }

    /// Attempts to add a decision
    ///
    /// Returns true if the solvable was undecided, false if it was already decided to the same value
    ///
    /// Returns an error if the solvable was decided to a different value (which means there is a conflict)
    #[inline]
    pub(crate) fn try_add_decision(&mut self, decision: Decision, level: u32) -> Result<bool, ()> {
        match self.map.value(decision.variable) {
            None => {
                self.map.set(decision.variable, decision.value, level);
                self.stack.push(decision);
                Ok(true)
            }
            Some(value) if value == decision.value => Ok(false),
            _ => Err(()),
        }
    }

    pub(crate) fn undo_until(&mut self, level: u32) {
        if level == 0 {
            self.clear();
            return;
        }

        while let Some(decision) = self.stack.last() {
            if self.level(decision.variable) <= level {
                break;
            }

            self.undo_last();
        }
    }

    pub(crate) fn undo_last(&mut self) -> (Decision, u32) {
        let decision = self.stack.pop().unwrap();
        self.map.reset(decision.variable);

        self.propagate_index = self.stack.len();
        self.sync_floor = self.sync_floor.min(self.stack.len());
        self.assert_floor = self.assert_floor.min(self.stack.len());

        let top_decision = self.stack.last().unwrap();
        (decision, self.map.level(top_decision.variable))
    }

    /// Returns the next decision in the log for which unit propagation still needs to run
    ///
    /// Side-effect: the decision will be marked as propagated
    #[inline]
    pub(crate) fn next_unpropagated(&mut self) -> Option<Decision> {
        let &decision = self.stack[self.propagate_index..].iter().next()?;
        self.propagate_index += 1;
        Some(decision)
    }

    /// The number of decisions on the stack.
    pub(crate) fn stack_len(&self) -> usize {
        self.stack.len()
    }

    /// Returns the lowest stack length observed since the previous call and
    /// re-arms the marker to `usize::MAX`.
    ///
    /// Every assignment that was popped since the previous call sat at a
    /// stack position at or beyond the returned floor (a truncation to
    /// length `n` pops exactly the positions `>= n`), so a consumer that
    /// remembers the position of an assignment can detect its removal by
    /// comparing the position against the floor. When nothing was undone
    /// the floor is `usize::MAX` and no position qualifies. A fresh or
    /// cleared tracker reports a floor of zero: everything is invalidated.
    pub(crate) fn take_assert_floor(&mut self) -> usize {
        std::mem::replace(&mut self.assert_floor, usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DenseIndex;

    fn decision(index: usize, value: bool) -> Decision {
        Decision::new(
            VariableId::from_index(index),
            value,
            ClauseId::install_root(),
        )
    }

    #[test]
    fn sync_floor_tracks_the_deepest_truncation_between_calls() {
        let mut tracker = DecisionTracker::default();
        assert_eq!(tracker.take_sync_floor(), 0);

        for i in 1..=4 {
            tracker
                .try_add_decision(decision(i, true), i as u32)
                .unwrap();
        }
        // Nothing was undone: the floor is the previous snapshot point.
        assert_eq!(tracker.take_sync_floor(), 0);
        assert_eq!(tracker.assignments().len(), 4);

        // Undo to level 2, then add new decisions on top: the floor must
        // point at the truncation, not at the final length.
        tracker.undo_until(2);
        for i in 5..=7 {
            tracker.try_add_decision(decision(i, false), 3).unwrap();
        }
        assert_eq!(tracker.take_sync_floor(), 2);
        assert_eq!(tracker.take_sync_floor(), 5);

        // A full clear resets the floor to zero.
        tracker.clear();
        assert_eq!(tracker.take_sync_floor(), 0);
    }

    #[test]
    fn assert_floor_reports_the_deepest_truncation_or_max_when_untouched() {
        let mut tracker = DecisionTracker::default();
        // A fresh tracker invalidates everything.
        assert_eq!(tracker.take_assert_floor(), 0);
        // Taking the floor re-arms it: nothing was undone since.
        assert_eq!(tracker.take_assert_floor(), usize::MAX);

        for i in 1..=4 {
            tracker
                .try_add_decision(decision(i, true), i as u32)
                .unwrap();
        }
        // Additions alone never lower the floor.
        assert_eq!(tracker.take_assert_floor(), usize::MAX);

        // Undo to level 2 (stack length 2), then regrow: the floor reports
        // the truncation point, not the final length.
        tracker.undo_until(2);
        for i in 5..=7 {
            tracker.try_add_decision(decision(i, false), 3).unwrap();
        }
        assert_eq!(tracker.take_assert_floor(), 2);
        assert_eq!(tracker.take_assert_floor(), usize::MAX);

        // A full clear resets the floor to zero: everything is invalidated.
        tracker.clear();
        assert_eq!(tracker.take_assert_floor(), 0);
    }
}
