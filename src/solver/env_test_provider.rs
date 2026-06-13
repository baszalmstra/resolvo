//! A minimal in-crate dependency provider for solver unit tests that need
//! environment packages. The integration tests use the richer BundleBox
//! provider; this one exists so unit tests can inspect solver internals
//! (generated clauses, decisions on environment literals, cell extraction).

use std::fmt::Display;

use ahash::HashMap;

use crate::{
    Candidates, Condition, ConditionalRequirement, Dependencies, DependencyProvider,
    EnvironmentPackage, Interner, KnownDependencies, NameId, PackageCandidates, SolvableId, Solver,
    SolverCache, StringId, VersionSetId, VersionSetRelation, VersionSetUnionId,
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

    async fn get_candidates(&self, name: NameId) -> Option<PackageCandidates> {
        if let Some(env_pkg) = self.env_packages.get(&name) {
            return Some(PackageCandidates::Environment(*env_pkg));
        }
        let candidates = self
            .pool
            .iter_solvables()
            .filter(|(_, solvable)| solvable.name == name)
            .map(|(id, _)| id)
            .collect();
        Some(PackageCandidates::Candidates(Candidates {
            candidates,
            ..Candidates::default()
        }))
    }

    async fn sort_candidates(&self, _cache: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        solvables.sort_by(|&a, &b| {
            let a = self.pool.resolve_solvable(a).record;
            let b = self.pool.resolve_solvable(b).record;
            b.cmp(&a)
        });
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        Dependencies::Known(
            self.dependencies
                .get(&solvable)
                .cloned()
                .unwrap_or_default(),
        )
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
