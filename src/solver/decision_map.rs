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

/// A [`DecisionMap`] slot: the variable's explicit assignment fused with its
/// at-most-one group membership (`NO_GROUP` when it has none), so a single
/// load answers both questions. The group word carries the group's mode in
/// its top bit ([`IMPLICIT_GROUP`]): only members of *implicit* groups can be
/// false without an explicit assignment, so the hot evaluation path of an
/// explicit-group member never touches the group table.
#[derive(Copy, Clone)]
struct Entry {
    decision: DecisionAndLevel,
    group: u32,
}

impl Entry {
    fn vacant() -> Self {
        Self {
            decision: DecisionAndLevel::undecided(),
            group: NO_GROUP,
        }
    }

    /// The group this variable belongs to, ignoring the mode flag.
    #[inline]
    fn group_id(self) -> Option<AmoGroupId> {
        if self.group == NO_GROUP {
            None
        } else {
            Some(AmoGroupId(self.group & !IMPLICIT_GROUP))
        }
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

/// Marker in [`Entry::group`] for variables without a group.
const NO_GROUP: u32 = u32::MAX;

/// Set in [`Entry::group`] when the group enforces at-most-one *implicitly*
/// (package-level decisions). Clear while the group is small enough that
/// explicit sibling propagation is cheaper; see
/// [`DecisionMap::IMPLICIT_GROUP_THRESHOLD`].
const IMPLICIT_GROUP: u32 = 1 << 31;

struct AmoGroup {
    /// The member that was last assigned true, if any. Maintained by
    /// [`DecisionMap::set`] and [`DecisionMap::reset`].
    ///
    /// For an *implicit* group this is exact: `chosen == Some(v)` implies `v`
    /// is explicitly assigned true, so the implicit-false path in
    /// [`DecisionMap::value`] never has to compare against the queried
    /// variable. (Two members can never be true at once: the second
    /// assignment already fails against the first member's implicit
    /// falsification of its siblings.)
    ///
    /// For an *explicit* group it is only a hint: a sibling can transiently
    /// be assigned true before the winner's sibling sweep runs and reports
    /// the conflict, and a backtrack can leave the hint stale. Consumers
    /// validate it against the current assignment before use;
    /// [`DecisionMap::flip_to_implicit`] repairs it when the group switches
    /// modes.
    chosen: Option<VariableId>,

    /// All member variables, in registration order.
    members: Vec<VariableId>,

    /// Whether this group enforces at-most-one implicitly (true once the
    /// member count crosses [`DecisionMap::IMPLICIT_GROUP_THRESHOLD`]) or
    /// through explicit sibling propagation while it is small.
    implicit: bool,

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
    /// The variable joined a group that has already chosen `chosen`. In an
    /// implicit group the variable is implicitly false from now on (without a
    /// trail entry); in an explicit group the caller must falsify it with an
    /// explicit assignment.
    AddedFalsified { chosen: VariableId },
    /// The variable is assigned true and became the group's choice.
    /// `falsified_others` is true when the group has other members, which the
    /// enforcement mode of the group now falsifies: implicitly (no trail
    /// entries) for an implicit group, or through the caller's explicit
    /// sibling sweep for an explicit one.
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
    /// One entry per variable. The group id is fused into the same entry as
    /// the assignment so the hot evaluation path resolves both with a single
    /// indexed load: evaluating an implicitly false sibling would otherwise
    /// chase two separate arrays on every literal evaluation.
    map: Vec<Entry>,

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
        let Some(entry) = self.map.get_mut(index) else {
            return;
        };
        entry.decision = DecisionAndLevel::undecided();
        let group = entry.group;
        if group != NO_GROUP {
            let group = &mut self.groups[(group & !IMPLICIT_GROUP) as usize];
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
            self.map.resize_with(index + 1, Entry::vacant);
        }

        // SAFE: because we ensured that vec contains at least the correct number of
        // elements.
        let entry = unsafe { self.map.get_unchecked_mut(index) };
        entry.decision = DecisionAndLevel::with_value_and_level(value, level);

        if value {
            let group = entry.group;
            if group != NO_GROUP {
                let implicit = group & IMPLICIT_GROUP != 0;
                let group = &mut self.groups[(group & !IMPLICIT_GROUP) as usize];
                // Only an implicit group makes this impossible; in an
                // explicit group a sibling can transiently be assigned true
                // before the winner's sweep reports the conflict.
                debug_assert!(
                    !implicit || group.chosen.is_none() || group.chosen == Some(variable_id),
                    "a second member of an implicit at-most-one group was assigned true"
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
        for entry in &mut self.map {
            entry.decision = DecisionAndLevel::undecided();
        }
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
            if entry.decision.value().is_some() {
                return entry.decision.level();
            }
        }
        // An implicitly false variable was falsified by its group's choice.
        if let Some(chosen) = self.implicitly_false_by(variable_id) {
            return self
                .map
                .get(chosen.to_index())
                .map_or(0, |entry| entry.decision.level());
        }
        0
    }

    #[inline(always)]
    pub fn value(&self, variable_id: VariableId) -> Option<bool> {
        let &entry = self.map.get(variable_id.to_index())?;
        if let Some(value) = entry.decision.value() {
            return Some(value);
        }
        // Not explicitly assigned; a member of an *implicit* group with a
        // chosen member is implicitly false. (`chosen` is always explicitly
        // true, so it cannot be the queried variable. `NO_GROUP` has the
        // implicit bit set, but is excluded by the comparison.)
        if entry.group & IMPLICIT_GROUP != 0 && entry.group != NO_GROUP {
            self.groups[(entry.group & !IMPLICIT_GROUP) as usize]
                .chosen
                .map(|_| false)
        } else {
            None
        }
    }

    /// Returns the value explicitly assigned to the variable, ignoring any
    /// implicit falsification through the variable's group.
    #[inline]
    pub fn explicit_value(&self, variable_id: VariableId) -> Option<bool> {
        self.map
            .get(variable_id.to_index())
            .and_then(|entry| entry.decision.value())
    }

    /// Returns the chosen member of the variable's group if, and only if, the
    /// variable is implicitly false: it has no explicit assignment but its
    /// group has chosen another member. The chosen member acts as the
    /// variable's "reason" through the virtual clause (¬chosen ∨ ¬variable).
    #[inline]
    pub fn implicitly_false_by(&self, variable_id: VariableId) -> Option<VariableId> {
        let &entry = self.map.get(variable_id.to_index())?;
        if entry.decision.value().is_some()
            || entry.group & IMPLICIT_GROUP == 0
            || entry.group == NO_GROUP
        {
            return None;
        }
        let chosen = self.groups[(entry.group & !IMPLICIT_GROUP) as usize].chosen?;
        debug_assert_ne!(
            chosen, variable_id,
            "the chosen member must be explicitly assigned true"
        );
        Some(chosen)
    }

    /// The member count at which a group switches from explicit sibling
    /// propagation to implicit (package-level) enforcement. Explicit
    /// propagation keeps evaluation of the (very common) falsified-sibling
    /// literals to a single load and is cheap while groups are small;
    /// implicit enforcement caps the per-selection trail work of huge groups
    /// at O(1) instead of O(members).
    pub const IMPLICIT_GROUP_THRESHOLD: usize = 256;

    /// Allocates a new, empty at-most-one group.
    pub fn alloc_amo_group(&mut self) -> AmoGroupId {
        let id = AmoGroupId(u32::try_from(self.groups.len()).expect("too many groups"));
        assert!(
            self.groups.len() < (IMPLICIT_GROUP as usize),
            "too many at-most-one groups"
        );
        self.groups.push(AmoGroup {
            chosen: None,
            members: Vec::new(),
            implicit: false,
            revision: 0,
        });
        id
    }

    /// Whether the group enforces at-most-one implicitly (see
    /// [`Self::IMPLICIT_GROUP_THRESHOLD`]).
    #[inline]
    pub fn amo_group_is_implicit(&self, group: AmoGroupId) -> bool {
        self.groups[group.to_index()].implicit
    }

    /// Switches a group to implicit enforcement, updating every member's
    /// fused entry. Called at most once per group when it crosses
    /// [`Self::IMPLICIT_GROUP_THRESHOLD`].
    fn flip_to_implicit(&mut self, group: AmoGroupId) {
        let members = std::mem::take(&mut self.groups[group.to_index()].members);
        for &member in &members {
            self.map[member.to_index()].group = group.0 | IMPLICIT_GROUP;
        }
        // The explicit-mode `chosen` is only a hint and can be stale after a
        // backtrack; implicit enforcement derives member values from it, so
        // recompute it from the actual assignment.
        let chosen = members
            .iter()
            .copied()
            .find(|&member| self.explicit_value(member) == Some(true));
        let entry = &mut self.groups[group.to_index()];
        entry.members = members;
        entry.chosen = chosen;
        entry.implicit = true;
        entry.revision += 1;
        self.amo_revision += 1;
    }

    /// Test-only hook to put a small group into implicit mode.
    #[cfg(test)]
    pub fn force_amo_group_implicit(&mut self, group: AmoGroupId) {
        if !self.groups[group.to_index()].implicit {
            self.flip_to_implicit(group);
        }
    }

    /// Returns the group the variable is a member of, if any.
    #[inline]
    pub fn amo_group_of(&self, variable_id: VariableId) -> Option<AmoGroupId> {
        self.map
            .get(variable_id.to_index())
            .and_then(|entry| entry.group_id())
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
        if let Some(entry) = self.map.get(index) {
            // Compare through the masked id: member entries carry the group's
            // mode in their top bit.
            if entry.group_id() == Some(group) {
                return AmoMemberAdded::AlreadyMember;
            }
            debug_assert!(
                entry.group == NO_GROUP,
                "a variable can only be a member of a single at-most-one group"
            );
        }

        if index >= self.map.len() {
            self.map.resize_with(index + 1, Entry::vacant);
        }

        let explicit_value = self.explicit_value(variable);
        self.groups[group.to_index()].members.push(variable);
        let implicit = self.groups[group.to_index()].implicit;
        self.map[index].group = if implicit {
            group.0 | IMPLICIT_GROUP
        } else {
            group.0
        };

        // Once the group grows past the threshold, flip it (and every member
        // entry) to implicit enforcement. This happens at most once per
        // group; from here on a package-level decision falsifies the members
        // without per-member trail entries, so cached value scans must start
        // validating against the group's revision.
        if !implicit && self.groups[group.to_index()].members.len() > Self::IMPLICIT_GROUP_THRESHOLD
        {
            self.flip_to_implicit(group);
        }

        // The explicit-mode `chosen` is only a hint; trust it exclusively
        // when the member it names is still assigned true.
        let chosen = self.groups[group.to_index()]
            .chosen
            .filter(|&chosen| chosen != variable && self.explicit_value(chosen) == Some(true));
        let group = &mut self.groups[group.to_index()];
        match (explicit_value, chosen) {
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
            // An explicitly false member needs no falsification: clauses
            // watching it were already woken when it was assigned.
            (None, Some(chosen)) => AmoMemberAdded::AddedFalsified { chosen },
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

    /// Builds a group in implicit mode with members `1..=count`.
    fn implicit_group(map: &mut DecisionMap, count: usize) -> AmoGroupId {
        let group = map.alloc_amo_group();
        for i in 1..=count {
            map.add_amo_member(group, var(i));
        }
        map.force_amo_group_implicit(group);
        group
    }

    #[test]
    fn group_choice_implicitly_falsifies_siblings() {
        let mut map = DecisionMap::default();
        let group = implicit_group(&mut map, 3);
        assert!(map.amo_group_is_implicit(group));

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
    fn explicit_groups_do_not_falsify_implicitly() {
        let mut map = DecisionMap::default();
        let group = map.alloc_amo_group();
        map.add_amo_member(group, var(1));
        map.add_amo_member(group, var(2));
        assert!(!map.amo_group_is_implicit(group));

        // A small group stays in explicit mode: choosing a member does not
        // change the siblings' values by itself (the caller assigns them).
        map.set(var(1), true, 1);
        assert_eq!(map.value(var(2)), None);
        assert_eq!(map.implicitly_false_by(var(2)), None);
    }

    #[test]
    fn groups_flip_to_implicit_past_the_threshold() {
        let mut map = DecisionMap::default();
        let group = map.alloc_amo_group();
        for i in 1..=DecisionMap::IMPLICIT_GROUP_THRESHOLD {
            map.add_amo_member(group, var(i));
        }
        assert!(!map.amo_group_is_implicit(group));
        let revision = map.amo_revision(Some(group));

        // One more member crosses the threshold: the group and every member
        // entry switch to implicit enforcement, and cached value scans must
        // revalidate.
        map.add_amo_member(group, var(DecisionMap::IMPLICIT_GROUP_THRESHOLD + 1));
        assert!(map.amo_group_is_implicit(group));
        assert_ne!(map.amo_revision(Some(group)), revision);

        map.set(var(1), true, 1);
        assert_eq!(map.value(var(2)), Some(false));
        assert_eq!(
            map.value(var(DecisionMap::IMPLICIT_GROUP_THRESHOLD + 1)),
            Some(false)
        );
        assert_eq!(map.implicitly_false_by(var(2)), Some(var(1)));
    }

    #[test]
    fn explicit_assignments_take_precedence() {
        let mut map = DecisionMap::default();
        let group = implicit_group(&mut map, 2);

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
        let group = implicit_group(&mut map, 1);
        map.set(var(1), true, 1);

        // An unassigned candidate registered after the choice is immediately
        // false-by-package.
        assert!(matches!(
            map.add_amo_member(group, var(2)),
            AmoMemberAdded::AddedFalsified { chosen } if chosen == var(1)
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
        let group = implicit_group(&mut map, 1);

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
        let group = implicit_group(&mut map, 2);
        map.set(var(1), true, 1);

        map.clear_assignments();
        assert_eq!(map.value(var(1)), None);
        assert_eq!(map.value(var(2)), None);
        assert_eq!(map.amo_group_of(var(2)), Some(group));
        assert!(map.amo_group_is_implicit(group));

        // The group still enforces at-most-one after the restart.
        map.set(var(2), true, 1);
        assert_eq!(map.value(var(1)), Some(false));
    }
}
