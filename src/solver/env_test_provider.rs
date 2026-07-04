//! A minimal in-crate dependency provider for solver unit tests that need
//! environment packages. The integration tests use the richer BundleBox
//! provider; this one exists so unit tests can inspect solver internals
//! (generated clauses, decisions on environment literals, cell extraction).

use std::fmt::Display;

use ahash::HashMap;

use crate::{
    Candidates, Condition, ConditionalRequirement, Dependencies, DependencyProvider,
    EnvironmentPackage, Interner, KnownDependencies, NameId, SolvableId, Solver, SolverCache,
    StringId, UniversalDependencyProvider, VersionSetId, VersionSetRelation, VersionSetUnionId,
    solver::clause::Clause, utils::Pool,
};

/// A simple half-open version range `[start, end)` used as the version
/// set of [`EnvTestProvider`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Range {
    start: u32,
    end: u32,
}

impl Range {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub fn contains(&self, version: u32) -> bool {
        version >= self.start && version < self.end
    }
}

impl Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

impl crate::utils::VersionSet for Range {
    type V = u32;
}

/// See the module documentation.
#[derive(Default)]
pub(crate) struct EnvTestProvider {
    pub pool: Pool<Range>,
    env_packages: HashMap<NameId, EnvironmentPackage>,
    dependencies: HashMap<SolvableId, KnownDependencies>,
    /// Per package name, the only candidate that may be selected
    /// ([`Candidates::locked`]).
    locked: HashMap<NameId, SolvableId>,
    /// Per package name, the candidate preferred over the sort order
    /// ([`Candidates::favored`]).
    favored: HashMap<NameId, SolvableId>,
    /// Per package name, the externally excluded candidates
    /// ([`Candidates::excluded`]).
    excluded: HashMap<NameId, Vec<(SolvableId, StringId)>>,
    /// Solvables whose [`Self::get_dependencies`] answer is
    /// [`Dependencies::Unknown`].
    unknown_deps: HashMap<SolvableId, StringId>,
}

impl EnvTestProvider {
    pub fn add_env_package(&mut self, name: &str, can_be_absent: bool) {
        let name_id = self.pool.intern_package_name(name);
        self.env_packages
            .insert(name_id, EnvironmentPackage { can_be_absent });
    }

    pub fn version_set(&self, name: &str, start: u32, end: u32) -> VersionSetId {
        let name_id = self.pool.intern_package_name(name);
        self.pool
            .intern_version_set(name_id, Range::new(start, end))
    }

    pub fn add_package(&mut self, name: &str, version: u32) -> SolvableId {
        let name_id = self.pool.intern_package_name(name);
        self.pool.intern_solvable(name_id, version)
    }

    pub fn set_dependencies(
        &mut self,
        solvable: SolvableId,
        requirements: Vec<ConditionalRequirement>,
        constrains: Vec<VersionSetId>,
    ) {
        self.dependencies.insert(
            solvable,
            KnownDependencies {
                requirements,
                constrains,
            },
        );
    }

    /// Marks `solvable` as the locked candidate of its package: no other
    /// candidate of the package may be selected.
    pub fn set_locked(&mut self, solvable: SolvableId) {
        let name = self.pool.resolve_solvable(solvable).name;
        self.locked.insert(name, solvable);
    }

    /// Marks `solvable` as the favored candidate of its package: it is tried
    /// before the (higher-version-first) sort order.
    pub fn set_favored(&mut self, solvable: SolvableId) {
        let name = self.pool.resolve_solvable(solvable).name;
        self.favored.insert(name, solvable);
    }

    /// Externally excludes `solvable` from selection, with `reason` used in
    /// error messages.
    pub fn set_excluded(&mut self, solvable: SolvableId, reason: &str) {
        let name = self.pool.resolve_solvable(solvable).name;
        let reason = self.pool.intern_string(reason);
        self.excluded
            .entry(name)
            .or_default()
            .push((solvable, reason));
    }

    /// Makes [`Self::get_dependencies`] answer [`Dependencies::Unknown`] for
    /// `solvable`, which the solver turns into an exclusion.
    pub fn set_unknown_deps(&mut self, solvable: SolvableId, reason: &str) {
        let reason = self.pool.intern_string(reason);
        self.unknown_deps.insert(solvable, reason);
    }
}

impl Interner for EnvTestProvider {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, solvable: SolvableId) -> impl Display + '_ {
        let solvable = self.pool.resolve_solvable(solvable);
        format!(
            "{}={}",
            self.pool.resolve_package_name(solvable.name),
            solvable.record
        )
    }

    fn display_name(&self, name: NameId) -> impl Display + '_ {
        self.pool.resolve_package_name(name).clone()
    }

    fn display_version_set(&self, version_set: VersionSetId) -> impl Display + '_ {
        *self.pool.resolve_version_set(version_set)
    }

    fn display_string(&self, string_id: StringId) -> impl Display + '_ {
        self.pool.resolve_string(string_id).to_owned()
    }

    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(version_set)
    }

    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.resolve_solvable(solvable).name
    }

    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.pool.resolve_version_set_union(version_set_union)
    }

    fn resolve_condition(&self, condition: crate::ConditionId) -> Condition {
        self.pool.resolve_condition(condition).clone()
    }
}

impl DependencyProvider for EnvTestProvider {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let range = self.pool.resolve_version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|&s| range.contains(self.pool.resolve_solvable(s).record) != inverse)
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let candidates = self
            .pool
            .iter_solvables()
            .filter(|(_, solvable)| solvable.name == name)
            .map(|(id, _)| id)
            .collect();
        Some(Candidates {
            candidates,
            locked: self.locked.get(&name).copied(),
            favored: self.favored.get(&name).copied(),
            excluded: self.excluded.get(&name).cloned().unwrap_or_default(),
            ..Candidates::default()
        })
    }

    async fn sort_candidates(&self, _cache: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        solvables.sort_by(|&a, &b| {
            let a = self.pool.resolve_solvable(a).record;
            let b = self.pool.resolve_solvable(b).record;
            b.cmp(&a)
        });
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        if let Some(&reason) = self.unknown_deps.get(&solvable) {
            return Dependencies::Unknown(reason);
        }
        Dependencies::Known(
            self.dependencies
                .get(&solvable)
                .cloned()
                .unwrap_or_default(),
        )
    }
}

impl UniversalDependencyProvider for EnvTestProvider {
    fn environment_package(&self, name: NameId) -> Option<EnvironmentPackage> {
        self.env_packages.get(&name).copied()
    }

    fn environment_version_set_relation(
        &self,
        a: VersionSetId,
        b: VersionSetId,
    ) -> VersionSetRelation {
        let a = self.pool.resolve_version_set(a);
        let b = self.pool.resolve_version_set(b);
        if a == b {
            VersionSetRelation::Equal
        } else if a.end <= b.start || b.end <= a.start {
            VersionSetRelation::Disjoint
        } else if b.start <= a.start && a.end <= b.end {
            // Every value matching `a` also matches `b`.
            VersionSetRelation::Subset
        } else if a.start <= b.start && b.end <= a.end {
            // Every value matching `b` also matches `a`.
            VersionSetRelation::Superset
        } else {
            VersionSetRelation::Unknown
        }
    }
}

/// Formats every clause of the solver as `Kind: lit or lit or ...` (one
/// line per clause, in allocation order) by visiting the clause literals.
pub(crate) fn dump_clauses(solver: &Solver<EnvTestProvider>) -> String {
    let state = &solver.state;
    let mut out = String::new();
    for kind in &state.clauses.kinds {
        if matches!(kind, Clause::InstallRoot) {
            out.push_str("InstallRoot\n");
            continue;
        }
        let name = match kind {
            Clause::InstallRoot => unreachable!(),
            Clause::Requires(..) => "Requires",
            Clause::Constrains(..) => "Constrains",
            Clause::ConstrainsExcluded(..) => "ConstrainsExcluded",
            Clause::ConstrainsParent(..) => "ConstrainsParent",
            Clause::ForbidMultipleInstances(..) => "ForbidMultipleInstances",
            Clause::Lock(..) => "Lock",
            Clause::Learnt(..) => "Learnt",
            Clause::Excluded(..) => "Excluded",
            Clause::AnyOf(..) => "AnyOf",
            Clause::EnvConstrains(..) => "EnvConstrains",
            Clause::EnvOracleConsistency(..) => "EnvOracleConsistency",
            Clause::EnvClause(..) => "EnvClause",
        };
        let mut literals = Vec::new();
        kind.visit_literals(
            &state.learnt_clauses,
            &state.requirement_to_sorted_candidates,
            &state.disjunctions,
            &state.env_constrains,
            &state.env_clauses,
            |literal| {
                literals.push(format!(
                    "{}{}",
                    if literal.negate() { "not " } else { "" },
                    literal
                        .variable()
                        .display(&state.variable_map, solver.provider())
                ));
            },
        );
        out.push_str(&format!("{}: {}\n", name, literals.join(" or ")));
    }
    out
}
