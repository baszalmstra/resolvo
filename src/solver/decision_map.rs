use std::cmp::Ordering;

use crate::VariableId;
use crate::id::DenseIndex;

/// Level values must leave the top magnitude bit free for
/// [`GROUP_FALSIFIED_BIT`].
const MAX_LEVEL: u32 = (1 << 30) - 1;

/// Set in the magnitude of a negative [`DecisionAndLevel`] when the variable
/// was falsified by its at-most-one group's choice rather than by a clause:
/// the assignment lives only in the map (no trail entry), is swept away when
/// the choice is undone, and its reason is the virtual pairwise clause
/// against the chosen member.
const GROUP_FALSIFIED_BIT: u32 = 1 << 30;

/// Represents a decision (i.e. an assignment to a variable) and the level at
/// which it was made
///
/// `= 0`: undecided
/// `> 0`: level of decision when the variable is set to true
/// `< 0`: level of decision when the variable is set to false; the magnitude
///        additionally carries [`GROUP_FALSIFIED_BIT`] for group-falsified
///        assignments
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq)]
struct DecisionAndLevel(i32);

impl DecisionAndLevel {
    fn undecided() -> DecisionAndLevel {
        DecisionAndLevel(0)
    }

    #[inline(always)]
    fn value(self) -> Option<bool> {
        match self.0.cmp(&0) {
            Ordering::Less => Some(false),
            Ordering::Equal => None,
            Ordering::Greater => Some(true),
        }
    }

    #[inline]
    fn level(self) -> u32 {
        self.0.unsigned_abs() & !GROUP_FALSIFIED_BIT
    }

    fn with_value_and_level(value: bool, level: u32) -> Self {
        debug_assert!(level <= MAX_LEVEL, "level is too large");
        Self(if value { level as i32 } else { -(level as i32) })
    }

    /// A false assignment made by the variable's group choosing another
    /// member.
    fn group_falsified(level: u32) -> Self {
        debug_assert!(level <= MAX_LEVEL, "level is too large");
        Self(-((level | GROUP_FALSIFIED_BIT) as i32))
    }

    #[inline]
    fn is_group_falsified(self) -> bool {
        self.0 < 0 && (self.0.unsigned_abs() & GROUP_FALSIFIED_BIT) != 0
    }
}

/// Identifies a set of variables of which at most one can be assigned true
/// (the candidates of a single package). See [`DecisionMap`] for how groups
/// participate in variable assignment.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub(crate) struct AmoGroupId(u32);

impl AmoGroupId {
    pub fn to_index(self) -> usize {
        self.0 as usize
    }
}

/// Marker in [`DecisionMap::variable_group`] for variables without a group.
const NO_GROUP: u32 = u32::MAX;

struct AmoGroup {
    /// The member that is currently assigned true, if any. Maintained by
    /// [`DecisionMap::set`] and [`DecisionMap::reset`].
    ///
    /// Exact: `chosen == Some(v)` implies `v` is assigned true, and choosing
    /// a member eagerly falsifies every unassigned sibling in the same step,
    /// so a second member can never be assigned true while a choice stands.
    chosen: Option<VariableId>,

    /// All member variables, in registration order.
    members: Vec<VariableId>,

    /// Bumped whenever this group's choice is set or cleared: the only events
    /// that change member values without a trail entry per member. Consumers
    /// that cache the result of scanning this group's member values (see
    /// [`crate::solver::decide_queue`]) validate against this instead of
    /// being invalidated per falsified member.
    revision: u64,
}

/// The result of [`DecisionMap::add_amo_member`].
pub(crate) enum AmoMemberAdded {
    /// The variable was already a member of the group.
    AlreadyMember,
    /// The variable joined the group and no assignment changed.
    Added,
    /// The variable joined a group whose choice is `chosen`, and has been
    /// falsified by it (without a trail entry). Clauses already watching the
    /// variable's positive literal have not been woken.
    AddedFalsified {
        #[allow(dead_code)]
        chosen: VariableId,
    },
    /// The variable is assigned true and became the group's choice.
    /// `falsified_others` is true when the group has other members, which
    /// have been falsified (without trail entries) and whose watchers have
    /// not been woken.
    AddedBecameChoice { falsified_others: bool },
    /// The variable is assigned true, but the group had already chosen
    /// `chosen` (which is also assigned true): the at-most-one invariant is
    /// violated and the caller must report a conflict.
    Conflict { chosen: VariableId },
}

/// A map of the assignments to solvables.
///
/// Besides clause-driven assignments this map also enforces *package-level*
/// at-most-one constraints natively: the candidates of a package form a
/// group, and assigning one member true eagerly falsifies every unassigned
/// sibling *in the map only* — one marked entry write per sibling, no trail
/// entry, no reason clause, no watcher routing. Undoing the choice sweeps the
/// marked entries away again. This replaces the materialized
/// (¬candidate ∨ ¬sibling) clauses of the binary at-most-one encoding: the
/// trail, the backtracking work, and conflict analysis stay O(1) per version
/// selection, while evaluating a falsified sibling stays a single load.
///
/// Assignments and group memberships live in *separate* arrays: assignments
/// are search state that is truncated wholesale on a restart and whose reads
/// dominate propagation (so entries stay 4 bytes), while memberships are part
/// of the encoded problem and survive restarts.
#[derive(Default)]
pub(crate) struct DecisionMap {
    /// The assignment of each variable. Indexes past the end are undecided;
    /// a restart truncates the vector instead of rewriting it.
    decisions: Vec<DecisionAndLevel>,

    /// The at-most-one group each variable belongs to (`NO_GROUP` for none).
    /// Grows monotonically; never truncated.
    variable_group: Vec<u32>,

    /// All at-most-one groups, indexed by [`AmoGroupId`].
    groups: Vec<AmoGroup>,

    /// Bumped whenever *any* group's choice is set or cleared, for consumers
    /// whose cached scan spans multiple groups. Prefer the per-group
    /// revision where the scanned candidates all belong to one group, so an
    /// unrelated package decision does not invalidate the cache.
    amo_revision: u64,
}

impl DecisionMap {
    #[cfg(feature = "diagnostics")]
    pub fn len(&self) -> usize {
        self.decisions.len().max(self.variable_group.len())
    }

    /// The group of the variable at `index`, or `NO_GROUP`.
    #[inline(always)]
    fn group_of_index(&self, index: usize) -> u32 {
        self.variable_group.get(index).copied().unwrap_or(NO_GROUP)
    }

    /// Mutable access to the decision slot for `index`, growing the vector as
    /// needed (a restart truncates it; group falsification must be able to
    /// write past the end again).
    #[inline]
    fn decision_mut(&mut self, index: usize) -> &mut DecisionAndLevel {
        if index >= self.decisions.len() {
            self.decisions
                .resize_with(index + 1, DecisionAndLevel::undecided);
        }
        // SAFE: resized above to contain `index`.
        unsafe { self.decisions.get_unchecked_mut(index) }
    }

    #[inline]
    pub fn reset(&mut self, variable_id: VariableId) {
        let index = variable_id.to_index();
        if index >= self.decisions.len() {
            return;
        }
        let group = self.group_of_index(index);
        if group == NO_GROUP {
            self.decisions[index] = DecisionAndLevel::undecided();
            return;
        }

        let group_index = group as usize;
        if self.groups[group_index].chosen == Some(variable_id) {
            // Undoing the choice sweeps away every member it falsified.
            self.decisions[index] = DecisionAndLevel::undecided();
            let members = std::mem::take(&mut self.groups[group_index].members);
            for &member in &members {
                let decision = self.decision_mut(member.to_index());
                if decision.is_group_falsified() {
                    *decision = DecisionAndLevel::undecided();
                }
            }
            let group = &mut self.groups[group_index];
            group.members = members;
            group.chosen = None;
            group.revision += 1;
            self.amo_revision += 1;
        } else if let Some(chosen) = self.groups[group_index].chosen {
            // A clause-assigned member reverts while the group's choice still
            // stands: it stays false, but now through the choice, at the
            // choice's level. Without this a popped member would read as
            // undecided and could be assigned true next to the chosen one.
            let level = self.decisions[chosen.to_index()].level();
            self.decisions[index] = DecisionAndLevel::group_falsified(level);
        } else {
            self.decisions[index] = DecisionAndLevel::undecided();
        }
    }

    #[inline]
    pub fn set(&mut self, variable_id: VariableId, value: bool, level: u32) {
        let index = variable_id.to_index();
        let decision = self.decision_mut(index);
        debug_assert!(
            !decision.is_group_falsified(),
            "a group-falsified member must fail its decision before reaching set()"
        );
        *decision = DecisionAndLevel::with_value_and_level(value, level);

        if value {
            let group = self.group_of_index(index);
            if group != NO_GROUP {
                self.choose(AmoGroupId(group), variable_id, level);
            }
        }
    }

    /// Records `variable_id` as its group's choice and eagerly falsifies
    /// every unassigned sibling in the map (no trail entries).
    fn choose(&mut self, group: AmoGroupId, variable_id: VariableId, level: u32) {
        let group_index = group.to_index();
        debug_assert!(
            self.groups[group_index].chosen.is_none()
                || self.groups[group_index].chosen == Some(variable_id),
            "a second member of an at-most-one group was assigned true"
        );
        let members = std::mem::take(&mut self.groups[group_index].members);
        for &member in &members {
            if member == variable_id {
                continue;
            }
            let decision = self.decision_mut(member.to_index());
            if decision.value().is_none() {
                *decision = DecisionAndLevel::group_falsified(level);
            }
        }
        let group = &mut self.groups[group_index];
        group.members = members;
        group.chosen = Some(variable_id);
        group.revision += 1;
        self.amo_revision += 1;
    }

    /// The revision a cached scan over candidate values must validate
    /// against: the group's own revision when every scanned candidate that
    /// belongs to a group belongs to `group`, or the map-wide revision when
    /// the scan spans multiple groups (`None`). See [`Self::amo_revision`].
    #[inline]
    pub fn amo_revision(&self, group: Option<AmoGroupId>) -> u64 {
        match group {
            Some(group) => self.groups[group.to_index()].revision,
            None => self.amo_revision,
        }
    }

    /// Clears all assignments (clause-driven and group-driven) but retains
    /// the group definitions, which are part of the encoded problem rather
    /// than the search state.
    pub fn clear_assignments(&mut self) {
        self.decisions.clear();
        for group in &mut self.groups {
            group.chosen = None;
            group.revision += 1;
        }
        self.amo_revision += 1;
    }

    #[inline]
    pub fn level(&self, variable_id: VariableId) -> u32 {
        self.decisions
            .get(variable_id.to_index())
            .map_or(0, |d| d.level())
    }

    #[inline(always)]
    pub fn value(&self, variable_id: VariableId) -> Option<bool> {
        self.decisions
            .get(variable_id.to_index())
            .and_then(|d| d.value())
    }

    /// Returns the chosen member of the variable's group if, and only if, the
    /// variable is false *because of* that choice (rather than through a
    /// clause with its own trail entry). The chosen member acts as the
    /// variable's reason through the virtual clause (¬chosen ∨ ¬variable).
    #[inline]
    pub fn group_falsified_by(&self, variable_id: VariableId) -> Option<VariableId> {
        let index = variable_id.to_index();
        let &decision = self.decisions.get(index)?;
        if !decision.is_group_falsified() {
            return None;
        }
        let group = self.group_of_index(index);
        let chosen = self.groups[group as usize]
            .chosen
            .expect("a group-falsified member implies a standing choice");
        debug_assert_ne!(chosen, variable_id);
        Some(chosen)
    }

    /// Allocates a new, empty at-most-one group.
    pub fn alloc_amo_group(&mut self) -> AmoGroupId {
        let id = u32::try_from(self.groups.len()).expect("too many groups");
        assert!(id != NO_GROUP, "too many at-most-one groups");
        self.groups.push(AmoGroup {
            chosen: None,
            members: Vec::new(),
            revision: 0,
        });
        AmoGroupId(id)
    }

    /// Returns the group the variable is a member of, if any.
    #[inline]
    pub fn amo_group_of(&self, variable_id: VariableId) -> Option<AmoGroupId> {
        match self.group_of_index(variable_id.to_index()) {
            NO_GROUP => None,
            group => Some(AmoGroupId(group)),
        }
    }

    /// The member of the group that is currently assigned true, if any.
    #[inline]
    pub fn amo_group_chosen(&self, group: AmoGroupId) -> Option<VariableId> {
        self.groups[group.to_index()].chosen
    }

    /// The members of the group in registration order.
    pub fn amo_group_members(&self, group: AmoGroupId) -> &[VariableId] {
        &self.groups[group.to_index()].members
    }

    /// The number of members of the group.
    #[inline]
    pub fn amo_group_member_count(&self, group: AmoGroupId) -> usize {
        self.groups[group.to_index()].members.len()
    }

    /// The `index`-th member of the group.
    #[inline]
    pub fn amo_group_member(&self, group: AmoGroupId, index: usize) -> VariableId {
        self.groups[group.to_index()].members[index]
    }

    /// Adds a variable to an at-most-one group, taking the variable's current
    /// assignment into account. See [`AmoMemberAdded`] for the possible
    /// outcomes; every outcome except [`AmoMemberAdded::AlreadyMember`] adds
    /// the membership (including [`AmoMemberAdded::Conflict`], because the
    /// group must stay consistent after the caller reports the conflict and
    /// the solver backtracks).
    pub fn add_amo_member(&mut self, group: AmoGroupId, variable: VariableId) -> AmoMemberAdded {
        let index = variable.to_index();
        let existing = self.group_of_index(index);
        if existing == group.0 {
            return AmoMemberAdded::AlreadyMember;
        }
        debug_assert!(
            existing == NO_GROUP,
            "a variable can only be a member of a single at-most-one group"
        );

        if index >= self.variable_group.len() {
            self.variable_group.resize(index + 1, NO_GROUP);
        }
        self.variable_group[index] = group.0;

        let value = self
            .decisions
            .get(index)
            .and_then(|decision| decision.value());
        self.groups[group.to_index()].members.push(variable);

        match (value, self.groups[group.to_index()].chosen) {
            (Some(true), Some(chosen)) => AmoMemberAdded::Conflict { chosen },
            (Some(true), None) => {
                let level = self.decisions[index].level();
                let falsified_others = self.groups[group.to_index()].members.len() > 1;
                self.choose(group, variable, level);
                AmoMemberAdded::AddedBecameChoice { falsified_others }
            }
            (None, Some(chosen)) => {
                // The group's choice predates the membership: falsify the new
                // member exactly as `choose` would have.
                let level = self.decisions[chosen.to_index()].level();
                *self.decision_mut(index) = DecisionAndLevel::group_falsified(level);
                AmoMemberAdded::AddedFalsified { chosen }
            }
            // An already false member needs no falsification: clauses
            // watching it were woken when it was assigned.
            _ => AmoMemberAdded::Added,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn var(index: usize) -> VariableId {
        VariableId::from_index(index)
    }

    fn group_with_members(map: &mut DecisionMap, count: usize) -> AmoGroupId {
        let group = map.alloc_amo_group();
        for i in 1..=count {
            assert!(matches!(
                map.add_amo_member(group, var(i)),
                AmoMemberAdded::Added
            ));
        }
        group
    }

    #[test]
    fn group_choice_falsifies_siblings() {
        let mut map = DecisionMap::default();
        let group = group_with_members(&mut map, 3);

        // Nothing is decided yet.
        assert_eq!(map.value(var(1)), None);
        assert_eq!(map.group_falsified_by(var(2)), None);

        // Choosing one member falsifies the others, at the choice's level,
        // without their own reasons.
        map.set(var(1), true, 3);
        assert_eq!(map.value(var(1)), Some(true));
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(map.value(var(3)), Some(false));
        assert_eq!(map.group_falsified_by(var(2)), Some(var(1)));
        assert_eq!(map.level(var(2)), 3);
        assert_eq!(map.amo_group_chosen(group), Some(var(1)));

        // Undoing the choice sweeps the falsifications away.
        map.reset(var(1));
        assert_eq!(map.value(var(1)), None);
        assert_eq!(map.value(var(2)), None);
        assert_eq!(map.group_falsified_by(var(2)), None);
        assert_eq!(map.level(var(2)), 0);
        assert_eq!(map.amo_group_chosen(group), None);
        assert_eq!(map.amo_group_members(group).len(), 3);
    }

    #[test]
    fn clause_assignments_take_precedence() {
        let mut map = DecisionMap::default();
        group_with_members(&mut map, 2);

        map.set(var(2), false, 1);
        map.set(var(1), true, 2);

        // A clause-assigned member keeps its own level and reason.
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(map.group_falsified_by(var(2)), None);
        assert_eq!(map.level(var(2)), 1);

        // Undoing the choice leaves the clause assignment alone.
        map.reset(var(1));
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(map.level(var(2)), 1);
    }

    #[test]
    fn popped_members_fall_back_to_the_standing_choice() {
        let mut map = DecisionMap::default();
        group_with_members(&mut map, 2);

        // var(2) is falsified by a clause first, then var(1) is chosen.
        map.set(var(2), false, 2);
        map.set(var(1), true, 3);
        assert_eq!(map.group_falsified_by(var(2)), None);

        // Undoing the clause assignment while the choice still stands must
        // leave the member false through the choice: otherwise it could be
        // assigned true next to the chosen member.
        map.reset(var(2));
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(map.group_falsified_by(var(2)), Some(var(1)));
        assert_eq!(map.level(var(2)), 3);

        // Undoing the choice then sweeps it away like any other member.
        map.reset(var(1));
        assert_eq!(map.value(var(2)), None);
    }

    #[test]
    fn late_members_observe_the_choice() {
        let mut map = DecisionMap::default();
        let group = group_with_members(&mut map, 1);
        map.set(var(1), true, 1);

        // An unassigned candidate registered after the choice is immediately
        // falsified by it.
        assert!(matches!(
            map.add_amo_member(group, var(2)),
            AmoMemberAdded::AddedFalsified { chosen } if chosen == var(1)
        ));
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(map.group_falsified_by(var(2)), Some(var(1)));

        // A candidate that is already true conflicts with the choice.
        map.set(var(3), true, 1);
        assert!(matches!(
            map.add_amo_member(group, var(3)),
            AmoMemberAdded::Conflict { chosen } if chosen == var(1)
        ));

        assert!(matches!(
            map.add_amo_member(group, var(2)),
            AmoMemberAdded::AlreadyMember
        ));
    }

    #[test]
    fn already_true_member_becomes_the_choice() {
        let mut map = DecisionMap::default();
        let group = group_with_members(&mut map, 1);

        let global_revision = map.amo_revision(None);
        let group_revision = map.amo_revision(Some(group));
        map.set(var(2), true, 4);
        assert!(matches!(
            map.add_amo_member(group, var(2)),
            AmoMemberAdded::AddedBecameChoice {
                falsified_others: true
            }
        ));
        assert_eq!(map.value(var(1)), Some(false));
        assert_eq!(map.level(var(1)), 4);
        assert_ne!(
            map.amo_revision(None),
            global_revision,
            "value scans must revalidate"
        );
        assert_ne!(
            map.amo_revision(Some(group)),
            group_revision,
            "group-stamped value scans must revalidate"
        );
    }

    #[test]
    fn clearing_assignments_retains_groups() {
        let mut map = DecisionMap::default();
        let group = group_with_members(&mut map, 2);
        map.set(var(1), true, 1);

        map.clear_assignments();
        assert_eq!(map.value(var(1)), None);
        assert_eq!(map.value(var(2)), None);
        assert_eq!(map.amo_group_of(var(2)), Some(group));
        assert_eq!(map.amo_group_chosen(group), None);

        // The group still enforces at-most-one after the restart.
        map.set(var(2), true, 1);
        assert_eq!(map.value(var(1)), Some(false));
    }
}
