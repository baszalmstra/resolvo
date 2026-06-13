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
    ConditionalRequirement, Dependencies, NameId, Problem, Requirement, SolvableId, Solver,
    UnsolvableOrCancelled, VersionSetId, VersionSetUnionId,
    snapshot::{DependencySnapshot, SnapshotProvider},
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

    /// Pin-bisect mode: causally localize what makes one problem slow. Solves
    /// the selected problem (use `--skip K -n K+1` to pick problem K), then
    /// re-solves it repeatedly with one package at a time pinned to the version
    /// it took in the baseline solution, and reports how much each pin collapses
    /// the solve time. The package whose pin recovers most of the gap to the
    /// fully-pinned floor is the bottleneck decision.
    #[clap(long)]
    pin_bisect: bool,

    /// How many packages to probe in `--pin-bisect`, ranked by candidate count
    /// (most version freedom first).
    #[clap(long, default_value = "15")]
    pin_bisect_k: usize,

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
    /// Of the distinct version sets constraining the hub, the largest fraction
    /// satisfiable by a *single* hub candidate. 1.0 means one version (e.g. one
    /// python) satisfies every constraint — no version conflict; lower values
    /// mean the constraints fragment across incompatible versions (divergence).
    hub_max_coverage: f64,
    /// Fraction of the hub's constraining version sets that do NOT admit the
    /// most-preferred (newest) hub candidate — i.e. how often the version the
    /// solver guesses first is excluded, the direct driver of version thrash.
    hub_top_exclusion: f64,
    /// 1 if some hub candidate satisfies *every* constraining version set (a
    /// globally compatible version exists), else 0.
    hub_globally_compatible: u8,
    /// Number of distinct hub candidates admitted by at least one constraint
    /// (how spread the viable-version choice is).
    hub_live_candidates: usize,
}

/// Measures how much the version sets constraining `hub` *diverge*: whether a
/// single hub candidate can satisfy all of them, how often the preferred
/// candidate is excluded, and how fragmented the viable choice is. Operates on
/// the `matching_candidates` the snapshot already stores per version set, so it
/// needs no version-spec parsing.
fn hub_divergence(
    snapshot: &DependencySnapshot,
    hub: NameId,
    hub_version_sets: &std::collections::HashSet<VersionSetId>,
) -> (f64, f64, u8, usize) {
    let Some(package) = snapshot.packages.get(hub) else {
        return (1.0, 0.0, 1, 0);
    };
    let total_vs = hub_version_sets.len();
    if total_vs == 0 {
        return (1.0, 0.0, 1, package.solvables.len());
    }

    // Most-preferred (order 0) candidate of the hub package.
    let preferred = package
        .solvables
        .iter()
        .copied()
        .min_by_key(|&s| snapshot.solvables.get(s).map(|sv| sv.order).unwrap_or(u32::MAX));

    // Per-candidate coverage: how many constraining version sets admit it.
    let mut max_coverage = 0usize;
    let mut live_candidates = 0usize;
    for &candidate in &package.solvables {
        let coverage = hub_version_sets
            .iter()
            .filter(|&&vs| {
                snapshot
                    .version_sets
                    .get(vs)
                    .is_some_and(|v| v.matching_candidates.contains(&candidate))
            })
            .count();
        if coverage > 0 {
            live_candidates += 1;
        }
        max_coverage = max_coverage.max(coverage);
    }

    let top_excluded = match preferred {
        Some(p) => hub_version_sets
            .iter()
            .filter(|&&vs| {
                snapshot
                    .version_sets
                    .get(vs)
                    .is_some_and(|v| !v.matching_candidates.contains(&p))
            })
            .count(),
        None => total_vs,
    };

    (
        max_coverage as f64 / total_vs as f64,
        top_excluded as f64 / total_vs as f64,
        u8::from(max_coverage == total_vs),
        live_candidates,
    )
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

    let empty = std::collections::HashSet::new();
    let hub_vs_set = hub_name_id
        .and_then(|n| vsets_per_name.get(&n))
        .unwrap_or(&empty);
    let (hub_max_coverage, hub_top_exclusion, hub_globally_compatible, hub_live_candidates) =
        match hub_name_id {
            Some(hub) => hub_divergence(snapshot, hub, hub_vs_set),
            None => (1.0, 0.0, 1, 0),
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
        hub_max_coverage,
        hub_top_exclusion,
        hub_globally_compatible,
        hub_live_candidates,
    }
}

#[derive(Debug, serde::Serialize)]
struct Record {
    package: String,
    duration: f64,
    error: Option<String>,
    records: Option<usize>,
}

/// A rebuildable description of one root requirement. Package requirements
/// allocate provider-specific version sets, so a problem cannot be replayed by
/// keeping its `ConditionalRequirement`s; this records the snapshot-stable
/// choice instead, letting the same problem be reconstructed on a fresh
/// provider for each pin-bisect re-solve.
#[derive(Clone)]
enum ReqSpec {
    Package(NameId),
    VersionSet(VersionSetId),
    Union(VersionSetUnionId),
}

/// Rebuilds a problem's requirements from its recipe on `provider`.
fn build_requirements(
    provider: &mut SnapshotProvider,
    recipe: &[ReqSpec],
) -> Vec<ConditionalRequirement> {
    recipe
        .iter()
        .map(|spec| match spec {
            ReqSpec::Package(name) => provider.add_package_requirement(*name, "*").into(),
            ReqSpec::VersionSet(vs) => (*vs).into(),
            ReqSpec::Union(u) => (*u).into(),
        })
        .collect()
}

/// Solves the recipe once on a fresh provider with `pins` applied, returning the
/// wall-clock duration and the solution (or `None` if it was unsolvable or
/// cancelled by the timeout).
fn solve_with_pins(
    snapshot: &DependencySnapshot,
    recipe: &[ReqSpec],
    pins: &[(NameId, SolvableId)],
    timeout: Duration,
) -> (Duration, Option<Vec<SolvableId>>) {
    let mut provider = SnapshotProvider::new(snapshot).with_timeout(SystemTime::now().add(timeout));
    for &(name, solvable) in pins {
        provider.pin(name, solvable);
    }
    let requirements = build_requirements(&mut provider, recipe);
    let problem = Problem::default().requirements(requirements);
    let mut solver = Solver::new(provider);
    let start = Instant::now();
    let result = solver.solve(problem);
    let duration = start.elapsed();
    (duration, result.ok())
}

/// Runs the pin-bisect probe described by [`Opts::pin_bisect`].
fn pin_bisect(snapshot: &DependencySnapshot, recipe: &[ReqSpec], opts: &Opts) {
    use std::cmp::Reverse;

    let timeout = Duration::from_secs(opts.timeout);
    eprintln!("pin-bisect: baseline solve (timeout {}s)...", opts.timeout);
    let (base, solution) = solve_with_pins(snapshot, recipe, &[], timeout);
    let Some(solution) = solution else {
        eprintln!(
            "baseline did not produce a solution in {:.1}s (unsolvable or timed out); \
             pin-bisect needs a solvable problem.",
            base.as_secs_f64()
        );
        return;
    };
    eprintln!(
        "baseline: {:.2}s, {} records",
        base.as_secs_f64(),
        solution.len()
    );

    // Map every installed package to the solvable it took.
    let solved: Vec<(NameId, SolvableId)> = solution
        .iter()
        .filter_map(|&s| snapshot.solvables.get(s).map(|sv| (sv.name, s)))
        .collect();

    // Floor: pin the entire solution. This is essentially verification time and
    // bounds how fast any single pin could make the solve.
    let (floor, _) = solve_with_pins(snapshot, recipe, &solved, timeout);
    eprintln!("floor (whole solution pinned): {:.3}s\n", floor.as_secs_f64());

    // Probe the installed packages with the most version freedom first.
    let mut ranked: Vec<(NameId, SolvableId, usize)> = solved
        .iter()
        .filter_map(|&(name, solvable)| {
            let candidates = snapshot.packages.get(name)?.solvables.len();
            (candidates > 1).then_some((name, solvable, candidates))
        })
        .collect();
    ranked.sort_by_key(|&(_, _, candidates)| Reverse(candidates));
    ranked.truncate(opts.pin_bisect_k);

    // Cap each probe a little above the baseline: a pin that does not help is
    // no faster than baseline, so there is nothing to learn past that point.
    let probe_timeout = timeout.min(base.mul_f64(1.25) + Duration::from_secs(1));

    let mut rows: Vec<(String, usize, f64, f64)> = Vec::new();
    for (name, solvable, candidates) in ranked {
        let display = snapshot
            .packages
            .get(name)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let (t, sol) = solve_with_pins(snapshot, recipe, &[(name, solvable)], probe_timeout);
        let capped = sol.is_none() && t >= probe_timeout.mul_f64(0.98);
        let secs = t.as_secs_f64();
        let speedup = base.as_secs_f64() / secs.max(1e-6);
        eprintln!(
            "  pinned {:32} ({:3} cands) -> {:7.3}s  {:.1}x{}",
            display,
            candidates,
            secs,
            speedup,
            if capped { " (capped)" } else { "" }
        );
        rows.push((display, candidates, secs, speedup));
    }

    rows.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap());
    println!("\n=== pin-bisect report ===");
    println!(
        "baseline {:.2}s   floor {:.3}s   gap {:.2}s",
        base.as_secs_f64(),
        floor.as_secs_f64(),
        (base - floor.min(base)).as_secs_f64()
    );
    println!("{:<34} {:>6} {:>9} {:>8}", "pinned package", "cands", "time", "speedup");
    for (name, cands, secs, speedup) in &rows {
        println!("{name:<34} {cands:>6} {secs:>8.3}s {speedup:>7.1}x");
    }
    if let Some((name, _, secs, speedup)) = rows.first() {
        println!(
            "\nbottleneck: pinning `{}` alone is {:.1}x faster ({:.3}s vs {:.2}s baseline){}",
            name,
            speedup,
            secs,
            base.as_secs_f64(),
            if *secs <= floor.as_secs_f64() * 2.0 {
                " — recovers essentially the whole gap to the floor"
            } else {
                ""
            }
        );
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
        // A rebuildable recipe of this problem (for `--pin-bisect`).
        let mut recipe: Vec<ReqSpec> = Vec::new();

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
                    recipe.push(ReqSpec::Package(package));
                }
                1 => {
                    // Add a version set requirement
                    let (version_set_id, version_set) =
                        snapshot.version_sets.iter().choose(&mut rng).unwrap();
                    requirements.push(version_set_id.into());
                    root_names.push(version_set.name);
                    recipe.push(ReqSpec::VersionSet(version_set_id));
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
                    recipe.push(ReqSpec::Union(version_set_union_id));
                }
                _ => unreachable!(),
            }
        }

        if i < opts.skip {
            continue;
        }

        if opts.pin_bisect {
            pin_bisect(&snapshot, &recipe, &opts);
            break;
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
