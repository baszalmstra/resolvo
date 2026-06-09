use std::fmt::Display;

use ahash::HashMap;

use crate::{
    DenseIndex, Interner, VariableId, VersionSetId,
    internal::solver_id::SolvableIdOrRoot,
    solver_id::{IdMap, SolverId},
};

/// All variables in the solver are stored in a `VariableMap`. This map is used
/// to keep track of the semantics of a variable, e.g. what a specific variable
/// represents.
///
/// The `VariableMap` is responsible for assigning unique identifiers to each
/// variable represented by [`VariableId`].
pub(crate) struct VariableMap<N: SolverId, S: SolverId> {
    /// A map from solvable id to variable id. Backed by the storage selected
    /// by the [`Layout`] marker.
    solvable_to_variable: S::Map<Option<VariableId>>,

    /// Records the origin of each variable, indexed by its [`VariableId`].
    ///
    /// Invariant: every allocated variable has its slot pushed in the same step
    /// it is handed out. Do not insert gaps; [`Self::origin`] indexes directly.
    origins: Vec<VariableOrigin<N, S>>,

    /// Maps a version set (on an environment package) to the variable that
    /// represents the "matches" literal `L_S` for that version set.
    env_matches_to_variable: HashMap<VersionSetId, VariableId>,

    /// Per-package list of all version sets for which a matches literal has
    /// been interned. Used to find sibling literals when computing oracle
    /// consistency clauses.
    env_matches_by_name: HashMap<N, Vec<VersionSetId>>,

    /// Maps an environment package name to the variable that represents the
    /// "absent" literal `Ab_p` for that package.
    env_absent_to_variable: HashMap<N, VariableId>,
}

/// Describes the origin of a variable.
#[derive(Clone, Copy, Debug)]
pub(crate) enum VariableOrigin<N, S> {
    /// The variable is the root of the decision tree.
    Root,

    /// The variable represents a specific solvable.
    Solvable(S),

    /// A variable that helps encode an at most one constraint.
    ForbidMultiple(N),

    /// A variable that indicates that any solvable of a particular package is
    /// part of the solution.
    AtLeastOne(N),

    /// A variable that represents the environment literal `L_S`: the
    /// environment package is present and its value matches version set `S`.
    EnvMatches(VersionSetId),

    /// A variable that represents the absent literal `Ab_p`: the environment
    /// package with name `N` is absent from the environment.
    EnvAbsent(N),
}

impl<N: SolverId, S: SolverId> Default for VariableMap<N, S> {
    fn default() -> Self {
        Self {
            solvable_to_variable: Default::default(),
            // Index 0 is reserved for the root variable.
            origins: vec![VariableOrigin::Root],
            env_matches_to_variable: HashMap::default(),
            env_matches_by_name: HashMap::default(),
            env_absent_to_variable: HashMap::default(),
        }
    }
}

impl<N: SolverId, S: SolverId> VariableMap<N, S> {
    /// Allocate a new variable with the given origin and hand back its id.
    ///
    /// This is the single place that mutates `origins`, preserving the
    /// invariant that every variable has exactly one origin slot. Always
    /// extends `origins` by exactly one slot.
    fn alloc(&mut self, origin: VariableOrigin<N, S>) -> VariableId {
        let id = self.origins.len();
        self.origins.push(origin);
        VariableId::from_index(id)
    }

    /// Allocate a variable for a new variable or reuse an existing one.
    pub fn intern_solvable(&mut self, solvable_id: S) -> VariableId {
        if let Some(variable_id) = self.solvable_to_variable.get(solvable_id) {
            return variable_id;
        }
        let variable_id = self.alloc(VariableOrigin::Solvable(solvable_id));
        debug_assert!(
            !variable_id.is_root(),
            "intern_solvable must never hand out the root variable id"
        );
        self.solvable_to_variable
            .set(solvable_id, Some(variable_id));
        variable_id
    }

    #[cfg(feature = "diagnostics")]
    pub fn count(&self) -> usize {
        self.origins.len()
    }

    #[cfg(feature = "diagnostics")]
    pub fn size_in_bytes(&self) -> usize {
        self.origins.capacity() * std::mem::size_of::<VariableOrigin<N, S>>()
    }

    /// Allocate a variable for a solvable or the root.
    pub fn intern_solvable_or_root(
        &mut self,
        solvable_or_root_id: SolvableIdOrRoot<S>,
    ) -> VariableId {
        match solvable_or_root_id.solvable() {
            Some(solvable_id) => self.intern_solvable(solvable_id),
            None => VariableId::root(),
        }
    }

    /// Allocate a variable that helps encode an at most one constraint.
    pub fn alloc_forbid_multiple_variable(&mut self, name: N) -> VariableId {
        self.alloc(VariableOrigin::ForbidMultiple(name))
    }

    /// Allocate a variable helps encode whether at least one solvable for a
    /// particular package is selected.
    pub fn alloc_at_least_one_variable(&mut self, name: N) -> VariableId {
        self.alloc(VariableOrigin::AtLeastOne(name))
    }

    /// Intern the "matches" literal `L_S` for a version set on an environment
    /// package. If this is a new version set, returns the newly allocated
    /// variable plus:
    ///   - the previously interned version sets for the same package (to be
    ///     used by the caller to emit oracle consistency clauses), and
    ///   - the absent variable for the same package, if it was already interned.
    ///
    /// On a repeat call (same version set id) the existing variable is returned
    /// and both secondary outputs are empty / None.
    pub fn intern_env_matches(
        &mut self,
        version_set_id: VersionSetId,
        package_name: N,
    ) -> (VariableId, Vec<VersionSetId>, Option<VariableId>) {
        if let Some(&var) = self.env_matches_to_variable.get(&version_set_id) {
            return (var, Vec::new(), None);
        }

        // Snapshot siblings before mutating.
        let previously_for_pkg: Vec<VersionSetId> = self
            .env_matches_by_name
            .get(&package_name)
            .cloned()
            .unwrap_or_default();
        let absent_var: Option<VariableId> =
            self.env_absent_to_variable.get(&package_name).copied();

        let var = self.alloc(VariableOrigin::EnvMatches(version_set_id));
        self.env_matches_to_variable.insert(version_set_id, var);
        self.env_matches_by_name
            .entry(package_name)
            .or_default()
            .push(version_set_id);

        (var, previously_for_pkg, absent_var)
    }

    /// Intern the "absent" literal `Ab_p` for an environment package.
    ///
    /// Returns `(variable, is_new, previously_interned_version_sets_for_pkg)`.
    /// If the variable already exists `is_new` is false and the third field
    /// is empty.
    pub fn intern_env_absent(&mut self, package_name: N) -> (VariableId, bool, Vec<VersionSetId>) {
        if let Some(&var) = self.env_absent_to_variable.get(&package_name) {
            return (var, false, Vec::new());
        }

        let previously_for_pkg: Vec<VersionSetId> = self
            .env_matches_by_name
            .get(&package_name)
            .cloned()
            .unwrap_or_default();

        let var = self.alloc(VariableOrigin::EnvAbsent(package_name));
        self.env_absent_to_variable.insert(package_name, var);

        (var, true, previously_for_pkg)
    }

    /// Look up an already-interned "matches" literal for a version set.
    /// Returns `None` if the literal has not been interned yet.
    pub fn get_env_matches(&self, version_set_id: VersionSetId) -> Option<VariableId> {
        self.env_matches_to_variable.get(&version_set_id).copied()
    }

    /// Look up an already-interned "absent" literal for a package name.
    /// Returns `None` if the literal has not been interned yet.
    // Only used from unit tests today; the M2 cell extraction will use it
    // from non-test code, at which point this allow can be dropped.
    #[allow(dead_code)]
    pub fn get_env_absent(&self, package_name: N) -> Option<VariableId> {
        self.env_absent_to_variable.get(&package_name).copied()
    }

    /// Returns the origin of a variable. The origin describes the semantics of
    /// a variable.
    #[inline]
    pub fn origin(&self, variable_id: VariableId) -> VariableOrigin<N, S> {
        self.origins[variable_id.to_index()]
    }
}

impl VariableId {
    /// Returns the solvable id associated with the variable if it represents a
    /// solvable.
    pub(crate) fn as_solvable<N: SolverId, S: SolverId>(
        self,
        variable_map: &VariableMap<N, S>,
    ) -> Option<S> {
        variable_map.origin(self).as_solvable()
    }

    /// Returns the solvable-or-root id associated with the variable.
    pub(crate) fn as_solvable_or_root<N: SolverId, S: SolverId>(
        self,
        variable_map: &VariableMap<N, S>,
    ) -> Option<SolvableIdOrRoot<S>> {
        variable_map.origin(self).as_solvable_or_root()
    }

    /// Returns an object that can be used to format the variable.
    pub(crate) fn display<'i, I: Interner>(
        self,
        variable_map: &'i VariableMap<I::NameId, I::SolvableId>,
        interner: &'i I,
    ) -> VariableDisplay<'i, I> {
        VariableDisplay {
            variable: self,
            interner,
            variable_map,
        }
    }
}

pub(crate) struct VariableDisplay<'i, I: Interner> {
    variable: VariableId,
    interner: &'i I,
    variable_map: &'i VariableMap<I::NameId, I::SolvableId>,
}

impl<I: Interner> Display for VariableDisplay<'_, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.variable_map.origin(self.variable) {
            VariableOrigin::Root => write!(f, "root"),
            VariableOrigin::Solvable(solvable_id) => {
                write!(f, "{}", self.interner.display_solvable(solvable_id))
            }
            VariableOrigin::ForbidMultiple(name) => {
                write!(f, "forbid-multiple({})", self.interner.display_name(name))
            }
            VariableOrigin::AtLeastOne(name) => {
                write!(f, "any-of({})", self.interner.display_name(name))
            }
            VariableOrigin::EnvMatches(version_set_id) => {
                write!(
                    f,
                    "env-matches({})",
                    self.interner.display_version_set(version_set_id)
                )
            }
            VariableOrigin::EnvAbsent(name) => {
                write!(f, "env-absent({})", self.interner.display_name(name))
            }
        }
    }
}

impl<N: SolverId, S: SolverId> VariableOrigin<N, S> {
    pub(crate) fn as_solvable(&self) -> Option<S> {
        match self {
            VariableOrigin::Solvable(solvable_id) => Some(*solvable_id),
            _ => None,
        }
    }

    pub(crate) fn as_solvable_or_root(&self) -> Option<SolvableIdOrRoot<S>> {
        match self {
            VariableOrigin::Solvable(solvable_id) => Some((*solvable_id).into()),
            VariableOrigin::Root => Some(SolvableIdOrRoot::root()),
            _ => None,
        }
    }
}
