//! Assertions are clauses that consist of a single literal, e.g. `¬A1` for
//! a candidate that may never be selected. Watches need two literals, so
//! an assertion has no watches and propagation never wakes it on its own:
//! its decision must already hold whenever propagation runs. The solver
//! keeps two append-only lists of such clauses: `negative_assertions`
//! (produced by the encoder) and the single-literal entries of
//! `learnt_clause_ids` (produced by conflict analysis).
//!
//! Re-applying every entry of both lists at the start of every propagation
//! round would keep that invariant, but almost every visit would be a
//! no-op: the variable is usually already decided, propagation runs at
//! least once per decision and once per conflict, and hard problems learn
//! tens of thousands of clauses. So instead we only visit the entries for
//! which something could have changed. Between two propagation rounds that
//! is exactly:
//!
//! 1. the entries appended since the last scan, and
//! 2. the entries whose satisfying assignment was popped by backtracking.
//!
//! The first set is cheap to find: the lists are append-only, so a cursor
//! per list marks how far we have scanned, and everything beyond it is
//! new. While scanning new learnt entries we also drop every multi-literal
//! clause once and for all (watches handle those, and a learnt clause
//! never changes after allocation); only the single-literal entries, which
//! are rare, are kept for re-inspection in `learnt_single`.
//!
//! For the second set we look at the trail: the chronological assignment
//! log (the `DecisionTracker` stack). Assignments are pushed onto it and
//! backtracking pops a suffix, so a truncation to length `n` pops exactly
//! the assignments at positions `>= n`. Rather than remembering a position
//! per entry we keep a single bound per list, `verified_up_to`: every
//! scanned entry holds its asserted value through an assignment at or
//! below that position. The decision tracker remembers the lowest trail
//! length reached since our previous scan, the floor
//! (`DecisionTracker::take_assert_floor`):
//!
//! ```text
//! backing list  [ a b c d e | f g ]
//!                           ^ cursor: f and g are new
//!
//! trail         [ .  .  .  .  .  .  .  . ]   (one assignment per slot)
//!                       ^           ^
//!                       floor       verified_up_to
//! ```
//!
//! If the floor stayed above `verified_up_to`, no truncation can have
//! popped a verifying assignment and the entire list is skipped. Otherwise
//! (the situation drawn above) some assignment may be gone, and the list
//! is marked dirty: the next scan re-applies every scanned single-literal
//! entry, in index order, before processing the new tail. Every visit runs
//! the ordinary per-entry code and then records the current trail top as
//! the new bound — the trail only grows within a round, so the position of
//! the last visit bounds all of them. A conflict aborts the scan with the
//! dirty flag still set, so the remaining entries are re-checked in the
//! next round.
//!
//! Note that none of this changes anything observable. A skipped entry
//! still holds its asserted value, so visiting it would just return
//! `Ok(false)` from `try_add_decision`: no decision, no level change, no
//! trace output, no conflict. And because dirty entries are re-applied in
//! index order before the tail, visits happen in the same order as a full
//! scan, so decisions, levels, traces, and which conflict surfaces first
//! are all unchanged.

/// Incremental scan state for one assertion list.
#[derive(Default)]
pub(crate) struct ScanState {
    /// Entries `< cursor` of the backing list have been inspected.
    cursor: usize,
    /// Upper bound on the trail positions of the assignments satisfying
    /// the inspected entries.
    verified_up_to: usize,
    /// A verified assignment may have been popped: re-verify every
    /// inspected entry. Sticky until a rescan completes without a
    /// conflict.
    dirty: bool,
}

impl ScanState {
    /// Mark the list dirty when a truncation since the previous call
    /// reached `verified_up_to`. `floor` is the lowest trail length
    /// observed in between ([`DecisionTracker::take_assert_floor`]).
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

    /// Record an entry verified through the assignment at (or below)
    /// trail position `position`. Positions are non-decreasing within a
    /// scan, so the last one bounds every inspected entry.
    #[inline]
    pub(crate) fn record(&mut self, position: usize) {
        self.verified_up_to = position;
    }
}

/// The scan state of both assertion lists, in scan order.
#[derive(Default)]
pub(crate) struct AssertionScans {
    /// `Solver::decide_assertions` (`SolverState::negative_assertions`).
    /// Every entry is single-literal; rescans walk `0..cursor` directly.
    pub(crate) negative: ScanState,
    /// `Solver::decide_learned` (`SolverState::learnt_clause_ids`).
    pub(crate) learnt: ScanState,
    /// Indices (ascending) of the single-literal entries of
    /// `learnt_clause_ids`: the only learnt entries a rescan visits.
    pub(crate) learnt_single: Vec<u32>,
}

impl AssertionScans {
    /// Sync both lists with the backtracking since the previous round.
    pub(crate) fn sync(&mut self, floor: usize) {
        self.negative.sync(floor);
        self.learnt.sync(floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_only_when_the_floor_reaches_the_verified_bound() {
        let mut list = ScanState::default();

        // A fresh list is conservatively dirty against the initial floor
        // of zero; the (empty) rescan completes immediately.
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

        // A truncation at or below the verified bound forces a rescan, and the
        // flag is sticky until a rescan completes.
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
