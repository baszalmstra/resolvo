use std::cmp::Ordering;

use crate::VariableId;
use crate::id::DenseIndex;
use crate::internal::id::ClauseId;
use crate::solver::clause::Literal;

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

/// The role a variable plays in the virtual at-most-one machinery.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PackageVar {
    /// Not part of any tracked package.
    None,
    /// The `index`-th candidate of the tracked package.
    Member { package: u32, index: u32 },
    /// The prefix variable `p_index` of the tracked package: "the selected
    /// candidate has index ≤ index".
    Prefix { package: u32, index: u32 },
}

/// The candidates of one package tracked for virtual at-most-one handling.
struct PackageState {
    /// The candidate that is currently assigned true: its variable, member
    /// index and assignment level. While set, every other member is
    /// implicitly false.
    selected: Option<(VariableId, u32, u32)>,
    /// All registered candidate variables of this package.
    members: Vec<VariableId>,
    /// The prefix variables, parallel to `members` (only allocated by the
    /// virtual-ladder encoding).
    prefix_vars: Vec<VariableId>,
    /// Per member: the pre-materialized boundary reason clauses
    /// `(¬member ∨ ¬p_{index-1})` and `(¬member ∨ p_index)` used when the
    /// member is selected (virtual-ladder encoding). Parallel to `members`.
    boundary_reasons: Vec<(Option<ClauseId>, ClauseId)>,
    /// Per prefix index `i`: the monotone chain clause `(¬p_i ∨ p_{i+1})`,
    /// available once `p_{i+1}` exists. Used as reasons by the experimental
    /// full-chain mode.
    chain_reasons: Vec<ClauseId>,
    /// The minimum still-allowed member index, derived from explicitly false
    /// prefix variables, along with the driving variable and its level.
    lo: u32,
    lo_driver: Option<(VariableId, u32)>,
    /// The maximum still-allowed member index, derived from explicitly true
    /// prefix variables, along with the driving variable and its level.
    hi: u32,
    hi_driver: Option<(VariableId, u32)>,
    /// The member index of the previous selection of this package. A pure
    /// heuristic memory (never undone): re-selections push explicit chain
    /// entries over the window between the old and new index, giving
    /// conflict analysis adjacent-step anchors where the search frontier
    /// just moved.
    last_selected: Option<u32>,
}

impl PackageState {
    fn new() -> Self {
        Self {
            selected: None,
            members: Vec::new(),
            prefix_vars: Vec::new(),
            boundary_reasons: Vec::new(),
            chain_reasons: Vec::new(),
            lo: 0,
            lo_driver: None,
            hi: u32::MAX,
            hi_driver: None,
            last_selected: None,
        }
    }
}

/// A map of the assignments to solvables.
///
/// With the virtual at-most-one encodings ([`crate::AmoEncoding::Virtual`]
/// and [`crate::AmoEncoding::VirtualLadder`]) the map additionally tracks one
/// *selection* per package — when a candidate of a tracked package is
/// assigned true, all other candidates evaluate to false without being
/// physically assigned — and, for the ladder variant, an interval `[lo, hi]`
/// of still-allowed candidate indices derived from explicit prefix variable
/// assignments.
#[derive(Default)]
pub(crate) struct DecisionMap {
    map: Vec<DecisionAndLevel>,

    /// The package role of each variable. Only populated for variables
    /// registered through [`Self::register_package_member`] or
    /// [`Self::register_package_prefix`].
    var_roles: Vec<PackageVar>,

    /// State per tracked package.
    packages: Vec<PackageState>,
}

impl DecisionMap {
    #[cfg(feature = "diagnostics")]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Clears all assignments, selections and intervals but keeps the package
    /// registry.
    pub fn reset_assignments(&mut self) {
        self.map.clear();
        for package in &mut self.packages {
            package.selected = None;
            package.lo = 0;
            package.lo_driver = None;
            package.hi = u32::MAX;
            package.hi_driver = None;
        }
    }

    #[inline]
    pub fn reset(&mut self, variable_id: VariableId) {
        let variable_id = variable_id.to_index();
        if variable_id < self.map.len() {
            // SAFE: because we check that the solvable id is within bounds
            unsafe { *self.map.get_unchecked_mut(variable_id) = DecisionAndLevel::undecided() };
        }
    }

    #[inline]
    pub fn set(&mut self, variable_id: VariableId, value: bool, level: u32) {
        let variable_id = variable_id.to_index();
        if variable_id >= self.map.len() {
            self.map
                .resize_with(variable_id + 1, DecisionAndLevel::undecided);
        }

        // SAFE: because we ensured that vec contains at least the correct number of
        // elements.
        unsafe {
            *self.map.get_unchecked_mut(variable_id) =
                DecisionAndLevel::with_value_and_level(value, level)
        };
    }

    #[inline]
    pub fn level(&self, variable_id: VariableId) -> u32 {
        match self.map.get(variable_id.to_index()) {
            Some(d) if d.value().is_some() => d.level(),
            _ => match self.derived_value_and_level(variable_id) {
                Some((_, level)) => level,
                None => 0,
            },
        }
    }

    #[inline(always)]
    pub fn value(&self, variable_id: VariableId) -> Option<bool> {
        match self.map.get(variable_id.to_index()).and_then(|d| d.value()) {
            Some(value) => Some(value),
            None => self
                .derived_value_and_level(variable_id)
                .map(|(value, _)| value),
        }
    }

    /// Returns the physically assigned value of a variable, ignoring values
    /// derived from package selections or prefix intervals.
    #[inline]
    pub fn explicit_value(&self, variable_id: VariableId) -> Option<bool> {
        self.map.get(variable_id.to_index()).and_then(|d| d.value())
    }

    /// Returns the derived (virtual) value and level of a variable, if any.
    /// Does *not* consult the explicit assignment.
    pub fn derived_value_and_level(&self, variable_id: VariableId) -> Option<(bool, u32)> {
        match self.var_role(variable_id) {
            PackageVar::None => None,
            PackageVar::Member { package, index } => {
                let state = &self.packages[package as usize];
                if let Some((selected, _, level)) = state.selected {
                    if selected != variable_id {
                        return Some((false, level));
                    }
                    return None;
                }
                if index < state.lo {
                    let (_, level) = state.lo_driver.expect("lo > 0 implies a driver");
                    return Some((false, level));
                }
                if index > state.hi {
                    let (_, level) = state.hi_driver.expect("bounded hi implies a driver");
                    return Some((false, level));
                }
                None
            }
            PackageVar::Prefix { package, index } => {
                let state = &self.packages[package as usize];
                if let Some((_, selected_index, level)) = state.selected {
                    return Some((selected_index <= index, level));
                }
                if state.hi <= index {
                    let (_, level) = state.hi_driver.expect("bounded hi implies a driver");
                    return Some((true, level));
                }
                if state.lo > index {
                    let (_, level) = state.lo_driver.expect("lo > 0 implies a driver");
                    return Some((false, level));
                }
                None
            }
        }
    }

    /// Returns the role of a variable in the virtual at-most-one machinery.
    #[inline]
    pub fn var_role(&self, variable_id: VariableId) -> PackageVar {
        self.var_roles
            .get(variable_id.to_index())
            .copied()
            .unwrap_or(PackageVar::None)
    }

    fn set_var_role(&mut self, variable_id: VariableId, role: PackageVar) {
        let index = variable_id.to_index();
        if index >= self.var_roles.len() {
            self.var_roles.resize(index + 1, PackageVar::None);
        }
        debug_assert_eq!(self.var_roles[index], PackageVar::None);
        self.var_roles[index] = role;
    }

    /// Allocates a new tracked package and returns its index.
    pub fn alloc_package(&mut self) -> u32 {
        let index = self.packages.len() as u32;
        self.packages.push(PackageState::new());
        index
    }

    /// Registers `variable_id` as the next candidate of `package` and returns
    /// its member index.
    pub fn register_package_member(&mut self, package: u32, variable_id: VariableId) -> u32 {
        let member_index = self.packages[package as usize].members.len() as u32;
        self.set_var_role(
            variable_id,
            PackageVar::Member {
                package,
                index: member_index,
            },
        );
        self.packages[package as usize].members.push(variable_id);
        member_index
    }

    /// Registers `variable_id` as the next prefix variable of `package`
    /// (virtual-ladder encoding), along with the pre-materialized boundary
    /// reason clauses of the member with the same index.
    pub fn register_package_prefix(
        &mut self,
        package: u32,
        variable_id: VariableId,
        boundary_reasons: (Option<ClauseId>, ClauseId),
    ) {
        let index = self.packages[package as usize].prefix_vars.len() as u32;
        self.set_var_role(variable_id, PackageVar::Prefix { package, index });
        self.packages[package as usize]
            .prefix_vars
            .push(variable_id);
        self.packages[package as usize]
            .boundary_reasons
            .push(boundary_reasons);
    }

    /// The boundary reason clauses of the `index`-th member of `package`.
    #[inline]
    pub fn boundary_reasons(&self, package: u32, index: usize) -> (Option<ClauseId>, ClauseId) {
        self.packages[package as usize].boundary_reasons[index]
    }

    /// Registers the chain clause `(¬p_index ∨ p_{index+1})` of `package`.
    pub fn register_chain_reason(&mut self, package: u32, clause_id: ClauseId) {
        self.packages[package as usize]
            .chain_reasons
            .push(clause_id);
    }

    /// The chain clause `(¬p_index ∨ p_{index+1})` of `package`.
    #[inline]
    pub fn chain_reason(&self, package: u32, index: usize) -> ClauseId {
        self.packages[package as usize].chain_reasons[index]
    }

    /// The driver of the lower interval bound, if any.
    #[inline]
    pub fn lo_driver(&self, package: u32) -> Option<(VariableId, u32)> {
        self.packages[package as usize].lo_driver
    }

    /// The driver of the upper interval bound, if any.
    #[inline]
    pub fn hi_driver(&self, package: u32) -> Option<(VariableId, u32)> {
        self.packages[package as usize].hi_driver
    }

    /// Records `index` as the package's latest selection and returns the
    /// previous one.
    pub fn swap_last_selected(&mut self, package: u32, index: u32) -> Option<u32> {
        std::mem::replace(
            &mut self.packages[package as usize].last_selected,
            Some(index),
        )
    }

    /// Returns the current selection of `package`: the selected variable, its
    /// member index, and the assignment level.
    #[inline]
    pub fn selection(&self, package: u32) -> Option<(VariableId, u32, u32)> {
        self.packages[package as usize].selected
    }

    pub fn set_selection(&mut self, package: u32, variable_id: VariableId, index: u32, level: u32) {
        debug_assert!(self.packages[package as usize].selected.is_none());
        self.packages[package as usize].selected = Some((variable_id, index, level));
    }

    pub fn clear_selection(&mut self, package: u32) {
        self.packages[package as usize].selected = None;
    }

    /// Updates the allowed-index interval of a package for an explicit prefix
    /// assignment `p_index := value`.
    pub fn apply_prefix_assignment(
        &mut self,
        package: u32,
        index: u32,
        value: bool,
        variable_id: VariableId,
        level: u32,
    ) {
        let state = &mut self.packages[package as usize];
        if value {
            // The selected candidate has index <= index.
            if index < state.hi {
                state.hi = index;
                state.hi_driver = Some((variable_id, level));
            }
        } else {
            // The selected candidate has index > index.
            if index + 1 > state.lo {
                state.lo = index + 1;
                state.lo_driver = Some((variable_id, level));
            }
        }
    }

    /// Recomputes the allowed-index interval of a package from the explicit
    /// prefix assignments. Called when a prefix assignment is undone.
    pub fn recompute_interval(&mut self, package: u32) {
        let mut lo = 0u32;
        let mut lo_driver = None;
        let mut hi = u32::MAX;
        let mut hi_driver = None;
        for (index, &prefix_var) in self.packages[package as usize]
            .prefix_vars
            .iter()
            .enumerate()
        {
            let index = index as u32;
            match self.explicit_value(prefix_var) {
                Some(true) if index < hi => {
                    hi = index;
                    hi_driver = Some((prefix_var, self.map[prefix_var.to_index()].level()));
                }
                Some(false) if index + 1 > lo => {
                    lo = index + 1;
                    lo_driver = Some((prefix_var, self.map[prefix_var.to_index()].level()));
                }
                _ => {}
            }
        }
        let state = &mut self.packages[package as usize];
        state.lo = lo;
        state.lo_driver = lo_driver;
        state.hi = hi;
        state.hi_driver = hi_driver;
    }

    /// The number of registered candidates of `package`.
    #[inline]
    pub fn package_member_count(&self, package: u32) -> usize {
        self.packages[package as usize].members.len()
    }

    /// Returns the `index`-th registered candidate of `package`.
    #[inline]
    pub fn package_member(&self, package: u32, index: usize) -> VariableId {
        self.packages[package as usize].members[index]
    }

    /// The number of allocated prefix variables of `package`.
    #[inline]
    pub fn package_prefix_count(&self, package: u32) -> usize {
        self.packages[package as usize].prefix_vars.len()
    }

    /// Returns the `index`-th prefix variable of `package`.
    #[inline]
    pub fn package_prefix(&self, package: u32, index: usize) -> VariableId {
        self.packages[package as usize].prefix_vars[index]
    }

    /// For a variable whose value is derived (not explicit), returns the
    /// explicitly assigned *driver* variable it can be resolved through, and
    /// the reason clause `(¬a ∨ lit)` that justifies the derivation.
    ///
    /// Returns `None` if the variable has no derived value.
    pub fn derived_reason(
        &self,
        variable_id: VariableId,
    ) -> Option<(VariableId, VariableId, Literal)> {
        match self.var_role(variable_id) {
            PackageVar::None => None,
            PackageVar::Member { package, index } => {
                let state = &self.packages[package as usize];
                if let Some((selected, _, _)) = state.selected {
                    if selected != variable_id {
                        // (¬selected ∨ ¬member)
                        return Some((selected, selected, variable_id.negative()));
                    }
                    return None;
                }
                // Use the *tightest* prefix bound that explains the
                // falsification (the most general explanation in lazy clause
                // generation terms), not the interval driver: the resulting
                // learnt clauses assert much stronger range restrictions.
                // The tight prefix is itself derived; resolution continues
                // through it if necessary.
                if index < state.lo {
                    // member → p_index: (¬member ∨ p_index), p_index false.
                    let tight = state.prefix_vars[index as usize];
                    return Some((tight, variable_id, tight.positive()));
                }
                if index > state.hi {
                    // (¬member ∨ ¬p_{index-1}), p_{index-1} true.
                    let tight = state.prefix_vars[(index - 1) as usize];
                    return Some((tight, variable_id, tight.negative()));
                }
                None
            }
            PackageVar::Prefix { package, index } => {
                let state = &self.packages[package as usize];
                if let Some((selected, selected_index, _)) = state.selected {
                    // (¬selected ∨ p) if the selection is at or below the
                    // prefix index, (¬selected ∨ ¬p) otherwise.
                    let value = selected_index <= index;
                    return Some((selected, selected, Literal::new(variable_id, !value)));
                }
                if state.hi <= index {
                    // p_hi → p_index (monotone): (¬p_hi ∨ p_index).
                    let (driver, _) = state.hi_driver.expect("bounded hi implies a driver");
                    return Some((driver, driver, variable_id.positive()));
                }
                if state.lo > index {
                    // p_index → p_{lo-1} (monotone): (¬p_index ∨ p_{lo-1}).
                    let (driver, _) = state.lo_driver.expect("lo > 0 implies a driver");
                    return Some((driver, variable_id, driver.positive()));
                }
                None
            }
        }
    }

    /// Returns the package a derived variable belongs to, if any.
    #[inline]
    pub fn package_of(&self, variable_id: VariableId) -> Option<u32> {
        match self.var_role(variable_id) {
            PackageVar::None => None,
            PackageVar::Member { package, .. } | PackageVar::Prefix { package, .. } => {
                Some(package)
            }
        }
    }
}
