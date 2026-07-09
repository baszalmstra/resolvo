use std::{
    fs::File,
    io::BufReader,
    ops::Add,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use std::collections::HashSet;

use clap::{Parser, ValueEnum};
use console::style;
use csv::WriterBuilder;
use itertools::Itertools;
use rand::{
    Rng, SeedableRng,
    distr::{Distribution, weighted::WeightedIndex},
    prelude::IteratorRandom,
    rngs::StdRng,
};
use resolvo::{
    CellCondition, Condition, ConditionalRequirement, EnvClause, EnvLiteral, EnvironmentModel,
    Interner, NameId, Problem, SignedEnvLiteral, SolvableId, Solver, UniversalFailure,
    UniversalProblem, UniversalSolution, UnsolvableOrCancelled, VersionSetId, VersionSetUnionId,
    snapshot::{DependencySnapshot, SnapshotProvider},
};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Plain solves against the simulated machine candidates.
    Concrete,
    /// Universal solves against the environment model (requires --env-model).
    Universal,
}

#[derive(Parser)]
#[clap(version = "0.1.0", author = "Bas Zalmstra <zalmstra.bas@gmail.com>")]
struct Opts {
    snapshot: String,

    /// The maximum number of requirements to solve
    #[clap(long, short = 'n', default_value = "1000")]
    limit: usize,

    /// Skip the first N problems (still drawing them from the rng so the
    /// corpus stays identical); useful to re-run individual outliers.
    #[clap(long, default_value = "0")]
    skip: usize,

    /// The timeout to use for solving requirements in seconds. If a solve takes
    /// longer if will be cancelled.
    #[clap(long, default_value = "60")]
    timeout: u64,

    /// The random seed to use for generating the requirements.
    #[clap(long, default_value = "0")]
    seed: u64,

    /// The benchmark mode: concrete (plain solve against the machine
    /// candidates) or universal (solve_universal against the environment
    /// model).
    #[clap(long, value_enum, default_value = "concrete")]
    mode: Mode,

    /// Path to a JSON environment model file (universal mode only). The file
    /// holds a CNF over environment literals; see EnvModelFile.
    #[clap(long)]
    env_model: Option<PathBuf>,

    /// The output CSV path.
    #[clap(long, default_value = "timings.csv")]
    output: PathBuf,

    /// Run the independent verifier on every universal solution and record
    /// the result. Off by default to keep full runs predictable; intended
    /// for smoke tests and outlier re-runs.
    #[clap(long)]
    verify: bool,

    /// Project every universal solution onto the simulated machine encoded
    /// in the snapshot (a literal evaluates true when its version set
    /// matches one of the machine candidates of its environment package)
    /// and record the projected record count. Note: this generic evaluation
    /// uses the snapshot's concrete matching, which deliberately differs
    /// from DAG-lineage semantics for packages like __archspec.
    #[clap(long)]
    project: bool,

    /// Enable tracing output (set RUST_LOG for verbosity, e.g. RUST_LOG=info)
    #[clap(long)]
    tracing: bool,

    /// Dump per-cell statistics of every universal solution
    /// (distinct solvable sets, per-axis fragmentation, per-cell conditions)
    /// to the given file.
    #[clap(long)]
    cells_dump: Option<PathBuf>,

    /// After every successful universal solve, re-solve the same problem on
    /// a FRESH provider and solver (cold cache) with the first solution's
    /// cells as the seed partition — the lockfile re-resolve flow — and
    /// record the reseed duration, enumeration pass count and whether the
    /// reseeded solution is byte-identical, in extra CSV columns.
    #[clap(long)]
    reseed: bool,

    /// With this probability, each generated root requirement becomes a
    /// conditional requirement `req IF version-set-of-another-randomly-chosen
    /// -concrete-package` (the condition targets a different package than the
    /// requirement). The default of 0 draws nothing extra from the rng, so
    /// the generated problem corpus stays byte-identical to earlier versions
    /// of this tool.
    #[clap(long, default_value = "0.0")]
    conditional_prob: f64,
}

/// One signed environment literal in the model file, e.g.
/// `{"package": "__cuda", "absent": true}` or
/// `{"package": "__glibc", "matches": ">=2.17,<3.0a0", "positive": false}`.
/// Exactly one of `absent`/`matches` must be present; `positive` defaults to
/// true. `matches` is resolved against the snapshot's version set display
/// strings.
#[derive(Debug, serde::Deserialize)]
struct ModelLiteral {
    package: String,
    #[serde(default)]
    matches: Option<String>,
    #[serde(default)]
    absent: bool,
    #[serde(default = "default_true")]
    positive: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Deserialize)]
struct EnvModelFile {
    clauses: Vec<Vec<ModelLiteral>>,
}

#[derive(Debug, serde::Serialize)]
struct Record {
    index: usize,
    mode: &'static str,
    requirements: String,
    duration: f64,
    outcome: &'static str,
    records: Option<usize>,
    cells: Option<usize>,
    env_literals: Option<usize>,
    verify: Option<String>,
    projected: Option<String>,
    error: Option<String>,
    /// Provider round-trips of the solve (see
    /// [`Solver::provider_fetch_count`]): what the lazy
    /// conditional-candidates path is meant to reduce.
    fetches: Option<usize>,
}

/// The CSV row written with `--reseed`: every [`Record`] column (in the same
/// order, so the file stays comparable) plus the reseed measurements. Kept
/// as a separate struct because the `csv` crate derives the header from the
/// struct fields, and the default (`--reseed` off) output must stay
/// byte-identical to before.
#[derive(Debug, serde::Serialize)]
struct ReseedRecord {
    index: usize,
    mode: &'static str,
    requirements: String,
    duration: f64,
    outcome: &'static str,
    records: Option<usize>,
    cells: Option<usize>,
    env_literals: Option<usize>,
    verify: Option<String>,
    projected: Option<String>,
    error: Option<String>,
    /// Provider round-trips of the original solve (see [`Record::fetches`]).
    fetches: Option<usize>,
    /// Wall time of the cold-cache seeded re-solve in seconds.
    reseed_duration: Option<f64>,
    /// Enumeration passes the seeded re-solve took (see
    /// `Solver::universal_enumeration_passes`).
    reseed_passes: Option<u32>,
    /// Whether the reseeded solution is byte-identical to the original
    /// (its `Debug` cell listing compares equal; ids are comparable because
    /// the fresh provider is constructed identically from the snapshot).
    reseed_identical: Option<bool>,
    /// Provider round-trips of the cold-cache seeded re-solve.
    reseed_fetches: Option<usize>,
}

impl ReseedRecord {
    fn new(record: Record, reseed: Option<ReseedMeasurement>) -> Self {
        let Record {
            index,
            mode,
            requirements,
            duration,
            outcome,
            records,
            cells,
            env_literals,
            verify,
            projected,
            error,
            fetches,
        } = record;
        ReseedRecord {
            index,
            mode,
            requirements,
            duration,
            outcome,
            records,
            cells,
            env_literals,
            verify,
            projected,
            error,
            fetches,
            reseed_duration: reseed.as_ref().map(|m| m.duration),
            reseed_passes: reseed.as_ref().map(|m| m.passes),
            reseed_identical: reseed.as_ref().map(|m| m.identical),
            reseed_fetches: reseed.as_ref().map(|m| m.fetches),
        }
    }
}

/// The outcome of one `--reseed` re-solve.
struct ReseedMeasurement {
    duration: f64,
    passes: u32,
    identical: bool,
    fetches: usize,
}

/// One randomly drawn root requirement, kept as a spec (instead of a built
/// [`ConditionalRequirement`]) so it can be replayed onto a FRESH provider:
/// `Package` requirements intern an additional version set (and conditional
/// requirements an additional condition) on the provider they are built
/// against, and the reseed flow needs the identical ids on its own provider
/// instance.
#[derive(Copy, Clone)]
struct RequirementSpec {
    target: TargetSpec,
    /// When set (drawn with `--conditional-prob`), the requirement is the
    /// conditional `target IF condition`, where the condition is a version
    /// set on a concrete package different from the target's.
    condition: Option<VersionSetId>,
}

/// The target of one randomly drawn root requirement.
#[derive(Copy, Clone)]
enum TargetSpec {
    /// `add_package_requirement(name, "*")`.
    Package(NameId),
    /// A version set requirement straight from the snapshot.
    VersionSet(VersionSetId),
    /// A version set union requirement straight from the snapshot.
    Union(VersionSetUnionId),
}

/// Builds the root requirements for `specs` against `provider`. Replaying
/// the same specs in the same order onto identically constructed providers
/// yields identical requirement ids (`add_package_requirement` and
/// `add_condition` allocate deterministically).
fn build_requirements(
    provider: &mut SnapshotProvider<'_>,
    specs: &[RequirementSpec],
) -> Vec<ConditionalRequirement> {
    specs
        .iter()
        .map(|&spec| {
            let mut requirement: ConditionalRequirement = match spec.target {
                TargetSpec::Package(name) => provider.add_package_requirement(name, "*").into(),
                TargetSpec::VersionSet(version_set) => version_set.into(),
                TargetSpec::Union(union) => union.into(),
            };
            if let Some(version_set) = spec.condition {
                requirement.condition =
                    Some(provider.add_condition(Condition::Requirement(version_set)));
            }
            requirement
        })
        .collect()
}

/// Formats a (possibly conditional) requirement. This tool only generates
/// `Condition::Requirement` conditions, so nested binary conditions do not
/// need to be rendered.
fn display_requirement(
    provider: &SnapshotProvider<'_>,
    requirement: &ConditionalRequirement,
) -> String {
    let target = requirement.requirement.display(provider).to_string();
    match requirement.condition {
        None => target,
        Some(condition) => match provider.resolve_condition(condition) {
            Condition::Requirement(version_set) => format!(
                "{target} if {} {}",
                provider.display_name(provider.version_set_name(version_set)),
                provider.display_version_set(version_set),
            ),
            Condition::Binary(..) => format!("{target} if <binary condition>"),
        },
    }
}

/// Resolves the environment model file against the snapshot: package names
/// must be environment packages, `matches` strings must equal the display of
/// a version set of that package.
fn resolve_env_model(model: &EnvModelFile, snapshot: &DependencySnapshot) -> EnvironmentModel {
    let find_package = |name: &str| -> NameId {
        snapshot
            .packages
            .iter()
            .find(|(_, package)| package.name == name)
            .unwrap_or_else(|| panic!("model references unknown package '{name}'"))
            .0
    };
    let find_version_set = |name_id: NameId, name: &str, display: &str| -> VersionSetId {
        snapshot
            .version_sets
            .iter()
            .find(|(_, version_set)| version_set.name == name_id && version_set.display == display)
            .unwrap_or_else(|| {
                let available = snapshot
                    .version_sets
                    .iter()
                    .filter(|(_, version_set)| version_set.name == name_id)
                    .map(|(_, version_set)| version_set.display.as_str())
                    .format(", ");
                panic!(
                    "model references unknown version set '{display}' of '{name}'; \
                     available: {available}"
                )
            })
            .0
    };

    model
        .clauses
        .iter()
        .map(|clause| {
            clause
                .iter()
                .map(|literal| {
                    let name_id = find_package(&literal.package);
                    let package = snapshot.packages.get(name_id).unwrap();
                    assert!(
                        package.environment.is_some(),
                        "model references '{}' which is not an environment package",
                        literal.package
                    );
                    let env_literal = match (&literal.matches, literal.absent) {
                        (Some(display), false) => EnvLiteral::Matches(find_version_set(
                            name_id,
                            &literal.package,
                            display,
                        )),
                        (None, true) => {
                            assert!(
                                package.environment.unwrap().can_be_absent,
                                "model uses 'absent' for '{}' which cannot be absent",
                                literal.package
                            );
                            EnvLiteral::Absent(name_id)
                        }
                        _ => panic!(
                            "model literal for '{}' must have exactly one of 'matches'/'absent'",
                            literal.package
                        ),
                    };
                    SignedEnvLiteral::new(env_literal, literal.positive)
                })
                .collect::<EnvClause<NameId>>()
        })
        .collect()
}

/// Merges two conjunctions when they contain exactly the same
/// literals and differ in the sign of at most one (mirror of the private
/// `merge_disjunct_pair` in resolvo::solver::universal).
fn merge_disjunct_pair(
    a: &CellCondition<NameId>,
    b: &CellCondition<NameId>,
) -> Option<CellCondition<NameId>> {
    if a.len() != b.len() {
        return None;
    }
    let mut differing = None;
    for (index, sa) in a.literals().enumerate() {
        let sb = b.literals().find(|sb| sb.literal == sa.literal)?;
        if sa.positive != sb.positive {
            if differing.is_some() {
                return None;
            }
            differing = Some(index);
        }
    }
    let merged: Vec<_> = match differing {
        None => a.literals().cloned().collect(),
        Some(drop_index) => a
            .literals()
            .enumerate()
            .filter(|&(index, _)| index != drop_index)
            .map(|(_, signed)| *signed)
            .collect(),
    };
    // The merged conjunction is a subset of `a`'s literals, which are already
    // non-contradictory, so normalization cannot fail here.
    Some(CellCondition::new(merged).expect("merged disjunct is contradiction-free"))
}

/// Simplifies a disjunction of conjunctions to a fixpoint (mirror
/// of the private `simplify_disjuncts` in resolvo::solver::universal).
fn simplify_disjuncts(mut disjuncts: Vec<CellCondition<NameId>>) -> Vec<CellCondition<NameId>> {
    'merge: loop {
        for first in 0..disjuncts.len() {
            for second in first + 1..disjuncts.len() {
                let Some(merged) = merge_disjunct_pair(&disjuncts[first], &disjuncts[second])
                else {
                    continue;
                };
                if merged.is_empty() {
                    return vec![CellCondition::default()];
                }
                disjuncts[first] = merged;
                disjuncts.remove(second);
                continue 'merge;
            }
        }
        return disjuncts;
    }
}

/// Dumps per-cell statistics of a universal solution: distinct
/// solvable sets across cells, the simplified disjunct count per set (an
/// achievable partition size with the current literal vocabulary), per-axis
/// fragmentation, and the full per-cell condition listing.
fn dump_cell_stats(
    path: &PathBuf,
    problem_index: usize,
    solution: &UniversalSolution<SolvableId, NameId>,
    provider: &SnapshotProvider<'_>,
    snapshot: &DependencySnapshot,
) {
    use std::io::Write;

    use resolvo::Interner;

    let mut out = std::io::BufWriter::new(File::create(path).unwrap());

    // Group cells by solvable set. The solvable lists are canonical (sorted
    // by solver variable id), so identical sets compare equal as vectors.
    let mut groups: Vec<(Vec<SolvableId>, Vec<usize>)> = Vec::new();
    for (idx, cell) in solution.cells().iter().enumerate() {
        match groups
            .iter_mut()
            .find(|(set, _)| set.as_slice() == cell.solvables())
        {
            Some((_, cells)) => cells.push(idx),
            None => groups.push((cell.solvables().to_vec(), vec![idx])),
        }
    }

    // Per-group simplified disjunct count: how many conjunctive cells the
    // group's region actually needs with the current literal vocabulary.
    let mut total_simplified = 0usize;
    let mut group_simplified: Vec<usize> = Vec::new();
    for (_, cells) in &groups {
        let disjuncts: Vec<CellCondition<NameId>> = cells
            .iter()
            .map(|&idx| solution.cells()[idx].condition().clone())
            .collect();
        let simplified = simplify_disjuncts(disjuncts).len();
        group_simplified.push(simplified);
        total_simplified += simplified;
    }

    writeln!(out, "=== problem {problem_index} cell statistics ===").unwrap();
    writeln!(
        out,
        "cells: {}  distinct solvable sets: {}  simplified partition size: {}",
        solution.cells().len(),
        groups.len(),
        total_simplified
    )
    .unwrap();

    // Axis statistics: per environment package, the distinct literals seen
    // in cell conditions and how many cells mention them.
    writeln!(out, "\n=== axis fragmentation ===").unwrap();
    let mut axis: Vec<(NameId, Vec<(String, usize, usize)>)> = Vec::new();
    for cell in solution.cells() {
        for signed in cell.condition().literals() {
            let (package, display) = match signed.literal {
                EnvLiteral::Absent(name) => (name, "absent".to_string()),
                EnvLiteral::Matches(vs) => {
                    let version_set = snapshot.version_sets.get(vs).unwrap();
                    (version_set.name, version_set.display.clone())
                }
            };
            let sign = signed.positive;
            let package_entry = match axis.iter_mut().find(|(p, _)| *p == package) {
                Some(entry) => entry,
                None => {
                    axis.push((package, Vec::new()));
                    axis.last_mut().unwrap()
                }
            };
            match package_entry.1.iter_mut().find(|(d, _, _)| *d == display) {
                Some((_, pos, neg)) => {
                    if sign {
                        *pos += 1;
                    } else {
                        *neg += 1;
                    }
                }
                None => package_entry.1.push((
                    display,
                    if sign { 1 } else { 0 },
                    if sign { 0 } else { 1 },
                )),
            }
        }
    }
    for (package, literals) in &axis {
        writeln!(
            out,
            "{}: {} distinct literals",
            provider.display_name(*package),
            literals.len()
        )
        .unwrap();
        for (display, pos, neg) in literals {
            writeln!(out, "  {display}: pos in {pos} cells, neg in {neg} cells").unwrap();
        }
    }

    // Per-group details: size, cells, simplified count, diff vs group 0.
    writeln!(out, "\n=== solvable-set groups ===").unwrap();
    let baseline = groups
        .first()
        .map(|(set, _)| set.clone())
        .unwrap_or_default();
    for (gid, (set, cells)) in groups.iter().enumerate() {
        let added: Vec<String> = set
            .iter()
            .filter(|s| !baseline.contains(s))
            .map(|&s| provider.display_solvable(s).to_string())
            .collect();
        let removed: Vec<String> = baseline
            .iter()
            .filter(|s| !set.contains(s))
            .map(|&s| provider.display_solvable(s).to_string())
            .collect();
        writeln!(
            out,
            "group {gid}: {} records, {} cells, simplified to {} cell(s); \
             vs group 0: +{} -{}",
            set.len(),
            cells.len(),
            group_simplified[gid],
            added.len(),
            removed.len(),
        )
        .unwrap();
        if gid > 0 {
            writeln!(out, "  added: {}", added.join(", ")).unwrap();
            writeln!(out, "  removed: {}", removed.join(", ")).unwrap();
        }
    }

    // Full per-cell listing.
    writeln!(out, "\n=== cells ===").unwrap();
    let group_of = |idx: usize| {
        groups
            .iter()
            .position(|(_, cells)| cells.contains(&idx))
            .unwrap()
    };
    for (idx, cell) in solution.cells().iter().enumerate() {
        writeln!(
            out,
            "cell {idx} (group {}): {}",
            group_of(idx),
            cell.condition().display(provider)
        )
        .unwrap();
    }

    eprintln!(
        "cells dump: {} cells, {} distinct solvable sets, simplified partition \
         size {} -> {}",
        solution.cells().len(),
        groups.len(),
        total_simplified,
        path.display()
    );
}

/// Truncates an error message to keep the CSV readable.
fn truncate_error(message: String) -> String {
    const LIMIT: usize = 400;
    if message.len() <= LIMIT {
        message
    } else {
        let mut cut = LIMIT;
        while !message.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &message[..cut])
    }
}

fn main() {
    let opts: Opts = Opts::parse();

    if opts.tracing {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    }

    eprintln!("Loading snapshot ...");
    let snapshot_file = BufReader::new(File::open(&opts.snapshot).unwrap());
    let snapshot: DependencySnapshot = serde_json::from_reader(snapshot_file).unwrap();

    let env_model = match (opts.mode, &opts.env_model) {
        (Mode::Universal, Some(path)) => {
            let model_file = BufReader::new(File::open(path).unwrap());
            let model: EnvModelFile = serde_json::from_reader(model_file).unwrap();
            Some(resolve_env_model(&model, &snapshot))
        }
        (Mode::Universal, None) => panic!("universal mode requires --env-model"),
        (Mode::Concrete, Some(_)) => panic!("--env-model only applies to universal mode"),
        (Mode::Concrete, None) => None,
    };
    if opts.reseed && opts.mode == Mode::Concrete {
        panic!("--reseed only applies to universal mode");
    }
    if !(0.0..=1.0).contains(&opts.conditional_prob) {
        panic!("--conditional-prob must be within 0..=1");
    }
    let mode_label = match opts.mode {
        Mode::Concrete => "concrete",
        Mode::Universal => "universal",
    };

    // The names of environment packages and the version sets / unions that
    // reference them. Both are excluded from random problem generation in
    // every mode (identically, so the corpus stays mode independent): real
    // resolution problems do not request virtual packages directly.
    let environment_names: HashSet<NameId> = snapshot
        .packages
        .iter()
        .filter(|(_, package)| package.environment.is_some())
        .map(|(name_id, _)| name_id)
        .collect();
    let is_env_version_set =
        |id: VersionSetId| environment_names.contains(&snapshot.version_sets.get(id).unwrap().name);

    let mut writer = WriterBuilder::new()
        .has_headers(true)
        .from_path(&opts.output)
        .unwrap();

    // Generate a range of problems.
    let mut rng = StdRng::seed_from_u64(opts.seed);
    let requirement_dist = WeightedIndex::new([
        10, // 10 times more likely to pick a package
        if !snapshot.version_sets.is_empty() {
            1
        } else {
            0
        },
        if !snapshot.version_set_unions.is_empty() {
            1
        } else {
            0
        },
    ])
    .unwrap();
    for i in 0..opts.limit {
        // Construct a problem with a random number of requirements. The
        // requirements are kept as replayable specs (see [`RequirementSpec`])
        // so that the `--reseed` flow can rebuild them identically on a
        // fresh provider; the rng consumption is identical to building them
        // directly, so the corpus is unchanged.
        let mut specs: Vec<RequirementSpec> = Vec::new();

        // Determine the number of requirements to solve for.
        let num_requirements = rng.random_range(1..=10usize);
        for _ in 0..num_requirements {
            let target = match requirement_dist.sample(&mut rng) {
                0 => {
                    // Add a package requirement
                    let (package, _) = snapshot
                        .packages
                        .iter()
                        .filter(|(_, package)| package.environment.is_none())
                        .choose(&mut rng)
                        .unwrap();
                    TargetSpec::Package(package)
                }
                1 => {
                    // Add a version set requirement
                    let (version_set_id, _) = snapshot
                        .version_sets
                        .iter()
                        .filter(|&(id, _)| !is_env_version_set(id))
                        .choose(&mut rng)
                        .unwrap();
                    TargetSpec::VersionSet(version_set_id)
                }
                2 => {
                    // Add a version set union requirement
                    let (version_set_union_id, _) = snapshot
                        .version_set_unions
                        .iter()
                        .filter(|(_, sets)| !sets.iter().any(|&id| is_env_version_set(id)))
                        .choose(&mut rng)
                        .unwrap();
                    TargetSpec::Union(version_set_union_id)
                }
                _ => unreachable!(),
            };

            // With --conditional-prob, make the requirement conditional on a
            // version set of another randomly chosen concrete package. The
            // strict `> 0.0` guard keeps the rng untouched at the default, so
            // the generated corpus stays byte-identical.
            let condition =
                if opts.conditional_prob > 0.0 && rng.random::<f64>() < opts.conditional_prob {
                    let target_names: Vec<NameId> = match target {
                        TargetSpec::Package(name) => vec![name],
                        TargetSpec::VersionSet(id) => {
                            vec![snapshot.version_sets.get(id).unwrap().name]
                        }
                        TargetSpec::Union(union) => snapshot
                            .version_set_unions
                            .get(union)
                            .unwrap()
                            .iter()
                            .map(|&id| snapshot.version_sets.get(id).unwrap().name)
                            .collect(),
                    };
                    snapshot
                        .version_sets
                        .iter()
                        .filter(|&(id, version_set)| {
                            !is_env_version_set(id) && !target_names.contains(&version_set.name)
                        })
                        .choose(&mut rng)
                        .map(|(id, _)| id)
                } else {
                    None
                };

            specs.push(RequirementSpec { target, condition });
        }

        if i < opts.skip {
            continue;
        }

        // Construct a fresh provider from the snapshot
        let mut provider = snapshot
            .provider()
            .with_timeout(SystemTime::now().add(Duration::from_secs(opts.timeout)));
        let requirements = build_requirements(&mut provider, &specs);

        eprintln!(
            "solving ({}/{})...\n{}",
            i + 1,
            opts.limit,
            requirements.iter().format_with("\n", |requirement, f| {
                f(&format_args!(
                    "- {}",
                    style(display_requirement(&provider, requirement)).dim()
                ))
            })
        );

        let problem_name = requirements
            .iter()
            .format_with("\n", |requirement, f| {
                f(&format_args!(
                    "{}",
                    display_requirement(&provider, requirement)
                ))
            })
            .to_string();

        let mut record = Record {
            index: i,
            mode: mode_label,
            requirements: problem_name,
            duration: 0.0,
            outcome: "ok",
            records: None,
            cells: None,
            env_literals: None,
            verify: None,
            projected: None,
            error: None,
            fetches: None,
        };

        let mut reseed_measurement: Option<ReseedMeasurement> = None;

        let start = Instant::now();
        match &env_model {
            None => {
                let problem = Problem::default().requirements(requirements);
                let mut solver = Solver::new(provider);
                let result = solver.solve(problem);
                record.duration = start.elapsed().as_secs_f64();
                record.fetches = Some(solver.provider_fetch_count());
                match result {
                    Ok(solution) => {
                        eprintln!(
                            "{}",
                            style(format!(
                                "==> OK in {:.2}ms, {} records",
                                record.duration * 1000.0,
                                solution.len(),
                            ))
                            .green()
                        );
                        record.records = Some(solution.len());
                    }
                    Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
                        eprintln!(
                            "{}",
                            style(format!("==> FAIL in {:.2}ms", record.duration * 1000.0))
                                .yellow()
                        );
                        record.outcome = "unsolvable";
                        record.error = Some(truncate_error(
                            conflict.display_user_friendly(&solver).to_string(),
                        ));
                    }
                    Err(UnsolvableOrCancelled::Cancelled(_)) => {
                        eprintln!(
                            "{}",
                            style(format!(
                                "==> CANCELLED after {:.2}ms",
                                record.duration * 1000.0
                            ))
                            .red()
                        );
                        record.outcome = "timeout";
                    }
                }
            }
            Some(model) => {
                let problem = UniversalProblem::new()
                    .requirements(requirements)
                    .environment_model(model.clone());
                let mut solver = Solver::new(provider);
                // Sweep override for the refutation switch (see
                // docs/design/universal-refutation-ordering.md); the
                // compiled-in default applies when the variable is unset.
                #[cfg(feature = "diagnostics")]
                if let Ok(limit) = std::env::var("RESOLVO_ENV_ORDERING_CONFLICT_LIMIT") {
                    solver.set_env_ordering_conflict_limit(
                        limit
                            .parse()
                            .expect("RESOLVO_ENV_ORDERING_CONFLICT_LIMIT must be an integer"),
                    );
                }
                #[cfg(feature = "diagnostics")]
                if let (Ok(factor), Ok(floor)) = (
                    std::env::var("RESOLVO_ENV_ORDERING_WORK_FACTOR"),
                    std::env::var("RESOLVO_ENV_ORDERING_WORK_FLOOR"),
                ) {
                    solver.set_env_ordering_work_budget(
                        factor
                            .parse()
                            .expect("RESOLVO_ENV_ORDERING_WORK_FACTOR must be an integer"),
                        floor
                            .parse()
                            .expect("RESOLVO_ENV_ORDERING_WORK_FLOOR must be an integer"),
                    );
                }
                let result = solver.solve_universal(problem);
                record.duration = start.elapsed().as_secs_f64();
                record.fetches = Some(solver.provider_fetch_count());
                // Coverage-precheck observability (one parseable line per
                // problem): precheck calls/breaks (each break is one avoided
                // final `run_sat` refutation), assembled formula sizes, and
                // build/search cost, plus the solve's conflict count for
                // A/B comparisons against precheck-less builds.
                #[cfg(feature = "diagnostics")]
                {
                    let stats = solver.coverage_precheck_stats();
                    eprintln!(
                        "PRECHECK index={} calls={} breaks={} avoided_run_sat={} clauses={} \
                         literals={} build_s={:.6} search_s={:.6} conflicts={}",
                        i,
                        stats.calls,
                        stats.breaks,
                        stats.avoided_run_sat_calls(),
                        stats.clauses_assembled,
                        stats.literals_assembled,
                        stats.build_duration.as_secs_f64(),
                        stats.search_duration.as_secs_f64(),
                        solver.conflict_count(),
                    );
                }
                match result {
                    Ok(solution) => {
                        let distinct: HashSet<_> = solution
                            .cells()
                            .iter()
                            .flat_map(|cell| cell.solvables().iter().copied())
                            .collect();
                        let mut literals: Vec<&EnvLiteral<NameId>> = Vec::new();
                        for signed in solution
                            .cells()
                            .iter()
                            .flat_map(|cell| cell.condition().literals())
                        {
                            if !literals.contains(&&signed.literal) {
                                literals.push(&signed.literal);
                            }
                        }
                        eprintln!(
                            "{}",
                            style(format!(
                                "==> OK in {:.2}ms, {} cells, {} distinct records",
                                record.duration * 1000.0,
                                solution.cells().len(),
                                distinct.len(),
                            ))
                            .green()
                        );
                        record.records = Some(distinct.len());
                        record.cells = Some(solution.cells().len());
                        record.env_literals = Some(literals.len());
                        if let Some(path) = &opts.cells_dump {
                            dump_cell_stats(path, i, &solution, solver.provider(), &snapshot);
                        }
                        if opts.project {
                            let projected = solution.project(|literal| match *literal {
                                EnvLiteral::Absent(name) => {
                                    snapshot.packages.get(name).unwrap().solvables.is_empty()
                                }
                                EnvLiteral::Matches(version_set) => {
                                    let version_set =
                                        snapshot.version_sets.get(version_set).unwrap();
                                    let package = snapshot.packages.get(version_set.name).unwrap();
                                    package.solvables.iter().any(|solvable| {
                                        version_set.matching_candidates.contains(solvable)
                                    })
                                }
                            });
                            record.projected = Some(match projected {
                                Some(solvables) => solvables.len().to_string(),
                                None => "none".to_string(),
                            });
                        }
                        if opts.verify {
                            record.verify = Some(match solution.verify(solver.provider()) {
                                Ok(()) => "ok".to_string(),
                                Err(violations) => {
                                    eprintln!(
                                        "{}",
                                        style(format!("==> VERIFY FAILED: {violations:?}")).red()
                                    );
                                    truncate_error(format!("{violations:?}"))
                                }
                            });
                        }
                        if opts.reseed {
                            // The lockfile re-resolve flow: a FRESH provider
                            // and solver (cold cache), seeded with the cells
                            // just found. The provider and requirements are
                            // rebuilt exactly like the originals, so every id
                            // embedded in the solutions is comparable and the
                            // reseeded output must be byte-identical.
                            let mut reseed_provider = snapshot.provider().with_timeout(
                                SystemTime::now().add(Duration::from_secs(opts.timeout)),
                            );
                            let reseed_requirements =
                                build_requirements(&mut reseed_provider, &specs);
                            let seeds: Vec<CellCondition<NameId>> = solution
                                .cells()
                                .iter()
                                .map(|cell| cell.condition().clone())
                                .collect();
                            let reseed_problem = UniversalProblem::new()
                                .requirements(reseed_requirements)
                                .environment_model(model.clone())
                                .seed_partition(seeds);
                            let mut reseed_solver = Solver::new(reseed_provider);
                            let reseed_start = Instant::now();
                            let reseed_result = reseed_solver.solve_universal(reseed_problem);
                            let duration = reseed_start.elapsed().as_secs_f64();
                            let passes = reseed_solver.universal_enumeration_passes();
                            let fetches = reseed_solver.provider_fetch_count();
                            let identical = match &reseed_result {
                                Ok(reseeded) => {
                                    format!("{:?}", solution.cells())
                                        == format!("{:?}", reseeded.cells())
                                }
                                Err(_) => false,
                            };
                            let line = format!(
                                "==> RESEED {} in {:.2}ms, {} pass(es)",
                                if identical { "identical" } else { "DIFFERENT" },
                                duration * 1000.0,
                                passes,
                            );
                            eprintln!(
                                "{}",
                                if identical {
                                    style(line).green()
                                } else {
                                    style(line).red()
                                }
                            );
                            reseed_measurement = Some(ReseedMeasurement {
                                duration,
                                passes,
                                identical,
                                fetches,
                            });
                        }
                    }
                    Err(UniversalFailure::Unsolvable { cell, conflict }) => {
                        eprintln!(
                            "{}",
                            style(format!("==> FAIL in {:.2}ms", record.duration * 1000.0))
                                .yellow()
                        );
                        record.outcome = "unsolvable";
                        record.error = Some(truncate_error(format!(
                            "cell {}: {}",
                            cell.display(solver.provider()),
                            conflict.display_user_friendly(&solver)
                        )));
                    }
                    Err(UniversalFailure::InvalidInput(invalid)) => {
                        eprintln!("{}", style(format!("==> INVALID INPUT: {invalid}")).red());
                        record.outcome = "invalid_input";
                        record.error = Some(truncate_error(format!("{invalid}")));
                    }
                    Err(UniversalFailure::Cancelled(_)) => {
                        eprintln!(
                            "{}",
                            style(format!(
                                "==> CANCELLED after {:.2}ms",
                                record.duration * 1000.0
                            ))
                            .red()
                        );
                        record.outcome = "timeout";
                    }
                    Err(_) => {
                        eprintln!("{}", style("==> UNEXPECTED FAILURE").red());
                        record.outcome = "error";
                    }
                }
                #[cfg(feature = "diagnostics")]
                {
                    eprintln!(
                        "    counters: {} conflicts, {} propagated, {} luby restarts, \
                         {} ordering suspensions, {} ordering restarts, {} budget aborts",
                        solver.conflict_count(),
                        solver.decisions_propagated(),
                        solver.restart_count(),
                        solver.env_ordering_suspensions(),
                        solver.env_ordering_restarts(),
                        solver.prefix_budget_aborts(),
                    );
                    let mut cell_decisions = solver.universal_cell_decisions().to_vec();
                    if !cell_decisions.is_empty() {
                        cell_decisions.sort_unstable();
                        let total: u64 = cell_decisions.iter().sum();
                        eprintln!(
                            "    cells recorded: {} (decisions total {}, median {}, max {})",
                            cell_decisions.len(),
                            total,
                            cell_decisions[cell_decisions.len() / 2],
                            cell_decisions.last().unwrap(),
                        );
                    }
                }
            }
        }

        if opts.reseed {
            writer
                .serialize(ReseedRecord::new(record, reseed_measurement))
                .unwrap();
        } else {
            writer.serialize(record).unwrap();
        }
        writer.flush().unwrap();
    }

    writer.flush().unwrap();
}
