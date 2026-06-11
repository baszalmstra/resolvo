//! Incremental tracking for the per-propagate assertion scans.
//!
//! An assertion (a single-literal clause) has no watches, so its decision
//! must hold whenever propagation runs: `Solver::propagate` historically
//! re-scanned all of `negative_assertions`, `unit_learnt_clause_ids` and
//! `env_clause_ids` on every call, even though almost every entry is a
//! no-op (`try_add_decision` on an already-decided variable). Between two
//! propagate calls only two things can make an entry actionable again:
//!
//! 1. the entry was appended since the last scan (all three lists are
//!    append-only), or
//! 2. the assignment that satisfied the entry was popped by backtracking.
//!
//! This structure tracks exactly those two sets. (1) is a per-list cursor.
//! (2) is detected through [`DecisionTracker::take_assert_floor`]: every
//! verified entry records the trail position of the assignment that
//! satisfies it (or a conservative upper bound, see
//! [`AssertionWatermark::verify`]), and a truncation of the trail below
//! that position moves the entry back into a per-list pending set. A
//! pending or unscanned entry is processed by the original per-entry code,
//! so the scan stays byte-for-byte the original on every entry it visits;
//! the entries it skips are exactly those whose `try_add_decision` would
//! be a no-op.
//!
//! [`DecisionTracker::take_assert_floor`]:
//! crate::solver::decision_tracker::DecisionTracker::take_assert_floor

use std::collections::{BTreeSet, BinaryHeap};

/// The number of assertion lists tracked, in scan order: negative
/// assertions, learnt clauses, env clauses.
pub(crate) const GROUP_COUNT: usize = 3;
/// `Solver::decide_assertions` (`SolverState::negative_assertions`).
pub(crate) const GROUP_NEGATIVE: usize = 0;
/// `Solver::decide_learned` (`SolverState::unit_learnt_clause_ids`,
/// already filtered to single-literal clauses at registration).
pub(crate) const GROUP_LEARNT: usize = 1;
/// `Solver::decide_env_assertions` (`SolverState::env_clause_ids`).
pub(crate) const GROUP_ENV: usize = 2;

/// One record per currently verified entry: the trail position at (or
/// above) the assignment that satisfies the entry, plus the entry's list
/// and index. Ordered by position so a [`BinaryHeap`] pops the records
/// invalidated by a trail truncation first.
type AppliedRecord = (usize, u8, u32);

#[derive(Default)]
pub(crate) struct AssertionWatermark {
    /// Entries `< cursor[g]` of list `g` have been inspected at least
    /// once. Each such entry is multi-literal (permanently skipped: learnt
    /// and env clause literal lists never change after allocation, and
    /// multi-literal clauses are handled by watches), currently verified
    /// (tracked by a record in `applied`), or pending re-verification in
    /// `pending[g]`.
    cursor: [usize; GROUP_COUNT],
    /// Indices below the cursor whose verifying assignment may have been
    /// popped by backtracking. Kept ordered: the original scan order
    /// within a list is ascending index, and every pending index is below
    /// the cursor, so scanning `pending[g]` first and the cursor tail
    /// second visits entries in exactly the original order.
    pending: [BTreeSet<usize>; GROUP_COUNT],
    /// Max-heap of [`AppliedRecord`]s, one per verified entry.
    applied: BinaryHeap<AppliedRecord>,
}

impl AssertionWatermark {
    /// Move every verified entry whose recorded trail position is at or
    /// beyond `floor` back into its pending set.
    ///
    /// `floor` is the lowest trail length observed since the previous
    /// sync ([`DecisionTracker::take_assert_floor`]): a truncation to
    /// length `n` pops exactly the assignments at positions `>= n`, so an
    /// assignment recorded at position `p` survived all truncations since
    /// the previous sync if and only if `p < floor`.
    ///
    /// [`DecisionTracker::take_assert_floor`]:
    /// crate::solver::decision_tracker::DecisionTracker::take_assert_floor
    pub(crate) fn sync(&mut self, floor: usize) {
        while let Some(&(position, group, index)) = self.applied.peek() {
            if position < floor {
                break;
            }
            self.applied.pop();
            self.pending[group as usize].insert(index as usize);
        }
    }

    /// The lowest pending index of `group`, if any. Pending entries are
    /// re-verified before the unscanned tail: their indices are always
    /// below the cursor.
    pub(crate) fn first_pending(&self, group: usize) -> Option<usize> {
        self.pending[group].first().copied()
    }

    /// The next never-scanned index of `group`, given the current length
    /// of the backing list (which only ever grows).
    pub(crate) fn next_unscanned(&self, group: usize, len: usize) -> Option<usize> {
        debug_assert!(self.cursor[group] <= len, "the assertion lists only grow");
        (self.cursor[group] < len).then_some(self.cursor[group])
    }

    /// Record that the pending entry `index` of `group` was re-verified:
    /// its variable holds the asserted value through the assignment at
    /// (or below) trail position `position`.
    pub(crate) fn verify_pending(&mut self, group: usize, index: usize, position: usize) {
        let removed = self.pending[group].remove(&index);
        debug_assert!(removed, "only pending entries are re-verified");
        self.applied.push((position, group as u8, index as u32));
    }

    /// Record the inspection of the unscanned entry at the cursor of
    /// `group` and advance the cursor. `position` is the verifying trail
    /// position as for [`Self::verify_pending`], or `None` for a
    /// multi-literal entry, which is never inspected again.
    pub(crate) fn verify_unscanned(&mut self, group: usize, position: Option<usize>) {
        if let Some(position) = position {
            self.applied
                .push((position, group as u8, self.cursor[group] as u32));
        }
        self.cursor[group] += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_invalidates_only_records_at_or_beyond_the_floor() {
        let mut watermark = AssertionWatermark::default();

        // Scan three new entries: two verified, one multi-literal.
        assert_eq!(watermark.next_unscanned(GROUP_LEARNT, 3), Some(0));
        watermark.verify_unscanned(GROUP_LEARNT, Some(10));
        assert_eq!(watermark.next_unscanned(GROUP_LEARNT, 3), Some(1));
        watermark.verify_unscanned(GROUP_LEARNT, None);
        assert_eq!(watermark.next_unscanned(GROUP_LEARNT, 3), Some(2));
        watermark.verify_unscanned(GROUP_LEARNT, Some(25));
        assert_eq!(watermark.next_unscanned(GROUP_LEARNT, 3), None);

        // A truncation to length 20 pops the assignment at position 25 but
        // not the one at position 10; the multi-literal entry never comes
        // back.
        watermark.sync(20);
        assert_eq!(watermark.first_pending(GROUP_LEARNT), Some(2));
        watermark.verify_pending(GROUP_LEARNT, 2, 21);
        assert_eq!(watermark.first_pending(GROUP_LEARNT), None);

        // No truncation: nothing is invalidated.
        watermark.sync(usize::MAX);
        assert_eq!(watermark.first_pending(GROUP_LEARNT), None);

        // A truncation to zero invalidates every verified entry, in every
        // group, but never the multi-literal one.
        assert_eq!(watermark.next_unscanned(GROUP_NEGATIVE, 1), Some(0));
        watermark.verify_unscanned(GROUP_NEGATIVE, Some(3));
        watermark.sync(0);
        assert_eq!(watermark.first_pending(GROUP_NEGATIVE), Some(0));
        assert_eq!(watermark.first_pending(GROUP_LEARNT), Some(0));
        watermark.verify_pending(GROUP_NEGATIVE, 0, 1);
        watermark.verify_pending(GROUP_LEARNT, 0, 2);
        assert_eq!(watermark.first_pending(GROUP_LEARNT), Some(2));
        watermark.verify_pending(GROUP_LEARNT, 2, 4);
        assert_eq!(watermark.first_pending(GROUP_LEARNT), None);
        assert_eq!(watermark.next_unscanned(GROUP_LEARNT, 3), None);
    }
}
