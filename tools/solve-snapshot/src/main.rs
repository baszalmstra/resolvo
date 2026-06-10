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
    ConditionalRequirement, EnvLiteral, EnvLiteralKind, EnvironmentModel, NameId, Problem, Solver,
    UniversalFailure, UniversalProblem, UnsolvableOrCancelled, VersionSetId,
    snapshot::DependencySnapshot,
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
                    let kind = match (&literal.matches, literal.absent) {
                        (Some(display), false) => EnvLiteralKind::Matches(find_version_set(
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
                            EnvLiteralKind::Absent
                        }
                        _ => panic!(
                            "model literal for '{}' must have exactly one of 'matches'/'absent'",
                            literal.package
                        ),
                    };
                    (
                        EnvLiteral {
                            package: name_id,
                            kind,
                        },
                        literal.positive,
                    )
                })
                .collect()
        })
        .collect()
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
        // Construct a fresh provider from the snapshot
        let mut provider = snapshot
            .provider()
            .with_universal_mode(opts.mode == Mode::Universal)
            .with_timeout(SystemTime::now().add(Duration::from_secs(opts.timeout)));

        // Construct a problem with a random number of requirements.
        let mut requirements: Vec<ConditionalRequirement> = Vec::new();

        // Determine the number of requirements to solve for.
        let num_requirements = rng.random_range(1..=10usize);
        for _ in 0..num_requirements {
            match requirement_dist.sample(&mut rng) {
                0 => {
                    // Add a package requirement
                    let (package, _) = snapshot
                        .packages
                        .iter()
                        .filter(|(_, package)| package.environment.is_none())
                        .choose(&mut rng)
                        .unwrap();
                    let package_requirement = provider.add_package_requirement(package, "*");
                    requirements.push(package_requirement.into());
                }
                1 => {
                    // Add a version set requirement
                    let (version_set_id, _) = snapshot
                        .version_sets
                        .iter()
                        .filter(|&(id, _)| !is_env_version_set(id))
                        .choose(&mut rng)
                        .unwrap();
                    requirements.push(version_set_id.into());
                }
                2 => {
                    // Add a version set union requirement
                    let (version_set_union_id, _) = snapshot
                        .version_set_unions
                        .iter()
                        .filter(|(_, sets)| !sets.iter().any(|&id| is_env_version_set(id)))
                        .choose(&mut rng)
                        .unwrap();
                    requirements.push(version_set_union_id.into());
                }
                _ => unreachable!(),
            }
        }

        if i < opts.skip {
            continue;
        }

        eprintln!(
            "solving ({}/{})...\n{}",
            i + 1,
            opts.limit,
            requirements.iter().format_with("\n", |requirement, f| {
                f(&format_args!(
                    "- {}",
                    style(requirement.requirement.display(&provider)).dim()
                ))
            })
        );

        let problem_name = requirements
            .iter()
            .format_with("\n", |requirement, f| {
                f(&format_args!(
                    "{}",
                    requirement.requirement.display(&provider)
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
        };

        let start = Instant::now();
        match &env_model {
            None => {
                let problem = Problem::default().requirements(requirements);
                let mut solver = Solver::new(provider);
                let result = solver.solve(problem);
                record.duration = start.elapsed().as_secs_f64();
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
                let result = solver.solve_universal(problem);
                record.duration = start.elapsed().as_secs_f64();
                match result {
                    Ok(solution) => {
                        let distinct: HashSet<_> = solution
                            .cells
                            .iter()
                            .flat_map(|(_, solvables)| solvables.iter().copied())
                            .collect();
                        let mut literals: Vec<&EnvLiteral<NameId>> = Vec::new();
                        for (literal, _) in solution
                            .cells
                            .iter()
                            .flat_map(|(condition, _)| condition.0.iter())
                        {
                            if !literals.contains(&literal) {
                                literals.push(literal);
                            }
                        }
                        eprintln!(
                            "{}",
                            style(format!(
                                "==> OK in {:.2}ms, {} cells, {} distinct records",
                                record.duration * 1000.0,
                                solution.cells.len(),
                                distinct.len(),
                            ))
                            .green()
                        );
                        record.records = Some(distinct.len());
                        record.cells = Some(solution.cells.len());
                        record.env_literals = Some(literals.len());
                        if opts.project {
                            let projected = solution.project(|literal| {
                                let package = snapshot.packages.get(literal.package).unwrap();
                                match &literal.kind {
                                    EnvLiteralKind::Absent => package.solvables.is_empty(),
                                    EnvLiteralKind::Matches(version_set) => {
                                        let version_set =
                                            snapshot.version_sets.get(*version_set).unwrap();
                                        package.solvables.iter().any(|solvable| {
                                            version_set.matching_candidates.contains(solvable)
                                        })
                                    }
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
                }
            }
        }

        writer.serialize(record).unwrap();
        writer.flush().unwrap();
    }

    writer.flush().unwrap();
}
