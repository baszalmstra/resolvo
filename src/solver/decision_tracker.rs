use crate::solver::{decision::Decision, decision_map::DecisionMap};
use crate::{VariableId, internal::id::ClauseId};

/// Tracks the assignments to solvables, keeping a log that can be used to backtrack (the *trail*:
/// assignments are pushed in chronological order and backtracking pops a suffix), and a map that
/// can be used to query the current value assigned
#[derive(Default)]
pub(crate) struct DecisionTracker {
    map: DecisionMap,
    stack: Vec<Decision>,
    propagate_index: usize,

    /// The lowest stack length observed since the last call to
    /// [`Self::take_assert_floor`]. The assertion scans
    /// (`crate::solver::assertion_scans`) use it to detect assignments
    /// popped by backtracking.
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

    pub(crate) fn level(&self, variable_id: VariableId) -> u32 {
        self.map.level(variable_id)
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

        // Record the trough for the assertion scans. A backtrack only pops
        // (never pushes), so the minimum stack length over the whole
        // sequence is its final length; updating the floor once here is
        // equivalent to updating it on every pop. Conflict analysis pops
        // directly via `undo_last` but always finishes with an
        // `undo_until` to the backjump level, which reaches the deepest
        // point, so that path is covered here too.
        self.assert_floor = self.assert_floor.min(self.stack.len());
    }

    pub(crate) fn undo_last(&mut self) -> (Decision, u32) {
        let decision = self.stack.pop().unwrap();
        self.map.reset(decision.variable);

        self.propagate_index = self.stack.len();

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
    #[inline]
    pub(crate) fn stack_len(&self) -> usize {
        self.stack.len()
    }

    /// Returns the lowest stack length observed since the previous call
    /// and re-arms the marker to `usize::MAX`.
    ///
    /// A truncation to length `n` pops exactly the positions `>= n`, so
    /// an assignment survived every truncation since the previous call
    /// if and only if its position is below the returned floor. When
    /// nothing was undone the floor is `usize::MAX`; a fresh or cleared
    /// tracker reports zero (everything was popped).
    #[inline]
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
