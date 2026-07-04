//! Provides [`DependencySnapshot`], an object that can capture a snapshot of a
//! dependency provider. This can be very useful to abstract over all the
//! ecosystem specific code and provide a serializable object that can later be
//! reused to solve dependencies.
//!
//! The [`DependencySnapshot`] can be serialized to disk if the `serde` feature
//! is enabled.
//!
//! The [`DependencySnapshot`] implements the [`DependencyProvider`] trait,
//! allowing it to be used as a dependency provider for the solver.

use std::{any::Any, collections::VecDeque, fmt::Display, time::SystemTime};

use ahash::{HashMap, HashSet};
use futures::FutureExt;

use crate::{
    Candidates, Condition, ConditionId, DenseIndex, Dependencies, DependencyProvider,
    EnvironmentPackage, HintDependenciesAvailable, Interner, Mapping, NameId, PackageCandidates,
    Requirement, SolvableId, SolverCache, StringId, UniversalDependencyProvider, VersionSetId,
    VersionSetRelation, VersionSetUnionId,
};

/// A single solvable in a [`DependencySnapshot`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Solvable {
    /// The string representation of this version set.
    pub display: String,

    /// The package name of this solvable.
    pub name: NameId,

    /// The order of this solvable compared to other solvables with the same
    /// `name`.
    pub order: u32,

    /// The dependencies of the solvable
    pub dependencies: Dependencies,

    /// Whether the dependencies of this solvable are available right
    /// away or if they need to be fetched.
    pub hint_dependencies_available: bool,
}

/// Information about a single version set in a [`DependencySnapshot`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VersionSet {
    /// The package name that this version set references.
    pub name: NameId,

    /// The string representation of this version set.
    pub display: String,

    /// The candidates that match this version set.
    pub matching_candidates: HashSet<SolvableId>,
}

/// A single package in a [`DependencySnapshot`].
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Package {
    /// The name of this package
    pub name: String,

    /// All the solvables for this package.
    pub solvables: Vec<SolvableId>,

    /// Excluded packages
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub excluded: Vec<(SolvableId, StringId)>,

    /// When set, this package is an environment package for universal
    /// solving. The `solvables` then describe the concrete candidates of a
    /// simulated machine, which [`SnapshotProvider`] presents from
    /// [`get_candidates`](DependencyProvider::get_candidates) during a concrete
    /// [`solve`](crate::Solver::solve); a
    /// [`solve_universal`](crate::Solver::solve_universal) instead classifies
    /// the package as an environment package via
    /// [`environment_package`](UniversalDependencyProvider::environment_package).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub environment: Option<EnvironmentPackage>,
}

/// A single precomputed relation between two version sets that target
/// environment packages, as stored in a [`DependencySnapshot`].
///
/// The entry states how [`from`](EnvVersionSetRelation::from) relates to
/// [`to`](EnvVersionSetRelation::to); the flipped order is answered by
/// inverting `Subset`/`Superset`. [`SnapshotProvider`] canonicalizes the whole
/// table on construction and rejects any unordered pair that is listed more
/// than once (see [`SnapshotRelationError`]), so at most one entry describes
/// any pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EnvVersionSetRelation {
    /// The first version set of the pair.
    pub from: VersionSetId,

    /// The second version set of the pair.
    pub to: VersionSetId,

    /// The relation of [`from`](EnvVersionSetRelation::from) to
    /// [`to`](EnvVersionSetRelation::to).
    pub relation: VersionSetRelation,
}

/// A snapshot of an object that implements [`DependencyProvider`].
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DependencySnapshot {
    /// All the solvables in the snapshot
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Mapping::is_empty")
    )]
    pub solvables: Mapping<SolvableId, Solvable>,

    /// All the version set unions in the snapshot
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Mapping::is_empty")
    )]
    pub version_set_unions: Mapping<VersionSetUnionId, HashSet<VersionSetId>>,

    /// All the version sets in the snapshot
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Mapping::is_empty")
    )]
    pub version_sets: Mapping<VersionSetId, VersionSet>,

    /// All the packages in the snapshot
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Mapping::is_empty")
    )]
    pub packages: Mapping<NameId, Package>,

    /// All the strings in the snapshot
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Mapping::is_empty")
    )]
    pub strings: Mapping<StringId, String>,

    /// All the conditions
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Mapping::is_empty")
    )]
    pub conditions: Mapping<ConditionId, Condition>,

    /// Pairwise relations between version sets that target environment
    /// packages, precomputed by the snapshot generator. Each unordered pair
    /// may be listed at most once; the provider answers the flipped order by
    /// inverting `Subset`/`Superset`. Missing entries (and entries for pairs
    /// never queried) mean [`VersionSetRelation::Unknown`]. A pair listed more
    /// than once is rejected when the [`SnapshotProvider`] is constructed (see
    /// [`SnapshotRelationError`]).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub environment_version_set_relations: Vec<EnvVersionSetRelation>,
}

impl DependencySnapshot {
    /// Construct a new [`DependencySnapshot`] from a [`DependencyProvider`]
    /// capturing its entire state. This function will recursively call all
    /// methods on the provider with the given `names`, `version_sets`, and
    /// `solvables`.
    ///
    /// This function assumes that the passed in [`DependencyProvider`] does not
    /// yield and will block until the snapshot is fully constructed. If you
    /// want to construct a snapshot from a provider that might yield, use
    /// [`Self::from_provider_async`] instead.
    pub fn from_provider(
        provider: impl DependencyProvider<NameId = NameId, SolvableId = SolvableId>,
        names: impl IntoIterator<Item = NameId>,
        version_sets: impl IntoIterator<Item = VersionSetId>,
        solvables: impl IntoIterator<Item = SolvableId>,
    ) -> Result<Self, Box<dyn Any>> {
        Self::from_provider_async(provider, names, version_sets, solvables)
            .now_or_never()
            .expect(
                "the DependencyProvider seems to have yielded. Use `from_provider_async` instead.",
            )
    }

    /// Construct a new [`DependencySnapshot`] from a [`DependencyProvider`]
    /// capturing its entire state. This function will recursively call all
    /// methods on the provider with the given `names`, `version_sets`, and
    /// `solvables`.
    pub async fn from_provider_async(
        provider: impl DependencyProvider<NameId = NameId, SolvableId = SolvableId>,
        names: impl IntoIterator<Item = NameId>,
        version_sets: impl IntoIterator<Item = VersionSetId>,
        solvables: impl IntoIterator<Item = SolvableId>,
    ) -> Result<Self, Box<dyn Any>> {
        #[derive(Hash, Copy, Clone, Debug, Eq, PartialEq)]
        pub enum Element {
            Solvable(SolvableId),
            VersionSet(VersionSetId),
            Package(NameId),
            String(StringId),
            Condition(ConditionId),
        }

        let cache = SolverCache::new(provider);

        let mut result = Self {
            solvables: Mapping::new(),
            version_set_unions: Mapping::new(),
            version_sets: Mapping::new(),
            packages: Mapping::new(),
            strings: Mapping::new(),
            conditions: Mapping::new(),
            environment_version_set_relations: Vec::new(),
        };

        let mut queue = names
            .into_iter()
            .map(Element::Package)
            .chain(version_sets.into_iter().map(Element::VersionSet))
            .chain(solvables.into_iter().map(Element::Solvable))
            .collect::<VecDeque<_>>();
        let mut seen = queue.iter().copied().collect::<HashSet<_>>();
        let mut available_hints = HashSet::default();
        while let Some(element) = queue.pop_front() {
            match element {
                Element::Package(name) => {
                    let display = cache.provider().display_name(name).to_string();
                    let package_candidates = cache.get_or_cache_candidates(name).await?;
                    let candidates = match package_candidates {
                        PackageCandidates::Candidates(c) => c,
                        PackageCandidates::Environment(_) => {
                            return Err(Box::new(format!(
                                "cannot snapshot environment package '{}'; \
                                 snapshotting environment packages is not supported",
                                display
                            )));
                        }
                    };
                    for solvable in candidates.candidates.iter() {
                        if seen.insert(Element::Solvable(*solvable)) {
                            queue.push_back(Element::Solvable(*solvable));
                        }
                    }
                    for &(excluded, reason) in &candidates.excluded {
                        if seen.insert(Element::Solvable(excluded)) {
                            queue.push_back(Element::Solvable(excluded));
                        }
                        if seen.insert(Element::String(reason)) {
                            queue.push_back(Element::String(reason));
                        }
                    }

                    let hint_dependencies_available = match &candidates.hint_dependencies_available
                    {
                        HintDependenciesAvailable::None => &candidates.candidates[0..0],
                        HintDependenciesAvailable::All => &candidates.candidates,
                        HintDependenciesAvailable::Some(candidates) => candidates,
                    };
                    available_hints.extend(hint_dependencies_available.iter().copied());

                    let package = Package {
                        name: display,
                        solvables: candidates.candidates.clone(),
                        excluded: candidates.excluded.clone(),
                        environment: None,
                    };

                    result.packages.insert(name, package);
                }
                Element::Solvable(solvable_id) => {
                    let name = cache.provider().solvable_name(solvable_id);
                    if seen.insert(Element::Package(name)) {
                        queue.push_back(Element::Package(name));
                    };

                    let dependencies = cache.get_or_cache_dependencies(solvable_id).await?;
                    match &dependencies {
                        Dependencies::Unknown(reason) => {
                            if seen.insert(Element::String(*reason)) {
                                queue.push_back(Element::String(*reason));
                            }
                        }
                        Dependencies::Known(deps) => {
                            for &dep in deps.constrains.iter() {
                                if seen.insert(Element::VersionSet(dep)) {
                                    queue.push_back(Element::VersionSet(dep));
                                }
                            }

                            for requirement in deps.requirements.iter() {
                                if let Some(condition) = requirement.condition {
                                    if seen.insert(Element::Condition(condition)) {
                                        queue.push_back(Element::Condition(condition))
                                    }
                                }
                                match requirement.requirement {
                                    Requirement::Single(version_set) => {
                                        if seen.insert(Element::VersionSet(version_set)) {
                                            queue.push_back(Element::VersionSet(version_set));
                                        }
                                    }
                                    Requirement::Union(version_set_union_id) => {
                                        let version_sets: HashSet<_> = cache
                                            .provider()
                                            .version_sets_in_union(version_set_union_id)
                                            .collect();

                                        for &version_set in version_sets.iter() {
                                            if seen.insert(Element::VersionSet(version_set)) {
                                                queue.push_back(Element::VersionSet(version_set));
                                            }
                                        }

                                        result
                                            .version_set_unions
                                            .insert(version_set_union_id, version_sets);
                                    }
                                }
                            }
                        }
                    }

                    let solvable = Solvable {
                        display: cache.provider().display_solvable(solvable_id).to_string(),
                        name,
                        order: 0,
                        dependencies: dependencies.clone(),
                        hint_dependencies_available: cache
                            .are_dependencies_available_for(solvable_id),
                    };

                    result.solvables.insert(solvable_id, solvable);
                }
                Element::String(string_id) => {
                    let string = cache.provider().display_string(string_id).to_string();
                    result.strings.insert(string_id, string);
                }
                Element::VersionSet(version_set_id) => {
                    let name = cache.provider().version_set_name(version_set_id);
                    if seen.insert(Element::Package(name)) {
                        queue.push_back(Element::Package(name));
                    };

                    let display = cache
                        .provider()
                        .display_version_set(version_set_id)
                        .to_string();
                    let matching_candidates = cache
                        .get_or_cache_matching_candidates(version_set_id)
                        .await?;

                    for matching_candidate in matching_candidates.iter() {
                        if seen.insert(Element::Solvable(*matching_candidate)) {
                            queue.push_back(Element::Solvable(*matching_candidate));
                        }
                    }

                    let version_set = VersionSet {
                        name,
                        display,
                        matching_candidates: matching_candidates.iter().copied().collect(),
                    };

                    result.version_sets.insert(version_set_id, version_set);
                }
                Element::Condition(condition_id) => {
                    let condition = cache.provider().resolve_condition(condition_id);
                    match condition {
                        Condition::Requirement(version_set) => {
                            if seen.insert(Element::VersionSet(version_set)) {
                                queue.push_back(Element::VersionSet(version_set))
                            }
                        }
                        Condition::Binary(_, lhs, rhs) => {
                            for cond in [lhs, rhs] {
                                if seen.insert(Element::Condition(cond)) {
                                    queue.push_back(Element::Condition(cond))
                                }
                            }
                        }
                    }
                    result.conditions.insert(condition_id, condition);
                }
            }
        }

        // Compute the order of the solvables
        for (_, package) in result.packages.iter() {
            let mut solvables = package.solvables.clone();
            cache
                .provider()
                .sort_candidates(&cache, &mut solvables)
                .await;

            for (order, solvable) in solvables.into_iter().enumerate() {
                let solvable = result
                    .solvables
                    .get_mut(solvable)
                    .expect("missing solvable");
                solvable.order = order as u32;
            }
        }

        Ok(result)
    }

    /// Returns an object that implements the [`DependencyProvider`] trait for
    /// this snapshot.
    pub fn provider(&self) -> SnapshotProvider<'_> {
        SnapshotProvider::new(self)
    }
}

/// The error returned by [`SnapshotProvider::try_new`] when a
/// [`DependencySnapshot`]'s environment relation table
/// (`environment_version_set_relations`) is inconsistent.
///
/// Every entry is canonicalized to a single key order (the lower version set
/// id first, inverting `Subset`/`Superset` when the stored pair is flipped)
/// before the table is indexed, so two entries that describe the same
/// unordered pair collide. Any collision is rejected: an exact restatement as
/// [`DuplicatePair`](SnapshotRelationError::DuplicatePair) and an incompatible
/// restatement as
/// [`ContradictoryRelation`](SnapshotRelationError::ContradictoryRelation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SnapshotRelationError {
    /// Two entries describe the same unordered version set pair with the same
    /// canonicalized relation. Each pair must be listed at most once.
    DuplicatePair {
        /// The lower-id version set of the colliding pair.
        first: VersionSetId,
        /// The higher-id version set of the colliding pair.
        second: VersionSetId,
        /// The relation both entries agree on (from `first` to `second`).
        relation: VersionSetRelation,
    },

    /// Two entries describe the same unordered version set pair with
    /// conflicting relations, leaving the table internally inconsistent.
    ContradictoryRelation {
        /// The lower-id version set of the colliding pair.
        first: VersionSetId,
        /// The higher-id version set of the colliding pair.
        second: VersionSetId,
        /// The relation implied by the earlier entry (from `first` to
        /// `second`).
        left: VersionSetRelation,
        /// The relation implied by the later entry (from `first` to `second`).
        right: VersionSetRelation,
    },
}

impl Display for SnapshotRelationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotRelationError::DuplicatePair {
                first,
                second,
                relation,
            } => write!(
                f,
                "the environment relation table lists the version set pair \
                 ({first:?}, {second:?}) more than once (relation {relation:?}); each \
                 unordered pair may appear at most once",
            ),
            SnapshotRelationError::ContradictoryRelation {
                first,
                second,
                left,
                right,
            } => write!(
                f,
                "the environment relation table gives contradictory relations for the \
                 version set pair ({first:?}, {second:?}): {left:?} and {right:?}",
            ),
        }
    }
}

impl std::error::Error for SnapshotRelationError {}

/// Inverts a relation for the flipped key order: `Subset` and `Superset` swap,
/// the symmetric relations (`Disjoint`, `Equal`, `Unknown`) are unchanged.
fn invert_relation(relation: VersionSetRelation) -> VersionSetRelation {
    match relation {
        VersionSetRelation::Subset => VersionSetRelation::Superset,
        VersionSetRelation::Superset => VersionSetRelation::Subset,
        symmetric => symmetric,
    }
}

/// Canonicalizes a relation entry to a single key order: the lower version set
/// id first. The returned relation reads from the first element of the key to
/// the second (inverted when the pair had to be flipped).
fn canonicalize_relation(
    a: VersionSetId,
    b: VersionSetId,
    relation: VersionSetRelation,
) -> ((VersionSetId, VersionSetId), VersionSetRelation) {
    if a.to_index() <= b.to_index() {
        ((a, b), relation)
    } else {
        ((b, a), invert_relation(relation))
    }
}

/// Provides a [`DependencyProvider`] implementation for a
/// [`DependencySnapshot`].
pub struct SnapshotProvider<'s> {
    snapshot: &'s DependencySnapshot,

    additional_version_sets: Vec<VersionSet>,
    stop_time: Option<SystemTime>,

    /// Lookup index over the snapshot's precomputed relation table.
    relations: HashMap<(VersionSetId, VersionSetId), VersionSetRelation>,

    /// The first version set id available for `additional_version_sets`: one
    /// past the highest id used by the snapshot. `Mapping::max` returns the
    /// highest inserted id itself, so using it directly would shadow the
    /// snapshot's last version set.
    additional_base: usize,

    /// Conditions interned through [`Self::add_condition`], mirroring
    /// `additional_version_sets`.
    additional_conditions: Vec<Condition>,

    /// The first condition id available for `additional_conditions`: one past
    /// the highest id used by the snapshot (see `additional_base`).
    additional_condition_base: usize,
}

impl<'s> From<&'s DependencySnapshot> for SnapshotProvider<'s> {
    fn from(value: &'s DependencySnapshot) -> Self {
        Self::new(value)
    }
}

impl<'s> SnapshotProvider<'s> {
    /// Create a new [`SnapshotProvider`] from a [`DependencySnapshot`].
    ///
    /// # Panics
    ///
    /// Panics with a descriptive [`SnapshotRelationError`] message if the
    /// snapshot's `environment_version_set_relations` table lists the same
    /// unordered version set pair more than once. Use
    /// [`try_new`](Self::try_new) to handle that case without panicking. A
    /// table produced by [`DependencySnapshot::from_provider`] is always
    /// consistent; only a hand-built or deserialized snapshot can violate this.
    pub fn new(snapshot: &'s DependencySnapshot) -> Self {
        Self::try_new(snapshot).unwrap_or_else(|err| panic!("{err}"))
    }

    /// Like [`new`](Self::new) but returns a [`SnapshotRelationError`] instead
    /// of panicking when the snapshot's environment relation table is
    /// inconsistent.
    ///
    /// The table is canonicalized here: every entry is keyed by its unordered
    /// version set pair (the lower id first, inverting `Subset`/`Superset` when
    /// the stored pair is flipped) and a pair listed twice is rejected. Once
    /// this returns `Ok`, the relation oracle answers from a table holding at
    /// most one entry per unordered pair.
    pub fn try_new(snapshot: &'s DependencySnapshot) -> Result<Self, SnapshotRelationError> {
        let mut relations: HashMap<(VersionSetId, VersionSetId), VersionSetRelation> =
            HashMap::default();
        for entry in &snapshot.environment_version_set_relations {
            let ((first, second), relation) =
                canonicalize_relation(entry.from, entry.to, entry.relation);
            if let Some(existing) = relations.insert((first, second), relation) {
                if existing == relation {
                    return Err(SnapshotRelationError::DuplicatePair {
                        first,
                        second,
                        relation,
                    });
                }
                return Err(SnapshotRelationError::ContradictoryRelation {
                    first,
                    second,
                    left: existing,
                    right: relation,
                });
            }
        }
        let additional_base = if snapshot.version_sets.is_empty() {
            0
        } else {
            snapshot.version_sets.max() + 1
        };
        let additional_condition_base = if snapshot.conditions.is_empty() {
            0
        } else {
            snapshot.conditions.max() + 1
        };
        Ok(Self {
            snapshot,
            additional_version_sets: Vec::new(),
            stop_time: None,
            relations,
            additional_base,
            additional_conditions: Vec::new(),
            additional_condition_base,
        })
    }

    /// Adds a timeout to this provider. Solving will stop when the specified
    /// time is reached.
    #[must_use]
    pub fn with_timeout(self, stop_time: SystemTime) -> Self {
        Self {
            stop_time: Some(stop_time),
            ..self
        }
    }

    /// Adds another requirement that matches any version of a package.
    /// If you use "*" as the matcher, it will match any version of the package.
    pub fn add_package_requirement(&mut self, name: NameId, matcher: &str) -> VersionSetId {
        let id = self.additional_base + self.additional_version_sets.len();
        let package = self.package(name);

        let matching_candidates = package
            .solvables
            .iter()
            .copied()
            .filter(|&s| matcher == "*" || self.solvable(s).display.contains(matcher))
            .collect();

        self.additional_version_sets.push(VersionSet {
            name,
            display: matcher.to_string(),
            matching_candidates,
        });

        VersionSetId::from_index(id)
    }

    /// Interns an additional [`Condition`] that is not part of the snapshot,
    /// e.g. to synthesize conditional requirements for benchmarking. Interning
    /// is deterministic: replaying the same calls in the same order onto
    /// identically constructed providers yields identical ids (mirroring
    /// [`Self::add_package_requirement`]).
    pub fn add_condition(&mut self, condition: Condition) -> ConditionId {
        let id = self.additional_condition_base + self.additional_conditions.len();
        self.additional_conditions.push(condition);
        ConditionId::from_index(id)
    }

    fn solvable(&self, solvable: SolvableId) -> &Solvable {
        self.snapshot
            .solvables
            .get(solvable)
            .expect("missing solvable")
    }

    fn package(&self, name_id: NameId) -> &Package {
        self.snapshot
            .packages
            .get(name_id)
            .expect("missing package")
    }

    fn string(&self, string_id: StringId) -> &String {
        self.snapshot
            .strings
            .get(string_id)
            .expect("missing string")
    }

    fn version_set(&self, version_set: VersionSetId) -> &VersionSet {
        let idx = version_set.to_index();
        if idx >= self.additional_base {
            &self.additional_version_sets[idx - self.additional_base]
        } else {
            self.snapshot
                .version_sets
                .get(version_set)
                .expect("missing version set")
        }
    }
}

impl Interner for SnapshotProvider<'_> {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, solvable: SolvableId) -> impl Display + '_ {
        &self.solvable(solvable).display
    }

    fn display_name(&self, name: NameId) -> impl Display + '_ {
        &self.package(name).name
    }

    fn display_version_set(&self, version_set: VersionSetId) -> impl Display + '_ {
        &self.version_set(version_set).display
    }

    fn display_string(&self, string_id: StringId) -> impl Display + '_ {
        self.string(string_id)
    }

    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.version_set(version_set).name
    }

    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.solvable(solvable).name
    }

    fn version_sets_in_union(
        &self,
        version_set_union_id: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.snapshot
            .version_set_unions
            .get(version_set_union_id)
            .expect("missing constraint")
            .iter()
            .copied()
    }

    fn resolve_condition(&self, condition: ConditionId) -> Condition {
        let idx = condition.to_index();
        if idx >= self.additional_condition_base {
            self.additional_conditions[idx - self.additional_condition_base].clone()
        } else {
            self.snapshot
                .conditions
                .get(condition)
                .expect("missing condition")
                .clone()
        }
    }
}

impl DependencyProvider for SnapshotProvider<'_> {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let version_set = self.version_set(version_set);
        candidates
            .iter()
            .copied()
            .filter(|c| version_set.matching_candidates.contains(c) != inverse)
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let package = self.package(name);
        Some(Candidates {
            candidates: package.solvables.clone(),
            favored: None,
            locked: None,
            excluded: package.excluded.clone(),
            hint_dependencies_available: HintDependenciesAvailable::Some(
                package
                    .solvables
                    .iter()
                    .copied()
                    .filter(|&s| self.solvable(s).hint_dependencies_available)
                    .collect(),
            ),
        })
    }

    async fn sort_candidates(&self, _solver: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        solvables.sort_by_key(|&s| self.solvable(s).order);
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        self.solvable(solvable).dependencies.clone()
    }

    fn should_cancel_with_value(&self) -> Option<Box<dyn Any>> {
        if let Some(stop_time) = &self.stop_time {
            if SystemTime::now() > *stop_time {
                return Some(Box::new(()));
            }
        }
        None
    }
}

impl UniversalDependencyProvider for SnapshotProvider<'_> {
    fn environment_package(&self, name: NameId) -> Option<EnvironmentPackage> {
        self.package(name).environment
    }

    fn environment_version_set_relation(
        &self,
        a: VersionSetId,
        b: VersionSetId,
    ) -> VersionSetRelation {
        if a == b {
            return VersionSetRelation::Equal;
        }
        if let Some(&relation) = self.relations.get(&(a, b)) {
            return relation;
        }
        if let Some(&relation) = self.relations.get(&(b, a)) {
            return invert_relation(relation);
        }
        VersionSetRelation::Unknown
    }
}
