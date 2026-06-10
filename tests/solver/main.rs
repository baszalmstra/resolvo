mod bundle_box;

use std::io::{Write, stderr};

use bundle_box::{BundleBoxProvider, Pack};
use insta::assert_snapshot;
use itertools::Itertools;
use resolvo::{
    ConditionalRequirement, DependencyProvider, EnvLiteral, EnvLiteralKind, Interner, NameId,
    Problem, SolvableId, Solver, UniversalFailure, UniversalProblem, UnsolvableOrCancelled,
    VersionSetId,
};
use tracing_test::traced_test;

/// Create a string from a [`Transaction`]
fn transaction_to_string(
    interner: &impl Interner<SolvableId = SolvableId>,
    solvables: &[SolvableId],
) -> String {
    use std::fmt::Write;
    let mut buf = String::new();
    for solvable in solvables
        .iter()
        .copied()
        .map(|s| interner.display_solvable(s).to_string())
        .sorted()
    {
        writeln!(buf, "{solvable}").unwrap();
    }

    buf
}

/// Unsat so that we can view the conflict
fn solve_unsat(mut provider: BundleBoxProvider, specs: &[&str]) -> String {
    let requirements = provider.requirements(specs);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    match solver.solve(problem) {
        Ok(_) => panic!("expected unsat, but a solution was found"),
        Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
            // Write the conflict graphviz to stderr
            let graph = conflict.graph(&solver);
            let mut output = stderr();
            writeln!(output, "UNSOLVABLE:").unwrap();
            graph
                .graphviz(&mut output, solver.provider(), true)
                .unwrap();
            writeln!(output, "\n").unwrap();

            // Format a user friendly error message
            conflict.display_user_friendly(&solver).to_string()
        }
        Err(UnsolvableOrCancelled::Cancelled(reason)) => *reason.downcast().unwrap(),
    }
}

/// Solve the problem and returns either a solution represented as a string or
/// an error string.
fn solve_snapshot(mut provider: BundleBoxProvider, specs: &[&str]) -> String {
    // The test dependency provider requires time support for sleeping
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    provider.sleep_before_return = true;

    let requirements = provider.requirements(specs);
    let mut solver = Solver::new(provider).with_runtime(runtime);
    let problem = Problem::new().requirements(requirements);
    match solver.solve(problem) {
        Ok(solvables) => transaction_to_string(solver.provider(), &solvables),
        Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
            // Write the conflict graphviz to stderr
            let graph = conflict.graph(&solver);
            let mut output = stderr();
            writeln!(output, "UNSOLVABLE:").unwrap();
            graph
                .graphviz(&mut output, solver.provider(), true)
                .unwrap();
            writeln!(output, "\n").unwrap();

            // Format a user friendly error message
            conflict.display_user_friendly(&solver).to_string()
        }
        Err(UnsolvableOrCancelled::Cancelled(reason)) => *reason.downcast().unwrap(),
    }
}

/// Test whether we can select a version, this is the most basic operation
#[test]
fn test_unit_propagation_1() {
    let mut provider = BundleBoxProvider::from_packages(&[("asdf", 1, vec![])]);
    let requirements = provider.requirements(&["asdf"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 1);
    let solvable = pool.resolve_solvable(solved[0]);

    assert_eq!(pool.resolve_package_name(solvable.name), "asdf");
    assert_eq!(solvable.record.version, 1);
}

/// Test if we can also select a nested version
#[test]
fn test_unit_propagation_nested() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("asdf", 1u32, vec!["efgh"]),
        ("efgh", 4u32, vec![]),
        ("dummy", 6u32, vec![]),
    ]);
    let requirements = provider.requirements(&["asdf"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 2);

    let solvable = pool.resolve_solvable(solved[0]);

    assert_eq!(pool.resolve_package_name(solvable.name), "asdf");
    assert_eq!(solvable.record.version, 1);

    let solvable = pool.resolve_solvable(solved[1]);

    assert_eq!(pool.resolve_package_name(solvable.name), "efgh");
    assert_eq!(solvable.record.version, 4);
}

/// Test if we can resolve multiple versions at once
#[test]
fn test_resolve_multiple() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("asdf", 1, vec![]),
        ("asdf", 2, vec![]),
        ("efgh", 4, vec![]),
        ("efgh", 5, vec![]),
    ]);
    let requirements = provider.requirements(&["asdf", "efgh"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let mut solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 2);
    solved.sort_by_key(|&s| pool.resolve_package_name(pool.resolve_solvable(s).name));

    let solvable = pool.resolve_solvable(solved[0]);

    assert_eq!(pool.resolve_package_name(solvable.name), "asdf");
    assert_eq!(solvable.record.version, 2);

    let solvable = pool.resolve_solvable(solved[1]);

    assert_eq!(pool.resolve_package_name(solvable.name), "efgh");
    assert_eq!(solvable.record.version, 5);
}

#[test]
fn test_resolve_with_concurrent_metadata_fetching() {
    let provider = BundleBoxProvider::from_packages(&[
        ("parent", 4, vec!["child1", "child2"]),
        ("child1", 3, vec![]),
        ("child2", 2, vec![]),
    ]);

    let max_concurrent_requests = provider.concurrent_requests_max.clone();

    let result = solve_snapshot(provider, &["parent"]);
    insta::assert_snapshot!(result);

    assert_eq!(2, max_concurrent_requests.get());
}

/// In case of a conflict the version should not be selected with the conflict
#[test]
#[traced_test]
fn test_resolve_with_conflict() {
    let provider = BundleBoxProvider::from_packages(&[
        ("asdf", 4, vec!["conflicting 1"]),
        ("asdf", 3, vec!["conflicting 0"]),
        ("efgh", 7, vec!["conflicting 0"]),
        ("efgh", 6, vec!["conflicting 0"]),
        ("conflicting", 1, vec![]),
        ("conflicting", 0, vec![]),
    ]);
    let result = solve_snapshot(provider, &["asdf", "efgh"]);
    insta::assert_snapshot!(result);
}

/// The non-existing package should not be selected
#[test]
#[traced_test]
fn test_resolve_with_nonexisting() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("asdf", 4, vec!["b"]),
        ("asdf", 3, vec![]),
        ("b", 1, vec!["idontexist"]),
    ]);
    let requirements = provider.requirements(&["asdf"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 1);

    let solvable = pool.resolve_solvable(solved[0]);

    assert_eq!(pool.resolve_package_name(solvable.name), "asdf");
    assert_eq!(solvable.record.version, 3);
}

#[test]
#[traced_test]
fn test_resolve_with_nested_deps() {
    let mut provider = BundleBoxProvider::from_packages(&[
        (
            "apache-airflow",
            3,
            vec!["opentelemetry-api 2..4", "opentelemetry-exporter-otlp"],
        ),
        (
            "apache-airflow",
            2,
            vec!["opentelemetry-api 2..4", "opentelemetry-exporter-otlp"],
        ),
        ("apache-airflow", 1, vec![]),
        ("opentelemetry-api", 3, vec!["opentelemetry-sdk"]),
        ("opentelemetry-api", 2, vec![]),
        ("opentelemetry-api", 1, vec![]),
        ("opentelemetry-exporter-otlp", 1, vec!["opentelemetry-grpc"]),
        ("opentelemetry-grpc", 1, vec!["opentelemetry-api 1"]),
    ]);
    let requirements = provider.requirements(&["apache-airflow"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 1);

    let solvable = pool.resolve_solvable(solved[0]);

    assert_eq!(pool.resolve_package_name(solvable.name), "apache-airflow");
    assert_eq!(solvable.record.version, 1);
}

#[test]
#[traced_test]
fn test_resolve_with_unknown_deps() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package(
        "opentelemetry-api",
        Pack::new(3).with_unknown_deps(),
        &[],
        &[],
    );
    provider.add_package("opentelemetry-api", Pack::new(2), &[], &[]);
    let requirements = provider.requirements(&["opentelemetry-api"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 1);

    let solvable = pool.resolve_solvable(solved[0]);

    assert_eq!(
        pool.resolve_package_name(solvable.name),
        "opentelemetry-api"
    );
    assert_eq!(solvable.record.version, 2);
}

#[test]
#[traced_test]
fn test_resolve_and_cancel() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package(
        "opentelemetry-api",
        Pack::new(3).with_unknown_deps(),
        &[],
        &[],
    );
    provider.add_package(
        "opentelemetry-api",
        Pack::new(2).cancel_during_get_dependencies(),
        &[],
        &[],
    );
    let error = solve_unsat(provider, &["opentelemetry-api"]);
    insta::assert_snapshot!(error);
}

/// Locking a specific package version in this case a lower version namely `3`
/// should result in the higher package not being considered
#[test]
fn test_resolve_locked_top_level() {
    let mut provider =
        BundleBoxProvider::from_packages(&[("asdf", 4, vec![]), ("asdf", 3, vec![])]);
    provider.set_locked("asdf", 3);

    let requirements = provider.requirements(&["asdf"]);

    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 1);
    let solvable_id = solved[0];
    assert_eq!(pool.resolve_solvable(solvable_id).record.version, 3);
}

/// Should ignore lock when it is not a top level package and a newer version
/// exists without it
#[test]
fn test_resolve_ignored_locked_top_level() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("asdf", 4, vec![]),
        ("asdf", 3, vec!["fgh"]),
        ("fgh", 1, vec![]),
    ]);

    provider.set_locked("fgh", 1);

    let requirements = provider.requirements(&["asdf"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let pool = &solver.provider().pool;

    assert_eq!(solved.len(), 1);
    let solvable = pool.resolve_solvable(solved[0]);

    assert_eq!(pool.resolve_package_name(solvable.name), "asdf");
    assert_eq!(solvable.record.version, 4);
}

/// Test checks if favoring without a conflict results in a package upgrade
#[test]
fn test_resolve_favor_without_conflict() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec![]),
        ("a", 2, vec![]),
        ("b", 1, vec![]),
        ("b", 2, vec![]),
    ]);
    provider.set_favored("a", 1);
    provider.set_favored("b", 1);

    // Already installed: A=1; B=1
    let result = solve_snapshot(provider, &["a", "b 2"]);
    insta::assert_snapshot!(result, @r###"
        a=1
        b=2
        "###);
}

#[test]
fn test_resolve_favor_with_conflict() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec!["c 1"]),
        ("a", 2, vec![]),
        ("b", 1, vec!["c 1"]),
        ("b", 2, vec!["c 2"]),
        ("c", 1, vec![]),
        ("c", 2, vec![]),
    ]);
    provider.set_favored("a", 1);
    provider.set_favored("b", 1);
    provider.set_favored("c", 1);

    let result = solve_snapshot(provider, &["a", "b 2"]);
    insta::assert_snapshot!(result, @r###"
        a=2
        b=2
        c=2
        "###);
}

#[test]
fn test_resolve_cyclic() {
    let mut provider =
        BundleBoxProvider::from_packages(&[("a", 2, vec!["b 0..10"]), ("b", 5, vec!["a 2..4"])]);
    let requirements = provider.requirements(&["a 0..100"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();

    let result = transaction_to_string(solver.provider(), &solved);
    insta::assert_snapshot!(result, @r###"
        a=2
        b=5
        "###);
}

#[test]
fn test_resolve_union_requirements() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec![]),
        ("b", 1, vec![]),
        ("c", 1, vec!["a"]),
        ("d", 1, vec!["b"]),
        ("e", 1, vec!["a | b"]),
    ]);

    // Make d conflict with a=1
    provider.add_package("f", 1.into(), &["b"], &["a 2"]);

    let result = solve_snapshot(provider, &["c | d", "e", "f"]);
    assert_snapshot!(result, @r###"
        b=1
        d=1
        e=1
        f=1
        "###);
}

#[test]
fn test_unsat_locked_and_excluded() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("asdf", 1, vec!["c 2"]),
        ("c", 2, vec![]),
        ("c", 1, vec![]),
    ]);
    provider.set_locked("c", 1);
    insta::assert_snapshot!(solve_snapshot(provider, &["asdf"]));
}

#[test]
#[tracing_test::traced_test]
fn test_unsat_no_candidates_for_child_1() {
    let provider = BundleBoxProvider::from_packages(&[("asdf", 1, vec!["c 2"]), ("c", 1, vec![])]);
    let error = solve_unsat(provider, &["asdf"]);
    insta::assert_snapshot!(error);
}

//
#[test]
fn test_unsat_no_candidates_for_child_2() {
    let provider = BundleBoxProvider::from_packages(&[("a", 41, vec!["B 0..20"])]);
    let error = solve_unsat(provider, &["a 0..1000"]);
    insta::assert_snapshot!(error);
}

// Versions requiring different versions of a missing dependency are each
// reported (conda/rattler#2476).
#[test]
fn test_unsat_no_candidates_distinct_requirements() {
    let provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec!["b 41..42"]),
        ("a", 2, vec!["b 41..42"]),
        ("a", 3, vec!["b 42..43"]),
        ("a", 4, vec!["b 43..44"]),
    ]);
    let error = solve_unsat(provider, &["a"]);
    insta::assert_snapshot!(error);
}

#[test]
fn test_unsat_missing_top_level_dep_1() {
    let provider = BundleBoxProvider::from_packages(&[("asdf", 1, vec![])]);
    let error = solve_unsat(provider, &["fghj"]);
    insta::assert_snapshot!(error);
}

#[test]
fn test_unsat_missing_top_level_dep_2() {
    let provider = BundleBoxProvider::from_packages(&[("a", 41, vec!["b 15"]), ("b", 15, vec![])]);
    let error = solve_unsat(provider, &["a 41", "b 14"]);
    insta::assert_snapshot!(error);
}

#[test]
#[tracing_test::traced_test]
fn test_unsat_after_backtracking() {
    let provider = BundleBoxProvider::from_packages(&[
        ("b", 7, vec!["d 1"]),
        ("b", 6, vec!["d 1"]),
        ("c", 1, vec!["d 2"]),
        ("c", 2, vec!["d 2"]),
        ("d", 2, vec![]),
        ("d", 1, vec![]),
        ("e", 1, vec![]),
        ("e", 2, vec![]),
    ]);

    let error = solve_unsat(provider, &["b", "c", "e"]);
    insta::assert_snapshot!(error);
}

#[test]
fn test_unsat_incompatible_root_requirements() {
    let provider = BundleBoxProvider::from_packages(&[("a", 2, vec![]), ("a", 5, vec![])]);
    let error = solve_unsat(provider, &["a 0..4", "a 5..10"]);
    insta::assert_snapshot!(error);
}

#[test]
fn test_unsat_bluesky_conflict() {
    let provider = BundleBoxProvider::from_packages(&[
        ("suitcase-utils", 54, vec![]),
        ("suitcase-utils", 53, vec![]),
        (
            "bluesky-widgets",
            42,
            vec![
                "bluesky-live 0..10",
                "numpy 0..10",
                "python 0..10",
                "suitcase-utils 0..54",
            ],
        ),
        ("bluesky-live", 1, vec![]),
        ("numpy", 1, vec![]),
        ("python", 1, vec![]),
    ]);
    let error = solve_unsat(
        provider,
        &["bluesky-widgets 0..100", "suitcase-utils 54..100"],
    );
    insta::assert_snapshot!(error);
}

#[test]
fn test_unsat_pubgrub_article() {
    // Taken from the pubgrub article: https://nex3.medium.com/pubgrub-2fb6470504f
    let provider = BundleBoxProvider::from_packages(&[
        ("menu", 15, vec!["dropdown 2..3"]),
        ("menu", 10, vec!["dropdown 1..2"]),
        ("dropdown", 2, vec!["icons 2"]),
        ("dropdown", 1, vec!["intl 3"]),
        ("icons", 2, vec![]),
        ("icons", 1, vec![]),
        ("intl", 5, vec![]),
        ("intl", 3, vec![]),
    ]);
    let error = solve_unsat(provider, &["menu", "icons 1", "intl 5"]);
    insta::assert_snapshot!(error);
}

#[test]
fn test_unsat_applies_graph_compression() {
    let provider = BundleBoxProvider::from_packages(&[
        ("a", 10, vec!["b"]),
        ("a", 9, vec!["b"]),
        ("b", 100, vec!["c 0..100"]),
        ("b", 42, vec!["c 0..100"]),
        ("c", 103, vec![]),
        ("c", 101, vec![]),
        ("c", 100, vec![]),
        ("c", 99, vec![]),
    ]);
    let error = solve_unsat(provider, &["a", "c 101..104"]);
    insta::assert_snapshot!(error);
}

#[test]
fn test_unsat_constrains() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 10, vec!["b 50..100"]),
        ("a", 9, vec!["b 50..100"]),
        ("b", 50, vec![]),
        ("b", 42, vec![]),
    ]);

    provider.add_package("c", 10.into(), &[], &["b 0..50"]);
    provider.add_package("c", 8.into(), &[], &["b 0..50"]);
    let error = solve_unsat(provider, &["a", "c"]);
    insta::assert_snapshot!(error);
}

#[test]
#[tracing_test::traced_test]
fn test_unsat_constrains_2() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec!["b"]),
        ("a", 2, vec!["b"]),
        ("b", 1, vec!["c 1"]),
        ("b", 2, vec!["c 2"]),
    ]);

    provider.add_package("c", 1.into(), &[], &["a 3"]);
    provider.add_package("c", 2.into(), &[], &["a 3"]);
    let error = solve_unsat(provider, &["a"]);
    insta::assert_snapshot!(error);
}

#[test]
fn test_missing_dep() {
    let provider = BundleBoxProvider::from_packages(&[("a", 2, vec!["missing"]), ("a", 1, vec![])]);
    insta::assert_snapshot!(solve_snapshot(provider, &["a"]));
}

#[test]
#[tracing_test::traced_test]
fn test_no_backtracking() {
    let provider = BundleBoxProvider::from_packages(&[
        ("quetz-server", 2, vec!["pydantic 10..20"]),
        ("quetz-server", 1, vec!["pydantic 0..10"]),
        ("pydantic", 1, vec![]),
        ("pydantic", 2, vec![]),
        ("pydantic", 3, vec![]),
        ("pydantic", 4, vec![]),
        ("pydantic", 5, vec![]),
        ("pydantic", 6, vec![]),
        ("pydantic", 7, vec![]),
        ("pydantic", 8, vec![]),
        ("pydantic", 9, vec![]),
        ("pydantic", 10, vec![]),
        ("pydantic", 11, vec![]),
        ("pydantic", 12, vec![]),
        ("pydantic", 13, vec![]),
        ("pydantic", 14, vec![]),
    ]);
    insta::assert_snapshot!(solve_snapshot(
        provider,
        &["quetz-server", "pydantic 0..10"]
    ));
}

#[test]
#[tracing_test::traced_test]
fn test_incremental_crash() {
    let provider = BundleBoxProvider::from_packages(&[
        ("a", 3, vec!["missing"]),
        ("a", 2, vec!["missing"]),
        ("a", 1, vec!["b"]),
        ("b", 2, vec!["a 2..4"]),
        ("b", 1, vec![]),
    ]);
    insta::assert_snapshot!(solve_snapshot(provider, &["a"]));
}

#[test]
#[traced_test]
fn test_excluded() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 2, vec!["b"]),
        ("a", 1, vec!["c"]),
        ("b", 1, vec![]),
        ("c", 1, vec![]),
    ]);
    provider.exclude("b", 1, "it is externally excluded");
    provider.exclude("c", 1, "it is externally excluded");
    insta::assert_snapshot!(solve_snapshot(provider, &["a"]));
}

#[test]
fn test_merge_excluded() {
    let mut provider = BundleBoxProvider::from_packages(&[("a", 1, vec![]), ("a", 2, vec![])]);
    provider.exclude("a", 1, "it is externally excluded");
    provider.exclude("a", 2, "it is externally excluded");
    insta::assert_snapshot!(solve_snapshot(provider, &["a"]));
}

#[test]
#[traced_test]
fn test_merge_installable() {
    let provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec![]),
        ("a", 2, vec![]),
        ("a", 3, vec![]),
        ("a", 4, vec![]),
    ]);
    insta::assert_snapshot!(solve_snapshot(provider, &["a 0..3", "a 3..5"]));
}

#[test]
fn test_root_excluded() {
    let mut provider = BundleBoxProvider::from_packages(&[("a", 1, vec![])]);
    provider.exclude("a", 1, "it is externally excluded");
    insta::assert_snapshot!(solve_snapshot(provider, &["a"]));
}

#[test]
fn test_constraints() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec!["b 0..10"]),
        ("b", 1, vec![]),
        ("b", 2, vec![]),
        ("c", 1, vec![]),
    ]);
    let requirements = provider.requirements(&["a 0..10"]);
    let constraints = provider.version_sets(&["b 1..2", "c"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new()
        .requirements(requirements)
        .constraints(constraints);
    let solved = solver.solve(problem).unwrap();

    let result = transaction_to_string(solver.provider(), &solved);
    insta::assert_snapshot!(result, @r###"
        a=1
        b=1
        "###);
}

#[test]
fn test_solve_with_additional() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec!["b 0..10"]),
        ("b", 1, vec![]),
        ("b", 2, vec![]),
        ("c", 1, vec![]),
        ("d", 1, vec![]),
        ("e", 1, vec!["d"]),
        ("locked", 1, vec![]),
        ("locked", 2, vec![]),
    ]);

    provider.set_locked("locked", 2);

    let requirements = provider.requirements(&["a 0..10"]);
    let constraints = provider.version_sets(&["b 1..2", "c"]);

    let extra_solvables = [
        provider.solvable_id("b", 2),
        provider.solvable_id("c", 1),
        provider.solvable_id("e", 1),
        // Does not obey the locked clause since it has not been requested
        // in a version set by another solvable
        provider.solvable_id("locked", 1),
        provider.solvable_id("unknown-deps", Pack::new(1).with_unknown_deps()),
    ];

    let mut solver = Solver::new(provider);

    let problem = Problem::new()
        .requirements(requirements)
        .constraints(constraints)
        .soft_requirements(extra_solvables);
    let solved = solver.solve(problem).unwrap();

    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
        a=1
        b=1
        c=1
        d=1
        e=1
        locked=1
        "###);
}

/// Test that a soft requirement which conflicts with the hard solution is
/// handled correctly when forbid clauses are created lazily.
///
/// Only a=1 matches the hard requirement. The soft requirement a=2 has a
/// circular dependency on "a", so encoding it registers both a=1 and a=2 as
/// forbid targets after both are already decided true. The solver should
/// detect the conflict and exclude a=2 from the solution.
#[test]
fn test_solve_with_soft_requirement_forbid_clause_conflict() {
    let mut provider = BundleBoxProvider::from_packages(&[("a", 1, vec![]), ("a", 2, vec!["a"])]);

    // Only a=1 matches the hard requirement, so a=2 is never a matching
    // candidate during the hard solve and no forbid clause is created.
    let requirements = provider.requirements(&["a 1..2"]);
    let extra_solvables = [provider.solvable_id("a", 2)];

    let mut solver = Solver::new(provider);
    let problem = Problem::new()
        .requirements(requirements)
        .soft_requirements(extra_solvables);
    let solved = solver.solve(problem).unwrap();

    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
    a=1
    "###);
}

#[test]
fn test_solve_with_additional_with_constrains() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec!["b 0..10"]),
        ("b", 1, vec![]),
        ("b", 2, vec![]),
        ("b", 3, vec![]),
        ("c", 1, vec![]),
        ("d", 1, vec!["f"]),
        ("e", 1, vec!["c"]),
    ]);

    provider.add_package("f", 1.into(), &[], &["c 2..3"]);
    provider.add_package("g", 1.into(), &[], &["b 2..3"]);
    provider.add_package("h", 1.into(), &[], &["b 1..2"]);
    provider.add_package("i", 1.into(), &[], &[]);
    provider.add_package("j", 1.into(), &["i"], &[]);
    provider.add_package("k", 1.into(), &["i"], &[]);
    provider.add_package("l", 1.into(), &["j", "k"], &[]);

    let requirements = provider.requirements(&["a 0..10", "e"]);
    let constraints = provider.version_sets(&["b 1..2", "c", "k 2..3"]);

    let extra_solvables = [
        provider.solvable_id("d", 1),
        provider.solvable_id("g", 1),
        provider.solvable_id("h", 1),
        provider.solvable_id("j", 1),
        provider.solvable_id("l", 1),
        provider.solvable_id("k", 1),
    ];

    let mut solver = Solver::new(provider);

    let problem = Problem::new()
        .requirements(requirements)
        .constraints(constraints)
        .soft_requirements(extra_solvables);

    let solved = solver.solve(problem).unwrap();

    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
        a=1
        b=1
        c=1
        e=1
        h=1
        i=1
        j=1
        "###);
}

#[test]
fn test_snapshot() {
    let provider = BundleBoxProvider::from_packages(&[
        ("menu", 15, vec!["dropdown 2..3"]),
        ("menu", 10, vec!["dropdown 1..2"]),
        ("dropdown", 2, vec!["icons 2"]),
        ("dropdown", 1, vec!["intl 3; if menu"]),
        ("icons", 2, vec![]),
        ("icons", 1, vec![]),
        ("intl", 5, vec![]),
        ("intl", 3, vec![]),
    ]);

    let menu_name_id = provider.package_name("menu");

    let snapshot = provider.into_snapshot();

    #[cfg(feature = "serde")]
    serialize_snapshot(&snapshot, "snapshot_pubgrub_menu.json");

    let mut snapshot_provider = snapshot.provider();
    let menu_req = snapshot_provider
        .add_package_requirement(menu_name_id, "*")
        .into();

    assert_snapshot!(solve_for_snapshot(snapshot_provider, &[menu_req], &[]));
}

#[test]
fn test_snapshot_union_requirements() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("icons", 2, vec![]),
        ("icons", 1, vec![]),
        ("intl", 5, vec![]),
        ("intl", 3, vec![]),
        ("union", 1, vec!["icons 2 | intl"]),
    ]);

    let requirements = provider.requirements(&["intl", "union"]);

    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]));
}

#[test]
fn test_union_empty_requirements() {
    let provider = BundleBoxProvider::from_packages(&[("a", 1, vec!["b 1 | c"]), ("b", 1, vec![])]);

    let result = solve_snapshot(provider, &["a"]);
    assert_snapshot!(result, @r"
    a=1
    b=1
    ");
}

#[test]
fn test_root_constraints() {
    let mut provider =
        BundleBoxProvider::from_packages(&[("icons", 1, vec![]), ("union", 1, vec!["icons"])]);

    let requirements = provider.requirements(&["union"]);
    let constraints = provider.version_sets(&["union 5"]);

    assert_snapshot!(solve_for_snapshot(provider, &requirements, &constraints));
}

#[test]
fn test_explicit_root_requirements() {
    let mut provider = BundleBoxProvider::from_packages(&[
        // `a` depends transitively on `b`
        ("a", 1, vec!["b"]),
        // `b` depends on `c`, but the highest version of `b` constrains `c` to `<2`.
        ("b", 1, vec!["c"]),
        ("b", 2, vec!["c 1..2"]),
        // `c` has more versions than `b`, so the heuristic will most likely pick `b` first.
        ("c", 1, vec![]),
        ("c", 2, vec![]),
        ("c", 3, vec![]),
        ("c", 4, vec![]),
        ("c", 5, vec![]),
    ]);

    // We require both `a` and `c` explicitly. The expected outcome will be that we
    // get the highest versions of `a` and `c` and a lower version of `b`.
    let requirements = provider.requirements(&["a", "c"]);

    let mut solver = Solver::new(provider);
    let problem = Problem::default().requirements(requirements);
    let solved = solver.solve(problem).unwrap();

    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
    a=1
    b=1
    c=5
    "###);
}

#[test]
fn test_conditional_requirements() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["baz; if bar"]),
        ("bar", 1, vec![]),
        ("baz", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=1
    baz=1
    foo=1
    "###);
}

#[test]
fn test_conditional_unsolvable() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["baz 2; if bar"]),
        ("bar", 1, vec![]),
        ("baz", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    foo * cannot be installed because there are no viable options:
    └─ foo 1 would require
       └─ baz >=2, <3, for which no candidates were found.
    The following packages are incompatible
    └─ bar * can be installed with any of the following options:
       └─ bar 1
    "###);
}

#[test]
fn test_conditional_unsolvable_without_condition() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec![]),
        ("foo", 2, vec!["baz 2; if bar"]), /* This will not be selected because baz 2 conflicts
                                            * with the requirement. */
        ("bar", 1, vec![]),
        ("baz", 1, vec![]),
        ("baz", 2, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar", "baz 1"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=1
    baz=1
    foo=1
    "###);
}

#[test]
fn test_conditional_requirements_version_set() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["baz; if bar 1"]),
        ("bar", 1, vec![]),
        ("bar", 2, vec![]),
        ("baz", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=2
    foo=1
    "###);
}

#[test]
fn test_conditional_and() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["icon; if bar and baz"]),
        ("bar", 1, vec![]),
        ("bar", 2, vec![]),
        ("baz", 1, vec![]),
        ("icon", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar", "baz"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=2
    baz=1
    foo=1
    icon=1
    "###);
}

#[test]
fn test_conditional_and_mismatch() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["icon; if bar and baz"]),
        ("bar", 1, vec![]),
        ("baz", 1, vec![]),
        ("icon", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=1
    foo=1
    "###);
}

#[test]
fn test_conditional_or() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["icon; if bar or baz"]),
        ("bar", 1, vec![]),
        ("baz", 1, vec![]),
        ("icon", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=1
    foo=1
    icon=1
    "###);
}

#[test]
fn test_conditional_complex() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["icon; if bar and baz or menu"]),
        ("bar", 1, vec![]),
        ("baz", 1, vec![]),
        ("icon", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar", "baz"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=1
    baz=1
    foo=1
    icon=1
    "###);
}

#[test]
#[traced_test]
fn test_condition_missing_requirement() {
    let mut provider =
        BundleBoxProvider::from_packages(&[("menu", 1, vec!["bla; if intl"]), ("intl", 1, vec![])]);

    let requirements = provider.requirements(&["menu"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @"menu=1");
}

#[cfg(feature = "serde")]
fn serialize_snapshot(
    snapshot: &resolvo::snapshot::DependencySnapshot,
    destination: impl AsRef<std::path::Path>,
) {
    let file = std::io::BufWriter::new(std::fs::File::create(destination.as_ref()).unwrap());
    serde_json::to_writer_pretty(file, snapshot).unwrap()
}

fn solve_for_snapshot<D: DependencyProvider<NameId = resolvo::NameId, SolvableId = SolvableId>>(
    provider: D,
    root_reqs: &[ConditionalRequirement],
    root_constraints: &[VersionSetId],
) -> String {
    let mut solver = Solver::new(provider);
    let problem = Problem::new()
        .requirements(root_reqs.to_vec())
        .constraints(root_constraints.to_vec());
    match solver.solve(problem) {
        Ok(solvables) => transaction_to_string(solver.provider(), &solvables),
        Err(UnsolvableOrCancelled::Unsolvable(conflict)) => {
            // Write the conflict graphviz to stderr
            let graph = conflict.graph(&solver);
            let mut output = stderr();
            writeln!(output, "UNSOLVABLE:").unwrap();
            graph
                .graphviz(&mut output, solver.provider(), true)
                .unwrap();
            writeln!(output, "\n").unwrap();

            // Format a user friendly error message
            conflict.display_user_friendly(&solver).to_string()
        }
        Err(UnsolvableOrCancelled::Cancelled(reason)) => *reason.downcast().unwrap(),
    }
}

// ============================================================================
// Constraint edge case tests
// ============================================================================
// These tests comprehensively cover the behavior of Constrains clauses
// (i.e. "if A is installed, B must NOT be installed") in both directions
// and various edge cases involving backtracking.

/// Forward direction: parent is installed, its constraint forbids certain versions
/// of another package. The solver should pick a compatible version.
/// Constraint spec "b 3..100" means b must be in [3,100). Versions outside that range are forbidden.
#[test]
fn test_constrains_forward_basic() {
    let mut provider = BundleBoxProvider::new();
    // a requires b and constrains b to >= 3 (forbids b=1, b=2)
    provider.add_package("a", 1.into(), &["b"], &["b 3..100"]);
    provider.add_package("b", 1.into(), &[], &[]);
    provider.add_package("b", 2.into(), &[], &[]);
    provider.add_package("b", 3.into(), &[], &[]);
    provider.add_package("b", 4.into(), &[], &[]);

    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // b=4 is the highest version in the allowed range [3,100)
    assert_snapshot!(result, @r###"
    a=1
    b=4
    "###);
}

/// Forward direction: parent constrains away ALL versions of a dependency
/// by specifying an empty allowed range.
#[test]
fn test_constrains_forward_all_forbidden() {
    let mut provider = BundleBoxProvider::new();
    // a requires b but its constraint of [100,200) excludes every available version
    provider.add_package("a", 1.into(), &["b"], &["b 100..200"]);
    provider.add_package("b", 1.into(), &[], &[]);
    provider.add_package("b", 2.into(), &[], &[]);

    let error = solve_unsat(provider, &["a"]);
    assert_snapshot!(error);
}

/// Reverse direction: b=2 is forced by `x`, then `a=3` which constrains b
/// to >= 3 can't be used (b=2 is outside [3,100)), so solver falls back to `a=2`.
#[test]
fn test_constrains_reverse_direction() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package("x", 1.into(), &["b 2..3"], &[]);
    provider.add_package("b", 1.into(), &[], &[]);
    provider.add_package("b", 2.into(), &[], &[]);
    provider.add_package("b", 3.into(), &[], &[]);
    provider.add_package("a", 2.into(), &[], &[]);
    // a=3 constrains b to [3,100), so b=1 and b=2 are forbidden
    provider.add_package("a", 3.into(), &[], &["b 3..100"]);

    let requirements = provider.requirements(&["x", "a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // b=2 forced by x, a=3 incompatible, falls back to a=2
    assert_snapshot!(result, @r###"
    a=2
    b=2
    x=1
    "###);
}

/// Reverse direction unsolvable: forced version conflicts with only parent version.
#[test]
fn test_constrains_reverse_unsat() {
    let mut provider = BundleBoxProvider::new();
    // x forces b=1
    provider.add_package("x", 1.into(), &["b 1..2"], &[]);
    provider.add_package("b", 1.into(), &[], &[]);
    provider.add_package("b", 2.into(), &[], &[]);
    // a constrains b to [2,100), so b=1 is forbidden
    provider.add_package("a", 1.into(), &[], &["b 2..100"]);

    let error = solve_unsat(provider, &["x", "a"]);
    assert_snapshot!(error);
}

/// Constraint with no version of constrained package selected.
/// The constraint should have no effect since the package isn't needed.
#[test]
fn test_constrains_no_version_selected() {
    let mut provider = BundleBoxProvider::new();
    // a constrains b to [100,200), forbidding all existing b versions
    // but nothing requires b, so constraint is irrelevant
    provider.add_package("a", 1.into(), &[], &["b 100..200"]);
    provider.add_package("b", 1.into(), &[], &[]);
    provider.add_package("b", 2.into(), &[], &[]);

    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
    a=1
    "###);
}

/// Matching version selected: the constraint allows the selected version.
#[test]
fn test_constrains_matching_version_ok() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package("x", 1.into(), &["b"], &[]);
    provider.add_package("b", 1.into(), &[], &[]);
    provider.add_package("b", 2.into(), &[], &[]);
    provider.add_package("b", 3.into(), &[], &[]);
    // a constrains b to [0,2), allowing b=1 and forbidding b=2 and b=3
    provider.add_package("a", 1.into(), &[], &["b 0..2"]);

    let requirements = provider.requirements(&["x", "a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // b=1 is the only version in the allowed range
    assert_snapshot!(result, @r###"
    a=1
    b=1
    x=1
    "###);
}

/// Backtracking through constraints: solver tries b=3 first, backtracks to b=1.
#[test]
fn test_constrains_backtracking() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package("a", 1.into(), &["b"], &[]);
    provider.add_package("b", 1.into(), &[], &[]);
    provider.add_package("b", 2.into(), &[], &[]);
    provider.add_package("b", 3.into(), &[], &[]);
    // c constrains b to [0,2), allowing b=1 and forbidding b=2 and b=3
    provider.add_package("c", 1.into(), &[], &["b 0..2"]);

    let requirements = provider.requirements(&["a", "c"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
    a=1
    b=1
    c=1
    "###);
}

/// Diamond dependency with constraints on both sides.
#[test]
fn test_constrains_diamond() {
    let mut provider = BundleBoxProvider::new();
    // a requires c and constrains c to [3,100), forbidding c=1 and c=2
    provider.add_package("a", 1.into(), &["c"], &["c 3..100"]);
    // b requires c and constrains c to [0,5), forbidding c=5
    provider.add_package("b", 1.into(), &["c"], &["c 0..5"]);
    provider.add_package("c", 1.into(), &[], &[]);
    provider.add_package("c", 2.into(), &[], &[]);
    provider.add_package("c", 3.into(), &[], &[]);
    provider.add_package("c", 4.into(), &[], &[]);
    provider.add_package("c", 5.into(), &[], &[]);

    let requirements = provider.requirements(&["a", "b"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // c must be in [3,5) -> c=3 or c=4, highest wins
    assert_snapshot!(result, @r###"
    a=1
    b=1
    c=4
    "###);
}

/// Stress test: many versions with constraints.
#[test]
fn test_constrains_many_versions() {
    let mut provider = BundleBoxProvider::new();
    for v in 1..=50u32 {
        provider.add_package("big", v.into(), &[], &[]);
    }
    // a requires big and constrains it to [46,100), allowing 46-50 and forbidding 1-45
    provider.add_package("a", 1.into(), &["big"], &["big 46..100"]);

    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
    a=1
    big=50
    "###);
}

/// Transitive constraint: a constrains b, which transitively affects c.
#[test]
fn test_constrains_transitive() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package("b", 1.into(), &["c 1..2"], &[]);
    provider.add_package("b", 2.into(), &["c 2..3"], &[]);
    provider.add_package("c", 1.into(), &[], &[]);
    provider.add_package("c", 2.into(), &[], &[]);
    provider.add_package("x", 1.into(), &["b"], &[]);
    // a constrains b to [2,100), so b=1 is forbidden
    provider.add_package("a", 1.into(), &[], &["b 2..100"]);

    let requirements = provider.requirements(&["x", "a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // a forbids b=1, so b=2 is selected, which requires c=2
    assert_snapshot!(result, @r###"
    a=1
    b=2
    c=2
    x=1
    "###);
}

/// A solvable that violates a constraint it itself declares cannot be
/// installed: `a=2` constrains `a 3..4` and is itself a non-matching
/// candidate, so the degenerate Constrains clause `(not a=2 or not a=2)` is
/// the assertion `not a=2` and the solver falls back to `a=1`. Found by the
/// universal-solve property test; the bug predates universal solving (the
/// encoder created a clause watching the same literal twice).
#[test]
fn test_self_constrains_falls_back_to_other_version() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package("a", 2.into(), &[], &["a 3..4"]);
    provider.add_package("a", 1.into(), &[], &[]);
    insta::assert_snapshot!(solve_snapshot(provider, &["a"]), @r"
    a=1
    ");
}

/// The unsolvable variant of the self-constrains case: the only candidate
/// violates its own constraint, so the requirement cannot be satisfied.
#[test]
fn test_self_constrains_unsolvable() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package("a", 1.into(), &[], &["a 2..3"]);
    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let result = solver.solve(problem);
    assert!(matches!(result, Err(UnsolvableOrCancelled::Unsolvable(_))));
}

// ===========================================================================
// M1: Environment literals in the encoder -- scenario tests
// ===========================================================================
//
// These tests exercise the three env-package encoding paths through a plain
// `solve()` call which gives the "baseline" solution: all environment literals
// default to false / undecided. None of these tests need `solve_universal`.

/// Conditional dependency on an environment package stays inactive in the
/// baseline solve (environment literal is undecided = false, so the condition
/// disjunction is not satisfied and the requirement is inactive).
#[test]
fn test_env_conditional_dep_inactive_in_baseline() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    // `a 1` has a conditional dep: requires `b 1..2` if `cuda 11..100`
    provider.add_package("a", Pack::new(1), &["b 1..2; if cuda 11..100"], &[]);
    provider.add_package("b", Pack::new(1), &[], &[]);

    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // Baseline: cuda is undecided (false), so b is NOT pulled in.
    assert_snapshot!(result, @r"
    a=1
    ");
}

/// A constraint on an env package that can be absent: in the baseline solve
/// the absent literal is false/undecided so the EnvConstrains clause is
/// satisfied by the absent literal (absent is undecided, counts as false, and
/// propagation does nothing), and the solve completes without a conflict.
#[test]
fn test_env_constrains_absent_branch_baseline() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    // `a 1` constrains `cuda 11..100` (meaning: if a is installed the env
    // must lack cuda or have cuda >=11).
    provider.add_package("a", Pack::new(1), &[], &["cuda 11..100"]);

    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // Baseline: cuda absent/undecided; solve succeeds, only a is installed.
    assert_snapshot!(result, @r"
    a=1
    ");
}

/// A requirement on an environment package forces that env literal to be true
/// and the solve still succeeds (no concrete solvable is produced, but the
/// solve does not fail).
#[test]
fn test_env_requirement_forces_literal_true() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", false);
    // Root requires `cuda 11..100`. This should force L_{cuda>=11} = true.
    let requirements = provider.requirements(&["cuda 11..100"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    // No concrete solvables -- the env literal variable is true but not a
    // solvable. The solution set of concrete solvables is empty.
    assert_eq!(solved, vec![]);
}

/// A conditional dependency on an environment package becomes ACTIVE when
/// the env literal is forced true by a requirement on the same version set:
/// root requires `cuda 11..100`, so `L_{cuda 11..100}` is propagated true,
/// which activates `a`'s conditional dependency on `b`.
#[test]
fn test_env_conditional_dep_active_when_literal_forced() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("a", Pack::new(1), &["b 1..2; if cuda 11..100"], &[]);
    provider.add_package("b", Pack::new(1), &[], &[]);

    let requirements = provider.requirements(&["a", "cuda 11..100"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r"
    a=1
    b=1
    ");
}

/// Oracle Subset implication: root requires `cuda 12..100` (forcing
/// `L_{12..100}` true) and `a` constrains `cuda 11..100`. Since every value
/// in [12,100) is also in [11,100), the oracle emits
/// `(not L_{12..100} or L_{11..100})` which forces `L_{11..100}` true and
/// satisfies the constraint. With `can_be_absent = false` the EnvConstrains
/// clause is the binary `(not a or L_{11..100})`, so a wrong-polarity or
/// wrong-direction oracle clause would make this unsolvable.
#[test]
fn test_env_oracle_subset_satisfies_constraint() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", false);
    provider.add_package("a", Pack::new(1), &[], &["cuda 11..100"]);

    let requirements = provider.requirements(&["a", "cuda 12..100"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r"
    a=1
    ");
}

/// Oracle Disjoint exclusion: two root requirements on disjoint version sets
/// of the same environment package force both literals true, which violates
/// the oracle clause `(not L_{0..5} or not L_{11..100})`. The solve must
/// fail. Without the disjoint oracle clause both literals could be true
/// simultaneously and the solve would (incorrectly) succeed.
#[test]
fn test_env_oracle_disjoint_conflict() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", false);

    let requirements = provider.requirements(&["cuda 0..5", "cuda 11..100"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let result = solver.solve(problem);
    // Conflict rendering for env literals is deferred to M5; only assert
    // that the solve fails as unsolvable.
    assert!(matches!(result, Err(UnsolvableOrCancelled::Unsolvable(_))));
}

/// `Requirement::Union` mixing an environment and a concrete version set is
/// rejected in v1 with a clear panic.
#[test]
#[should_panic(expected = "mixing environment and concrete version sets")]
fn test_env_union_mixing_concrete_panics() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("b", Pack::new(1), &[], &[]);

    let requirements = provider.requirements(&["b 1..2 | cuda 11..100"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let _ = solver.solve(problem);
}

/// Environment-package encoding must also work under a real async runtime
/// (tokio with `sleep_before_return = true`): classification of environment
/// packages happens inside the queued futures where awaiting is legal, not
/// via synchronous cache peeks that only hold under `NowOrNeverRuntime`.
///
/// `a` requires the env package itself (forcing the literal true) and has a
/// conditional dependency on `b` that becomes active because of it. `a` also
/// constrains the env package, exercising the constraint path as well.
#[test]
fn test_env_encoding_under_async_runtime() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["cuda 11..100", "b 1..2; if cuda 11..100"],
        &["cuda 5..100"],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);

    // `solve_snapshot` uses the tokio runtime and sets `sleep_before_return`.
    let result = solve_snapshot(provider, &["a"]);
    assert_snapshot!(result, @r"
    a=1
    b=1
    ");
}

// ===========================================================================
// M2: Universal solve (enumeration loop) -- scenario tests
// ===========================================================================

/// Parses a signed environment literal for the test DSL used in
/// [`universal_solve_snapshot`]. `"cuda absent"` is the absent literal,
/// anything else is parsed as a [`Spec`] (e.g. `"cuda 11..100"`). A
/// `"not "` prefix negates the literal.
fn parse_env_literal(
    provider: &mut BundleBoxProvider,
    literal: &str,
) -> (EnvLiteral<NameId>, bool) {
    let (literal, positive) = match literal.strip_prefix("not ") {
        Some(rest) => (rest, false),
        None => (literal, true),
    };
    let env_literal = match literal.strip_suffix(" absent") {
        Some(name) => EnvLiteral {
            package: provider.pool.intern_package_name(name),
            kind: EnvLiteralKind::Absent,
        },
        None => {
            let version_set = provider.version_sets(&[literal])[0];
            EnvLiteral {
                package: provider.version_set_name(version_set),
                kind: EnvLiteralKind::Matches(version_set),
            }
        }
    };
    (env_literal, positive)
}

/// Runs a universal solve and formats the resulting cell partition (or the
/// failure) as a string for inline snapshots. `model` is a CNF: each inner
/// slice is a disjunction of signed environment literals in the DSL of
/// [`parse_env_literal`].
fn universal_solve_snapshot(
    mut provider: BundleBoxProvider,
    specs: &[&str],
    model: &[&[&str]],
) -> String {
    use std::fmt::Write;

    let requirements = provider.requirements(specs);
    let environment_model = model
        .iter()
        .map(|disjunction| {
            disjunction
                .iter()
                .map(|literal| parse_env_literal(&mut provider, literal))
                .collect()
        })
        .collect();

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(requirements)
        .environment_model(environment_model);
    match solver.solve_universal(problem) {
        Ok(solution) => {
            let mut buf = String::new();
            for (condition, solvables) in &solution.cells {
                writeln!(buf, "cell: {}", condition.display(solver.provider())).unwrap();
                for solvable in solvables
                    .iter()
                    .map(|&s| solver.provider().display_solvable(s).to_string())
                    .sorted()
                {
                    writeln!(buf, "  {solvable}").unwrap();
                }
            }
            buf
        }
        Err(UniversalFailure::Unsolvable { cell, .. }) => {
            format!("unsolvable in cell: {}", cell.display(solver.provider()))
        }
        Err(UniversalFailure::Cancelled(_)) => "cancelled".to_string(),
    }
}

/// Scenario (a): a conditional dependency on `cuda` with the model
/// `cuda absent OR cuda >=11`. Two cells, baseline (cuda not matching, i.e.
/// absent within the model) first. The first cell's condition comes from the
/// conditional requires clause `(not a or not L_11 or b)`: with `a` installed
/// and `b` not installed only `not L_11` supports the clause. The second
/// cell's condition is empty after generalization (the dependency is
/// satisfied by a concrete solvable) and is restored to `L_11` by the
/// disjointness repair against the first cell.
#[test]
fn test_universal_conditional_dependency_two_cells() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("a", Pack::new(1), &["b 1..2; if cuda 11..100"], &[]);
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[&["cuda absent", "cuda 11..100"]]);
    assert_snapshot!(result, @r"
    cell: not (cuda in >=11, <100)
      a=1
    cell: cuda in >=11, <100
      a=1
      b=1
    ");
}

/// Scenario (b): candidate-level split. `pkg=2` (sorted first) requires
/// `glibc >=2.28` and `pkg=1` requires `glibc >=2.17`; the model is
/// `glibc >=2.17` (versions encoded as integers: 217, 228). The first cell
/// records `L_228` (forced true by the requires clause of `pkg=2`). The
/// second cell records `L_217` and must be generalized correctly: the repair
/// step appends `not L_228` because the oracle cannot prove
/// `glibc in 217..1000` disjoint from `glibc in 228..1000`.
#[test]
fn test_universal_candidate_split_generalization() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("glibc", false);
    provider.add_package("pkg", Pack::new(2), &["glibc 228..1000"], &[]);
    provider.add_package("pkg", Pack::new(1), &["glibc 217..1000"], &[]);

    let result = universal_solve_snapshot(provider, &["pkg"], &[&["glibc 217..1000"]]);
    assert_snapshot!(result, @r"
    cell: glibc in >=228, <1000
      pkg=2
    cell: glibc in >=217, <1000 AND not (glibc in >=228, <1000)
      pkg=1
    ");
}

/// Scenario (c): constrains split with an uncovered unsolvable gap. `a`
/// constrains `cuda >=11` and the model allows `cuda absent OR cuda >=10`.
/// The cells for `cuda absent` and `cuda >=11` are solvable, but the region
/// `cuda present in [10, 11)` is inside the model and unsolvable, so the
/// whole universal solve must fail with that witness region (total coverage
/// semantics).
#[test]
fn test_universal_constrains_split_unsolvable_gap() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("a", Pack::new(1), &[], &["cuda 11..100"]);

    let result = universal_solve_snapshot(provider, &["a"], &[&["cuda absent", "cuda 10..100"]]);
    assert_snapshot!(result, @"unsolvable in cell: not (cuda absent) AND cuda in >=10, <100 AND not (cuda in >=11, <100)");
}

/// Scenario (d): non-fragmentation. An environment package that is declared
/// (and even mentioned in the model) but never load bearing must yield
/// exactly one cell with the empty condition.
#[test]
fn test_universal_non_fragmentation_single_cell() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("a", Pack::new(1), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[&["cuda absent", "cuda 11..100"]]);
    assert_snapshot!(result, @r"
    cell: <all environments>
      a=1
    ");
}

/// Scenario (e): an unsolvable region. The only build of `a` requires
/// `glibc >=2.28`, but the model pins glibc to `[2.17, 2.28)`. The oracle
/// proves the two ranges disjoint, so no cell is solvable and the failure
/// must carry the witness region.
#[test]
fn test_universal_unsolvable_region() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("glibc", false);
    provider.add_package("a", Pack::new(1), &["glibc 228..1000"], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[&["glibc 217..228"]]);
    assert_snapshot!(result, @"unsolvable in cell: glibc in >=217, <228 AND not (glibc in >=228, <1000)");
}

/// Two independent conditional dependencies on different environment
/// packages. The first cell records both literals false, so its blocking
/// clause consists of two positive literals; `decide()` must actively make
/// progress on it (watches alone never fire when both literals stay
/// undecided) or the enumeration loop would rediscover the baseline cell
/// forever. The full cross product of the two conditions is enumerated:
/// four pairwise disjoint cells.
#[test]
fn test_universal_independent_conditions_cross_product() {
    let result = universal_solve_snapshot(cross_product_provider(), &["a"], &[]);
    assert_snapshot!(result, @r"
    cell: not (cuda in >=11, <100) AND not (rocm in >=5, <10)
      a=1
    cell: cuda in >=11, <100 AND not (rocm in >=5, <10)
      a=1
      b=1
    cell: cuda in >=11, <100 AND rocm in >=5, <10
      a=1
      b=1
      c=1
    cell: not (cuda in >=11, <100) AND rocm in >=5, <10
      a=1
      c=1
    ");
}

fn cross_product_provider() -> BundleBoxProvider {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_environment_package("rocm", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["b 1..2; if cuda 11..100", "c 1..2; if rocm 5..10"],
        &[],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);
    provider.add_package("c", Pack::new(1), &[], &[]);
    provider
}

/// Solving the same universal problem twice in fresh solvers must produce
/// identical output (deterministic cell order, conditions and solvables).
#[test]
fn test_universal_determinism() {
    let first = universal_solve_snapshot(cross_product_provider(), &["a"], &[]);
    let second = universal_solve_snapshot(cross_product_provider(), &["a"], &[]);
    assert_eq!(first, second);
}

/// A plain (non-universal) `solve` over the same universe as
/// [`test_universal_conditional_dependency_two_cells`] is completely
/// unaffected by the universal machinery: it yields the baseline solution
/// (all environment literals false, conditional dependency inactive).
#[test]
fn test_universal_plain_solve_unchanged() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("a", Pack::new(1), &["b 1..2; if cuda 11..100"], &[]);
    provider.add_package("b", Pack::new(1), &[], &[]);

    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r"
    a=1
    ");
}

/// A negative model literal: the model `not (cuda in 11..100)` excludes the
/// matching region entirely, so the conditional dependency can never
/// activate and the enumeration stops after the baseline cell. The cell is
/// still conditioned on `not L_11` (the condition complement is load
/// bearing), and the blocking clause `(L_11)` then contradicts the model
/// assertion, which the witness check recognizes as full coverage.
#[test]
fn test_universal_negative_model_literal() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("a", Pack::new(1), &["b 1..2; if cuda 11..100"], &[]);
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[&["not cuda 11..100"]]);
    assert_snapshot!(result, @r"
    cell: not (cuda in >=11, <100)
      a=1
    ");
}

/// Cancellation during a universal solve propagates out as
/// [`UniversalFailure::Cancelled`].
#[test]
fn test_universal_cancellation() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_package("a", Pack::new(1).cancel_during_get_dependencies(), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[&["cuda absent", "cuda 11..100"]]);
    assert_snapshot!(result, @"cancelled");
}

/// AND-condition over two environment packages where one literal is forced
/// true by the model. The condition complement disjunction is
/// `(not L_cuda or not L_rocm)` and the baseline solution has
/// `L_rocm = true` (model assertion) and `L_cuda` undecided: the clause is
/// supported by `not L_cuda` ALONE. The rocm literal must not be recorded
/// (its complement is false and contributes nothing); recording it would
/// claim the solution is valid where rocm does not match, which is wrong.
#[test]
fn test_universal_and_condition_with_forced_literal() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_environment_package("rocm", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["b 1..2; if cuda 11..100 and rocm 5..100"],
        &[],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[&["rocm 5..100"]]);
    assert_snapshot!(result, @r"
    cell: not (cuda in >=11, <100)
      a=1
    cell: cuda in >=11, <100
      a=1
      b=1
    ");
}

/// AND-condition where BOTH environment literals are false in the baseline:
/// exactly the FIRST literal of the disjunction (in encoding order, cuda
/// before rocm) is recorded, which pins the determinism rule and gives the
/// most general baseline cell (it covers any rocm value). The partition is
/// three cells, not a full cross product.
#[test]
fn test_universal_and_condition_baseline_records_first_literal() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_environment_package("rocm", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["b 1..2; if cuda 11..100 and rocm 5..100"],
        &[],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[]);
    assert_snapshot!(result, @r"
    cell: not (cuda in >=11, <100)
      a=1
    cell: cuda in >=11, <100 AND not (rocm in >=5, <100)
      a=1
    cell: cuda in >=11, <100 AND rocm in >=5, <100
      a=1
      b=1
    ");
}

/// The supervisor reproduction variant where the rocm literal is forced
/// true by a ROOT REQUIREMENT instead of (only) the model: the requirement
/// clause `(not root or L_rocm)` is genuine support, so `rocm in 5..100`
/// legitimately appears in every cell condition, while the rocm complement
/// still contributes nothing to the conditional clause.
#[test]
fn test_universal_and_condition_with_required_env_package() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_environment_package("rocm", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["b 1..2; if cuda 11..100 and rocm 5..100"],
        &[],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a", "rocm 5..100"], &[&["rocm 5..100"]]);
    assert_snapshot!(result, @r"
    cell: rocm in >=5, <100 AND not (cuda in >=11, <100)
      a=1
    cell: rocm in >=5, <100 AND cuda in >=11, <100
      a=1
      b=1
    ");
}

/// OR-condition over two environment packages: the DNF machinery produces
/// TWO conditional requires clauses, one per disjunct. Each clause records
/// its own complement literal in the baseline, and the enumeration covers
/// the space in three cells (the cuda cell generalizes over rocm because
/// `b` is installed there regardless of rocm).
#[test]
fn test_universal_or_condition_dnf_partition() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_environment_package("rocm", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["b 1..2; if cuda 11..100 or rocm 5..100"],
        &[],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result = universal_solve_snapshot(provider, &["a"], &[]);
    assert_snapshot!(result, @r"
    cell: not (cuda in >=11, <100) AND not (rocm in >=5, <100)
      a=1
    cell: cuda in >=11, <100
      a=1
      b=1
    cell: not (cuda in >=11, <100) AND rocm in >=5, <100
      a=1
      b=1
    ");
}

// ===========================================================================
// M3: Output layer (merged presence view and conditional edges)
// ===========================================================================

/// Runs a universal solve and formats the merged presence view and the
/// aggregated conditional edges for inline snapshots.
fn universal_merged_and_edges_snapshot(
    mut provider: BundleBoxProvider,
    specs: &[&str],
    model: &[&[&str]],
) -> String {
    use std::fmt::Write;

    let requirements = provider.requirements(specs);
    let environment_model = model
        .iter()
        .map(|disjunction| {
            disjunction
                .iter()
                .map(|literal| parse_env_literal(&mut provider, literal))
                .collect()
        })
        .collect();

    let mut solver = Solver::new(provider);
    let problem = UniversalProblem::new()
        .requirements(requirements)
        .environment_model(environment_model);
    let solution = solver.solve_universal(problem).expect("solvable");
    let provider = solver.provider();

    let mut buf = String::new();
    writeln!(buf, "merged:").unwrap();
    for (solvable, presence) in solution.merged() {
        writeln!(
            buf,
            "  {} -> {}",
            provider.display_solvable(solvable),
            presence.display(provider)
        )
        .unwrap();
    }
    writeln!(buf, "edges:").unwrap();
    for (edge, presence) in solution.edges() {
        let parent = match edge.parent {
            Some(parent) => provider.display_solvable(parent).to_string(),
            None => "<root>".to_string(),
        };
        let target = match edge.target {
            Some(target) => provider.display_solvable(target).to_string(),
            None => "<environment>".to_string(),
        };
        writeln!(
            buf,
            "  {parent} -> {} -> {target} [{}]",
            edge.requirement.display(provider),
            presence.display(provider)
        )
        .unwrap();
    }
    buf
}

/// merged() over the cross-product scenario: `a` is in every cell and gets
/// the always-true presence; the cells containing `b` (resp. `c`) differ only
/// in the sign of the rocm (resp. cuda) literal, so the pairwise merge drops
/// it. The edges carry the same presences: the root edge to `a` is
/// unconditional, the conditional dependencies of `a` are active exactly
/// where their guard literal is true.
#[test]
fn test_universal_merged_and_edges_cross_product() {
    let result = universal_merged_and_edges_snapshot(cross_product_provider(), &["a"], &[]);
    assert_snapshot!(result, @r"
    merged:
      a=1 -> <all environments>
      b=1 -> cuda in >=11, <100
      c=1 -> rocm in >=5, <10
    edges:
      <root> -> a * -> a=1 [<all environments>]
      a=1 -> b >=1, <2 -> b=1 [cuda in >=11, <100]
      a=1 -> c >=1, <2 -> c=1 [rocm in >=5, <10]
    ");
}

/// merged() with a presence that does not simplify to a single conjunction:
/// in the OR-condition scenario `b` is installed in the `cuda` cell and in
/// the `not cuda AND rocm` cell. The two disjuncts have different lengths,
/// so they stay an OR (the multi-literal disjunct is parenthesized).
#[test]
fn test_universal_merged_or_condition_keeps_disjunction() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_environment_package("rocm", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["b 1..2; if cuda 11..100 or rocm 5..100"],
        &[],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result = universal_merged_and_edges_snapshot(provider, &["a"], &[]);
    assert_snapshot!(result, @r"
    merged:
      a=1 -> <all environments>
      b=1 -> cuda in >=11, <100 OR (not (cuda in >=11, <100) AND rocm in >=5, <100)
    edges:
      <root> -> a * -> a=1 [<all environments>]
      a=1 -> b >=1, <2 -> b=1 [cuda in >=11, <100 OR (not (cuda in >=11, <100) AND rocm in >=5, <100)]
    ");
}

/// An edge whose requirement is on an environment package has no target
/// solvable: the environment itself satisfies it. The root edge to `rocm` is
/// active in every cell and aggregates to the always-true presence.
#[test]
fn test_universal_edges_env_requirement_has_no_target() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);
    provider.add_environment_package("rocm", true);
    provider.add_package(
        "a",
        Pack::new(1),
        &["b 1..2; if cuda 11..100 and rocm 5..100"],
        &[],
    );
    provider.add_package("b", Pack::new(1), &[], &[]);

    let result =
        universal_merged_and_edges_snapshot(provider, &["a", "rocm 5..100"], &[&["rocm 5..100"]]);
    assert_snapshot!(result, @r"
    merged:
      a=1 -> <all environments>
      b=1 -> rocm in >=5, <100 AND cuda in >=11, <100
    edges:
      <root> -> a * -> a=1 [<all environments>]
      <root> -> rocm >=5, <100 -> <environment> [<all environments>]
      a=1 -> b >=1, <2 -> b=1 [rocm in >=5, <100 AND cuda in >=11, <100]
    ");
}

/// The environment model may only reference environment packages; a
/// concrete package is reported with a clear panic.
#[test]
#[should_panic(expected = "is not an environment package")]
fn test_universal_model_rejects_concrete_package() {
    let mut provider = BundleBoxProvider::new();
    provider.add_package("b", Pack::new(1), &[], &[]);

    let _ = universal_solve_snapshot(provider, &[], &[&["b 1..2"]]);
}

/// An absent literal in the model for a package declared with
/// `can_be_absent: false` is reported with a clear panic.
#[test]
#[should_panic(expected = "can_be_absent: false")]
fn test_universal_model_rejects_absent_literal_for_present_package() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", false);

    let _ = universal_solve_snapshot(provider, &[], &[&["cuda absent"]]);
}

/// An empty disjunction in the model makes the model unsatisfiable and is
/// reported with a clear panic.
#[test]
#[should_panic(expected = "empty disjunction")]
fn test_universal_model_rejects_empty_disjunction() {
    let mut provider = BundleBoxProvider::new();
    provider.add_environment_package("cuda", true);

    let _ = universal_solve_snapshot(provider, &[], &[&[]]);
}

/// Multiple constraints from different parents on the same package.
#[test]
fn test_constrains_multiple_parents() {
    let mut provider = BundleBoxProvider::new();
    for v in 1..=8u32 {
        provider.add_package("pkg", v.into(), &[], &[]);
    }
    provider.add_package("x", 1.into(), &["pkg"], &[]);
    // a constrains pkg to [3,100), forbidding pkg=1 and pkg=2
    provider.add_package("a", 1.into(), &[], &["pkg 3..100"]);
    // b constrains pkg to [0,8), forbidding pkg=8
    provider.add_package("b", 1.into(), &[], &["pkg 0..8"]);

    let requirements = provider.requirements(&["x", "a", "b"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    // pkg must be in [3,8) -> 3..7, highest = 7
    assert_snapshot!(result, @r###"
    a=1
    b=1
    pkg=7
    x=1
    "###);
}
