use ahash::HashMap;
use elsa::FrozenMap;
use event_listener::Event;
use std::{any::Any, cell::RefCell, rc::Rc};

use crate::{
    Dependencies, DependencyProvider, EnvironmentPackage, HintDependenciesAvailable, Interner,
    PackageCandidates, Requirement, VersionSetId, VersionSetRelation,
    internal::{
        arena::Arena,
        id::{CandidatesId, DependenciesId},
        solver_id::Frozen,
    },
    solver_id::{IdSet, SolverId},
};

/// Universal-solve hook that classifies a package name as an environment
/// package (see [`SolverCache::set_env_classify`]). A plain `fn` pointer so
/// the cache stays generic over `D: DependencyProvider`.
pub(crate) type EnvClassifyHook<D> = fn(&D, <D as Interner>::NameId) -> Option<EnvironmentPackage>;

/// Universal-solve hook that answers the version-set relation oracle for
/// environment packages (see [`SolverCache::set_env_relation`]). A plain `fn`
/// pointer so the cache stays generic over `D: DependencyProvider`.
pub(crate) type EnvRelationHook<D> = fn(&D, VersionSetId, VersionSetId) -> VersionSetRelation;

/// Keeps a cache of previously computed and/or requested information about
/// solvables and version sets.
pub struct SolverCache<D: DependencyProvider> {
    provider: D,

    /// A mapping from package name to a list of candidates (or environment package info).
    candidates: Arena<CandidatesId, PackageCandidates<D::SolvableId>>,
    package_name_to_candidates: Frozen<<D::NameId as SolverId>::Map<Option<CandidatesId>>>,
    package_name_to_candidates_in_flight: RefCell<HashMap<D::NameId, Rc<Event>>>,

    /// A mapping of `VersionSetId` to the candidates that match that set.
    version_set_candidates: FrozenMap<VersionSetId, Vec<D::SolvableId>, ahash::RandomState>,

    /// A mapping of `VersionSetId` to the candidates that do not match that set
    /// (only candidates of the package indicated by the version set are
    /// included).
    version_set_inverse_candidates: FrozenMap<VersionSetId, Vec<D::SolvableId>, ahash::RandomState>,

    /// A mapping of [`Requirement`] to a sorted list of candidates that fulfill
    /// that requirement.
    requirement_to_sorted_candidates:
        FrozenMap<Requirement, Vec<D::SolvableId>, ahash::RandomState>,

    /// A mapping from a solvable to a list of dependencies
    solvable_dependencies: Arena<DependenciesId, Dependencies>,
    solvable_to_dependencies: Frozen<<D::SolvableId as SolverId>::Map<Option<DependenciesId>>>,

    /// A mapping that indicates that the dependencies for a particular solvable
    /// can cheaply be retrieved from the dependency provider. This
    /// information is provided by the DependencyProvider when the
    /// candidates for a package are requested.
    hint_dependencies_available: RefCell<<D::SolvableId as SolverId>::Set>,

    /// Universal-solve hook that classifies a package name as an environment
    /// package. Installed by [`Solver::solve_universal`](crate::Solver) before
    /// encoding; `None` for a plain solve, in which case every package is
    /// concrete and `get_candidates` alone determines its candidates. When
    /// installed it is consulted (and its result cached) before
    /// [`DependencyProvider::get_candidates`], so a package it classifies as
    /// an environment package is never passed to `get_candidates`. A plain
    /// `fn` pointer so the cache stays generic over `D: DependencyProvider`.
    env_classify: Option<EnvClassifyHook<D>>,

    /// Universal-solve hook answering the version-set relation oracle for
    /// environment packages. Installed by
    /// [`Solver::solve_universal`](crate::Solver) alongside
    /// [`Self::env_classify`]; `None` for a plain solve, which never interns
    /// environment literals and so never queries it. Lives on the cache (which
    /// outlives the per-solve state reset) so the encoder can thread it into
    /// [`SolverState::intern_env_matches_with_oracle_clauses`](crate::solver::SolverState).
    env_relation: Option<EnvRelationHook<D>>,

    /// Memo for [`Self::env_version_set_relation`]: the relation of two
    /// version sets is a pure function of the provider, so each ordered pair
    /// is asked of the provider at most once per cache lifetime. Keyed by
    /// the ordered pair as queried (the relation is direction-sensitive:
    /// Subset and Superset flip). Like the other provider caches it survives
    /// the per-pass `SolverState` resets of a universal enumeration, so the
    /// oracle-consistency encoding of reseed passes and the disjointness
    /// repair of every cell share the answers.
    env_relation_memo: RefCell<HashMap<(VersionSetId, VersionSetId), VersionSetRelation>>,
}

impl<D: DependencyProvider> SolverCache<D> {
    /// Constructs a new instance from a provider.
    pub fn new(provider: D) -> Self {
        Self {
            provider,
            candidates: Default::default(),
            package_name_to_candidates: Default::default(),
            package_name_to_candidates_in_flight: Default::default(),
            version_set_candidates: Default::default(),
            version_set_inverse_candidates: Default::default(),
            requirement_to_sorted_candidates: Default::default(),
            solvable_dependencies: Default::default(),
            solvable_to_dependencies: Default::default(),
            hint_dependencies_available: Default::default(),
            env_classify: None,
            env_relation: None,
            env_relation_memo: Default::default(),
        }
    }

    /// Returns a counter that increases whenever the cache learns something
    /// new from the provider (a package's candidates or a solvable's
    /// dependencies). Used by `solve_universal`'s reseed fixed-point
    /// iteration: cached dependencies gate the encoder's eager cascade, so an
    /// enumeration is only reproducible by an identical later call once a
    /// pass completes without growing the cache.
    pub(crate) fn fetch_count(&self) -> usize {
        self.candidates.len() + self.solvable_dependencies.len()
    }

    /// Returns the [`DependencyProvider`] used by this cache.
    pub fn provider(&self) -> &D {
        &self.provider
    }

    /// Installs the environment-package classification hook used by universal
    /// solving (see [`SolverCache::env_classify`]). Must be called before any
    /// candidates are cached so the classification is consistent for the whole
    /// solve.
    pub(crate) fn set_env_classify(&mut self, classify: EnvClassifyHook<D>) {
        self.env_classify = Some(classify);
    }

    /// Installs the environment version-set relation oracle hook used by
    /// universal solving (see [`SolverCache::env_relation`]).
    pub(crate) fn set_env_relation(&mut self, relation: EnvRelationHook<D>) {
        self.env_relation = Some(relation);
    }

    /// Answers the environment version-set relation oracle through the memo
    /// (see [`SolverCache::env_relation_memo`]): the provider is asked at
    /// most once per ordered pair, repeated queries hit the map.
    ///
    /// Only reachable during a universal solve, which installs the relation
    /// hook before any encoding; a missing hook is a bug.
    pub(crate) fn env_version_set_relation(
        &self,
        a: VersionSetId,
        b: VersionSetId,
    ) -> VersionSetRelation {
        let relation = self
            .env_relation
            .expect("env_relation hook must be installed for universal solves");
        *self
            .env_relation_memo
            .borrow_mut()
            .entry((a, b))
            .or_insert_with(|| relation(&self.provider, a, b))
    }

    /// Returns the candidates for the package with the given name. This will
    /// either ask the [`DependencyProvider`] for the entries or a cached
    /// value.
    ///
    /// If the provider has requested the solving process to be cancelled, the
    /// cancellation value will be returned as an `Err(...)`.
    ///
    /// When an environment-classification hook is installed (universal solves,
    /// see [`SolverCache::set_env_classify`]) it is consulted before
    /// [`DependencyProvider::get_candidates`]: a package it classifies as an
    /// environment package is cached as
    /// [`PackageCandidates::Environment`] and never fetched. Otherwise the
    /// provider's concrete candidates are cached.
    pub(crate) async fn get_or_cache_candidates(
        &self,
        package_name: D::NameId,
    ) -> Result<&PackageCandidates<D::SolvableId>, Box<dyn Any>> {
        // If we already have the candidates for this package cached we can simply
        // return
        let candidates_id = match self.package_name_to_candidates.get(package_name) {
            Some(id) => id,
            None => {
                // Since getting the candidates from the provider is a potentially blocking
                // operation, we want to check beforehand whether we should cancel the solving
                // process
                if let Some(value) = self.provider.should_cancel_with_value() {
                    return Err(value);
                }

                // Check if there is an in-flight request
                let in_flight_request = self
                    .package_name_to_candidates_in_flight
                    .borrow()
                    .get(&package_name)
                    .cloned();
                match in_flight_request {
                    Some(in_flight) => {
                        // Found an in-flight request, wait for that request to finish and return
                        // the computed result.
                        in_flight.listen().await;
                        self.package_name_to_candidates
                            .get(package_name)
                            .expect("after waiting for a request the result should be available")
                    }
                    None => {
                        // Prepare an in-flight notifier for other requests coming in.
                        self.package_name_to_candidates_in_flight
                            .borrow_mut()
                            .insert(package_name, Rc::new(Event::new()));

                        // Consult the environment-classification hook (if a
                        // universal solve installed one) before fetching: a
                        // package classified as an environment package has no
                        // concrete candidates and must not be passed to
                        // `get_candidates`. Otherwise fetch the concrete
                        // candidates from the DependencyProvider.
                        let package_candidates = match self
                            .env_classify
                            .and_then(|classify| classify(&self.provider, package_name))
                        {
                            Some(env_pkg) => PackageCandidates::Environment(env_pkg),
                            None => PackageCandidates::Candidates(
                                self.provider
                                    .get_candidates(package_name)
                                    .await
                                    .unwrap_or_default(),
                            ),
                        };

                        // Store information about which solvables dependency information is
                        // easy to retrieve.
                        if let PackageCandidates::Candidates(ref candidates) = package_candidates {
                            let mut hint_dependencies_available =
                                self.hint_dependencies_available.borrow_mut();
                            let dependencies_available_candidates =
                                match &candidates.hint_dependencies_available {
                                    HintDependenciesAvailable::None => &candidates.candidates[0..0],
                                    HintDependenciesAvailable::All => &candidates.candidates,
                                    HintDependenciesAvailable::Some(candidates) => candidates,
                                };
                            for &hint_candidate in dependencies_available_candidates.iter() {
                                hint_dependencies_available.insert(hint_candidate);
                            }
                        }

                        // Allocate an ID so we can refer to the candidates from everywhere
                        let candidates_id = self.candidates.alloc(package_candidates);
                        self.package_name_to_candidates
                            .set(package_name, Some(candidates_id));

                        // Remove the in-flight request now that we inserted the result and notify
                        // any waiters
                        let notifier = self
                            .package_name_to_candidates_in_flight
                            .borrow_mut()
                            .remove(&package_name)
                            .expect("notifier should be there");
                        notifier.notify(usize::MAX);

                        candidates_id
                    }
                }
            }
        };

        // Returns a reference from the arena
        Ok(&self.candidates[candidates_id])
    }

    /// Returns the candidates of a package that match the specified version
    /// set.
    ///
    /// If the provider has requested the solving process to be cancelled, the
    /// cancellation value will be returned as an `Err(...)`.
    pub async fn get_or_cache_matching_candidates(
        &self,
        version_set_id: VersionSetId,
    ) -> Result<&[D::SolvableId], Box<dyn Any>> {
        match self.version_set_candidates.get(&version_set_id) {
            Some(candidates) => Ok(candidates),
            None => {
                let package_name_id = self.provider.version_set_name(version_set_id);

                tracing::trace!(
                    "Getting matching candidates for package: {}",
                    self.provider.display_name(package_name_id)
                );

                let package_candidates = self.get_or_cache_candidates(package_name_id).await?;
                let candidates = match package_candidates {
                    PackageCandidates::Candidates(c) => c,
                    PackageCandidates::Environment(_) => {
                        panic!(
                            "internal error: candidates were requested for environment package \
                             '{}'; the encoder must classify environment packages before \
                             fetching candidates",
                            self.provider.display_name(package_name_id)
                        )
                    }
                };
                tracing::trace!("Got {:?} matching candidates", candidates.candidates.len());

                let matching_candidates = self
                    .provider
                    .filter_candidates(&candidates.candidates, version_set_id, false)
                    .await;

                tracing::trace!(
                    "Filtered {:?} matching candidates",
                    matching_candidates.len()
                );

                Ok(self
                    .version_set_candidates
                    .insert(version_set_id, matching_candidates))
            }
        }
    }

    /// Returns the candidates that do *not* match the specified requirement.
    ///
    /// If the provider has requested the solving process to be cancelled, the
    /// cancellation value will be returned as an `Err(...)`.
    pub async fn get_or_cache_non_matching_candidates(
        &self,
        version_set_id: VersionSetId,
    ) -> Result<&[D::SolvableId], Box<dyn Any>> {
        match self.version_set_inverse_candidates.get(&version_set_id) {
            Some(candidates) => Ok(candidates),
            None => {
                let package_name_id = self.provider.version_set_name(version_set_id);

                tracing::trace!(
                    "Getting NON-matching candidates for package: {:?}",
                    self.provider.display_name(package_name_id).to_string()
                );

                let package_candidates = self.get_or_cache_candidates(package_name_id).await?;
                let candidates = match package_candidates {
                    PackageCandidates::Candidates(c) => c,
                    PackageCandidates::Environment(_) => {
                        panic!(
                            "internal error: candidates were requested for environment package \
                             '{}'; the encoder must classify environment packages before \
                             fetching candidates",
                            self.provider.display_name(package_name_id)
                        )
                    }
                };
                tracing::trace!(
                    "Got {:?} NON-matching candidates",
                    candidates.candidates.len()
                );

                let matching_candidates: Vec<D::SolvableId> = self
                    .provider
                    .filter_candidates(&candidates.candidates, version_set_id, true)
                    .await
                    .into_iter()
                    .collect();

                tracing::trace!(
                    "Filtered {:?} matching candidates",
                    matching_candidates.len()
                );

                Ok(self
                    .version_set_inverse_candidates
                    .insert(version_set_id, matching_candidates))
            }
        }
    }

    /// Returns the candidates fulfilling the [`Requirement`] sorted from
    /// highest to lowest within each version set comprising the
    /// [`Requirement`].
    ///
    /// If the provider has requested the solving process to be cancelled, the
    /// cancellation value will be returned as an `Err(...)`.
    pub async fn get_or_cache_sorted_candidates(
        &self,
        requirement: Requirement,
    ) -> Result<&[D::SolvableId], Box<dyn Any>> {
        match requirement {
            Requirement::Single(version_set_id) => {
                self.get_or_cache_sorted_candidates_for_version_set(version_set_id)
                    .await
            }
            Requirement::Union(version_set_union_id) => {
                match self.requirement_to_sorted_candidates.get(&requirement) {
                    Some(candidates) => Ok(candidates),
                    None => {
                        let sorted_candidates = futures::future::try_join_all(
                            self.provider()
                                .version_sets_in_union(version_set_union_id)
                                .map(|version_set_id| {
                                    self.get_or_cache_sorted_candidates_for_version_set(
                                        version_set_id,
                                    )
                                }),
                        )
                        .await?
                        .into_iter()
                        .flatten()
                        .copied()
                        .collect();

                        Ok(self
                            .requirement_to_sorted_candidates
                            .insert(requirement, sorted_candidates))
                    }
                }
            }
        }
    }

    /// Returns the sorted candidates for a singular version set requirement
    /// (akin to a [`Requirement::Single`]).
    pub(crate) async fn get_or_cache_sorted_candidates_for_version_set(
        &self,
        version_set_id: VersionSetId,
    ) -> Result<&[D::SolvableId], Box<dyn Any>> {
        let requirement = version_set_id.into();
        if let Some(candidates) = self.requirement_to_sorted_candidates.get(&requirement) {
            return Ok(candidates);
        }

        let package_name_id = self.provider.version_set_name(version_set_id);
        tracing::trace!(
            "Getting sorted matching candidates for package: {:?}",
            self.provider.display_name(package_name_id).to_string()
        );

        let matching_candidates = self
            .get_or_cache_matching_candidates(version_set_id)
            .await?;
        let package_candidates = self.get_or_cache_candidates(package_name_id).await?;
        let candidates = match package_candidates {
            PackageCandidates::Candidates(c) => c,
            PackageCandidates::Environment(_) => {
                panic!(
                    "internal error: candidates were requested for environment package '{}'; \
                     the encoder must classify environment packages before fetching candidates",
                    self.provider.display_name(package_name_id)
                )
            }
        };

        // Sort all the candidates in order in which they should be tried by the solver.
        let mut sorted_candidates = Vec::with_capacity(matching_candidates.len());
        sorted_candidates.extend_from_slice(matching_candidates);
        self.provider
            .sort_candidates(self, &mut sorted_candidates)
            .await;

        // If we have a solvable that we favor, we sort that to the front. This ensures
        // that the version that is favored is picked first.
        if let Some(favored_id) = candidates.favored {
            if let Some(pos) = sorted_candidates.iter().position(|&s| s == favored_id) {
                // Move the element at `pos` to the front of the array
                sorted_candidates[0..=pos].rotate_right(1);
            }
        }

        Ok(self
            .requirement_to_sorted_candidates
            .insert(requirement, sorted_candidates))
    }

    /// Returns the dependencies of a solvable. Requests the solvables from the
    /// [`DependencyProvider`] if they are not known yet.
    ///
    /// If the provider has requested the solving process to be cancelled, the
    /// cancellation value will be returned as an `Err(...)`.
    pub async fn get_or_cache_dependencies(
        &self,
        solvable_id: D::SolvableId,
    ) -> Result<&Dependencies, Box<dyn Any>> {
        let dependencies_id = match self.solvable_to_dependencies.get(solvable_id) {
            Some(id) => id,
            None => {
                // Since getting the dependencies from the provider is a potentially blocking
                // operation, we want to check beforehand whether we should cancel the solving
                // process
                if let Some(value) = self.provider.should_cancel_with_value() {
                    return Err(value);
                }

                let dependencies = self.provider.get_dependencies(solvable_id).await;
                let dependencies_id = self.solvable_dependencies.alloc(dependencies);
                self.solvable_to_dependencies
                    .set(solvable_id, Some(dependencies_id));
                dependencies_id
            }
        };

        Ok(&self.solvable_dependencies[dependencies_id])
    }

    /// Returns true if the dependencies for the given solvable are "cheaply"
    /// available. This means either the dependency provider indicated that
    /// the dependencies for a solvable are available or the dependencies
    /// have already been requested.
    pub fn are_dependencies_available_for(&self, solvable: D::SolvableId) -> bool {
        if self.solvable_to_dependencies.get(solvable).is_some() {
            true
        } else {
            self.hint_dependencies_available.borrow().contains(solvable)
        }
    }
}
