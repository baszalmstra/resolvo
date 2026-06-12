use crate::solver::{
    decision::Decision,
    decision_map::{DecisionMap, PackageVar},
};
use crate::{VariableId, internal::id::ClauseId};

/// Tracks the assignments to solvables, keeping a log that can be used to backtrack, and a map that
/// can be used to query the current value assigned
#[derive(Default)]
pub(crate) struct DecisionTracker {
    map: DecisionMap,
    stack: Vec<Decision>,
    propagate_index: usize,
}

impl DecisionTracker {
    pub(crate) fn clear(&mut self) {
        // Keep the package registry of the map: it mirrors the encoding
        // state (which candidates belong to which package), not the search
        // state.
        self.map.reset_assignments();
        self.stack.clear();
        self.propagate_index = 0;
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

    #[inline]
    pub(crate) fn map_mut(&mut self) -> &mut DecisionMap {
        &mut self.map
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
                match self.map.var_role(decision.variable) {
                    // A true assignment of a tracked package candidate
                    // selects it for the whole package: all sibling
                    // candidates now evaluate to false without being
                    // physically assigned. Ladder packages (with prefix
                    // variables) express the selection through two explicit
                    // boundary prefix assignments pushed right behind the
                    // member's own trail entry.
                    PackageVar::Member { package, index } if decision.value => {
                        if self.map.package_prefix_count(package) == 0 {
                            self.map
                                .set_selection(package, decision.variable, index, level);
                        } else {
                            self.stack.push(decision);
                            return self
                                .push_selection_boundaries(package, index, level)
                                .map(|_| true);
                        }
                    }
                    // An explicit prefix assignment narrows the allowed
                    // candidate index interval of the package.
                    PackageVar::Prefix { package, index } => {
                        self.map.apply_prefix_assignment(
                            package,
                            index,
                            decision.value,
                            decision.variable,
                            level,
                        );
                    }
                    _ => {}
                }
                self.stack.push(decision);
                Ok(true)
            }
            Some(value) if value == decision.value => Ok(false),
            _ => Err(()),
        }
    }

    /// Pushes the two explicit boundary prefix assignments
    /// (`p_{index-1} := false`, `p_index := true`) for a selection of the
    /// `index`-th member of `package` (virtual-ladder encoding). The reasons
    /// are the pre-materialized boundary clauses of the member.
    pub(crate) fn push_selection_boundaries(
        &mut self,
        package: u32,
        index: u32,
        level: u32,
    ) -> Result<(), ()> {
        let (lower_reason, upper_reason) = self.map.boundary_reasons(package, index as usize);
        if let Some(lower_reason) = lower_reason {
            let p = self.map.package_prefix(package, (index - 1) as usize);
            self.try_add_decision(Decision::new(p, false, lower_reason), level)?;
        }
        let p = self.map.package_prefix(package, index as usize);
        self.try_add_decision(Decision::new(p, true, upper_reason), level)?;
        Ok(())
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
        match self.map.var_role(decision.variable) {
            PackageVar::Member { package, .. } if decision.value => {
                if let Some((selected, _, _)) = self.map.selection(package) {
                    if selected == decision.variable {
                        self.map.clear_selection(package);
                    }
                }
            }
            PackageVar::Prefix { package, .. } => {
                self.map.recompute_interval(package);
            }
            _ => {}
        }

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
}
