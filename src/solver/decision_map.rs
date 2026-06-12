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

/// Marker for variables that are not a member of any tracked package.
const NO_PACKAGE: u32 = u32::MAX;

/// The candidates of one package tracked for virtual at-most-one handling.
struct PackageState {
    /// The candidate that is currently assigned true (and the level of that
    /// assignment), if any. While set, every other member is implicitly
    /// false.
    selected: Option<(VariableId, u32)>,
    /// All registered candidate variables of this package.
    members: Vec<VariableId>,
}

/// A map of the assignments to solvables.
///
/// With the virtual at-most-one encoding ([`crate::AmoEncoding::Virtual`])
/// the map additionally tracks one *selection* per package: when a candidate
/// of a tracked package is assigned true, all other candidates of that
/// package evaluate to false without being physically assigned.
#[derive(Default)]
pub(crate) struct DecisionMap {
    map: Vec<DecisionAndLevel>,

    /// The package index of each variable, or [`NO_PACKAGE`]. Only populated
    /// for candidates registered through [`Self::register_package_member`].
    var_to_package: Vec<u32>,

    /// State per tracked package.
    packages: Vec<PackageState>,
}

impl DecisionMap {
    #[cfg(feature = "diagnostics")]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Clears all assignments and selections but keeps the package registry.
    pub fn reset_assignments(&mut self) {
        self.map.clear();
        for package in &mut self.packages {
            package.selected = None;
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
            _ => match self.virtual_selection(variable_id) {
                Some((_, level)) => level,
                None => 0,
            },
        }
    }

    #[inline(always)]
    pub fn value(&self, variable_id: VariableId) -> Option<bool> {
        match self.map.get(variable_id.to_index()).and_then(|d| d.value()) {
            Some(value) => Some(value),
            None => self.virtual_selection(variable_id).map(|_| false),
        }
    }

    /// Returns the physically assigned value of a variable, ignoring virtual
    /// falsification through a package selection.
    #[inline]
    pub fn explicit_value(&self, variable_id: VariableId) -> Option<bool> {
        self.map.get(variable_id.to_index()).and_then(|d| d.value())
    }

    /// If `variable_id` is a member of a tracked package whose selection is a
    /// *different* candidate, returns that selected candidate and the level
    /// of its assignment. Such a variable evaluates to false.
    #[inline]
    pub fn virtual_selection(&self, variable_id: VariableId) -> Option<(VariableId, u32)> {
        let &package = self.var_to_package.get(variable_id.to_index())?;
        if package == NO_PACKAGE {
            return None;
        }
        match self.packages[package as usize].selected {
            Some((selected, level)) if selected != variable_id => Some((selected, level)),
            _ => None,
        }
    }

    /// Allocates a new tracked package and returns its index.
    pub fn alloc_package(&mut self) -> u32 {
        let index = self.packages.len() as u32;
        self.packages.push(PackageState {
            selected: None,
            members: Vec::new(),
        });
        index
    }

    /// Registers `variable_id` as a candidate of `package`.
    pub fn register_package_member(&mut self, package: u32, variable_id: VariableId) {
        let index = variable_id.to_index();
        if index >= self.var_to_package.len() {
            self.var_to_package.resize(index + 1, NO_PACKAGE);
        }
        debug_assert_eq!(self.var_to_package[index], NO_PACKAGE);
        self.var_to_package[index] = package;
        self.packages[package as usize].members.push(variable_id);
    }

    /// Returns the tracked package `variable_id` belongs to, if any.
    #[inline]
    pub fn package_of(&self, variable_id: VariableId) -> Option<u32> {
        match self.var_to_package.get(variable_id.to_index()) {
            Some(&package) if package != NO_PACKAGE => Some(package),
            _ => None,
        }
    }

    /// Returns the current selection of `package`.
    #[inline]
    pub fn selection(&self, package: u32) -> Option<(VariableId, u32)> {
        self.packages[package as usize].selected
    }

    pub fn set_selection(&mut self, package: u32, variable_id: VariableId, level: u32) {
        debug_assert!(self.packages[package as usize].selected.is_none());
        self.packages[package as usize].selected = Some((variable_id, level));
    }

    pub fn clear_selection(&mut self, package: u32) {
        self.packages[package as usize].selected = None;
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
}
