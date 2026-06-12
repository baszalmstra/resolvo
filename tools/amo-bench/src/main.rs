//! Benchmark for the different at-most-one ("forbid multiple instances")
//! clause encodings.
//!
//! Generates synthetic layered dependency problems in which packages have many
//! candidate versions and shared dependencies whose acceptable version ranges
//! shift with the version of the depending package. This forces the solver to
//! backtrack through many candidates of the same package, which is exactly the
//! workload where the at-most-one clauses are exercised.
//!
//! ```sh
//! cargo run --release -p amo-bench -- --versions 20,100,500 --seeds 5
//! ```

use std::time::{Duration, Instant, SystemTime};

use clap::Parser;
use rand::{Rng, SeedableRng, rngs::StdRng};
use resolvo::{
    AmoEncoding, DenseIndex, Dependencies, KnownDependencies, NameId, Problem, SolvableId, Solver,
    UnsolvableOrCancelled, VersionSetId,
    snapshot::{DependencySnapshot, Package, Solvable, VersionSet},
};

#[derive(Parser)]
struct Opts {
    /// Number of dependency layers.
    #[clap(long, default_value = "4")]
    layers: usize,

    /// Number of packages per layer.
    #[clap(long, default_value = "6")]
    width: usize,

    /// Comma separated list of candidate counts per package to benchmark.
    #[clap(long, value_delimiter = ',', default_value = "20,100,500")]
    versions: Vec<usize>,

    /// Number of dependencies of each solvable on the next layer.
    #[clap(long, default_value = "3")]
    deps: usize,

    /// Half-width of the version range of a dependency edge, as a fraction of
    /// the number of versions. Smaller values create more conflicts.
    #[clap(long, default_value = "0.05")]
    spread: f64,

    /// Maximum downward shift of a dependency edge's version range, as a
    /// fraction of the number of versions. Larger values force deeper
    /// backtracking before the ranges of two parents intersect.
    #[clap(long, default_value = "0.3")]
    max_offset: f64,

    /// Number of random problem instances per cell.
    #[clap(long, default_value = "5")]
    seeds: u64,

    /// Comma separated list of encodings to benchmark
    /// (pairwise, binary, sequential, hybrid:<n>).
    #[clap(
        long,
        value_delimiter = ',',
        default_value = "binary,pairwise,sequential,hybrid:16"
    )]
    encodings: Vec<String>,

    /// Per-solve timeout in seconds.
    #[clap(long, default_value = "30")]
    timeout: u64,
}

struct GenParams {
    layers: usize,
    width: usize,
    versions: usize,
    deps: usize,
    spread: f64,
    max_offset: f64,
    seed: u64,
}

/// Generates a layered synthetic problem. Returns the snapshot and the version
/// sets to use as the root requirements (one full-range requirement per
/// package in the first layer).
fn generate(p: &GenParams) -> (DependencySnapshot, Vec<VersionSetId>) {
    let mut snapshot = DependencySnapshot::default();
    let versions = p.versions;
    let half = ((p.spread * versions as f64) as i64).max(1);
    let max_offset = (p.max_offset * versions as f64) as i64;

    let name_id = |l: usize, w: usize| NameId::from_index(l * p.width + w);
    let solvable_id =
        |l: usize, w: usize, v: usize| SolvableId::from_index((l * p.width + w) * versions + v);

    let mut next_version_set = 0usize;
    let mut alloc_version_set = |snapshot: &mut DependencySnapshot, vs: VersionSet| {
        let id = VersionSetId::from_index(next_version_set);
        next_version_set += 1;
        snapshot.version_sets.insert(id, vs);
        id
    };

    for l in 0..p.layers {
        for w in 0..p.width {
            snapshot.packages.insert(
                name_id(l, w),
                Package {
                    name: format!("pkg-{l}-{w}"),
                    solvables: (0..versions).map(|v| solvable_id(l, w, v)).collect(),
                    excluded: Vec::new(),
                },
            );

            // The dependency targets and the shift of their version ranges are
            // a property of the package, not of the version: every version of
            // the package depends on the same packages, but the acceptable
            // version range moves along with the depending version. This
            // mimics packages whose requirements are re-pinned every release.
            let mut edge_rng =
                StdRng::seed_from_u64(p.seed ^ (l as u64) << 32 ^ (w as u64) << 16 ^ 0x5eed_0001);
            let edges: Vec<(usize, i64)> = (0..p.deps)
                .filter(|_| l + 1 < p.layers)
                .map(|_| {
                    (
                        edge_rng.random_range(0..p.width),
                        edge_rng.random_range(0..=max_offset),
                    )
                })
                .collect();

            for v in 0..versions {
                let requirements = edges
                    .iter()
                    .map(|&(target_w, offset)| {
                        let center = v as i64 - offset;
                        let lo = (center - half).clamp(0, versions as i64 - 1) as usize;
                        let hi = (center + half).clamp(0, versions as i64 - 1) as usize;
                        let target = name_id(l + 1, target_w);
                        let vs = alloc_version_set(
                            &mut snapshot,
                            VersionSet {
                                name: target,
                                display: format!("pkg-{}-{target_w} {lo}..={hi}", l + 1),
                                matching_candidates: (lo..=hi)
                                    .map(|tv| solvable_id(l + 1, target_w, tv))
                                    .collect(),
                            },
                        );
                        vs.into()
                    })
                    .collect();

                snapshot.solvables.insert(
                    solvable_id(l, w, v),
                    Solvable {
                        display: format!("pkg-{l}-{w}={v}"),
                        name: name_id(l, w),
                        // Prefer the newest version.
                        order: (versions - 1 - v) as u32,
                        dependencies: Dependencies::Known(KnownDependencies {
                            requirements,
                            constrains: Vec::new(),
                        }),
                        hint_dependencies_available: false,
                    },
                );
            }
        }
    }

    // Full-range root requirements on the first layer.
    let root_version_sets = (0..p.width)
        .map(|w| {
            alloc_version_set(
                &mut snapshot,
                VersionSet {
                    name: name_id(0, w),
                    display: format!("pkg-0-{w} *"),
                    matching_candidates: (0..versions).map(|v| solvable_id(0, w, v)).collect(),
                },
            )
        })
        .collect();

    (snapshot, root_version_sets)
}

#[derive(Clone)]
struct Measurement {
    encoding: String,
    versions: usize,
    seed: u64,
    duration: Duration,
    clauses: usize,
    result: &'static str,
}

fn main() {
    let opts = Opts::parse();
    let encodings: Vec<(String, AmoEncoding)> = opts
        .encodings
        .iter()
        .map(|name| {
            (
                name.clone(),
                name.parse().unwrap_or_else(|e: String| panic!("{e}")),
            )
        })
        .collect();

    let mut measurements: Vec<Measurement> = Vec::new();

    println!("encoding,versions,seed,duration_s,clauses,result");
    for &versions in &opts.versions {
        for seed in 0..opts.seeds {
            let (snapshot, root_version_sets) = generate(&GenParams {
                layers: opts.layers,
                width: opts.width,
                versions,
                deps: opts.deps,
                spread: opts.spread,
                max_offset: opts.max_offset,
                seed,
            });

            for (name, encoding) in &encodings {
                let provider = snapshot
                    .provider()
                    .with_timeout(SystemTime::now() + Duration::from_secs(opts.timeout));
                let mut solver = Solver::new(provider).with_amo_encoding(*encoding);
                let problem = Problem::new()
                    .requirements(root_version_sets.iter().map(|&vs| vs.into()).collect());

                let start = Instant::now();
                let result = solver.solve(problem);
                let duration = start.elapsed();

                let result = match result {
                    Ok(_) => "sat",
                    Err(UnsolvableOrCancelled::Unsolvable(_)) => "unsat",
                    Err(UnsolvableOrCancelled::Cancelled(_)) => "timeout",
                };

                let m = Measurement {
                    encoding: name.clone(),
                    versions,
                    seed,
                    duration,
                    clauses: solver.clause_count(),
                    result,
                };
                println!(
                    "{},{},{},{:.4},{},{}",
                    m.encoding,
                    m.versions,
                    m.seed,
                    m.duration.as_secs_f64(),
                    m.clauses,
                    m.result
                );
                measurements.push(m);
            }
        }
    }

    // Aggregated summary per (versions, encoding).
    eprintln!();
    eprintln!(
        "{:<14} {:>8} {:>12} {:>12} {:>12} {:>10} {:>8}",
        "encoding", "versions", "mean_s", "median_s", "max_s", "clauses", "results"
    );
    for &versions in &opts.versions {
        for (name, _) in &encodings {
            let mut cell: Vec<&Measurement> = measurements
                .iter()
                .filter(|m| m.versions == versions && &m.encoding == name)
                .collect();
            if cell.is_empty() {
                continue;
            }
            cell.sort_by_key(|m| m.duration);
            let mean =
                cell.iter().map(|m| m.duration.as_secs_f64()).sum::<f64>() / cell.len() as f64;
            let median = cell[cell.len() / 2].duration.as_secs_f64();
            let max = cell.last().unwrap().duration.as_secs_f64();
            let clauses = cell.iter().map(|m| m.clauses).sum::<usize>() as f64 / cell.len() as f64;
            let sat = cell.iter().filter(|m| m.result == "sat").count();
            let unsat = cell.iter().filter(|m| m.result == "unsat").count();
            let timeout = cell.iter().filter(|m| m.result == "timeout").count();
            eprintln!(
                "{:<14} {:>8} {:>12.4} {:>12.4} {:>12.4} {:>10.0} {:>8}",
                name,
                versions,
                mean,
                median,
                max,
                clauses,
                format!("{sat}s/{unsat}u/{timeout}t")
            );
        }
        eprintln!();
    }
}
