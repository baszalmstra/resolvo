//! Incremental scan state for the per-propagate assertion lists.
//!
//! Assertions are clauses that consist of a single literal, e.g. `¬A1` for
//! a candidate that may never be selected. Watches need two literals, so an
//! assertion has no watches and propagation never wakes it on its own: its
//! decision must already hold whenever propagation runs. The solver keeps
//! three append-only lists of such clauses: `negative_assertions` and the
//! single-literal entries of `unit_learnt_clause_ids` (both produced by the
//! encoder / conflict analysis), and the single-literal entries of
//! `env_clause_ids` (the environment model and blocking clauses of a
//! universal solve).
//!
//! Re-applying every entry of every list at the start of every propagation
//! round would keep that invariant, but almost every visit would be a no-op:
//! the variable is usually already decided, propagation runs at least once
//! per decision and once per conflict, and hard problems learn tens of
//! thousands of clauses. So instead we only visit the entries for which
//! something could have changed. Between two propagation rounds that is
//! exactly:
//!
//! 1. the entries appended since the last scan, and
//! 2. the entries whose satisfying assignment was popped by backtracking.
//!
//! The first set is cheap to find: the lists are append-only, so a `cursor`
//! per list marks how far we have scanned, and everything beyond it is new.
//! `negative_assertions` and `unit_learnt_clause_ids` are single-literal by
//! construction, so a rescan walks `0..cursor` directly. `env_clause_ids`
//! holds the multi-literal model/blocking clauses too (handled by watches),
//! so its single-literal entries are collected in `env_single` and only
//! those are re-inspected.
//!
//! For the second set we look at the trail. A truncation to length `n` pops
//! exactly the assignments at positions `>= n`. Rather than remembering a
//! position per entry we keep a single bound per list, `verified_up_to`:
//! every scanned entry holds its asserted value through an assignment at or
//! below that position (positions are non-decreasing within a scan, so the
//! last visit bounds them all). The decision tracker remembers the lowest
//! trail length reached since our previous scan, the floor
//! ([`DecisionTracker::take_assert_floor`]). If the floor stayed above
//! `verified_up_to`, no truncation can have popped a verifying assignment
//! and the whole list is skipped with one integer comparison. Otherwise the
//! list is marked `dirty`: the next scan re-applies every inspected
//! single-literal entry, in index order, before processing the new tail.
//! The flag is sticky until a rescan completes without a conflict, so a
//! conflict mid-rescan leaves the remaining entries pending, exactly like
//! the historical early return.
//!
//! Every visit runs the unchanged per-entry code (`apply_negative_assertion`
//! / `apply_learnt_assertion` / `apply_env_assertion`), so a skipped scan
//! consists solely of visits on which `try_add_decision` would have returned
//! `Ok(false)`, and a rescan reproduces the historical full scan exactly.
//!
//! [`DecisionTracker::take_assert_floor`]:
//! crate::solver::decision_tracker::DecisionTracker::take_assert_floor

/// Incremental scan state for one assertion list.
#[derive(Default)]
pub(crate) struct ScanState {
    /// Entries `< cursor` of the backing list have been inspected.
    cursor: usize,
    /// Upper bound on the trail positions of the assignments satisfying the
    /// inspected entries.
    verified_up_to: usize,
    /// A verified assignment may have been popped: re-verify every inspected
    /// entry. Sticky until a rescan completes without a conflict.
    dirty: bool,
}

impl ScanState {
    /// Mark the list dirty when a truncation since the previous call reached
    /// `verified_up_to`. `floor` is the lowest trail length observed in
    /// between ([`DecisionTracker::take_assert_floor`]).
    ///
    /// [`DecisionTracker::take_assert_floor`]:
    /// crate::solver::decision_tracker::DecisionTracker::take_assert_floor
    #[inline]
    pub(crate) fn sync(&mut self, floor: usize) {
        if self.verified_up_to >= floor {
            self.dirty = true;
        }
    }

    /// Whether the inspected entries must be re-verified.
    #[inline]
    pub(crate) fn needs_rescan(&self) -> bool {
        self.dirty
    }

    /// Record a completed, conflict-free rescan.
    #[inline]
    pub(crate) fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// The number of entries inspected so far.
    #[inline]
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// Advance past the entry at the cursor.
    #[inline]
    pub(crate) fn advance(&mut self) {
        self.cursor += 1;
    }

    /// Record an entry verified through the assignment at (or below) trail
    /// position `position`. Positions are non-decreasing within a scan, so
    /// the last one bounds every inspected entry.
    #[inline]
    pub(crate) fn record(&mut self, position: usize) {
        self.verified_up_to = position;
    }
}

/// The scan state of all assertion lists, in scan order.
#[derive(Default)]
pub(crate) struct AssertionScans {
    /// `Solver::decide_assertions` (`SolverState::negative_assertions`).
    /// Every entry is single-literal; rescans walk `0..cursor` directly.
    pub(crate) negative: ScanState,
    /// `Solver::decide_learned` (`SolverState::unit_learnt_clause_ids`,
    /// already filtered to single-literal clauses at registration); rescans
    /// walk `0..cursor` directly.
    pub(crate) learnt: ScanState,
    /// `Solver::decide_env_assertions` (`SolverState::env_clause_ids`, which
    /// also holds the multi-literal model/blocking clauses).
    pub(crate) env: ScanState,
    /// Indices (ascending) of the single-literal entries of `env_clause_ids`:
    /// the only env entries a rescan visits.
    pub(crate) env_single: Vec<u32>,
}

impl AssertionScans {
    /// Sync every list with the backtracking since the previous round.
    pub(crate) fn sync(&mut self, floor: usize) {
        self.negative.sync(floor);
        self.learnt.sync(floor);
        self.env.sync(floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_only_when_the_floor_reaches_the_verified_bound() {
        let mut list = ScanState::default();
        // A fresh list is conservatively dirty against the initial floor of
        // zero; the (empty) rescan completes immediately.
        list.sync(0);
        assert!(list.needs_rescan());
        list.mark_clean();
        // Verify three entries at positions 4, 9, 25.
        for position in [4, 9, 25] {
            list.record(position);
            list.advance();
        }
        assert_eq!(list.cursor(), 3);
        // No truncation: nothing to re-verify.
        list.sync(usize::MAX);
        assert!(!list.needs_rescan());
        // A truncation that stayed above the verified bound proves all
        // verified assignments survived.
        list.sync(26);
        assert!(!list.needs_rescan());
        // A truncation at or below the verified bound forces a rescan, and
        // the flag is sticky until a rescan completes.
        list.sync(25);
        assert!(list.needs_rescan());
        list.sync(usize::MAX);
        assert!(list.needs_rescan());
        // A completed rescan at a lower trail top tightens the bound.
        list.record(7);
        list.mark_clean();
        list.sync(8);
        assert!(!list.needs_rescan());
        list.sync(7);
        assert!(list.needs_rescan());
    }
}
