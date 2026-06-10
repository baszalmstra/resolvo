//! Property test for universal solving (design doc, milestone M3).
//!
//! A deterministic seeded generator produces small random universes with
//! environment packages, concrete packages, plain and conditional
//! dependencies, constrains and a random environment model. Each universe is
//! solved with [`Solver::solve_universal`] and the result is checked against
//! the generated metadata directly:
//!
//! - On success: `verify()` passes, and for a sample of concrete environments
//!   drawn from the model, `project()` returns the unique matching cell whose
//!   solvable set is a valid solution (every active requirement satisfied, no
//!   constraint violated, at most one solvable per package name). The merged
//!   presence view and the conditional edges are cross-checked against the
//!   projection.
//! - On failure ([`UniversalFailure::Unsolvable`]): for a sample of concrete
//!   environments inside the witness cell, brute-force enumeration over all
//!   install sets (at most one solvable per package) confirms that no valid
//!   solution exists.
//!
//! This test lives in-crate (not in `tests/`) because it drives
//! [`EnvTestProvider`], which is deliberately `cfg(test)`-private.

use crate::{
    Condition, ConditionId, ConditionalRequirement, EnvLiteral, EnvLiteralKind, LogicalOperator,
    NameId, Solver, UniversalFailure, UniversalProblem, Violation,
    solver::env_test_provider::EnvTestProvider,
};

/// Number of seeds to run. Tuned so that the whole test finishes within a few
/// seconds in debug builds.
const SEED_COUNT: u64 = 1000;

/// Maximum number of accepted environment samples checked per solved seed.
const MAX_SAMPLES_PER_SEED: usize = 20;

/// Number of sampling attempts per seed; attempts whose sample does not
/// satisfy the environment model are discarded.
const SAMPLE_ATTEMPTS: usize = 200;

/// Environment package values are sampled from `0..ENV_VALUE_SPACE`.
const ENV_VALUE_SPACE: u32 = 11;

// ===========================================================================
// Deterministic RNG (xorshift64*), no external dependencies.
// ===========================================================================

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Mix the seed so that consecutive seeds produce unrelated streams,
        // and force it to be non-zero (xorshift has a fixed point at 0).
        Rng(seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0x2545_F491_4F6C_DD1D)
            | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A uniform-ish value in `0..n`. `n` must be non-zero.
    fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % u64::from(n)) as u32
    }

    /// A uniform-ish value in `lo..hi`. `lo < hi` must hold.
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.below(hi - lo)
    }

    /// Returns true with probability `num / den`.
    fn chance(&mut self, num: u32, den: u32) -> bool {
        self.below(den) < num
    }
}

// ===========================================================================
// Generated universe model (the generator-side mirror of the provider).
// ===========================================================================

struct Universe {
    env_packages: Vec<EnvPkg>,
    packages: Vec<ConcretePkg>,
    root_requirements: Vec<GenRequirement>,
    /// CNF over environment literals; each inner vec is a disjunction.
    model: Vec<Vec<GenModelLiteral>>,
}

struct EnvPkg {
    can_be_absent: bool,
}

struct ConcretePkg {
    /// Index `i` holds version `i + 1`.
    versions: Vec<PkgVersion>,
}

struct PkgVersion {
    requirements: Vec<GenRequirement>,
    constrains: Vec<GenConstrain>,
}

struct GenRequirement {
    target: GenTarget,
    /// Half-open version range `[lo, hi)`.
    lo: u32,
    hi: u32,
    condition: Option<GenCondition>,
}

#[derive(Clone, Copy)]
enum GenTarget {
    Concrete(usize),
    Env(usize),
}

enum GenCondition {
    /// The environment package `pkg` is present with a value in `[lo, hi)`.
    Env {
        pkg: usize,
        lo: u32,
        hi: u32,
    },
    And(Box<GenCondition>, Box<GenCondition>),
    Or(Box<GenCondition>, Box<GenCondition>),
}

enum GenConstrain {
    /// If the parent is installed, env package `pkg` must be absent or have a
    /// value in `[lo, hi)`.
    Env { pkg: usize, lo: u32, hi: u32 },
    /// If the parent is installed and a solvable of `pkg` is installed, its
    /// version must be in `[lo, hi)`.
    Concrete { pkg: usize, lo: u32, hi: u32 },
}

enum GenModelLiteral {
    Matches {
        pkg: usize,
        lo: u32,
        hi: u32,
        positive: bool,
    },
    Absent {
        pkg: usize,
        positive: bool,
    },
}

fn env_name(index: usize) -> String {
    format!("env{index}")
}

fn pkg_name(index: usize) -> String {
    format!("pkg{index}")
}

/// A half-open range over the environment value space, biased towards
/// moderately sized ranges so conditions overlap interestingly.
fn gen_env_range(rng: &mut Rng) -> (u32, u32) {
    let lo = rng.range(0, ENV_VALUE_SPACE - 2);
    let hi = rng.range(lo + 1, ENV_VALUE_SPACE);
    (lo, hi)
}

fn gen_condition(rng: &mut Rng, env_count: usize, depth: u32) -> GenCondition {
    if depth == 0 || rng.chance(3, 5) {
        let (lo, hi) = gen_env_range(rng);
        GenCondition::Env {
            pkg: rng.below(env_count as u32) as usize,
            lo,
            hi,
        }
    } else {
        let lhs = Box::new(gen_condition(rng, env_count, depth - 1));
        let rhs = Box::new(gen_condition(rng, env_count, depth - 1));
        if rng.chance(1, 2) {
            GenCondition::And(lhs, rhs)
        } else {
            GenCondition::Or(lhs, rhs)
        }
    }
}

fn gen_universe(rng: &mut Rng) -> Universe {
    let env_count = rng.range(1, 4) as usize;
    let env_packages = (0..env_count)
        .map(|_| EnvPkg {
            can_be_absent: rng.chance(1, 2),
        })
        .collect::<Vec<_>>();

    let pkg_count = rng.range(2, 6) as usize;
    let mut packages = Vec::new();
    for p in 0..pkg_count {
        let version_count = rng.range(1, 4) as usize;
        let mut versions = Vec::new();
        for _ in 0..version_count {
            let mut requirements = Vec::new();
            let mut constrains = Vec::new();

            // Concrete dependencies, some guarded by environment conditions.
            // Ranges are biased wide so that a decent share of the universes
            // is solvable; narrow ranges (often empty against one-version
            // packages) still occur and exercise the unsolvable path.
            for _ in 0..rng.below(3) {
                let mut target = rng.below(pkg_count as u32) as usize;
                if target == p {
                    target = (target + 1) % pkg_count;
                }
                let lo = if rng.chance(1, 2) { 1 } else { rng.range(1, 4) };
                let hi = if rng.chance(1, 2) {
                    4
                } else {
                    rng.range(lo + 1, 5)
                };
                let condition = if rng.chance(1, 2) {
                    let depth = rng.range(1, 3);
                    Some(gen_condition(rng, env_count, depth))
                } else {
                    None
                };
                requirements.push(GenRequirement {
                    target: GenTarget::Concrete(target),
                    lo,
                    hi,
                    condition,
                });
            }

            // A direct requirement on an environment package.
            if rng.chance(1, 5) {
                let (lo, hi) = gen_env_range(rng);
                let condition = if rng.chance(1, 4) {
                    Some(gen_condition(rng, env_count, 1))
                } else {
                    None
                };
                requirements.push(GenRequirement {
                    target: GenTarget::Env(rng.below(env_count as u32) as usize),
                    lo,
                    hi,
                    condition,
                });
            }

            // Constrains on environment packages.
            if rng.chance(1, 3) {
                let (lo, hi) = gen_env_range(rng);
                constrains.push(GenConstrain::Env {
                    pkg: rng.below(env_count as u32) as usize,
                    lo,
                    hi,
                });
            }

            // Constrains on concrete packages.
            if rng.chance(1, 5) {
                let lo = rng.range(1, 4);
                let hi = rng.range(lo + 1, 5);
                constrains.push(GenConstrain::Concrete {
                    pkg: rng.below(pkg_count as u32) as usize,
                    lo,
                    hi,
                });
            }

            versions.push(PkgVersion {
                requirements,
                constrains,
            });
        }
        packages.push(ConcretePkg { versions });
    }

    // Root requirements: a non-empty subset of the concrete packages, each
    // with the full version range.
    let mut root_requirements = Vec::new();
    for p in 0..pkg_count {
        if rng.chance(2, 3) {
            root_requirements.push(GenRequirement {
                target: GenTarget::Concrete(p),
                lo: 1,
                hi: 4,
                condition: None,
            });
        }
    }
    if root_requirements.is_empty() {
        root_requirements.push(GenRequirement {
            target: GenTarget::Concrete(0),
            lo: 1,
            hi: 4,
            condition: None,
        });
    }

    // The environment model: one or two disjunctions of one to three signed
    // environment literals. Absent literals only for absentable packages.
    let mut model = Vec::new();
    for _ in 0..rng.range(1, 3) {
        let mut disjunction = Vec::new();
        for _ in 0..rng.range(1, 4) {
            let pkg = rng.below(env_count as u32) as usize;
            let positive = rng.chance(3, 4);
            if env_packages[pkg].can_be_absent && rng.chance(1, 3) {
                disjunction.push(GenModelLiteral::Absent { pkg, positive });
            } else {
                let (lo, hi) = gen_env_range(rng);
                disjunction.push(GenModelLiteral::Matches {
                    pkg,
                    lo,
                    hi,
                    positive,
                });
            }
        }
        model.push(disjunction);
    }

    Universe {
        env_packages,
        packages,
        root_requirements,
        model,
    }
}

// ===========================================================================
// Building the provider and the problem from a universe.
// ===========================================================================

fn build_provider(universe: &Universe) -> EnvTestProvider {
    let mut provider = EnvTestProvider::default();
    for (e, env) in universe.env_packages.iter().enumerate() {
        provider.add_env_package(&env_name(e), env.can_be_absent);
    }

    // Intern all solvables before wiring dependencies, then attach the
    // dependencies of each version.
    let mut solvable_ids = Vec::new();
    for (p, pkg) in universe.packages.iter().enumerate() {
        let ids = (1..=pkg.versions.len() as u32)
            .map(|v| provider.add_package(&pkg_name(p), v))
            .collect::<Vec<_>>();
        solvable_ids.push(ids);
    }
    for (p, pkg) in universe.packages.iter().enumerate() {
        for (vi, version) in pkg.versions.iter().enumerate() {
            let requirements = version
                .requirements
                .iter()
                .map(|requirement| build_requirement(&provider, requirement))
                .collect();
            let constrains = version
                .constrains
                .iter()
                .map(|constrain| match *constrain {
                    GenConstrain::Env { pkg, lo, hi } => {
                        provider.version_set(&env_name(pkg), lo, hi)
                    }
                    GenConstrain::Concrete { pkg, lo, hi } => {
                        provider.version_set(&pkg_name(pkg), lo, hi)
                    }
                })
                .collect();
            provider.set_dependencies(solvable_ids[p][vi], requirements, constrains);
        }
    }
    provider
}

fn build_requirement(
    provider: &EnvTestProvider,
    requirement: &GenRequirement,
) -> ConditionalRequirement {
    let version_set = match requirement.target {
        GenTarget::Concrete(p) => {
            provider.version_set(&pkg_name(p), requirement.lo, requirement.hi)
        }
        GenTarget::Env(e) => provider.version_set(&env_name(e), requirement.lo, requirement.hi),
    };
    ConditionalRequirement {
        condition: requirement
            .condition
            .as_ref()
            .map(|condition| intern_condition(provider, condition)),
        requirement: version_set.into(),
    }
}

fn intern_condition(provider: &EnvTestProvider, condition: &GenCondition) -> ConditionId {
    match condition {
        GenCondition::Env { pkg, lo, hi } => {
            let version_set = provider.version_set(&env_name(*pkg), *lo, *hi);
            provider
                .pool
                .intern_condition(Condition::Requirement(version_set))
        }
        GenCondition::And(lhs, rhs) => {
            let lhs = intern_condition(provider, lhs);
            let rhs = intern_condition(provider, rhs);
            provider
                .pool
                .intern_condition(Condition::Binary(LogicalOperator::And, lhs, rhs))
        }
        GenCondition::Or(lhs, rhs) => {
            let lhs = intern_condition(provider, lhs);
            let rhs = intern_condition(provider, rhs);
            provider
                .pool
                .intern_condition(Condition::Binary(LogicalOperator::Or, lhs, rhs))
        }
    }
}

fn build_environment_model(
    provider: &EnvTestProvider,
    universe: &Universe,
) -> Vec<Vec<(EnvLiteral<NameId>, bool)>> {
    universe
        .model
        .iter()
        .map(|disjunction| {
            disjunction
                .iter()
                .map(|literal| match *literal {
                    GenModelLiteral::Matches {
                        pkg,
                        lo,
                        hi,
                        positive,
                    } => {
                        let version_set = provider.version_set(&env_name(pkg), lo, hi);
                        (
                            EnvLiteral {
                                package: provider.pool.intern_package_name(env_name(pkg)),
                                kind: EnvLiteralKind::Matches(version_set),
                            },
                            positive,
                        )
                    }
                    GenModelLiteral::Absent { pkg, positive } => (
                        EnvLiteral {
                            package: provider.pool.intern_package_name(env_name(pkg)),
                            kind: EnvLiteralKind::Absent,
                        },
                        positive,
                    ),
                })
                .collect()
        })
        .collect()
}

// ===========================================================================
// The independent validity checker (works on the generated universe, never
// on solver state).
// ===========================================================================

/// A concrete environment: per env package, the value or `None` for absent.
type EnvSample = Vec<Option<u32>>;

/// An install set: per concrete package, the installed version or `None`.
type InstallSet = Vec<Option<u32>>;

fn in_range(value: u32, lo: u32, hi: u32) -> bool {
    value >= lo && value < hi
}

fn eval_condition(condition: &GenCondition, env: &EnvSample) -> bool {
    match condition {
        GenCondition::Env { pkg, lo, hi } => env[*pkg].is_some_and(|v| in_range(v, *lo, *hi)),
        GenCondition::And(lhs, rhs) => eval_condition(lhs, env) && eval_condition(rhs, env),
        GenCondition::Or(lhs, rhs) => eval_condition(lhs, env) || eval_condition(rhs, env),
    }
}

fn requirement_satisfied(
    requirement: &GenRequirement,
    installed: &InstallSet,
    env: &EnvSample,
) -> bool {
    match requirement.target {
        GenTarget::Concrete(p) => {
            installed[p].is_some_and(|v| in_range(v, requirement.lo, requirement.hi))
        }
        GenTarget::Env(e) => env[e].is_some_and(|v| in_range(v, requirement.lo, requirement.hi)),
    }
}

/// Checks whether `installed` is a valid solution of `universe` in the
/// concrete environment `env`: all root requirements satisfied, every active
/// requirement of every installed solvable satisfied, and no constraint of
/// any installed solvable violated.
fn is_valid_solution(universe: &Universe, installed: &InstallSet, env: &EnvSample) -> bool {
    for requirement in &universe.root_requirements {
        if !requirement_satisfied(requirement, installed, env) {
            return false;
        }
    }
    for (p, version) in installed.iter().enumerate() {
        let Some(version) = version else { continue };
        let metadata = &universe.packages[p].versions[(*version - 1) as usize];
        for requirement in &metadata.requirements {
            let active = requirement
                .condition
                .as_ref()
                .is_none_or(|condition| eval_condition(condition, env));
            if active && !requirement_satisfied(requirement, installed, env) {
                return false;
            }
        }
        for constrain in &metadata.constrains {
            match *constrain {
                GenConstrain::Env { pkg, lo, hi } => {
                    if env[pkg].is_some_and(|v| !in_range(v, lo, hi)) {
                        return false;
                    }
                }
                GenConstrain::Concrete { pkg, lo, hi } => {
                    if installed[pkg].is_some_and(|v| !in_range(v, lo, hi)) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Brute-force check that no install set (at most one version per package) is
/// a valid solution in environment `env`. The universes are tiny (at most
/// 4^5 = 1024 candidate sets) so plain enumeration is fine.
fn no_valid_solution_exists(universe: &Universe, env: &EnvSample) -> bool {
    let pkg_count = universe.packages.len();
    let mut installed: InstallSet = vec![None; pkg_count];
    loop {
        if is_valid_solution(universe, &installed, env) {
            return false;
        }
        // Advance the odometer: None -> Some(1) -> ... -> Some(max) -> None.
        let mut position = 0;
        loop {
            if position == pkg_count {
                return true;
            }
            let max = universe.packages[position].versions.len() as u32;
            match installed[position] {
                None => {
                    installed[position] = Some(1);
                    break;
                }
                Some(v) if v < max => {
                    installed[position] = Some(v + 1);
                    break;
                }
                Some(_) => {
                    installed[position] = None;
                    position += 1;
                }
            }
        }
    }
}

// ===========================================================================
// Evaluating solver-side conditions against a concrete environment sample.
// ===========================================================================

fn eval_env_literal(
    provider: &EnvTestProvider,
    env_name_ids: &[NameId],
    literal: &EnvLiteral<NameId>,
    env: &EnvSample,
) -> bool {
    let index = env_name_ids
        .iter()
        .position(|&name| name == literal.package)
        .expect("environment literal references a generated environment package");
    match literal.kind {
        EnvLiteralKind::Matches(version_set) => env[index].is_some_and(|value| {
            provider
                .pool
                .resolve_version_set(version_set)
                .contains(value)
        }),
        EnvLiteralKind::Absent => env[index].is_none(),
    }
}

fn cell_condition_holds(
    provider: &EnvTestProvider,
    env_name_ids: &[NameId],
    condition: &crate::CellCondition<NameId>,
    env: &EnvSample,
) -> bool {
    condition
        .0
        .iter()
        .all(|(literal, sign)| eval_env_literal(provider, env_name_ids, literal, env) == *sign)
}

fn model_satisfied(universe: &Universe, env: &EnvSample) -> bool {
    universe.model.iter().all(|disjunction| {
        disjunction.iter().any(|literal| match *literal {
            GenModelLiteral::Matches {
                pkg,
                lo,
                hi,
                positive,
            } => env[pkg].is_some_and(|v| in_range(v, lo, hi)) == positive,
            GenModelLiteral::Absent { pkg, positive } => env[pkg].is_none() == positive,
        })
    })
}

fn sample_env(rng: &mut Rng, universe: &Universe) -> EnvSample {
    universe
        .env_packages
        .iter()
        .map(|env| {
            if env.can_be_absent && rng.chance(1, 4) {
                None
            } else {
                Some(rng.below(ENV_VALUE_SPACE))
            }
        })
        .collect()
}

// ===========================================================================
// The property test itself.
// ===========================================================================

#[derive(Default)]
struct Stats {
    solved: usize,
    unsolvable: usize,
    samples_checked: usize,
    unsolvable_samples_checked: usize,
    cells_total: usize,
}

fn run_seed(seed: u64, stats: &mut Stats) {
    let mut rng = Rng::new(seed);
    let universe = gen_universe(&mut rng);
    let provider = build_provider(&universe);

    let env_name_ids = (0..universe.env_packages.len())
        .map(|e| provider.pool.intern_package_name(env_name(e)))
        .collect::<Vec<_>>();
    let pkg_name_ids = (0..universe.packages.len())
        .map(|p| provider.pool.intern_package_name(pkg_name(p)))
        .collect::<Vec<_>>();

    let root_requirements = universe
        .root_requirements
        .iter()
        .map(|requirement| build_requirement(&provider, requirement))
        .collect::<Vec<_>>();
    let environment_model = build_environment_model(&provider, &universe);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(root_requirements)
        .environment_model(environment_model);

    match solver.solve_universal(problem) {
        Ok(solution) => {
            stats.solved += 1;
            stats.cells_total += solution.cells.len();
            let provider = solver.provider();

            // (a) The independent verifier accepts the solution. The test
            // oracle gives definite answers for everything the enumerator
            // relies on, so even UnprovenDisjointness counts as a failure.
            if let Err(violations) = solution.verify(provider) {
                let violations: Vec<Violation<NameId>> = violations;
                panic!("seed {seed}: verify() failed: {violations:?}");
            }

            // The merged view and the edges, evaluated per sample below.
            let merged = solution.merged();
            let edges = solution.edges();

            // (b) Sample concrete environments from the model.
            let mut accepted = 0;
            for _ in 0..SAMPLE_ATTEMPTS {
                if accepted >= MAX_SAMPLES_PER_SEED {
                    break;
                }
                let env = sample_env(&mut rng, &universe);
                if !model_satisfied(&universe, &env) {
                    continue;
                }
                accepted += 1;
                stats.samples_checked += 1;

                // Exactly one cell must match, counted manually.
                let matching = solution
                    .cells
                    .iter()
                    .enumerate()
                    .filter(|(_, (condition, _))| {
                        cell_condition_holds(provider, &env_name_ids, condition, &env)
                    })
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                assert_eq!(
                    matching.len(),
                    1,
                    "seed {seed}: environment {env:?} matches cells {matching:?} instead of \
                     exactly one"
                );

                // project() must find the same cell.
                let projected = solution
                    .project(|literal| eval_env_literal(provider, &env_name_ids, literal, &env))
                    .unwrap_or_else(|| {
                        panic!("seed {seed}: project() returned None for environment {env:?}")
                    });
                assert_eq!(
                    projected,
                    &solution.cells[matching[0]].1[..],
                    "seed {seed}: project() returned a different cell than the manual match"
                );

                // The projected set must be a valid solution: at most one
                // solvable per package name, all active requirements
                // satisfied, no constraint violated.
                let mut installed: InstallSet = vec![None; universe.packages.len()];
                for &solvable in projected {
                    let resolved = provider.pool.resolve_solvable(solvable);
                    let index = pkg_name_ids
                        .iter()
                        .position(|&name| name == resolved.name)
                        .expect("solvable belongs to a generated package");
                    assert!(
                        installed[index].is_none(),
                        "seed {seed}: two solvables of package {index} installed at once"
                    );
                    installed[index] = Some(resolved.record);
                }
                assert!(
                    is_valid_solution(&universe, &installed, &env),
                    "seed {seed}: projected set {installed:?} is not a valid solution for \
                     environment {env:?}"
                );

                // Cross-check merged(): a solvable's presence holds in this
                // environment if and only if the solvable is in the
                // projected cell. (Presence simplification is an exact
                // equivalence within the model.)
                for (solvable, presence) in &merged {
                    let holds = presence.0.iter().any(|disjunct| {
                        cell_condition_holds(provider, &env_name_ids, disjunct, &env)
                    });
                    assert_eq!(
                        holds,
                        projected.contains(solvable),
                        "seed {seed}: merged presence of {} disagrees with projection for \
                         environment {env:?}",
                        crate::Interner::display_solvable(provider, *solvable),
                    );
                }

                // Cross-check edges(): an active edge's parent and target
                // must be installed in the projected cell.
                for (edge, presence) in &edges {
                    let holds = presence.0.iter().any(|disjunct| {
                        cell_condition_holds(provider, &env_name_ids, disjunct, &env)
                    });
                    if !holds {
                        continue;
                    }
                    if let Some(parent) = edge.parent {
                        assert!(
                            projected.contains(&parent),
                            "seed {seed}: active edge parent not installed for environment \
                             {env:?}"
                        );
                    }
                    if let Some(target) = edge.target {
                        assert!(
                            projected.contains(&target),
                            "seed {seed}: active edge target not installed for environment \
                             {env:?}"
                        );
                    }
                }
            }
        }
        Err(UniversalFailure::Unsolvable { cell, .. }) => {
            stats.unsolvable += 1;
            let provider = solver.provider();

            // Sample environments inside (model AND witness cell) and prove
            // by brute force that no valid solution exists there. If the
            // region is vacuous (an artifact of Unknown oracle answers) no
            // sample is found, which proves nothing and is fine.
            let mut accepted = 0;
            for _ in 0..SAMPLE_ATTEMPTS {
                if accepted >= 5 {
                    break;
                }
                let env = sample_env(&mut rng, &universe);
                if !model_satisfied(&universe, &env)
                    || !cell_condition_holds(provider, &env_name_ids, &cell, &env)
                {
                    continue;
                }
                accepted += 1;
                stats.unsolvable_samples_checked += 1;
                assert!(
                    no_valid_solution_exists(&universe, &env),
                    "seed {seed}: solver reported unsolvable cell {} but environment {env:?} \
                     has a valid solution",
                    cell.display(provider),
                );
            }
        }
        Err(UniversalFailure::Cancelled(_)) => {
            panic!("seed {seed}: unexpected cancellation");
        }
    }
}

#[test]
fn test_universal_solve_property() {
    let mut stats = Stats::default();
    for seed in 0..SEED_COUNT {
        run_seed(seed, &mut stats);
    }
    eprintln!(
        "universal property test: {} seeds ({} solved with {} cells total, {} unsolvable), \
         {} solution samples checked, {} unsolvable samples brute-forced",
        SEED_COUNT,
        stats.solved,
        stats.cells_total,
        stats.unsolvable,
        stats.samples_checked,
        stats.unsolvable_samples_checked,
    );
    assert!(
        stats.solved > 0 && stats.unsolvable > 0,
        "the generator must produce both solvable and unsolvable universes to be useful"
    );
    assert!(
        stats.samples_checked > 0,
        "at least some environment samples must have been checked"
    );
}
