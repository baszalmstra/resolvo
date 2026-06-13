use std::{
    fs::File,
    io::BufReader,
    ops::Add,
    time::{Duration, Instant, SystemTime},
};

use clap::Parser;
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
    ConditionalRequirement, Dependencies, NameId, Problem, Requirement, Solver,
    UnsolvableOrCancelled, VersionSetId, snapshot::DependencySnapshot,
};

#[derive(Parser)]
#[clap(version = "0.1.0", author = "Bas Zalmstra <zalmstra.bas@gmail.com>")]
struct Opts {
    snapshot: String,

    /// The maximum number of requirements to solve
    #[clap(long, short = 'n', default_value = "1000")]
    limit: usize,

    /// The timeout to use for solving requirements in seconds. If a solve takes
    /// longer if will be cancelled.
    #[clap(long, default_value = "60")]
    timeout: u64,

    /// The random seed to use for generating the requirements.
    #[clap(long, default_value = "0")]
    seed: u64,

    /// Skip the first N problems: they are still generated (to advance the
    /// RNG deterministically) but not solved or recorded. Combined with
    /// `--seed S -n K+1 --skip K` this replays exactly problem K of the run
    /// with seed S, e.g. to re-measure an outlier in isolation.
    #[clap(long, default_value = "0")]
    skip: usize,

    /// Enable tracing output (set RUST_LOG for verbosity, e.g. RUST_LOG=info)
    #[clap(long)]
    tracing: bool,

    /// Static hub-profiling mode: instead of solving, compute each problem's
    /// dependency closure and rank package names by in-degree (how many
    /// requires-edges in the closure point at them). Writes `profile.csv`.
    /// Uses the identical RNG sequence as a normal run, so row `i` lines up
    /// with row `i` of a `timings.csv` from the same `--seed`, letting the two
    /// be joined to correlate hub structure with solve time. Does not solve.
    #[clap(long)]
    profile: bool,
}

/// Per-problem static hub profile (see [`Opts::profile`]).
#[derive(Debug, serde::Serialize)]
struct ProfileRecord {
    /// Number of distinct package names reachable from the root requirements.
    closure_names: usize,
    /// Total solvables across those names (≈ the "records" of a solve).
    closure_solvables: usize,
    /// Total requires-edges in the closure (denominator for hub concentration).
    total_requires: usize,
    /// The highest in-degree package name in the closure (the hub).
    hub_name: String,
    /// In-degree of the hub: requires-edges in the closure pointing at it.
    hub_parent_refs: usize,
    /// Number of candidates (solvables) the hub has.
    hub_candidates: usize,
    /// Distinct version sets over the hub that appear as requirements (the
    /// number of distinct mutex constraints over the hub).
    hub_version_sets: usize,
    /// Fraction of all requires-edges in the closure that point at the hub.
    hub_concentration: f64,
}

/// Walks the dependency closure of `root_names` over the snapshot graph and
/// returns, per reachable name, its in-degree (requires-edges pointing at it)
/// and the distinct version sets over it that appear as requirements. The
/// traversal is by package name: once a name is reached, every one of its
/// solvables (and their dependencies) is included, mirroring how the solver
/// encodes a package wholesale once it becomes involved.
fn profile_problem(snapshot: &DependencySnapshot, root_names: &[NameId]) -> ProfileRecord {
    use std::collections::{HashMap, HashSet, VecDeque};

    let mut closure: HashSet<NameId> = HashSet::new();
    let mut queue: VecDeque<NameId> = VecDeque::new();
    for &name in root_names {
        if closure.insert(name) {
            queue.push_back(name);
        }
    }

    let mut parent_refs: HashMap<NameId, usize> = HashMap::new();
    let mut vsets_per_name: HashMap<NameId, HashSet<VersionSetId>> = HashMap::new();
    let mut total_requires = 0usize;

    // Resolve a requirement to its target version sets, counting each as an
    // in-edge to the referenced package name and enqueueing that name.
    let visit_version_set =
        |vs: VersionSetId,
         parent_refs: &mut HashMap<NameId, usize>,
         vsets_per_name: &mut HashMap<NameId, HashSet<VersionSetId>>,
         closure: &mut HashSet<NameId>,
         queue: &mut VecDeque<NameId>| {
            let Some(version_set) = snapshot.version_sets.get(vs) else {
                return;
            };
            let target = version_set.name;
            *parent_refs.entry(target).or_default() += 1;
            vsets_per_name.entry(target).or_default().insert(vs);
            if closure.insert(target) {
                queue.push_back(target);
            }
        };

    while let Some(name) = queue.pop_front() {
        let Some(package) = snapshot.packages.get(name) else {
            continue;
        };
        for &solvable_id in &package.solvables {
            let Some(solvable) = snapshot.solvables.get(solvable_id) else {
                continue;
            };
            let Dependencies::Known(deps) = &solvable.dependencies else {
                continue;
            };
            for req in &deps.requirements {
                match req.requirement {
                    Requirement::Single(vs) => {
                        total_requires += 1;
                        visit_version_set(
                            vs,
                            &mut parent_refs,
                            &mut vsets_per_name,
                            &mut closure,
                            &mut queue,
                        );
                    }
                    Requirement::Union(union_id) => {
                        if let Some(members) = snapshot.version_set_unions.get(union_id) {
                            for &vs in members {
                                total_requires += 1;
                                visit_version_set(
                                    vs,
                                    &mut parent_refs,
                                    &mut vsets_per_name,
                                    &mut closure,
                                    &mut queue,
                                );
                            }
                        }
                    }
                }
            }
            // `constrains` only pull a name into the closure (no install
            // pressure), so they expand reachability but are not counted as
            // in-degree.
            for &vs in &deps.constrains {
                if let Some(version_set) = snapshot.version_sets.get(vs) {
                    if closure.insert(version_set.name) {
                        queue.push_back(version_set.name);
                    }
                }
            }
        }
    }

    let closure_solvables = closure
        .iter()
        .filter_map(|&n| snapshot.packages.get(n))
        .map(|p| p.solvables.len())
        .sum();

    let (hub_name_id, hub_parent_refs) = parent_refs
        .iter()
        .max_by_key(|(_, refs)| **refs)
        .map(|(&n, &refs)| (Some(n), refs))
        .unwrap_or((None, 0));

    let hub_name = hub_name_id
        .and_then(|n| snapshot.packages.get(n))
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let hub_candidates = hub_name_id
        .and_then(|n| snapshot.packages.get(n))
        .map(|p| p.solvables.len())
        .unwrap_or(0);
    let hub_version_sets = hub_name_id
        .and_then(|n| vsets_per_name.get(&n))
        .map(|s| s.len())
        .unwrap_or(0);
    let hub_concentration = if total_requires > 0 {
        hub_parent_refs as f64 / total_requires as f64
    } else {
        0.0
    };

    ProfileRecord {
        closure_names: closure.len(),
        closure_solvables,
        total_requires,
        hub_name,
        hub_parent_refs,
        hub_candidates,
        hub_version_sets,
        hub_concentration,
    }
}

#[derive(Debug, serde::Serialize)]
struct Record {
    package: String,
    duration: f64,
    error: Option<String>,
    records: Option<usize>,
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
    let snapshot_file = BufReader::new(File::open(opts.snapshot).unwrap());
    let snapshot: DependencySnapshot = serde_json::from_reader(snapshot_file).unwrap();

    let mut writer = WriterBuilder::new()
        .has_headers(true)
        .from_path("timings.csv")
        .unwrap();

    let mut profile_writer = opts.profile.then(|| {
        WriterBuilder::new()
            .has_headers(true)
            .from_path("profile.csv")
            .unwrap()
    });

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
            .with_timeout(SystemTime::now().add(Duration::from_secs(opts.timeout)));

        // Construct a problem with a random number of requirements.
        let mut requirements: Vec<ConditionalRequirement> = Vec::new();
        // The root package names of this problem (for `--profile`).
        let mut root_names: Vec<NameId> = Vec::new();

        // Determine the number of requirements to solve for.
        let num_requirements = rng.random_range(1..=10usize);
        for _ in 0..num_requirements {
            match requirement_dist.sample(&mut rng) {
                0 => {
                    // Add a package requirement
                    let (package, _) = snapshot.packages.iter().choose(&mut rng).unwrap();
                    let package_requirement = provider.add_package_requirement(package, "*");
                    requirements.push(package_requirement.into());
                    root_names.push(package);
                }
                1 => {
                    // Add a version set requirement
                    let (version_set_id, version_set) =
                        snapshot.version_sets.iter().choose(&mut rng).unwrap();
                    requirements.push(version_set_id.into());
                    root_names.push(version_set.name);
                }
                2 => {
                    // Add a version set union requirement
                    let (version_set_union_id, members) =
                        snapshot.version_set_unions.iter().choose(&mut rng).unwrap();
                    requirements.push(version_set_union_id.into());
                    for &vs in members {
                        if let Some(version_set) = snapshot.version_sets.get(vs) {
                            root_names.push(version_set.name);
                        }
                    }
                }
                _ => unreachable!(),
            }
        }

        if i < opts.skip {
            continue;
        }

        if opts.profile {
            let profile = profile_problem(&snapshot, &root_names);
            profile_writer
                .as_mut()
                .expect("profile writer")
                .serialize(profile)
                .unwrap();
            if i % 50 == 0 {
                profile_writer.as_mut().unwrap().flush().unwrap();
            }
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

        let start = Instant::now();

        let problem = Problem::default().requirements(requirements);
        let mut solver = Solver::new(provider);
        let mut records = None;
        let mut error = None;
        let result = solver.solve(problem);
        let duration = start.elapsed();
        match result {
            Ok(solution) => {
                eprintln!(
                    "{}",
                    style(format!(
                        "==> OK in {:.2}ms, {} records",
                        duration.as_secs_f64() * 1000.0,
                        solution.len(),
                    ))
                    .green()
                );
                records = Some(solution.len())
            }
            Err(UnsolvableOrCancelled::Unsolvable(problem)) => {
                eprintln!(
                    "{}",
                    style(format!(
                        "==> FAIL in {:.2}ms",
                        duration.as_secs_f64() * 1000.0
                    ))
                    .yellow()
                );
                error = Some(problem.display_user_friendly(&solver).to_string());
            }
            Err(_) => {
                eprintln!(
                    "{}",
                    style(format!(
                        "==> CANCELLED after {:.2}ms",
                        duration.as_secs_f64() * 1000.0
                    ))
                    .red()
                );
            }
        }

        writer
            .serialize(Record {
                package: problem_name,
                duration: duration.as_secs_f64(),
                error,
                records,
            })
            .unwrap();

        if i % 10 == 0 {
            writer.flush().unwrap();
        }
    }

    writer.flush().unwrap();
    if let Some(mut profile_writer) = profile_writer {
        profile_writer.flush().unwrap();
    }
}
