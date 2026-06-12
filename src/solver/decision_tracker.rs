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
    /// Set on the first conflict. Until then selections of virtual-ladder
    /// packages only push their two boundary prefix assignments; afterwards
    /// the full prefix chain is pushed, giving conflict analysis
    /// adjacent-step anchors. Conflict-free solves (backtracking storms)
    /// never pay for the chain.
    pub(crate) chain_enabled: bool,
}

impl DecisionTracker {
    pub(crate) fn clear(&mut self) {
        // Keep the package registry of the map: it mirrors the encoding
        // state (which candidates belong to which package), not the search
        // state.
        self.map.reset_assignments();
        self.stack.clear();
        self.propagate_index = 0;
        self.chain_enabled = false;
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
                        let trail_index = self.stack.len();
                        self.map.apply_prefix_assignment(
                            package,
                            index,
                            decision.value,
                            decision.variable,
                            level,
                            trail_index,
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

        // Once the solver has started conflicting, push the full prefix
        // chain explicitly (justified by the monotone chain clauses, in
        // chain order). Explicit chain entries give conflict analysis the
        // same adjacent-step range granularity as the clausal sequential
        // encoding; see the learning-gap research in
        // docs/virtual-sibling-negations.md. They are pure learning aids:
        // skipping them never affects soundness, which is why conflict-free
        // solves (backtracking storms) skip them entirely.
        if self.chain_enabled {
            for i in (0..index.saturating_sub(1)).rev() {
                let reason = self.map.chain_reason(package, i as usize);
                self.force_prefix_assignment(package, i, false, reason, level);
            }
            let count = self.map.package_prefix_count(package) as u32;
            for i in (index + 1)..count {
                let reason = self.map.chain_reason(package, (i - 1) as usize);
                self.force_prefix_assignment(package, i, true, reason, level);
            }
        }

        Ok(())
    }

    /// Forces an explicit prefix assignment onto the trail even if the value
    /// is already derived. Used for selection anchors: explicit prefix
    /// entries are usable as unique implication points by conflict analysis,
    /// which derived values are not.
    pub(crate) fn force_prefix_assignment(
        &mut self,
        package: u32,
        index: u32,
        value: bool,
        reason: ClauseId,
        level: u32,
    ) {
        let p = self.map.package_prefix(package, index as usize);
        if self.map.explicit_value(p).is_some() {
            return;
        }
        debug_assert_ne!(self.map.value(p), Some(!value), "inconsistent anchor");
        self.map.set(p, value, level);
        self.map
            .apply_prefix_assignment(package, index, value, p, level, self.stack.len());
        self.stack.push(Decision::new(p, value, reason));
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
                self.map.undo_prefix_assignment(package, self.stack.len());
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
