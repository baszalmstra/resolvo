use std::cmp::Ordering;

use crate::VariableId;
use crate::id::DenseIndex;

/// Represents a decision (i.e. an assignment to a variable) and the level at
/// which it was made
///
/// `= 0`: undecided
/// `> 0`: level of decision when the variable is set to true
/// `< 0`: level of decision when the variable is set to false
#[repr(transparent)]
#[derive(Copy, Clone)]
struct DecisionAndLevel(i32);

impl DecisionAndLevel {
    fn undecided() -> DecisionAndLevel {
        DecisionAndLevel(0)
    }

    fn value(self) -> Option<bool> {
        match self.0.cmp(&0) {
            Ordering::Less => Some(false),
            Ordering::Equal => None,
            Ordering::Greater => Some(true),
        }
    }

    fn level(self) -> u32 {
        self.0.unsigned_abs()
    }

    fn with_value_and_level(value: bool, level: u32) -> Self {
        debug_assert!(level <= (i32::MAX as u32), "level is too large");
        Self(if value { level as i32 } else { -(level as i32) })
    }
}

/// Identifies a set of variables of which at most one can be assigned true
/// (the candidates of a single package). See [`DecisionMap`] for how groups
/// participate in variable evaluation.
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
    /// Invariant: `chosen == Some(v)` implies `v` is explicitly assigned true
    /// in the map, so the implicit-false path in [`DecisionMap::value`] never
    /// has to compare against the queried variable.
    chosen: Option<VariableId>,

    /// All member variables, in registration order.
    members: Vec<VariableId>,

    /// Bumped whenever this group's choice is set or cleared: the only events
    /// that change member values without touching the members themselves.
    /// Consumers that cache the result of scanning this group's member values
    /// (see [`crate::solver::decide_queue`]) validate against this instead of
    /// being invalidated per implicitly changed member.
    revision: u64,
}

/// The result of [`DecisionMap::add_amo_member`].
pub(crate) enum AmoMemberAdded {
    /// The variable was already a member of the group.
    AlreadyMember,
    /// The variable joined the group and no assignment changed implicitly.
    Added,
    /// The variable joined a group that has already chosen `chosen`; the
    /// variable is implicitly false from now on (without a trail entry).
    AddedImplicitlyFalse,
    /// The variable is assigned true and became the group's choice.
    /// `falsified_others` is true when the group has other members, which are
    /// implicitly false from now on (without a trail entry).
    AddedBecameChoice { falsified_others: bool },
    /// The variable is assigned true, but the group had already chosen
    /// `chosen` (which is also assigned true): the at-most-one invariant is
    /// violated and the caller must report a conflict.
    Conflict { chosen: VariableId },
}

/// A map of the assignments to solvables.
///
/// Besides explicit assignments this map also tracks *package-level* decisions
/// natively: the candidates of a package form an at-most-one group, and as
/// soon as one member is assigned true every other member evaluates to false
/// implicitly, without a trail entry per sibling. This replaces the
/// materialized (¬candidate ∨ ¬sibling) clauses of the binary at-most-one
/// encoding, so selecting a version of an n-version package costs O(1) trail
/// work instead of O(n).
#[derive(Default)]
pub(crate) struct DecisionMap {
    map: Vec<DecisionAndLevel>,

    /// The group of each variable, indexed by variable index; `NO_GROUP` (or
    /// out of bounds) means the variable is not a member of any group.
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
        self.map.len()
    }

    #[inline]
    pub fn reset(&mut self, variable_id: VariableId) {
        let index = variable_id.to_index();
        if index < self.map.len() {
            // SAFE: because we check that the solvable id is within bounds
            unsafe { *self.map.get_unchecked_mut(index) = DecisionAndLevel::undecided() };
        }
        if let Some(group) = self.amo_group_of(variable_id) {
            let group = &mut self.groups[group.to_index()];
            if group.chosen == Some(variable_id) {
                group.chosen = None;
                group.revision += 1;
                self.amo_revision += 1;
            }
        }
    }

    #[inline]
    pub fn set(&mut self, variable_id: VariableId, value: bool, level: u32) {
        let index = variable_id.to_index();
        if index >= self.map.len() {
            self.map.resize_with(index + 1, DecisionAndLevel::undecided);
        }

        // SAFE: because we ensured that vec contains at least the correct number of
        // elements.
        unsafe {
            *self.map.get_unchecked_mut(index) =
                DecisionAndLevel::with_value_and_level(value, level)
        };

        if value {
            if let Some(group) = self.amo_group_of(variable_id) {
                let group = &mut self.groups[group.to_index()];
                debug_assert!(
                    group.chosen.is_none() || group.chosen == Some(variable_id),
                    "a second member of an at-most-one group was assigned true"
                );
                group.chosen = Some(variable_id);
                group.revision += 1;
                self.amo_revision += 1;
            }
        }
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

    /// Clears all assignments (explicit and package-level) but retains the
    /// group definitions, which are part of the encoded problem rather than
    /// the search state.
    pub fn clear_assignments(&mut self) {
        self.map.clear();
        for group in &mut self.groups {
            group.chosen = None;
            group.revision += 1;
        }
        self.amo_revision += 1;
    }

    #[inline]
    pub fn level(&self, variable_id: VariableId) -> u32 {
        let index = variable_id.to_index();
        if let Some(&entry) = self.map.get(index) {
            if entry.value().is_some() {
                return entry.level();
            }
        }
        // An implicitly false variable was falsified by its group's choice.
        if let Some(chosen) = self.implicitly_false_by(variable_id) {
            return self
                .map
                .get(chosen.to_index())
                .map_or(0, |entry| entry.level());
        }
        0
    }

    #[inline(always)]
    pub fn value(&self, variable_id: VariableId) -> Option<bool> {
        let index = variable_id.to_index();
        if let Some(value) = self.map.get(index).and_then(|d| d.value()) {
            return Some(value);
        }
        // Not explicitly assigned; a member of a group with a chosen member is
        // implicitly false. (`chosen` is always explicitly true, so it cannot
        // be the queried variable.)
        match self.variable_group.get(index) {
            Some(&group) if group != NO_GROUP => self.groups[group as usize].chosen.map(|_| false),
            _ => None,
        }
    }

    /// Returns the value explicitly assigned to the variable, ignoring any
    /// implicit falsification through the variable's group.
    #[inline]
    pub fn explicit_value(&self, variable_id: VariableId) -> Option<bool> {
        self.map.get(variable_id.to_index()).and_then(|d| d.value())
    }

    /// Returns the chosen member of the variable's group if, and only if, the
    /// variable is implicitly false: it has no explicit assignment but its
    /// group has chosen another member. The chosen member acts as the
    /// variable's "reason" through the virtual clause (¬chosen ∨ ¬variable).
    #[inline]
    pub fn implicitly_false_by(&self, variable_id: VariableId) -> Option<VariableId> {
        let index = variable_id.to_index();
        if self.map.get(index).is_some_and(|d| d.value().is_some()) {
            return None;
        }
        match self.variable_group.get(index) {
            Some(&group) if group != NO_GROUP => {
                let chosen = self.groups[group as usize].chosen?;
                debug_assert_ne!(
                    chosen, variable_id,
                    "the chosen member must be explicitly assigned true"
                );
                Some(chosen)
            }
            _ => None,
        }
    }

    /// Allocates a new, empty at-most-one group.
    pub fn alloc_amo_group(&mut self) -> AmoGroupId {
        let id = AmoGroupId(u32::try_from(self.groups.len()).expect("too many groups"));
        self.groups.push(AmoGroup {
            chosen: None,
            members: Vec::new(),
            revision: 0,
        });
        id
    }

    /// Returns the group the variable is a member of, if any.
    #[inline]
    pub fn amo_group_of(&self, variable_id: VariableId) -> Option<AmoGroupId> {
        match self.variable_group.get(variable_id.to_index()) {
            Some(&group) if group != NO_GROUP => Some(AmoGroupId(group)),
            _ => None,
        }
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
        if self.variable_group.get(index) == Some(&group.0) {
            return AmoMemberAdded::AlreadyMember;
        }
        debug_assert!(
            !matches!(self.variable_group.get(index), Some(&g) if g != NO_GROUP),
            "a variable can only be a member of a single at-most-one group"
        );

        if index >= self.variable_group.len() {
            self.variable_group.resize(index + 1, NO_GROUP);
        }
        self.variable_group[index] = group.0;

        let explicit_value = self.explicit_value(variable);
        let group = &mut self.groups[group.to_index()];
        group.members.push(variable);

        match (explicit_value, group.chosen) {
            (Some(true), Some(chosen)) => AmoMemberAdded::Conflict { chosen },
            (Some(true), None) => {
                group.chosen = Some(variable);
                group.revision += 1;
                let falsified_others = group.members.len() > 1;
                // The siblings' effective values just changed; invalidate
                // cached value scans (see [`Self::amo_revision`]).
                self.amo_revision += 1;
                AmoMemberAdded::AddedBecameChoice { falsified_others }
            }
            // An explicitly false member is not *implicitly* false: clauses
            // watching it were already woken when it was assigned.
            (None, Some(_)) => AmoMemberAdded::AddedImplicitlyFalse,
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

    #[test]
    fn group_choice_implicitly_falsifies_siblings() {
        let mut map = DecisionMap::default();
        let group = map.alloc_amo_group();
        for i in 1..=3 {
            assert!(matches!(
                map.add_amo_member(group, var(i)),
                AmoMemberAdded::Added
            ));
        }

        // Nothing is decided yet.
        assert_eq!(map.value(var(1)), None);
        assert_eq!(map.implicitly_false_by(var(2)), None);

        // Choosing one member makes the others false without touching them.
        map.set(var(1), true, 3);
        assert_eq!(map.value(var(1)), Some(true));
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(map.value(var(3)), Some(false));
        assert_eq!(map.explicit_value(var(2)), None);
        assert_eq!(map.implicitly_false_by(var(2)), Some(var(1)));
        assert_eq!(map.level(var(2)), 3);

        // Undoing the choice reverts the siblings implicitly.
        map.reset(var(1));
        assert_eq!(map.value(var(2)), None);
        assert_eq!(map.implicitly_false_by(var(2)), None);
        assert_eq!(map.level(var(2)), 0);
    }

    #[test]
    fn explicit_assignments_take_precedence() {
        let mut map = DecisionMap::default();
        let group = map.alloc_amo_group();
        map.add_amo_member(group, var(1));
        map.add_amo_member(group, var(2));

        map.set(var(2), false, 1);
        map.set(var(1), true, 2);

        // An explicitly false member is not implicitly false.
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(map.explicit_value(var(2)), Some(false));
        assert_eq!(map.implicitly_false_by(var(2)), None);
        assert_eq!(map.level(var(2)), 1);
    }

    #[test]
    fn late_members_observe_the_choice() {
        let mut map = DecisionMap::default();
        let group = map.alloc_amo_group();
        map.add_amo_member(group, var(1));
        map.set(var(1), true, 1);

        // An unassigned candidate registered after the choice is immediately
        // false-by-package.
        assert!(matches!(
            map.add_amo_member(group, var(2)),
            AmoMemberAdded::AddedImplicitlyFalse
        ));
        assert_eq!(map.value(var(2)), Some(false));

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
        let group = map.alloc_amo_group();
        map.add_amo_member(group, var(1));

        let global_revision = map.amo_revision(None);
        let group_revision = map.amo_revision(Some(group));
        map.set(var(2), true, 1);
        assert!(matches!(
            map.add_amo_member(group, var(2)),
            AmoMemberAdded::AddedBecameChoice {
                falsified_others: true
            }
        ));
        assert_eq!(map.value(var(1)), Some(false));
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
        let group = map.alloc_amo_group();
        map.add_amo_member(group, var(1));
        map.add_amo_member(group, var(2));
        map.set(var(1), true, 1);

        map.clear_assignments();
        assert_eq!(map.value(var(1)), None);
        assert_eq!(map.value(var(2)), None);
        assert_eq!(map.amo_group_of(var(2)), Some(group));

        // The group still enforces at-most-one after the restart.
        map.set(var(2), true, 1);
        assert_eq!(map.value(var(1)), Some(false));
    }
}
