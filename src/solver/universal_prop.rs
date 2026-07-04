//! Property test for universal solving (design doc, milestone M3).
//!
//! A deterministic seeded generator produces small random universes with
//! environment packages, concrete packages, plain and conditional
//! dependencies (conditions mix environment and concrete-package leaves
//! under And/Or), two-member [`Requirement::Union`] requirements, constrains,
//! root-level constraints, per-package provider knobs (locked, favored,
//! excluded and unknown-dependency versions) and a random environment model.
//! Each universe is solved with [`Solver::solve_universal`] and the result is
//! checked against the generated metadata directly:
//!
//! - On success: `verify()` passes, and for EVERY concrete environment the
//!   model admits (the space is small enough to enumerate exhaustively),
//!   `project()` returns the unique matching cell whose solvable set is a
//!   valid solution (every active requirement satisfied, no constraint
//!   violated, at most one solvable per package name, every version
//!   installable). The merged presence view is cross-checked against the
//!   projection; the conditional edges are checked in both directions
//!   (soundness per environment, completeness per cell against the edges the
//!   metadata guarantees — see `expected_edges_for_cell`). Per cell, a
//!   maximality oracle additionally proves that no installed package could
//!   be substituted by a HIGHER version while staying valid throughout the
//!   cell (see `maximality_skip` for the packages whose preference order is
//!   legitimately not highest-first).
//! - On success, additionally (milestone M4): re-solving the same universe
//!   with the solution's cells as the seed partition yields a verified
//!   disjoint cover whose projections are validated on the same exhaustive
//!   environment grid, usually byte-identical to the original (a minority of
//!   partitions instead HEALS: see the inline comment); reseeding the
//!   reseeded partition is a byte-identical fixed point; and re-solving with
//!   the seeds in REVERSE order still yields a verified disjoint cover with
//!   exhaustively validated projections (the content may legitimately
//!   differ, because generalization and disjointness repair depend on which
//!   cells were recorded earlier).
//! - On failure ([`UniversalFailure::Unsolvable`]): for every concrete
//!   environment inside (model AND witness cell), brute-force enumeration
//!   over all install sets (at most one solvable per package) confirms that
//!   no valid solution exists.
//!
//! This test lives in-crate (not in `tests/`) because it drives
//! [`EnvTestProvider`], which is deliberately `cfg(test)`-private.

use crate::{
    CellCondition, Condition, ConditionId, ConditionalRequirement, EnvClause, EnvLiteral,
    EnvironmentModel, LogicalOperator, NameId, Requirement, SignedEnvLiteral, Solver,
    UniversalFailure, UniversalProblem, VersionSetId, Violation,
    solver::env_test_provider::EnvTestProvider,
};

/// SOLVER BUG (exposed by this stage, FIXED): a conditional requirement
/// whose condition references a concrete package that stays entirely
/// UNDECIDED in a solution used to trip `capture_cell_edges`/`extract_cell`,
/// which evaluated condition complement literals under the
/// undecided-counts-as-false completion: an undecided complement solvable
/// counted as false, so the condition counted as "holds", but `decide()`
/// only enforces a conditional requirement when every complement literal is
/// *assigned* false, so no candidate of the requirement was ever installed.
/// The capture then hit `debug_assert!(false, "bug: an active requirement
/// of an installed parent has no installed candidate")` (universal.rs) in
/// debug builds. The capture now evaluates condition complements by
/// ASSIGNMENT, exactly like `decide()`. See
/// `test_universal_concrete_condition_untouched_package` for the minimized
/// regression test.
///
/// While `false`, the generator draws the packages of concrete condition
/// leaves only from the ROOT-REQUIRED packages: those are installed in
/// every recorded cell, so all their candidates are decided (installed one
/// true, the rest false through the at-most-one clauses) and the completion
/// agrees with `decide()`. `true` (the default since the fix) lifts the
/// restriction.
const GEN_CONCRETE_CONDITIONS_ON_UNREQUIRED_PACKAGES: bool = true;

/// SOLVER BUG (exposed by this stage, FIXED): universal mode used to
/// resolve conditional requirements eagerly (`queue_conditional_requirement`
/// short-circuited to `queue_requirement_candidates`), and that path never
/// ran `queue_package` for the requirement's target packages. As a result,
/// `Candidates::locked` and `Candidates::excluded` of a package reachable
/// ONLY through conditional requirements were silently ignored by
/// `solve_universal`: the solver installed a non-locked or excluded
/// version. Plain `solve` was unaffected. The eager path first gained the
/// missing `queue_package` calls; since universal mode switched to the
/// shared lazy conditional path, the target packages are queued when a
/// disjunct fires (`on_condition_data_available` /
/// `queue_deferred_requirement`), which is the mechanism the plain solve
/// always used. See `test_universal_locked_behind_conditional_requirement`
/// for the minimized regression test (generator seed 150 was the original
/// finding).
///
/// While `false`, the generator only marks ROOT-REQUIRED packages as
/// locked/excluded: root requirements are unconditional, so
/// `on_dependencies_available` queues their packages and the lock/exclusion
/// clauses are emitted. `true` (the default since the fix) lifts the
/// restriction. Favored (a pure preference) and unknown-deps (whose
/// exclusion is emitted when the solvable's dependencies are fetched, which
/// happens before any install survives) were never affected and stay
/// unrestricted.
const GEN_LOCKED_EXCLUDED_ON_CONDITIONAL_ONLY_PACKAGES: bool = true;

/// SOLVER BUG (exposed by this stage, FIXED): a concrete condition version
/// set with an EMPTY complement (every version of the package matches)
/// encodes as `not C_selected`, where the at-least-one tracker variable
/// `C_selected` is forced true by `AnyOf` clauses when a candidate of the
/// package is installed. That linkage broke across backtracking: when the
/// tracker variable is created while the candidate is already installed,
/// the retroactive `C_selected := true` decision is made at the CURRENT
/// (encode-time) level, so a later backjump could pop it while the
/// candidate itself (assigned at a shallower level) survived -- and nothing
/// ever re-derived it (the `AnyOf` watch only fires on new assignments of
/// the candidate, and the assertion scans do not cover it). A requirement
/// whose condition disjunct is gated on `not C_selected` was then silently
/// skipped by `decide()` for the rest of the enumeration: `solve_universal`
/// returned cells whose solvables violated an active conditional
/// requirement (the condition package IS installed, the requirement's
/// target is NOT). The tracker implications are now registered in
/// `implied_gate_requirers` and repaired by `force_stuck_gates` in the
/// solution-completeness loop, exactly like stranded shared-requires gates.
/// See `test_universal_empty_complement_condition_lost_tracker` for the
/// minimized regression test (generator seed 309 was the original finding).
/// The same loss was latently possible in a plain `solve` (the deferred
/// path encodes the same `not C_selected` literal) and is repaired by the
/// same loop.
///
/// While `false`, the generator keeps every concrete condition leaf's
/// version range strictly narrower than the package's version count, so the
/// complement is never empty and the at-least-one tracker is never used by
/// generated conditions. `true` (the default since the fix) lifts the
/// restriction.
const GEN_EMPTY_COMPLEMENT_CONCRETE_CONDITIONS: bool = true;

/// Number of seeds to run. Tuned so that the whole test finishes within a few
/// seconds in debug builds.
const SEED_COUNT: u64 = 1000;

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
    /// Root-level constraints ([`UniversalProblem::constraints`]).
    root_constrains: Vec<GenConstrain>,
    /// CNF over environment literals; each inner vec is a disjunction.
    model: Vec<Vec<GenModelLiteral>>,
}

struct EnvPkg {
    can_be_absent: bool,
}

#[derive(Default)]
struct ConcretePkg {
    /// Index `i` holds version `i + 1`.
    versions: Vec<PkgVersion>,
    /// The only selectable version ([`crate::Candidates::locked`]), if any.
    locked: Option<u32>,
    /// The version tried before the sort order
    /// ([`crate::Candidates::favored`]), if any. A preference only: it never
    /// changes which install sets are valid.
    favored: Option<u32>,
    /// An externally excluded version ([`crate::Candidates::excluded`]).
    excluded: Option<u32>,
    /// A version whose dependencies are [`crate::Dependencies::Unknown`],
    /// which the solver excludes exactly like an external exclusion.
    unknown_deps: Option<u32>,
}

impl ConcretePkg {
    /// Whether `version` can appear in any solution: not excluded, not
    /// unknown-deps, and not locked out by a different locked version.
    fn installable(&self, version: u32) -> bool {
        self.excluded != Some(version)
            && self.unknown_deps != Some(version)
            && self.locked.is_none_or(|locked| locked == version)
    }
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
    /// When set, the requirement is a [`Requirement::Union`] of `[lo, hi)`
    /// and this second range, both on the same target package. (Unions
    /// mixing environment and concrete packages deliberately panic in the
    /// encoder and are never generated.)
    union2: Option<(u32, u32)>,
    condition: Option<GenCondition>,
}

#[derive(Clone, Copy)]
enum GenTarget {
    Concrete(usize),
    Env(usize),
}

#[derive(Debug)]
enum GenCondition {
    /// The environment package `pkg` is present with a value in `[lo, hi)`.
    Env {
        pkg: usize,
        lo: u32,
        hi: u32,
    },
    /// A version of the concrete package `pkg` in `[lo, hi)` is installed.
    ///
    /// The oracle evaluates this against the install set with the public
    /// documented semantics ("the condition is only true if the requirement
    /// is true", i.e. a matching candidate is installed). The encoder's
    /// complement encoding can over-enforce relative to this: when every
    /// complement candidate of the package is ASSIGNED false without a
    /// matching install (e.g. through forbid propagation or learnt clauses),
    /// `decide()` fires the requirement even though the documented condition
    /// does not hold. Over-enforcement only ADDS installs, which
    /// `is_valid_solution` accepts; a package left entirely UNDECIDED leaves
    /// the requirement unenforced, agreeing with the documented semantics
    /// (cell capture uses the same assigned-false rule as `decide()`, see
    /// [`GEN_CONCRETE_CONDITIONS_ON_UNREQUIRED_PACKAGES`]). So the oracle
    /// can use the documented semantics throughout, including in the
    /// unsolvable brute force.
    Concrete {
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

/// A half-open range over the concrete version space `1..=5`, biased so that
/// a third of the ranges cover every version.
fn gen_pkg_range(rng: &mut Rng) -> (u32, u32) {
    let lo = if rng.chance(1, 3) { 1 } else { rng.range(1, 4) };
    let hi = if rng.chance(1, 3) {
        6
    } else {
        rng.range(lo + 1, 6)
    };
    (lo, hi)
}

/// `condition_pkgs` are the concrete packages a `Concrete` condition leaf may
/// reference: the root-required packages, unless
/// [`GEN_CONCRETE_CONDITIONS_ON_UNREQUIRED_PACKAGES`] lifts the restriction
/// (see there for the solver bug this avoids). `version_counts` holds every
/// package's version count so the leaf range can be kept strictly narrower
/// than the package (a non-empty complement; see
/// [`GEN_EMPTY_COMPLEMENT_CONCRETE_CONDITIONS`] for the solver bug that
/// restriction avoids).
fn gen_condition(
    rng: &mut Rng,
    env_count: usize,
    condition_pkgs: &[usize],
    version_counts: &[u32],
    depth: u32,
) -> GenCondition {
    if depth == 0 || rng.chance(3, 5) {
        // A quarter of the leaves are concrete-package conditions; they mix
        // freely with environment leaves under And/Or.
        if rng.chance(1, 4) {
            let pkg = condition_pkgs[rng.below(condition_pkgs.len() as u32) as usize];
            let (mut lo, mut hi) = gen_pkg_range(rng);
            if !GEN_EMPTY_COMPLEMENT_CONCRETE_CONDITIONS {
                // Keep at least one version of the package outside the
                // range. A one-version package leaves only constant-false
                // ranges (still a valid non-empty complement).
                let version_count = version_counts[pkg];
                if lo == 1 && hi > version_count {
                    if version_count >= 2 {
                        hi = version_count;
                    } else {
                        lo = 2;
                        hi = hi.max(3);
                    }
                }
            }
            GenCondition::Concrete { pkg, lo, hi }
        } else {
            let (lo, hi) = gen_env_range(rng);
            GenCondition::Env {
                pkg: rng.below(env_count as u32) as usize,
                lo,
                hi,
            }
        }
    } else {
        let lhs = Box::new(gen_condition(
            rng,
            env_count,
            condition_pkgs,
            version_counts,
            depth - 1,
        ));
        let rhs = Box::new(gen_condition(
            rng,
            env_count,
            condition_pkgs,
            version_counts,
            depth - 1,
        ));
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

    // Choose the root-required packages up front: concrete condition leaves
    // may only reference them (see
    // [`GEN_CONCRETE_CONDITIONS_ON_UNREQUIRED_PACKAGES`]), so the set must
    // exist before any version metadata is generated.
    let mut root_required = (0..pkg_count).map(|_| rng.chance(2, 3)).collect::<Vec<_>>();
    if !root_required.contains(&true) {
        root_required[0] = true;
    }
    let condition_pkgs = if GEN_CONCRETE_CONDITIONS_ON_UNREQUIRED_PACKAGES {
        (0..pkg_count).collect::<Vec<_>>()
    } else {
        (0..pkg_count)
            .filter(|&p| root_required[p])
            .collect::<Vec<_>>()
    };

    // Version counts are also fixed up front: concrete condition leaves need
    // them to keep their ranges strictly narrower than the package (see
    // [`GEN_EMPTY_COMPLEMENT_CONCRETE_CONDITIONS`]). A quarter of the
    // packages is "fat" (4-5 versions) so that wide requirements on them
    // cross `REQUIRES_AUX_ENCODING_THRESHOLD` and the property gate exercises
    // the shared-requires gate encoding in universal mode.
    let version_counts = (0..pkg_count)
        .map(|_| {
            if rng.chance(1, 4) {
                rng.range(4, 6)
            } else {
                rng.range(1, 4)
            }
        })
        .collect::<Vec<u32>>();

    let mut packages = Vec::new();
    for p in 0..pkg_count {
        let version_count = version_counts[p] as usize;
        let mut versions = Vec::new();
        for _ in 0..version_count {
            let mut requirements = Vec::new();
            let mut constrains = Vec::new();

            // Concrete dependencies, some guarded by environment/concrete
            // conditions and a few of them unions of two ranges. Ranges are
            // biased wide so that a decent share of the universes is
            // solvable; narrow ranges (often empty against one-version
            // packages) still occur and exercise the unsolvable path.
            for _ in 0..rng.below(3) {
                let mut target = rng.below(pkg_count as u32) as usize;
                if target == p {
                    target = (target + 1) % pkg_count;
                }
                let lo = if rng.chance(1, 2) { 1 } else { rng.range(1, 4) };
                let hi = if rng.chance(1, 2) {
                    // Wide enough to cover fat packages, so those
                    // requirements have >= 4 candidates and use the shared
                    // gate. On regular (1-3 version) packages this is
                    // equivalent to the previous wide bound of 4.
                    6
                } else {
                    rng.range(lo + 1, 5)
                };
                // A small share of the requirements is a union of two ranges
                // of the same package (`Requirement::Union`). Skipped when
                // the second range collides with the first: the pool interns
                // version sets, so equal ranges would alias to one id.
                let union2 = if rng.chance(1, 8) {
                    let second = gen_pkg_range(rng);
                    (second != (lo, hi)).then_some(second)
                } else {
                    None
                };
                let condition = if rng.chance(1, 2) {
                    let depth = rng.range(1, 3);
                    Some(gen_condition(
                        rng,
                        env_count,
                        &condition_pkgs,
                        &version_counts,
                        depth,
                    ))
                } else {
                    None
                };
                requirements.push(GenRequirement {
                    target: GenTarget::Concrete(target),
                    lo,
                    hi,
                    union2,
                    condition,
                });
            }

            // A direct requirement on an environment package, occasionally a
            // union of two ranges of the same environment package.
            if rng.chance(1, 5) {
                let (lo, hi) = gen_env_range(rng);
                let union2 = if rng.chance(1, 6) {
                    let second = gen_env_range(rng);
                    (second != (lo, hi)).then_some(second)
                } else {
                    None
                };
                let condition = if rng.chance(1, 4) {
                    Some(gen_condition(
                        rng,
                        env_count,
                        &condition_pkgs,
                        &version_counts,
                        1,
                    ))
                } else {
                    None
                };
                requirements.push(GenRequirement {
                    target: GenTarget::Env(rng.below(env_count as u32) as usize),
                    lo,
                    hi,
                    union2,
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

        // Provider knobs, all low probability so most packages stay plain:
        // locked (only that version selectable), favored (a pure preference),
        // excluded and unknown-deps (both make the version unselectable).
        // Locked/excluded are restricted to root-required packages (see
        // [`GEN_LOCKED_EXCLUDED_ON_CONDITIONAL_ONLY_PACKAGES`] for the solver
        // bug the restriction avoids).
        let version_count = versions.len() as u32;
        let mut pkg = ConcretePkg {
            versions,
            ..ConcretePkg::default()
        };
        let may_lock_exclude = root_required[p] || GEN_LOCKED_EXCLUDED_ON_CONDITIONAL_ONLY_PACKAGES;
        if rng.chance(1, 12) {
            let locked = rng.range(1, version_count + 1);
            if may_lock_exclude {
                pkg.locked = Some(locked);
            }
        }
        if rng.chance(1, 5) {
            pkg.favored = Some(rng.range(1, version_count + 1));
        }
        if rng.chance(1, 12) {
            let excluded = rng.range(1, version_count + 1);
            if may_lock_exclude {
                pkg.excluded = Some(excluded);
            }
        }
        if rng.chance(1, 20) {
            pkg.unknown_deps = Some(rng.range(1, version_count + 1));
        }
        packages.push(pkg);
    }

    // Root requirements: the pre-chosen non-empty subset of the concrete
    // packages, each with the full version range.
    let root_requirements = (0..pkg_count)
        .filter(|&p| root_required[p])
        .map(|p| GenRequirement {
            target: GenTarget::Concrete(p),
            lo: 1,
            hi: 6,
            union2: None,
            condition: None,
        })
        .collect::<Vec<_>>();

    // Root constraints ([`UniversalProblem::constraints`]), low probability:
    // a concrete constraint narrows one package everywhere, an environment
    // constraint bounds the environment space itself (and fails the whole
    // solve when the model reaches outside it, so it is rarer).
    let mut root_constrains = Vec::new();
    if rng.chance(1, 8) {
        let lo = rng.range(1, 4);
        let hi = rng.range(lo + 1, 6);
        root_constrains.push(GenConstrain::Concrete {
            pkg: rng.below(pkg_count as u32) as usize,
            lo,
            hi,
        });
    }
    if rng.chance(1, 14) {
        let (lo, hi) = gen_env_range(rng);
        root_constrains.push(GenConstrain::Env {
            pkg: rng.below(env_count as u32) as usize,
            lo,
            hi,
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
        root_constrains,
        model,
    }
}

// ===========================================================================
// Building the provider and the problem from a universe.
// ===========================================================================

/// The provider built from a [`Universe`], together with the id-level
/// artifacts the oracle needs to cross-check solver output: the solvable id
/// of every `(package, version)` and the built [`ConditionalRequirement`] of
/// every generated version requirement (unions are interned per build and
/// not deduplicated, so the exact built objects are required to compare
/// [`CellEdge::requirement`]s).
struct BuiltUniverse {
    provider: EnvTestProvider,
    /// `solvable_ids[p][v - 1]` is the solvable id of `pkg{p}` version `v`.
    solvable_ids: Vec<Vec<crate::SolvableId>>,
    /// `version_requirements[p][v - 1][i]` is the built requirement `i` of
    /// `pkg{p}` version `v`, parallel to the generated metadata.
    version_requirements: Vec<Vec<Vec<ConditionalRequirement>>>,
}

fn build_provider(universe: &Universe) -> BuiltUniverse {
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
        if let Some(locked) = pkg.locked {
            provider.set_locked(ids[(locked - 1) as usize]);
        }
        if let Some(favored) = pkg.favored {
            provider.set_favored(ids[(favored - 1) as usize]);
        }
        if let Some(excluded) = pkg.excluded {
            provider.set_excluded(ids[(excluded - 1) as usize], "generated exclusion");
        }
        if let Some(unknown) = pkg.unknown_deps {
            provider.set_unknown_deps(ids[(unknown - 1) as usize], "generated unknown deps");
        }
        solvable_ids.push(ids);
    }
    let mut version_requirements: Vec<Vec<Vec<ConditionalRequirement>>> = Vec::new();
    for (p, pkg) in universe.packages.iter().enumerate() {
        let mut per_version = Vec::new();
        for (vi, version) in pkg.versions.iter().enumerate() {
            let requirements: Vec<ConditionalRequirement> = version
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
            provider.set_dependencies(solvable_ids[p][vi], requirements.clone(), constrains);
            per_version.push(requirements);
        }
        version_requirements.push(per_version);
    }
    BuiltUniverse {
        provider,
        solvable_ids,
        version_requirements,
    }
}

fn build_requirement(
    provider: &EnvTestProvider,
    requirement: &GenRequirement,
) -> ConditionalRequirement {
    let target_name = match requirement.target {
        GenTarget::Concrete(p) => pkg_name(p),
        GenTarget::Env(e) => env_name(e),
    };
    let version_set = provider.version_set(&target_name, requirement.lo, requirement.hi);
    let built: Requirement = match requirement.union2 {
        None => version_set.into(),
        Some((lo2, hi2)) => {
            // A union of two version sets of the SAME package (the generator
            // never mixes environment and concrete members; that combination
            // deliberately panics in the encoder).
            let second = provider.version_set(&target_name, lo2, hi2);
            provider
                .pool
                .intern_version_set_union(version_set, std::iter::once(second))
                .into()
        }
    };
    ConditionalRequirement {
        condition: requirement
            .condition
            .as_ref()
            .map(|condition| intern_condition(provider, condition)),
        requirement: built,
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
        GenCondition::Concrete { pkg, lo, hi } => {
            let version_set = provider.version_set(&pkg_name(*pkg), *lo, *hi);
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
) -> EnvironmentModel<NameId> {
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
                        SignedEnvLiteral::new(EnvLiteral::Matches(version_set), positive)
                    }
                    GenModelLiteral::Absent { pkg, positive } => SignedEnvLiteral::new(
                        EnvLiteral::Absent(provider.pool.intern_package_name(env_name(pkg))),
                        positive,
                    ),
                })
                .collect::<EnvClause<NameId>>()
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

/// Evaluates a condition against a concrete environment and an install set,
/// with the documented condition semantics: a leaf holds iff a matching
/// candidate is installed (concrete leaf) / the environment package is
/// present with a matching value (environment leaf). For concrete leaves this
/// matches the solver only because the generator restricts them to
/// root-required packages; see the [`GenCondition::Concrete`] docs.
fn eval_condition(condition: &GenCondition, env: &EnvSample, installed: &InstallSet) -> bool {
    match condition {
        GenCondition::Env { pkg, lo, hi } => env[*pkg].is_some_and(|v| in_range(v, *lo, *hi)),
        GenCondition::Concrete { pkg, lo, hi } => {
            installed[*pkg].is_some_and(|v| in_range(v, *lo, *hi))
        }
        GenCondition::And(lhs, rhs) => {
            eval_condition(lhs, env, installed) && eval_condition(rhs, env, installed)
        }
        GenCondition::Or(lhs, rhs) => {
            eval_condition(lhs, env, installed) || eval_condition(rhs, env, installed)
        }
    }
}

fn requirement_satisfied(
    requirement: &GenRequirement,
    installed: &InstallSet,
    env: &EnvSample,
) -> bool {
    let value = match requirement.target {
        GenTarget::Concrete(p) => installed[p],
        GenTarget::Env(e) => env[e],
    };
    // A union requirement is satisfied when any member matches.
    value.is_some_and(|v| {
        in_range(v, requirement.lo, requirement.hi)
            || requirement
                .union2
                .is_some_and(|(lo2, hi2)| in_range(v, lo2, hi2))
    })
}

/// Checks a constraint of an installed parent (or of the root, which is
/// always installed).
fn constrain_respected(constrain: &GenConstrain, installed: &InstallSet, env: &EnvSample) -> bool {
    match *constrain {
        GenConstrain::Env { pkg, lo, hi } => env[pkg].is_none_or(|v| in_range(v, lo, hi)),
        GenConstrain::Concrete { pkg, lo, hi } => {
            installed[pkg].is_none_or(|v| in_range(v, lo, hi))
        }
    }
}

/// Checks whether `installed` is a valid solution of `universe` in the
/// concrete environment `env`: every installed version is installable (not
/// excluded, not unknown-deps, not locked out), all root requirements
/// satisfied, no root constraint violated, every active requirement of every
/// installed solvable satisfied, and no constraint of any installed solvable
/// violated. (A favored version is a pure preference and does not affect
/// validity.)
fn is_valid_solution(universe: &Universe, installed: &InstallSet, env: &EnvSample) -> bool {
    for (p, version) in installed.iter().enumerate() {
        if version.is_some_and(|v| !universe.packages[p].installable(v)) {
            return false;
        }
    }
    for requirement in &universe.root_requirements {
        if !requirement_satisfied(requirement, installed, env) {
            return false;
        }
    }
    for constrain in &universe.root_constrains {
        if !constrain_respected(constrain, installed, env) {
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
                .is_none_or(|condition| eval_condition(condition, env, installed));
            if active && !requirement_satisfied(requirement, installed, env) {
                return false;
            }
        }
        for constrain in &metadata.constrains {
            if !constrain_respected(constrain, installed, env) {
                return false;
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
        .position(|&name| name == literal.package(provider))
        .expect("environment literal references a generated environment package");
    match *literal {
        EnvLiteral::Matches(version_set) => env[index].is_some_and(|value| {
            provider
                .pool
                .resolve_version_set(version_set)
                .contains(value)
        }),
        EnvLiteral::Absent(_) => env[index].is_none(),
    }
}

fn cell_condition_holds(
    provider: &EnvTestProvider,
    env_name_ids: &[NameId],
    condition: &crate::CellCondition<NameId>,
    env: &EnvSample,
) -> bool {
    condition.literals().all(|signed| {
        eval_env_literal(provider, env_name_ids, &signed.literal, env) == signed.positive
    })
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

/// The packages the per-cell maximality oracle must skip because the solver
/// legitimately does not prefer their highest valid version:
///
/// - a FAVORED version is deliberately tried before the version sort order;
/// - a package targeted by any `Requirement::Union` is decided per union
///   member in member order (`decide()` walks the sorted candidates of the
///   first version set before the second), so a lower version matching the
///   first member legitimately wins over a higher version matching only the
///   second.
///
/// For every other package the solver's greedy highest-first candidate order
/// guarantees per-cell maximality: a higher version was skipped only when
/// some trail reason falsified it, and cell extraction pins the environment
/// assignments of exactly those reasons (the steering pins), so the higher
/// version stays invalid throughout the cell.
fn maximality_skip(universe: &Universe) -> Vec<bool> {
    let mut skip: Vec<bool> = universe
        .packages
        .iter()
        .map(|pkg| pkg.favored.is_some())
        .collect();
    let mark = |requirement: &GenRequirement, skip: &mut Vec<bool>| {
        if requirement.union2.is_some() {
            if let GenTarget::Concrete(target) = requirement.target {
                skip[target] = true;
            }
        }
    };
    for requirement in &universe.root_requirements {
        mark(requirement, &mut skip);
    }
    for pkg in &universe.packages {
        for version in &pkg.versions {
            for requirement in &version.requirements {
                mark(requirement, &mut skip);
            }
        }
    }
    skip
}

/// The DNF of a generated condition tree: a disjunction of conjunctions of
/// leaf conditions (mirroring `convert_conditions_to_dnf`).
fn gen_condition_dnf(condition: &GenCondition) -> Vec<Vec<&GenCondition>> {
    match condition {
        GenCondition::Env { .. } | GenCondition::Concrete { .. } => vec![vec![condition]],
        GenCondition::Or(lhs, rhs) => {
            let mut dnf = gen_condition_dnf(lhs);
            dnf.append(&mut gen_condition_dnf(rhs));
            dnf
        }
        GenCondition::And(lhs, rhs) => {
            let left = gen_condition_dnf(lhs);
            let right = gen_condition_dnf(rhs);
            let mut dnf = Vec::with_capacity(left.len() * right.len());
            for l in &left {
                for r in &right {
                    let mut merged = l.clone();
                    merged.extend(r.iter().copied());
                    dnf.push(merged);
                }
            }
            dnf
        }
    }
}

/// Whether the edge capture is GUARANTEED to consider `condition` held for
/// the cell described by `cell_condition` with install set `installed`.
///
/// The capture evaluates condition complements by trail ASSIGNMENT (the
/// same rule as `decide()`), i.e. cell-canonically, NOT per environment: a
/// guard that holds only in part of a cell while the requirement's target
/// is installed for other reasons is legitimately dropped (the cell
/// partition is refined by install sets, not by edge activity; generator
/// seed 10 is a concrete example, where a conditional requirement on an
/// unconditionally installed target records no edge even in the sub-region
/// where its guard holds). The metadata-level derivation must therefore be
/// the sound under-approximation of the assignment semantics:
///
/// - a concrete leaf is guaranteed-true when a matching version of the
///   package is installed (the complement candidates are then assigned
///   false through the at-most-one clauses);
/// - an environment leaf is guaranteed-true only when the cell condition
///   pins its literal positively (it is then assigned true on the trail
///   throughout the cell). An env literal that merely happens to hold in
///   part of the cell without being pinned may be undecided on the trail,
///   where the completion counts it false.
///
/// A condition is guaranteed held when some DNF disjunct has every leaf
/// guaranteed-true.
fn condition_edge_guaranteed(
    condition: &GenCondition,
    installed: &InstallSet,
    cell_condition: &CellCondition<NameId>,
    provider: &EnvTestProvider,
) -> bool {
    gen_condition_dnf(condition).iter().any(|disjunct| {
        disjunct.iter().all(|leaf| match leaf {
            GenCondition::Concrete { pkg, lo, hi } => {
                installed[*pkg].is_some_and(|v| in_range(v, *lo, *hi))
            }
            GenCondition::Env { pkg, lo, hi } => {
                let version_set = provider.version_set(&env_name(*pkg), *lo, *hi);
                cell_condition.literals().any(|signed| {
                    signed.positive && signed.literal == EnvLiteral::Matches(version_set)
                })
            }
            GenCondition::And(..) | GenCondition::Or(..) => {
                unreachable!("DNF disjuncts contain only leaves")
            }
        })
    })
}

/// Derives the dependency edges the generated metadata requires a cell to
/// carry: one edge per root requirement and per guaranteed-active
/// requirement of every installed version (see [`condition_edge_guaranteed`]
/// for the deliberate, documented under-approximation of conditional
/// activity), with the parent solvable, the BUILT requirement (the exact
/// object handed to the solver, needed because union ids are not
/// deduplicated) and the installed target (`None` for a requirement on an
/// environment package, which the environment itself satisfies).
///
/// `sample_env` is any modeled environment of the cell, used to resolve
/// requirement satisfaction (guaranteed-active requirements are satisfied
/// uniformly across the cell: their guards are pinned or install-set-bound).
#[allow(clippy::too_many_arguments)]
fn expected_edges_for_cell(
    universe: &Universe,
    root_built: &[ConditionalRequirement],
    solvable_ids: &[Vec<crate::SolvableId>],
    version_requirements: &[Vec<Vec<ConditionalRequirement>>],
    installed: &InstallSet,
    cell_condition: &CellCondition<NameId>,
    provider: &EnvTestProvider,
    sample_env: &EnvSample,
) -> Vec<crate::CellEdge<crate::SolvableId>> {
    let mut expected = Vec::new();
    let mut push = |generated: &GenRequirement,
                    built: &ConditionalRequirement,
                    parent: Option<crate::SolvableId>| {
        // The caller has already asserted validity, so an active requirement
        // is satisfied; the guard only protects the unwrap below.
        if !requirement_satisfied(generated, installed, sample_env) {
            return;
        }
        let target = match generated.target {
            GenTarget::Concrete(t) => {
                let version = installed[t].expect("a satisfied concrete requirement is installed");
                Some(solvable_ids[t][(version - 1) as usize])
            }
            GenTarget::Env(_) => None,
        };
        expected.push(crate::CellEdge {
            parent,
            requirement: built.requirement,
            target,
        });
    };

    for (generated, built) in universe.root_requirements.iter().zip(root_built) {
        debug_assert!(
            generated.condition.is_none(),
            "root requirements are unconditional"
        );
        push(generated, built, None);
    }
    for (p, version) in installed.iter().enumerate() {
        let Some(version) = version else { continue };
        let metadata = &universe.packages[p].versions[(*version - 1) as usize];
        let built_requirements = &version_requirements[p][(*version - 1) as usize];
        let parent = Some(solvable_ids[p][(*version - 1) as usize]);
        for (generated, built) in metadata.requirements.iter().zip(built_requirements) {
            let guaranteed = generated.condition.as_ref().is_none_or(|condition| {
                condition_edge_guaranteed(condition, installed, cell_condition, provider)
            });
            if guaranteed {
                push(generated, built, parent);
            }
        }
    }
    expected
}

/// Exhaustively projects `solution` onto every modeled environment and
/// asserts each projection is a valid solution of the generated metadata.
/// Returns the number of environments checked. Used for the reseeded and
/// reordered partitions, whose projections must hold on the SAME exhaustive
/// grid as the original solution's.
#[allow(clippy::too_many_arguments)]
fn assert_projections_valid(
    seed: u64,
    label: &str,
    universe: &Universe,
    provider: &EnvTestProvider,
    env_name_ids: &[NameId],
    pkg_name_ids: &[NameId],
    solution: &crate::UniversalSolution,
) -> usize {
    let mut checked = 0;
    for env in enumerate_envs(universe) {
        if !model_satisfied(universe, &env) {
            continue;
        }
        checked += 1;
        let projected = solution
            .project(|literal| eval_env_literal(provider, env_name_ids, literal, &env))
            .unwrap_or_else(|| {
                panic!("seed {seed}: {label} project() returned None for environment {env:?}")
            });
        let mut installed: InstallSet = vec![None; universe.packages.len()];
        for &solvable in projected {
            let resolved = provider.pool.resolve_solvable(solvable);
            let index = pkg_name_ids
                .iter()
                .position(|&name| name == resolved.name)
                .expect("solvable belongs to a generated package");
            installed[index] = Some(resolved.record);
        }
        assert!(
            is_valid_solution(universe, &installed, &env),
            "seed {seed}: {label} projection {installed:?} is not a valid solution for \
             environment {env:?}"
        );
    }
    checked
}

/// Every concrete environment the universe's packages can take: each package
/// ranges over `0..ENV_VALUE_SPACE` plus, if it can be absent, `None`. With
/// `env_count <= 3` this is at most `12^3 = 1728` environments, so callers can
/// enumerate the whole space and verify coverage/disjointness directly rather
/// than by sampling.
fn enumerate_envs(universe: &Universe) -> Vec<EnvSample> {
    let mut envs: Vec<EnvSample> = vec![Vec::new()];
    for env_pkg in &universe.env_packages {
        let mut next = Vec::with_capacity(envs.len() * (ENV_VALUE_SPACE as usize + 1));
        for partial in &envs {
            for value in 0..ENV_VALUE_SPACE {
                let mut extended = partial.clone();
                extended.push(Some(value));
                next.push(extended);
            }
            if env_pkg.can_be_absent {
                let mut extended = partial.clone();
                extended.push(None);
                next.push(extended);
            }
        }
        envs = next;
    }
    envs
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
    unsolvable_nonvacuous: usize,
    cells_total: usize,
    multi_cell_solved: usize,
    maximality_checks: usize,
    expected_edges_checked: usize,
    reseeded_samples_checked: usize,
    reordered_samples_checked: usize,
    reseeded_identical: usize,
    fixed_point_identical: usize,
    reordered_verified: usize,
}

fn run_seed(seed: u64, stats: &mut Stats) {
    let mut rng = Rng::new(seed);
    let universe = gen_universe(&mut rng);
    let BuiltUniverse {
        provider,
        solvable_ids,
        version_requirements,
    } = build_provider(&universe);

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
    let root_constraints: Vec<VersionSetId> = universe
        .root_constrains
        .iter()
        .map(|constrain| match *constrain {
            GenConstrain::Env { pkg, lo, hi } => provider.version_set(&env_name(pkg), lo, hi),
            GenConstrain::Concrete { pkg, lo, hi } => provider.version_set(&pkg_name(pkg), lo, hi),
        })
        .collect();
    let environment_model = build_environment_model(&provider, &universe);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(root_requirements.clone())
        .constraints(root_constraints.clone())
        .environment_model(environment_model.clone());

    match solver.solve_universal(problem) {
        Ok(solution) => {
            stats.solved += 1;
            stats.cells_total += solution.cells().len();
            if solution.cells().len() > 1 {
                stats.multi_cell_solved += 1;
            }
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

            // The modeled environments of each cell and the cell's install
            // set, grouped during the exhaustive walk below and consumed by
            // the per-cell maximality oracle afterwards.
            let mut cell_envs: Vec<Vec<EnvSample>> = vec![Vec::new(); solution.cells().len()];
            let mut cell_installed: Vec<Option<InstallSet>> = vec![None; solution.cells().len()];

            // (b) Exhaustively check every concrete environment the model
            // admits. The modeled space is tiny (at most ~12^3), so this
            // verifies coverage (every modeled environment matches a cell) and
            // pairwise disjointness (it matches exactly one) directly, with an
            // oracle fully independent of `solution.verify()` (which shares the
            // witness engine and relation oracle with the solver).
            for env in enumerate_envs(&universe) {
                if !model_satisfied(&universe, &env) {
                    continue;
                }
                stats.samples_checked += 1;

                // Exactly one cell must match, counted manually.
                let matching = solution
                    .cells()
                    .iter()
                    .enumerate()
                    .filter(|(_, cell)| {
                        cell_condition_holds(provider, &env_name_ids, cell.condition(), &env)
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
                    solution.cells()[matching[0]].solvables(),
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
                cell_envs[matching[0]].push(env.clone());
                cell_installed[matching[0]].get_or_insert_with(|| installed.clone());

                // Cross-check merged(): a solvable's presence holds in this
                // environment if and only if the solvable is in the
                // projected cell. (Presence simplification is an exact
                // equivalence within the model.)
                for (solvable, presence) in &merged {
                    let holds = presence.disjuncts().any(|disjunct| {
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
                    let holds = presence.disjuncts().any(|disjunct| {
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

            // Edge COMPLETENESS: every edge the generated metadata
            // guarantees for a cell (root requirements plus guaranteed-active
            // requirements of installed versions, with their installed
            // targets; see expected_edges_for_cell for the documented
            // under-approximation of conditional activity) must appear in
            // edges() with a presence that holds at every modeled
            // environment of the cell. Together with the per-environment
            // soundness direction above this bounds edges() from both sides.
            for (cell_index, envs) in cell_envs.iter().enumerate() {
                if envs.is_empty() {
                    continue;
                }
                let installed = cell_installed[cell_index]
                    .as_ref()
                    .expect("a cell with modeled environments recorded its install set");
                for expected in expected_edges_for_cell(
                    &universe,
                    &root_requirements,
                    &solvable_ids,
                    &version_requirements,
                    installed,
                    solution.cells()[cell_index].condition(),
                    provider,
                    &envs[0],
                ) {
                    stats.expected_edges_checked += 1;
                    for env in envs {
                        let active = edges.iter().any(|(edge, presence)| {
                            *edge == expected
                                && presence.disjuncts().any(|disjunct| {
                                    cell_condition_holds(provider, &env_name_ids, disjunct, env)
                                })
                        });
                        assert!(
                            active,
                            "seed {seed}: expected edge {expected:?} of cell {cell_index} is \
                             missing or inactive for environment {env:?}"
                        );
                    }
                }
            }

            // Per-cell maximality: for every installed package, no HIGHER
            // version can be substituted while keeping the install set valid
            // throughout the cell (checked against every modeled environment
            // of the cell; a substitution valid at only SOME of its
            // environments is legitimate, since the solver must pick one set
            // for the whole cell). Packages whose preference order is
            // legitimately not highest-first are skipped (see
            // [`maximality_skip`]). This catches version-preference
            // regressions that pure validity cannot.
            let skip = maximality_skip(&universe);
            for (cell_index, envs) in cell_envs.iter().enumerate() {
                if envs.is_empty() {
                    continue;
                }
                let installed = cell_installed[cell_index]
                    .as_ref()
                    .expect("a cell with modeled environments recorded its install set");
                for (p, pkg) in universe.packages.iter().enumerate() {
                    let Some(version) = installed[p] else {
                        continue;
                    };
                    if skip[p] {
                        continue;
                    }
                    for higher in (version + 1)..=(pkg.versions.len() as u32) {
                        let mut substituted = installed.clone();
                        substituted[p] = Some(higher);
                        stats.maximality_checks += 1;
                        assert!(
                            !envs
                                .iter()
                                .all(|env| is_valid_solution(&universe, &substituted, env)),
                            "seed {seed}: cell {cell_index} installs pkg{p}={version} but \
                             pkg{p}={higher} stays valid throughout the cell (version \
                             preference regression)"
                        );
                    }
                }
            }

            // (c) Seed stability (M4): re-solve the same universe with this
            // solution's cells as the seed partition, on the same solver
            // (the real flow: the provider and all interned ids persist;
            // the solver state is reset per call).
            //
            // The reseeded partition is NOT always byte-identical to the
            // original: a cell's original solution can be steered by
            // transient search state (learnt clauses from conflicts in
            // earlier cells of the SAME run can exclude a candidate that is
            // perfectly valid in the cell's region), while the seeded
            // replay, which assumes the cell's condition up front and so
            // avoids those conflicts, legitimately finds a better solution
            // whose load-bearing support can also be more general, absorbing
            // later seeds. This is the healing behavior of design doc 5.7;
            // generator seed 18 is a concrete counterexample to identity.
            // Byte-identical reproduction of conflict-free re-solves is
            // pinned by the scenario tests in tests/solver. What must hold
            // here unconditionally:
            //   - the reseeded partition is a verified disjoint cover, and
            //     its projections are valid solutions (checked on the full
            //     modeled environment grid);
            //   - one more reseed round is a fixed point: a seeded
            //     `solve_universal` internally iterates the enumeration on
            //     its own output until a pass (over a saturated provider
            //     cache) reproduces its seed list, so the returned partition
            //     replays byte-identically.
            let seeds: Vec<CellCondition<NameId>> = solution
                .cells()
                .iter()
                .map(|cell| cell.condition().clone())
                .collect();
            let reseeded = match solver.solve_universal(
                UniversalProblem::new()
                    .requirements(root_requirements.clone())
                    .constraints(root_constraints.clone())
                    .environment_model(environment_model.clone())
                    .seed_partition(seeds.clone()),
            ) {
                Ok(reseeded) => reseeded,
                Err(failure) => panic!(
                    "seed {seed}: seeded re-solve failed where the unseeded solve succeeded: \
                     {failure:?}"
                ),
            };
            if let Err(violations) = reseeded.verify(solver.provider()) {
                let violations: Vec<Violation<NameId>> = violations;
                panic!("seed {seed}: reseeded solve failed verify(): {violations:?}");
            }
            // Validate the reseeded projections on the SAME exhaustive
            // environment grid as the original solution's.
            stats.reseeded_samples_checked += assert_projections_valid(
                seed,
                "reseeded",
                &universe,
                solver.provider(),
                &env_name_ids,
                &pkg_name_ids,
                &reseeded,
            );
            // A cell bundles its condition, solvables and edges, so comparing
            // the cell slices covers both the conditions and the edges.
            if format!("{:?}", solution.cells()) == format!("{:?}", reseeded.cells()) {
                stats.reseeded_identical += 1;
            }

            // The fixed-point round: reseeding the RESEEDED partition must
            // reproduce it byte-identically, cells and edges.
            let reseeded_seeds: Vec<CellCondition<NameId>> = reseeded
                .cells()
                .iter()
                .map(|cell| cell.condition().clone())
                .collect();
            let fixed_point = match solver.solve_universal(
                UniversalProblem::new()
                    .requirements(root_requirements.clone())
                    .constraints(root_constraints.clone())
                    .environment_model(environment_model.clone())
                    .seed_partition(reseeded_seeds),
            ) {
                Ok(fixed_point) => fixed_point,
                Err(failure) => panic!(
                    "seed {seed}: fixed-point re-solve failed where the seeded solve \
                     succeeded: {failure:?}"
                ),
            };
            // Cells bundle conditions and edges, so one comparison of the cell
            // slices covers both.
            assert_eq!(
                format!("{:?}", reseeded.cells()),
                format!("{:?}", fixed_point.cells()),
                "seed {seed}: reseeding the reseeded partition produced different cells"
            );
            stats.fixed_point_identical += 1;

            // (d) Seed order independence of VALIDITY: with the seeds in
            // reverse order the partition may legitimately differ in
            // CONTENT, not just order. Cell generalization records only the
            // load-bearing literals of the solution found under the seed's
            // assumptions, and the disjointness repair then re-specializes
            // against whatever cells were recorded EARLIER, so a cell seeded
            // first can absorb regions that the original enumeration split
            // off (see the seed-order scenario test in tests/solver). The
            // contract that does hold regardless of order: the result is a
            // disjoint cover of the model, checked by the independent
            // verifier.
            if seeds.len() > 1 {
                let mut reversed = seeds;
                reversed.reverse();
                let reordered = match solver.solve_universal(
                    UniversalProblem::new()
                        .requirements(root_requirements.clone())
                        .constraints(root_constraints.clone())
                        .environment_model(environment_model.clone())
                        .seed_partition(reversed),
                ) {
                    Ok(reordered) => reordered,
                    Err(failure) => panic!(
                        "seed {seed}: reversed-seed solve failed where the unseeded solve \
                         succeeded: {failure:?}"
                    ),
                };
                if let Err(violations) = reordered.verify(solver.provider()) {
                    let violations: Vec<Violation<NameId>> = violations;
                    panic!("seed {seed}: reversed-seed solve failed verify(): {violations:?}");
                }
                // The reordered partition may legitimately differ in content,
                // but its projections must be valid on the same exhaustive
                // environment grid as the original solution's.
                stats.reordered_samples_checked += assert_projections_valid(
                    seed,
                    "reordered",
                    &universe,
                    solver.provider(),
                    &env_name_ids,
                    &pkg_name_ids,
                    &reordered,
                );
                stats.reordered_verified += 1;
            }
        }
        Err(UniversalFailure::Unsolvable { cell, .. }) => {
            stats.unsolvable += 1;
            let provider = solver.provider();

            // Exhaustively enumerate (model AND witness cell) and prove by
            // brute force that no valid solution exists at any point. A
            // vacuous region (an artifact of Unknown oracle answers) yields no
            // points, which proves nothing; those are counted separately so
            // the suite-level assertion can require that the bulk of
            // unsolvable verdicts were checked against a non-empty region
            // (otherwise a spurious "unsolvable" on a region the sampler never
            // hit would go undetected).
            let mut region_points = 0;
            for env in enumerate_envs(&universe) {
                if !model_satisfied(&universe, &env)
                    || !cell_condition_holds(provider, &env_name_ids, &cell, &env)
                {
                    continue;
                }
                region_points += 1;
                stats.unsolvable_samples_checked += 1;
                assert!(
                    no_valid_solution_exists(&universe, &env),
                    "seed {seed}: solver reported unsolvable cell {} but environment {env:?} \
                     has a valid solution",
                    cell.display(provider),
                );
            }
            if region_points > 0 {
                stats.unsolvable_nonvacuous += 1;
            }
        }
        Err(UniversalFailure::InvalidInput(invalid)) => {
            panic!("seed {seed}: unexpected invalid input: {invalid}");
        }
        Err(UniversalFailure::Cancelled(_)) => {
            panic!("seed {seed}: unexpected cancellation");
        }
    }
}

#[test]
fn test_universal_solve_property() {
    use crate::solver::prop_counters::hits;

    // Coverage floors: every solver path this generator is responsible for
    // must be hit at least once over the corpus, so generator drift cannot
    // silently zero out coverage. Counters are process-global (other tests
    // may add hits concurrently), so the floors are asserted on the deltas
    // over this test's run, which is monotone-sound.
    let floor_counters: [(&str, &std::sync::atomic::AtomicU64); 10] = [
        (
            "EXTRACT_SATISFIED_BY_CONCRETE",
            &hits::EXTRACT_SATISFIED_BY_CONCRETE,
        ),
        (
            "ENCODE_CONDITION_COMPLEMENT_SOLVABLES",
            &hits::ENCODE_CONDITION_COMPLEMENT_SOLVABLES,
        ),
        (
            "ENCODE_CONDITION_COMPLEMENT_ENV",
            &hits::ENCODE_CONDITION_COMPLEMENT_ENV,
        ),
        ("ENCODE_UNION_CONCRETE", &hits::ENCODE_UNION_CONCRETE),
        ("ENCODE_UNION_ENV", &hits::ENCODE_UNION_ENV),
        ("ENCODE_ROOT_CONSTRAINT", &hits::ENCODE_ROOT_CONSTRAINT),
        ("ENCODE_LOCKED", &hits::ENCODE_LOCKED),
        ("ENCODE_EXCLUDED", &hits::ENCODE_EXCLUDED),
        ("ENCODE_UNKNOWN_DEPS", &hits::ENCODE_UNKNOWN_DEPS),
        ("CACHE_FAVORED_SORTED", &hits::CACHE_FAVORED_SORTED),
    ];
    let counters_before = floor_counters.map(|(_, counter)| hits::get(counter));

    let mut stats = Stats::default();
    for seed in 0..SEED_COUNT {
        run_seed(seed, &mut stats);
    }
    eprintln!(
        "universal property test: {} seeds ({} solved with {} cells total, {} multi-cell, \
         {} unsolvable of which {} non-vacuous), {} solution samples checked, \
         {} unsolvable samples brute-forced, {} maximality substitutions refuted, \
         {} expected edges matched, {} reseeded + {} reordered samples checked, \
         {}/{} seeded re-solves byte-identical, {} fixed-point rounds identical, \
         {} reversed-seed solves verified",
        SEED_COUNT,
        stats.solved,
        stats.cells_total,
        stats.multi_cell_solved,
        stats.unsolvable,
        stats.unsolvable_nonvacuous,
        stats.samples_checked,
        stats.unsolvable_samples_checked,
        stats.maximality_checks,
        stats.expected_edges_checked,
        stats.reseeded_samples_checked,
        stats.reordered_samples_checked,
        stats.reseeded_identical,
        stats.solved,
        stats.fixed_point_identical,
        stats.reordered_verified,
    );
    // The standing coverage dashboard (see `solver::prop_counters`). Counter
    // values include hits from concurrently running tests, so per-path floors
    // asserted below are monotone deltas over the values at test start.
    eprintln!(
        "universal property coverage counters:\n{}",
        crate::solver::prop_counters::hits::dump(),
    );
    assert!(
        stats.solved > 0 && stats.unsolvable > 0,
        "the generator must produce both solvable and unsolvable universes to be useful"
    );
    assert!(
        stats.samples_checked > 0,
        "at least some environment samples must have been checked"
    );
    // Corpus-shape floor: the multi-cell solved seeds are what exercise cell
    // transitions, disjointness repair and seeding; generator drift must not
    // hollow them out.
    assert!(
        stats.multi_cell_solved >= 150,
        "corpus floor: at least 150 solved seeds must enumerate multiple cells (got {})",
        stats.multi_cell_solved,
    );
    // Corpus-shape floor: at least three quarters of the unsolvable verdicts
    // must be checked against a non-empty (model AND witness) region;
    // otherwise the brute-force soundness check proves nothing for them. A
    // purely vacuous witness is legitimate (an artifact of Unknown oracle
    // answers), but a corpus dominated by vacuous witnesses would make the
    // unsolvable path hollow.
    assert!(
        stats.unsolvable_nonvacuous * 4 >= stats.unsolvable * 3,
        "corpus floor: at least 75% of unsolvable verdicts should be brute-forced against a \
         non-empty region (got {}/{})",
        stats.unsolvable_nonvacuous,
        stats.unsolvable,
    );
    // The strengthened per-cell assertions must not be vacuous.
    assert!(
        stats.maximality_checks > 0,
        "at least some higher-version substitutions must have been refuted"
    );
    assert!(
        stats.expected_edges_checked > 0,
        "at least some expected edges must have been derived and matched"
    );
    assert!(
        stats.reseeded_samples_checked > 0 && stats.reordered_samples_checked > 0,
        "the reseeded and reordered partitions must have been projected exhaustively"
    );
    // Regression floor for the stabilizing effect of seeding. The generator
    // is deliberately conflict-heavy, so a sizable share of partitions heals
    // on the first reseed (about 21% at the time of writing: the original
    // cell's solution was steered by transient learnt state, the seeded
    // replay finds a better one); a drop below half would mean seeding lost
    // its stabilizing effect entirely.
    assert!(
        stats.reseeded_identical * 2 >= stats.solved,
        "most seeded re-solves should reproduce the partition byte-identically (got {}/{})",
        stats.reseeded_identical,
        stats.solved,
    );
    assert!(
        stats.reordered_verified > 0,
        "at least some multi-cell partitions must have been re-solved in reverse seed order"
    );
    for ((name, counter), before) in floor_counters.into_iter().zip(counters_before) {
        assert!(
            hits::get(counter) > before,
            "coverage floor: the corpus no longer exercises {name} (see prop_counters)"
        );
    }
}

// ===========================================================================
// Targeted scenarios for the generator features of this module, and the
// minimized regression tests for the solver bugs the generator originally
// exposed (see the GEN_* flags at the top of the module).
// ===========================================================================

/// Formats a solution's cells as `condition -> [solvables]` lines.
fn cells_to_string(
    solver: &Solver<EnvTestProvider>,
    solution: &crate::UniversalSolution,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for cell in solution.cells() {
        let solvables = cell
            .solvables()
            .iter()
            .map(|&s| crate::Interner::display_solvable(solver.provider(), s).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            out,
            "{} -> [{}]",
            cell.condition().display(solver.provider()),
            solvables
        )
        .unwrap();
    }
    out
}

/// A locked package behind an UNCONDITIONAL requirement is respected by
/// `solve_universal`: only the locked version is installed even though a
/// higher version exists.
#[test]
fn test_universal_locked_unconditional_requirement() {
    let mut provider = EnvTestProvider::default();
    provider.add_env_package("env0", false);
    let e_any = provider.version_set("env0", 0, 11);
    let a1 = provider.add_package("a", 1);
    let b1 = provider.add_package("b", 1);
    let _b2 = provider.add_package("b", 2);
    provider.set_locked(b1);
    let b_any = provider.version_set("b", 1, 3);
    provider.set_dependencies(a1, vec![b_any.into()], vec![]);
    let a_any = provider.version_set("a", 1, 2);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(vec![a_any.into()])
        .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
            SignedEnvLiteral::new(EnvLiteral::Matches(e_any), true),
        ])]));
    let solution = solver.solve_universal(problem).expect("solvable");
    insta::assert_snapshot!(cells_to_string(&solver, &solution), @"<all environments> -> [a=1, b=1]");
}

/// Regression test (see [`GEN_LOCKED_EXCLUDED_ON_CONDITIONAL_ONLY_PACKAGES`]):
/// a locked package reachable ONLY through a conditional requirement is
/// respected by `solve_universal` (design doc 5.5: "Candidates::locked /
/// favored keep working unchanged for concrete packages"). The eager
/// universal condition path used to skip `queue_package` for the
/// requirement's target package, so `on_candidates_available` (which emits
/// the lock and exclusion clauses) never ran for it and the second cell
/// installed `b=2` although `b` is locked to `b=1`.
#[test]
fn test_universal_locked_behind_conditional_requirement() {
    let mut provider = EnvTestProvider::default();
    provider.add_env_package("env0", false);
    let e_57 = provider.version_set("env0", 5, 7);
    let a1 = provider.add_package("a", 1);
    let b1 = provider.add_package("b", 1);
    let _b2 = provider.add_package("b", 2);
    provider.set_locked(b1);
    let b_any = provider.version_set("b", 1, 3);
    let cond = provider.pool.intern_condition(Condition::Requirement(e_57));
    provider.set_dependencies(
        a1,
        vec![ConditionalRequirement {
            condition: Some(cond),
            requirement: b_any.into(),
        }],
        vec![],
    );
    let a_any = provider.version_set("a", 1, 2);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(vec![a_any.into()])
        .environment_model(EnvironmentModel::default());
    let solution = solver.solve_universal(problem).expect("solvable");
    // The condition splits the space: a baseline cell without b, and an
    // `env0 in 5..7` cell where the requirement is active and must be
    // satisfied by the LOCKED version b=1.
    let mut saw_b1 = false;
    for cell in solution.cells() {
        assert!(
            !cell.solvables().contains(&_b2),
            "b is locked to b=1, but cell {} installs b=2",
            cell.condition().display(solver.provider()),
        );
        saw_b1 |= cell.solvables().contains(&b1);
    }
    assert!(
        saw_b1,
        "some cell must activate the conditional requirement and install the locked b=1"
    );
}

/// Regression test (see [`GEN_CONCRETE_CONDITIONS_ON_UNREQUIRED_PACKAGES`]):
/// a conditional requirement whose concrete condition package stays entirely
/// UNDECIDED (`c` is required by nothing) must not be treated as active by
/// cell capture. `decide()` enforces a conditional requirement only when
/// every complement literal is ASSIGNED false, and
/// `capture_cell_edges`/`extract_cell` now use the same rule, so the
/// untouched condition package leaves the requirement inactive: one cell,
/// no `b`, no edge for the gated requirement (and no debug panic on
/// "an active requirement of an installed parent has no installed
/// candidate").
#[test]
fn test_universal_concrete_condition_untouched_package() {
    let mut provider = EnvTestProvider::default();
    provider.add_env_package("env0", false);
    let e_any = provider.version_set("env0", 0, 11);
    let a1 = provider.add_package("a", 1);
    let _b1 = provider.add_package("b", 1);
    let _c1 = provider.add_package("c", 1);
    let _c2 = provider.add_package("c", 2);
    // a requires b if (c in 2..3); the complement {c=1} is non-empty but c is
    // never touched by the solve.
    let b_any = provider.version_set("b", 1, 2);
    let c_23 = provider.version_set("c", 2, 3);
    let cond = provider.pool.intern_condition(Condition::Requirement(c_23));
    provider.set_dependencies(
        a1,
        vec![ConditionalRequirement {
            condition: Some(cond),
            requirement: b_any.into(),
        }],
        vec![],
    );
    let a_any = provider.version_set("a", 1, 2);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(vec![a_any.into()])
        .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
            SignedEnvLiteral::new(EnvLiteral::Matches(e_any), true),
        ])]));
    let solution = solver.solve_universal(problem).expect("solvable");
    assert_eq!(solution.cells().len(), 1);
    let cell = &solution.cells()[0];
    // The condition (c in 2..3) never fired: c is untouched, so the
    // requirement on b is inactive. Only a is installed and the gated
    // requirement contributes no edge.
    assert_eq!(cell.solvables(), &[a1]);
    assert!(
        !cell.solvables().contains(&_b1),
        "the gated requirement must not be enforced for an untouched condition package"
    );
    assert!(
        cell.edges()
            .iter()
            .all(|edge| edge.requirement != b_any.into()),
        "an inactive conditional requirement must not produce an edge"
    );
}

/// The empty-complement (at-least-one tracker) encoding of a concrete
/// condition works when the encode order is favorable: `a`'s dependencies
/// (and with them the tracker) are encoded before the condition package `d`
/// is installed, so the tracker assignment is derived by a live watch. This
/// is the standing coverage for `DisjunctionComplement::Empty`, which the
/// generator deliberately avoids (see
/// [`GEN_EMPTY_COMPLEMENT_CONCRETE_CONDITIONS`]); the hit counter proves the
/// arm fired.
#[test]
fn test_universal_empty_complement_concrete_condition() {
    use crate::solver::prop_counters::hits;
    let empty_before = hits::get(&hits::ENCODE_CONDITION_COMPLEMENT_EMPTY);

    // a requires b if ((env0 in 5..7) OR (d in 1..2)); d has one version and
    // is root-required, so the second disjunct always holds and b must be
    // installed in every cell.
    let mut provider = EnvTestProvider::default();
    provider.add_env_package("env0", false);
    let e_57 = provider.version_set("env0", 5, 7);
    let a1 = provider.add_package("a", 1);
    let b1 = provider.add_package("b", 1);
    let _d1 = provider.add_package("d", 1);
    let b_any = provider.version_set("b", 1, 2);
    let d_12 = provider.version_set("d", 1, 2);
    let c_env = provider.pool.intern_condition(Condition::Requirement(e_57));
    let c_d = provider.pool.intern_condition(Condition::Requirement(d_12));
    let cond = provider
        .pool
        .intern_condition(Condition::Binary(LogicalOperator::Or, c_env, c_d));
    provider.set_dependencies(
        a1,
        vec![ConditionalRequirement {
            condition: Some(cond),
            requirement: b_any.into(),
        }],
        vec![],
    );
    let a_any = provider.version_set("a", 1, 2);
    let d_any = provider.version_set("d", 1, 2);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(vec![a_any.into(), d_any.into()])
        .environment_model(EnvironmentModel::default());
    let solution = solver.solve_universal(problem).expect("solvable");
    insta::assert_snapshot!(cells_to_string(&solver, &solution), @"<all environments> -> [a=1, d=1, b=1]");
    for cell in solution.cells() {
        assert!(cell.solvables().contains(&b1));
    }

    assert!(
        hits::get(&hits::ENCODE_CONDITION_COMPLEMENT_EMPTY) > empty_before,
        "the empty-complement encoding arm must have fired"
    );
}

/// Regression test (see [`GEN_EMPTY_COMPLEMENT_CONCRETE_CONDITIONS`],
/// minimized from generator seed 309): the at-least-one tracker assignment
/// (`C_selected(d) := true`, made retroactively at encode level when `a=2`'s
/// dependencies are encoded with `d=1` already installed) is popped by the
/// backjump out of the `env0 in 5..7` conflict, while `d=1` (root level)
/// survives. The `AnyOf` watches never re-fire for an assignment that
/// predates the clause, so nothing re-derived the tracker: when `a=1` was
/// installed afterwards, its `c if (d in 1..2)` clause reused the existing
/// tracker variable, `decide()` skipped the clause (condition literal
/// undecided), and the second cell violated the active conditional
/// requirement (`d` installed, `c` not). The tracker implications are now
/// registered in `implied_gate_requirers`, so `force_stuck_gates` re-derives
/// the stranded tracker in the solution-completeness loop and every cell
/// installs `c`.
#[test]
fn test_universal_empty_complement_condition_lost_tracker() {
    let mut provider = EnvTestProvider::default();
    provider.add_env_package("env0", false);
    let e_57 = provider.version_set("env0", 5, 7);
    let a2 = provider.add_package("a", 2);
    let a1 = provider.add_package("a", 1);
    let c1 = provider.add_package("c", 1);
    let _d1 = provider.add_package("d", 1);
    let c_any = provider.version_set("c", 1, 2);
    let d_12 = provider.version_set("d", 1, 2);
    // x has no candidates: a=2 is unsolvable wherever env0 in 5..7 holds.
    let x_any = provider.version_set("x", 1, 2);
    let cond_env = provider.pool.intern_condition(Condition::Requirement(e_57));
    let cond_d = provider.pool.intern_condition(Condition::Requirement(d_12));
    provider.set_dependencies(
        a2,
        vec![
            ConditionalRequirement {
                condition: Some(cond_env),
                requirement: x_any.into(),
            },
            ConditionalRequirement {
                condition: Some(cond_d),
                requirement: c_any.into(),
            },
        ],
        vec![],
    );
    provider.set_dependencies(
        a1,
        vec![ConditionalRequirement {
            condition: Some(cond_d),
            requirement: c_any.into(),
        }],
        vec![],
    );
    let a_any = provider.version_set("a", 1, 3);
    let d_any = provider.version_set("d", 1, 2);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(vec![a_any.into(), d_any.into()])
        .environment_model(EnvironmentModel::default());
    let solution = solver.solve_universal(problem).expect("solvable");
    for cell in solution.cells() {
        // d is installed in every cell, so the condition (d in 1..2) holds
        // and c must be installed in every cell. Today the second cell
        // (env0 in 5..7 -> [a=1, d=1]) misses it.
        assert!(
            cell.solvables().contains(&c1),
            "cell {} misses conditionally required c",
            cell.condition().display(solver.provider()),
        );
    }
}

// ===========================================================================
// Targeted tests for paths the generator cannot reach structurally: the
// trail-reuse abandonment fallback, the relation oracle's Equal arm, the
// trail-reshape full retraction, and the reseed-orbit early exit.
// ===========================================================================

/// Builds the provider of the trail-reuse scenarios: `top` (one version,
/// root-required) needs `y` only where `env0 in 5..10` holds, and `y` drags
/// a chain of `chain_len` two-version packages (`z0` .. `z{n-1}`) behind it.
/// Every chain install is a real `decide()` level, so the second cell stacks
/// `chain_len + 1` ordinary decision levels above the environment literal.
fn build_env_tail_chain_provider(chain_len: usize) -> (EnvTestProvider, VersionSetId) {
    let mut provider = EnvTestProvider::default();
    provider.add_env_package("env0", false);
    let e_5_10 = provider.version_set("env0", 5, 10);
    let top = provider.add_package("top", 1);
    let y2 = provider.add_package("y", 2);
    let y1 = provider.add_package("y", 1);
    let mut z_ids = Vec::new();
    for i in 0..chain_len {
        let name = format!("z{i}");
        let z2 = provider.add_package(&name, 2);
        let z1 = provider.add_package(&name, 1);
        z_ids.push((z1, z2));
    }
    let y_any = provider.version_set("y", 1, 3);
    let cond = provider
        .pool
        .intern_condition(Condition::Requirement(e_5_10));
    provider.set_dependencies(
        top,
        vec![ConditionalRequirement {
            condition: Some(cond),
            requirement: y_any.into(),
        }],
        vec![],
    );
    let z0_any = provider.version_set("z0", 1, 3);
    provider.set_dependencies(y1, vec![z0_any.into()], vec![]);
    provider.set_dependencies(y2, vec![z0_any.into()], vec![]);
    for (i, &(z1, z2)) in z_ids.iter().enumerate() {
        let requirements: Vec<ConditionalRequirement> = if i + 1 < chain_len {
            let next_any = provider.version_set(&format!("z{}", i + 1), 1, 3);
            vec![next_any.into()]
        } else {
            vec![]
        };
        provider.set_dependencies(z1, requirements.clone(), vec![]);
        provider.set_dependencies(z2, requirements, vec![]);
    }
    let top_any = provider.version_set("top", 1, 2);
    (provider, top_any)
}

/// Trail-reuse abandonment fallback: with the kept-prefix work budget forced
/// to zero (the test-only override), the first prefix-started run aborts
/// with `PrefixBudgetExhausted` and `solve_universal` re-enumerates from
/// scratch with reuse disabled. The fallback must complete, produce the same
/// partition as an unhindered solve (trail reuse does not change the
/// partition of this scenario, so the fallback's reuse-free enumeration is
/// byte-comparable), and stay a reseed fixed point.
#[test]
fn test_universal_trail_reuse_abandonment_fallback() {
    use crate::solver::prop_counters::hits;
    let abandoned_before = hits::get(&hits::UNIVERSAL_REUSE_ABANDONED);
    let abort_before = hits::get(&hits::PREFIX_BUDGET_ABORT);

    // The baseline partition, solved without any override.
    let (provider, top_any) = build_env_tail_chain_provider(3);
    let mut baseline_solver = Solver::new(provider);
    let baseline = baseline_solver
        .solve_universal(UniversalProblem::new().requirements(vec![top_any.into()]))
        .expect("solvable");

    // The same problem with the prefix budget forced to zero: the transition
    // into the second cell keeps a trail prefix, arms the budget, aborts and
    // falls back to a reuse-free enumeration.
    let (provider, top_any) = build_env_tail_chain_provider(3);
    let mut solver = Solver::new(provider);
    solver.set_test_prefix_budget_override(Some(0));
    let solution = solver
        .solve_universal(UniversalProblem::new().requirements(vec![top_any.into()]))
        .expect("the fallback must complete");

    // (a) The fallback ran: both the propagation-side abort and the
    // enumeration-side abandonment fired.
    assert!(
        hits::get(&hits::PREFIX_BUDGET_ABORT) > abort_before,
        "the zero budget must abort the prefix-started run"
    );
    assert!(
        hits::get(&hits::UNIVERSAL_REUSE_ABANDONED) > abandoned_before,
        "the abandonment fallback must have run"
    );

    // (b) The partition verifies and is byte-identical to the unhindered
    // (trail-reuse) enumeration, which for this scenario equals the
    // reuse-free one.
    assert_eq!(solution.verify(solver.provider()), Ok(()));
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", baseline.cells()),
        "the fallback enumeration must produce the baseline partition"
    );

    // (c) Reseed fixed point after abandonment: re-solving with the
    // partition's own conditions as seeds (still under the zero budget)
    // reproduces it byte-identically.
    let seeds: Vec<CellCondition<NameId>> = solution
        .cells()
        .iter()
        .map(|cell| cell.condition().clone())
        .collect();
    let reseeded = solver
        .solve_universal(
            UniversalProblem::new()
                .requirements(vec![top_any.into()])
                .seed_partition(seeds),
        )
        .expect("the seeded fallback must complete");
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", reseeded.cells()),
        "the reseed fixed point must hold after abandonment"
    );
}

/// Trail reshape: recording the second cell of the chain scenario would pop
/// more than `TRAIL_RESHAPE_ORDINARY_LEVELS` ordinary decision levels (the
/// eleven two-version chain installs sit above the environment literal), so
/// the enumeration widens the retraction to a full one. The partition still
/// verifies and reseeds byte-identically; the hit counter proves the branch
/// fired.
#[test]
fn test_universal_trail_reshape_full_retract() {
    use crate::solver::prop_counters::hits;
    let reshape_before = hits::get(&hits::TRAIL_RESHAPE_FULL_RETRACT);

    let (provider, top_any) = build_env_tail_chain_provider(10);
    let mut solver = Solver::new(provider);
    let solution = solver
        .solve_universal(UniversalProblem::new().requirements(vec![top_any.into()]))
        .expect("solvable");

    assert!(
        hits::get(&hits::TRAIL_RESHAPE_FULL_RETRACT) > reshape_before,
        "the trail-reshape full retraction must have fired"
    );
    assert_eq!(solution.verify(solver.provider()), Ok(()));
    assert_eq!(solution.cells().len(), 2, "one env-free cell, one env tail");

    // Reseed fixed point across the reshape.
    let seeds: Vec<CellCondition<NameId>> = solution
        .cells()
        .iter()
        .map(|cell| cell.condition().clone())
        .collect();
    let reseeded = solver
        .solve_universal(
            UniversalProblem::new()
                .requirements(vec![top_any.into()])
                .seed_partition(seeds),
        )
        .expect("seeded re-solve");
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", reseeded.cells()),
        "the reseed fixed point must hold across the trail reshape"
    );
}

/// Builds the provider of the witness-probe scenarios: `top` (one version,
/// root-required) always needs `a`, needs `b` where `env0 in 6..12` holds
/// (splitting the coverable space into a with-`b` and a without-`b` cell)
/// and needs the candidate-less package `doom` where `env0 in 12..20`
/// holds. The model admits `env0 in 0..model_hi`: with `model_hi = 12` the
/// oracle proves `env0 in 12..20` impossible (disjoint ranges) and the
/// whole modeled space is coverable, while `model_hi = 20` admits the doom
/// corner -- the miniature of the real-world "glibc >= 3.0" corner that
/// makes tail problems env-dependent-unsat only AFTER solvable cells were
/// recorded. Returns the provider, the root requirement version set and the
/// model version set.
fn build_witness_probe_provider(model_hi: u32) -> (EnvTestProvider, VersionSetId, VersionSetId) {
    let mut provider = EnvTestProvider::default();
    provider.add_env_package("env0", false);
    let e_model = provider.version_set("env0", 0, model_hi);
    let e_mid = provider.version_set("env0", 6, 12);
    let e_doom = provider.version_set("env0", 12, 20);
    let top = provider.add_package("top", 1);
    let a1 = provider.add_package("a", 1);
    let b1 = provider.add_package("b", 1);
    let a_any = provider.version_set("a", 1, 2);
    let b_any = provider.version_set("b", 1, 2);
    // No candidate of "doom" exists; the version set is still interned.
    let doom_any = provider.version_set("doom", 1, 2);
    let mid_cond = provider
        .pool
        .intern_condition(Condition::Requirement(e_mid));
    let doom_cond = provider
        .pool
        .intern_condition(Condition::Requirement(e_doom));
    provider.set_dependencies(
        top,
        vec![
            a_any.into(),
            ConditionalRequirement {
                condition: Some(mid_cond),
                requirement: b_any.into(),
            },
            ConditionalRequirement {
                condition: Some(doom_cond),
                requirement: doom_any.into(),
            },
        ],
        vec![],
    );
    provider.set_dependencies(a1, vec![], vec![]);
    provider.set_dependencies(b1, vec![], vec![]);
    let top_any = provider.version_set("top", 1, 2);
    (provider, top_any, e_model)
}

/// Assembles the [`UniversalProblem`] of the witness-probe scenarios.
fn witness_probe_problem(
    top_any: VersionSetId,
    e_model: VersionSetId,
    seeds: Vec<CellCondition<NameId>>,
) -> UniversalProblem {
    UniversalProblem::new()
        .requirements(vec![top_any.into()])
        .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
            SignedEnvLiteral::new(EnvLiteral::Matches(e_model), true),
        ])]))
        .seed_partition(seeds)
}

/// Witness-pinned unsat: with the witness-probe budget forced to zero,
/// every free episode aborts on its first propagated decision and the
/// enumeration is driven entirely by witness-directed solves; the doomed
/// `env0 in 12..20` corner is eventually pinned as assumptions, proven
/// unsolvable, and the verdict (cell and user-facing conflict) must be
/// byte-identical to the one exhaustive coverage reports without the probe.
#[test]
fn test_universal_witness_probe_pinned_unsat() {
    use crate::solver::prop_counters::hits;
    let trips_before = hits::get(&hits::WITNESS_PROBE_TRIP);
    let verdicts_before = hits::get(&hits::WITNESS_PROBE_VERDICT);

    // The baseline verdict, produced by exhaustive coverage (the production
    // budget is untrippable on this tiny universe).
    let (provider, top_any, e_model) = build_witness_probe_provider(20);
    let mut baseline_solver = Solver::new(provider);
    let baseline = baseline_solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect_err("the env0 in 12..20 corner requires the candidate-less doom");
    let UniversalFailure::Unsolvable {
        cell: baseline_cell,
        conflict: baseline_conflict,
    } = baseline
    else {
        panic!("expected an unsolvable verdict");
    };
    assert_eq!(baseline_solver.witness_probe_trips(), 0);

    let (provider, top_any, e_model) = build_witness_probe_provider(20);
    let mut solver = Solver::new(provider);
    solver.set_test_witness_probe_override(Some(Some(0)));
    let failure = solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect_err("the probe must reproduce the unsolvable verdict");
    let UniversalFailure::Unsolvable { cell, conflict } = failure else {
        panic!("expected an unsolvable verdict");
    };

    assert!(
        hits::get(&hits::WITNESS_PROBE_TRIP) > trips_before,
        "the zero budget must trip the probe"
    );
    assert!(
        hits::get(&hits::WITNESS_PROBE_VERDICT) > verdicts_before,
        "the doomed witness region must produce the verdict"
    );
    assert!(solver.witness_probe_trips() > 0);
    assert_eq!(
        format!("{:?}", cell),
        format!("{:?}", baseline_cell),
        "the witness-pinned verdict cell must be byte-identical to exhaustive coverage's"
    );
    assert_eq!(
        conflict.display_user_friendly(&solver).to_string(),
        baseline_conflict
            .display_user_friendly(&baseline_solver)
            .to_string(),
        "the scoped conflict must be byte-identical to exhaustive coverage's"
    );
}

/// Witness-pinned solution: with the budget forced to zero on a SOLVABLE
/// two-cell universe, every tripped free episode escalates to a
/// witness-directed solve that records a normal cell, and the enumeration
/// completes with a verified partition that is a reseed fixed point and is
/// reproduced byte-identically by an identical run (determinism).
#[test]
fn test_universal_witness_probe_pinned_solution() {
    use crate::solver::prop_counters::hits;
    let escalated_before = hits::get(&hits::WITNESS_PROBE_ESCALATED);

    let (provider, top_any, e_model) = build_witness_probe_provider(12);
    let mut solver = Solver::new(provider);
    solver.set_test_witness_probe_override(Some(Some(0)));
    let solution = solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect("the modeled space is fully coverable");

    assert!(
        hits::get(&hits::WITNESS_PROBE_ESCALATED) > escalated_before,
        "tripped episodes must escalate to witness-directed solves"
    );
    assert!(solver.witness_probe_trips() > 0);
    assert_eq!(solution.verify(solver.provider()), Ok(()));
    assert!(
        solution.cells().len() >= 2,
        "the conditional b requirement must split the space into at least two cells"
    );

    // Reseed fixed point across probe escalations (still under the zero
    // budget): re-solving with the partition's own conditions as seeds
    // reproduces it byte-identically.
    let seeds: Vec<CellCondition<NameId>> = solution
        .cells()
        .iter()
        .map(|cell| cell.condition().clone())
        .collect();
    let reseeded = solver
        .solve_universal(witness_probe_problem(top_any, e_model, seeds))
        .expect("the seeded re-solve must complete");
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", reseeded.cells()),
        "the reseed fixed point must hold across probe escalations"
    );

    // Determinism: the probe deadline is a deterministic propagation
    // counter, so an identical fresh run reproduces the partition
    // byte-identically.
    let (provider, top_any, e_model) = build_witness_probe_provider(12);
    let mut second = Solver::new(provider);
    second.set_test_witness_probe_override(Some(Some(0)));
    let repeat = second
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect("the repeated run must complete");
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", repeat.cells()),
        "identical runs under the probe must be byte-identical"
    );
}

/// Witness-None break: on the solvable witness-probe universe the free
/// episode after the last recorded cell is the final refutation of a
/// successful solve; tripping it must terminate the enumeration through the
/// witness=None coverage break with the exact partition exhaustive
/// refutation produces.
#[test]
fn test_universal_witness_probe_coverage_break() {
    use crate::solver::prop_counters::hits;
    let breaks_before = hits::get(&hits::WITNESS_PROBE_COVERAGE_BREAK);

    let (provider, top_any, e_model) = build_witness_probe_provider(12);
    let mut baseline_solver = Solver::new(provider);
    let baseline = baseline_solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect("solvable");

    let (provider, top_any, e_model) = build_witness_probe_provider(12);
    let mut solver = Solver::new(provider);
    solver.set_test_witness_probe_override(Some(Some(0)));
    let solution = solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect("the probed solve must complete");

    assert!(
        hits::get(&hits::WITNESS_PROBE_COVERAGE_BREAK) > breaks_before,
        "the tripped final refutation must terminate through the coverage break"
    );
    assert_eq!(solution.verify(solver.provider()), Ok(()));
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", baseline.cells()),
        "the coverage break must not change the partition"
    );
}

/// The probe must never arm during seeded (assumption) episodes: with the
/// budget forced to zero, ANY armed episode trips on its first propagated
/// decision, so a fully seeded re-solve of a two-cell partition must record
/// exactly ONE trip -- the free episode after the seeds (the final
/// refutation, which terminates through the witness=None break). A seeded
/// episode arming the probe would trip additionally and change the count.
#[test]
fn test_universal_witness_probe_never_arms_under_assumptions() {
    let (provider, top_any, e_model) = build_witness_probe_provider(12);
    let mut solver = Solver::new(provider);
    let solution = solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect("solvable");
    assert_eq!(
        solver.witness_probe_trips(),
        0,
        "the production budget is untrippable on this universe"
    );

    let seeds: Vec<CellCondition<NameId>> = solution
        .cells()
        .iter()
        .map(|cell| cell.condition().clone())
        .collect();
    solver.set_test_witness_probe_override(Some(Some(0)));
    let reseeded = solver
        .solve_universal(witness_probe_problem(top_any, e_model, seeds))
        .expect("the seeded re-solve must complete");
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", reseeded.cells()),
        "the seeded re-solve must reproduce the partition"
    );
    assert_eq!(
        solver.witness_probe_trips(),
        1,
        "exactly the one free episode after the seeds may trip"
    );
}

/// Disabled-probe byte-identity on a pinned scenario: the armed but
/// untripped production probe must not change any partition, so a solve
/// under the default (untrippable here) budget and a solve with the probe
/// disabled outright must be byte-identical.
#[test]
fn test_universal_witness_probe_disabled_byte_identity() {
    let (provider, top_any, e_model) = build_witness_probe_provider(12);
    let mut default_solver = Solver::new(provider);
    let default_solution = default_solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect("solvable");
    assert_eq!(default_solver.witness_probe_trips(), 0);

    let (provider, top_any, e_model) = build_witness_probe_provider(12);
    let mut disabled_solver = Solver::new(provider);
    disabled_solver.set_test_witness_probe_override(Some(None));
    let disabled_solution = disabled_solver
        .solve_universal(witness_probe_problem(top_any, e_model, Vec::new()))
        .expect("solvable");
    assert_eq!(disabled_solver.witness_probe_trips(), 0);

    assert_eq!(
        format!("{:?}", default_solution.cells()),
        format!("{:?}", disabled_solution.cells()),
        "an armed but untripped probe must not change the partition"
    );
    // Pin the partition itself so silent drift of the shared scenario is
    // visible in this test rather than only in the comparisons above.
    insta::assert_snapshot!(
        cells_to_string(&default_solver, &default_solution),
        @r"
    not (env0 in 6..12) AND not (env0 in 12..20) -> [top=1, a=1]
    env0 in 6..12 AND not (env0 in 12..20) -> [top=1, a=1, b=1]
    "
    );
}

/// The `VersionSetRelation::Equal` arm of the oracle-consistency encoding is
/// structurally unreachable through plain ranges (the pool dedups equal
/// ranges to one version set id), so this test interns an ALIASED copy of
/// the model's range (distinct id, same range; see `Range::new_aliased`).
/// Interning the second literal emits both implication clauses; the solve
/// produces one verified cell and stays a reseed fixed point.
#[test]
fn test_universal_oracle_equal_relation() {
    use crate::solver::prop_counters::hits;
    let equal_before = hits::get(&hits::ORACLE_EQUAL_CLAUSES);

    let mut provider = EnvTestProvider::default();
    provider.add_env_package("cuda", false);
    let cuda_plain = provider.version_set("cuda", 5, 10);
    let cuda_alias = provider.version_set_aliased("cuda", 5, 10, 1);
    assert_ne!(
        cuda_plain, cuda_alias,
        "the alias must intern a distinct id"
    );
    let a1 = provider.add_package("a", 1);
    // The model uses the plain range; a's requirement uses the aliased one,
    // so both literals intern and the oracle is asked about the pair.
    provider.set_dependencies(a1, vec![cuda_alias.into()], vec![]);
    let a_any = provider.version_set("a", 1, 2);

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(vec![a_any.into()])
        .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
            SignedEnvLiteral::new(EnvLiteral::Matches(cuda_plain), true),
        ])]));
    let solution = solver.solve_universal(problem).expect("solvable");

    assert!(
        hits::get(&hits::ORACLE_EQUAL_CLAUSES) > equal_before,
        "the Equal oracle arm must have emitted its implication clauses"
    );
    assert_eq!(solution.verify(solver.provider()), Ok(()));
    assert_eq!(solution.cells().len(), 1);
    assert!(solution.cells()[0].solvables().contains(&a1));

    // Reseed fixed point through the Equal-linked literals.
    let seeds: Vec<CellCondition<NameId>> = solution
        .cells()
        .iter()
        .map(|cell| cell.condition().clone())
        .collect();
    let reseeded = solver
        .solve_universal(
            UniversalProblem::new()
                .requirements(vec![a_any.into()])
                .environment_model(EnvironmentModel::new(vec![EnvClause::new(vec![
                    SignedEnvLiteral::new(EnvLiteral::Matches(cuda_plain), true),
                ])]))
                .seed_partition(seeds),
        )
        .expect("seeded re-solve");
    assert_eq!(
        format!("{:?}", solution.cells()),
        format!("{:?}", reseeded.cells()),
    );
}

/// The reseed-orbit early exit (`inputs_tried.contains(&output)` in
/// `solve_universal_impl`) fires when a seeded enumeration cycles between
/// seed lists without reaching a fixed point, which reversed seed orders can
/// provoke. Exactly one corpus universe hit it at the time this test was
/// written (generator seed 79 of the pre-concrete-conditions generator);
/// the universe is PINNED here as a literal so generator drift cannot
/// silently lose the only deterministic coverage of the orbit exit.
#[test]
fn test_universal_reseed_orbit_pinned() {
    use crate::solver::prop_counters::hits;

    fn e(pkg: usize, lo: u32, hi: u32) -> GenCondition {
        GenCondition::Env { pkg, lo, hi }
    }
    fn and(l: GenCondition, r: GenCondition) -> GenCondition {
        GenCondition::And(Box::new(l), Box::new(r))
    }
    fn or(l: GenCondition, r: GenCondition) -> GenCondition {
        GenCondition::Or(Box::new(l), Box::new(r))
    }
    fn req(target: usize, lo: u32, hi: u32, condition: Option<GenCondition>) -> GenRequirement {
        GenRequirement {
            target: GenTarget::Concrete(target),
            lo,
            hi,
            union2: None,
            condition,
        }
    }
    fn env_req(target: usize, lo: u32, hi: u32, condition: Option<GenCondition>) -> GenRequirement {
        GenRequirement {
            target: GenTarget::Env(target),
            lo,
            hi,
            union2: None,
            condition,
        }
    }
    fn version(requirements: Vec<GenRequirement>, constrains: Vec<GenConstrain>) -> PkgVersion {
        PkgVersion {
            requirements,
            constrains,
        }
    }
    fn cenv(pkg: usize, lo: u32, hi: u32) -> GenConstrain {
        GenConstrain::Env { pkg, lo, hi }
    }
    fn cpkg(pkg: usize, lo: u32, hi: u32) -> GenConstrain {
        GenConstrain::Concrete { pkg, lo, hi }
    }
    fn plain(versions: Vec<PkgVersion>) -> ConcretePkg {
        ConcretePkg {
            versions,
            ..ConcretePkg::default()
        }
    }
    fn m(pkg: usize, lo: u32, hi: u32) -> GenModelLiteral {
        GenModelLiteral::Matches {
            pkg,
            lo,
            hi,
            positive: true,
        }
    }

    let universe = Universe {
        env_packages: vec![
            EnvPkg {
                can_be_absent: false,
            },
            EnvPkg {
                can_be_absent: false,
            },
        ],
        packages: vec![
            plain(vec![
                version(vec![req(3, 3, 4, None)], vec![cenv(0, 2, 4)]),
                version(
                    vec![
                        req(3, 1, 3, Some(or(e(1, 2, 8), e(0, 0, 10)))),
                        req(1, 1, 4, None),
                    ],
                    vec![cpkg(2, 3, 4)],
                ),
                version(vec![req(4, 3, 6, None)], vec![]),
                version(
                    vec![
                        req(4, 1, 4, Some(e(1, 6, 8))),
                        req(1, 3, 4, Some(and(e(0, 5, 7), e(0, 4, 9)))),
                    ],
                    vec![cenv(1, 8, 10), cpkg(4, 2, 4)],
                ),
                version(vec![req(1, 3, 4, None), req(2, 1, 6, None)], vec![]),
            ]),
            plain(vec![
                version(vec![req(0, 1, 4, Some(e(0, 8, 10)))], vec![]),
                version(
                    vec![
                        req(4, 1, 4, Some(or(e(1, 2, 7), e(1, 1, 7)))),
                        req(0, 1, 6, Some(or(e(1, 4, 10), e(0, 6, 8)))),
                    ],
                    vec![cpkg(2, 1, 3)],
                ),
                version(vec![req(0, 2, 3, None)], vec![]),
            ]),
            plain(vec![
                version(
                    vec![
                        req(0, 2, 6, Some(and(e(0, 3, 8), or(e(0, 7, 10), e(1, 4, 9))))),
                        req(3, 1, 4, None),
                    ],
                    vec![cenv(0, 3, 6), cpkg(3, 1, 4)],
                ),
                version(
                    vec![
                        req(0, 1, 6, None),
                        req(0, 1, 6, None),
                        env_req(1, 2, 6, None),
                    ],
                    vec![],
                ),
                version(
                    vec![
                        req(4, 1, 6, None),
                        req(4, 1, 6, Some(and(e(0, 0, 8), e(0, 3, 10)))),
                    ],
                    vec![],
                ),
            ]),
            plain(vec![version(
                vec![req(2, 1, 4, Some(e(1, 6, 10)))],
                vec![cpkg(3, 1, 4)],
            )]),
            plain(vec![
                version(
                    vec![
                        req(0, 1, 6, Some(e(0, 5, 10))),
                        req(0, 1, 3, Some(e(1, 8, 9))),
                    ],
                    vec![cenv(1, 3, 5)],
                ),
                version(
                    vec![req(0, 1, 6, None), req(2, 1, 6, Some(e(0, 7, 8)))],
                    vec![cenv(0, 5, 9), cpkg(2, 1, 2)],
                ),
                version(vec![req(3, 1, 6, None)], vec![cenv(1, 3, 5)]),
                version(
                    vec![
                        req(1, 2, 6, Some(or(e(1, 6, 7), e(1, 4, 9)))),
                        req(0, 1, 6, None),
                        env_req(1, 6, 10, Some(e(1, 6, 7))),
                    ],
                    vec![],
                ),
            ]),
        ],
        root_requirements: (0..5).map(|p| req(p, 1, 6, None)).collect(),
        root_constrains: vec![],
        model: vec![
            vec![m(1, 7, 10), m(1, 8, 10), m(1, 3, 8)],
            vec![m(0, 3, 10), m(0, 1, 4)],
        ],
    };

    let provider = build_provider(&universe).provider;
    let root_requirements: Vec<ConditionalRequirement> = universe
        .root_requirements
        .iter()
        .map(|requirement| build_requirement(&provider, requirement))
        .collect();
    let environment_model = build_environment_model(&provider, &universe);

    // Mirror the property test's solve sequence exactly: original solve,
    // in-order reseed, fixed-point reseed, then the REVERSED reseed that
    // closes the orbit (a reversed seed list is one no enumeration produced,
    // so iterating on it can cycle without a fixed point).
    let mut solver = Solver::new(provider);
    let problem = |seeds: Vec<CellCondition<NameId>>| {
        UniversalProblem::new()
            .requirements(root_requirements.clone())
            .environment_model(environment_model.clone())
            .seed_partition(seeds)
    };
    let solution = solver.solve_universal(problem(vec![])).expect("solvable");
    assert!(
        solution.cells().len() > 1,
        "the pinned universe must enumerate multiple cells"
    );
    let seeds: Vec<CellCondition<NameId>> = solution
        .cells()
        .iter()
        .map(|cell| cell.condition().clone())
        .collect();
    let reseeded = solver
        .solve_universal(problem(seeds.clone()))
        .expect("reseed");
    let reseeded_seeds: Vec<CellCondition<NameId>> = reseeded
        .cells()
        .iter()
        .map(|cell| cell.condition().clone())
        .collect();
    let _fixed_point = solver
        .solve_universal(problem(reseeded_seeds))
        .expect("fixed point");

    let orbit_before = hits::get(&hits::RESEED_ORBIT_CLOSED);
    let mut reversed = seeds;
    reversed.reverse();
    let reordered = solver
        .solve_universal(problem(reversed))
        .expect("reversed reseed");
    assert!(
        hits::get(&hits::RESEED_ORBIT_CLOSED) > orbit_before,
        "the pinned universe must close a reseed orbit under reversed seeds"
    );
    // The orbit exit still returns a verified disjoint cover.
    assert_eq!(reordered.verify(solver.provider()), Ok(()));
}
