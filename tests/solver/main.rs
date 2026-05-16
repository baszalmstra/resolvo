mod bundle_box;

use std::io::{Write, stderr};

use bundle_box::{BundleBoxProvider, Pack};
use insta::assert_snapshot;
use itertools::Itertools;
use resolvo::{
    ConditionalRequirement, DependencyProvider, Interner, Problem, SolvableId, Solver,
    UnsolvableOrCancelled, VersionSetId,
};
use tracing_test::traced_test;

/// Create a string from a [`Transaction`]
fn transaction_to_string(interner: &impl Interner, solvables: &[SolvableId]) -> String {
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

// ---------------------------------------------------------------------------
// Lazy conditional candidate loading
// ---------------------------------------------------------------------------
//
// The tests below cover the edge cases of the lazy-conditional-candidates
// mechanism: deferred requirements should only fetch the gated packages once
// the condition actually fires, multi-disjunct conditions should fire
// per-disjunct, deferral must survive backtracking, etc.

/// The conditional requirement's "if C" gate never fires, so the gated package
/// `bla` (and its dependency `dep`) should never have its candidates fetched.
#[test]
#[traced_test]
fn test_lazy_conditional_skips_unreached_candidates() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["bla; if bar"]),
        ("bla", 1, vec!["dep"]),
        ("dep", 1, vec![]),
        ("bar", 1, vec![]),
    ]);

    // We do *not* require `bar`, so the condition `if bar` never fires.
    let requirements = provider.requirements(&["foo"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @"foo=1");

    let fetched = solver.provider().requested_package_names();
    assert!(
        !fetched.iter().any(|name| name == "bla"),
        "expected `bla` to remain unfetched, got {fetched:?}"
    );
    assert!(
        !fetched.iter().any(|name| name == "dep"),
        "expected `dep` to remain unfetched, got {fetched:?}"
    );
}

/// When the condition fires the deferred clause is encoded and the gated
/// package is fetched. Sanity check that the eager observable behaviour still
/// produces the right solution.
#[test]
#[traced_test]
fn test_lazy_conditional_fires_when_condition_holds() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["bla; if bar"]),
        ("bla", 1, vec![]),
        ("bar", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=1
    bla=1
    foo=1
    "###);
}

/// `a OR b` is a multi-disjunct condition. Firing only `a` should still
/// produce a correct solution: the `a`-disjunct's clause is encoded when `a`
/// is selected, and the `b`-disjunct stays deferred (and is never needed).
#[test]
#[traced_test]
fn test_lazy_conditional_multi_disjunct_fires_per_disjunct() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["bla; if a or b"]),
        ("bla", 1, vec![]),
        ("a", 1, vec![]),
        ("b", 1, vec![]),
    ]);

    // Only require `a`, not `b`. The disjunct gated on `a` must fire so that
    // `bla` is included.
    let requirements = provider.requirements(&["foo", "a"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    a=1
    bla=1
    foo=1
    "###);
}

/// Multiple `requires ... if C` for the same condition should all be encoded
/// when `C` fires. This exercises the "Multiple conditional requirements share
/// the same `ConditionId`" edge case.
#[test]
#[traced_test]
fn test_lazy_conditional_shared_condition_fires_all() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["bla; if cond", "ext; if cond"]),
        ("bla", 1, vec![]),
        ("ext", 1, vec![]),
        ("cond", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "cond"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bla=1
    cond=1
    ext=1
    foo=1
    "###);
}

/// When the condition fires *and* the requirement is unsolvable, the solver
/// must report the conflict, not panic and not loop forever.
#[test]
#[traced_test]
fn test_lazy_conditional_late_arriving_conflict_is_reported() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["bla 2; if bar"]),
        ("bar", 1, vec![]),
        ("bla", 1, vec![]),
    ]);

    // The condition `if bar` will fire because we require `bar`; the gated
    // requirement asks for `bla 2` but only `bla 1` exists, so the resolution
    // is unsolvable.
    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    foo * cannot be installed because there are no viable options:
    └─ foo 1 would require
       └─ bla >=2, <3, for which no candidates were found.
    The following packages are incompatible
    └─ bar * can be installed with any of the following options:
       └─ bar 1
    "###);
}

/// Force backtracking through a deferred conditional requirement: `b 1` forces
/// `cond`, which fires the deferred `bla; if cond`. Then the solver finds a
/// conflict on `bla 2` and must backtrack. After backtracking it picks `b 2`
/// (which does not force `cond`), and the deferred clause is *already*
/// encoded (clauses are not unlearnt). The solver must still find a solution
/// and must not encode the deferred clause twice.
#[test]
#[traced_test]
fn test_lazy_conditional_survives_backtracking() {
    // Force the solver down a path that fires the deferred condition and
    // then backtracks:
    //
    // - root requires `foo` and `cond`. `cond` is required unconditionally
    //   so the condition `if cond` is guaranteed to fire.
    // - `foo` has two versions; the higher (`foo=2`) carries `bla 2`
    //   (no candidate -> conflict) in addition to the deferred `bla; if
    //   cond`. The conflict is only discovered after the deferred clause
    //   fires and forces a `bla` selection.
    // - After backtracking the solver settles on `foo=1`, which only has
    //   the deferred `bla; if cond` requirement. The deferred clause is
    //   reused from the failed branch (CDCL does not unlearn clauses).
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 2, vec!["bla; if cond", "bla 2"]),
        ("foo", 1, vec!["bla; if cond"]),
        ("bla", 1, vec![]),
        ("cond", 1, vec![]),
    ]);

    let requirements = provider.requirements(&["foo", "cond"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
    bla=1
    cond=1
    foo=1
    "###);

    // The `if cond` disjunct fired (cond is required), so the deferred
    // entry was drained. The map must be empty even though the solve went
    // through a conflict + backtrack on `foo=2`. A future regression that
    // accidentally re-enqueues an entry on backtrack would leave a
    // leftover here -- this is the observable hook for the "remove on
    // first fire" invariant.
    assert_eq!(
        solver.deferred_requirements_count(),
        0,
        "deferred entries must be drained on first fire and never re-added"
    );

    // Resolve the exact same problem on a fresh solver and record its
    // clause count. The lazy path is deterministic, so a re-solve from
    // scratch produces the same clauses. If the original solve's
    // backtracking accidentally caused a double-encode of any deferred
    // clause, its clause count would be higher than the fresh re-solve's.
    let fresh_clauses = {
        let mut provider = BundleBoxProvider::from_packages(&[
            ("foo", 2, vec!["bla; if cond", "bla 2"]),
            ("foo", 1, vec!["bla; if cond"]),
            ("bla", 1, vec![]),
            ("cond", 1, vec![]),
        ]);
        let requirements = provider.requirements(&["foo", "cond"]);
        let mut fresh_solver = Solver::new(provider);
        let problem = Problem::new().requirements(requirements);
        let _ = fresh_solver.solve(problem).unwrap();
        fresh_solver.clauses_count()
    };
    assert_eq!(
        solver.clauses_count(),
        fresh_clauses,
        "clause count must match a fresh re-solve; a mismatch would mean a \
         deferred entry was encoded more than once"
    );
}

/// `condition: None` requirements must continue to behave eagerly: the gated
/// package should be fetched even though we never decide on it. This is the
/// existing behaviour, kept here as a regression guard.
#[test]
#[traced_test]
fn test_lazy_conditional_unconditional_unaffected() {
    let mut provider =
        BundleBoxProvider::from_packages(&[("foo", 1, vec!["bla"]), ("bla", 1, vec![])]);

    let requirements = provider.requirements(&["foo"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);
    assert_snapshot!(result, @r###"
    bla=1
    foo=1
    "###);

    let fetched = solver.provider().requested_package_names();
    assert!(
        fetched.iter().any(|name| name == "bla"),
        "unconditional requirements must still fetch their candidates; got {fetched:?}"
    );
}

/// Empty-complement condition: `if bar` is `bar *`, whose complement is
/// always empty (every candidate of `bar` matches `*`). The encoder relies
/// on the at-least-one tracker for `bar` to gate the requires clause. The
/// deferred path must agree with that semantics: the condition holds iff
/// any candidate of `bar` is selected.
#[test]
#[traced_test]
fn test_lazy_conditional_empty_complement_fires() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("foo", 1, vec!["bla; if bar"]),
        // Multiple versions of bar — the VS `bar *` still matches all of
        // them, so its complement is empty.
        ("bar", 1, vec![]),
        ("bar", 2, vec![]),
        ("bla", 1, vec![]),
    ]);

    // With `bar` requested, the condition fires and the deferred clause is
    // encoded.
    let requirements = provider.requirements(&["foo", "bar"]);
    assert_snapshot!(solve_for_snapshot(provider, &requirements, &[]), @r###"
    bar=2
    bla=1
    foo=1
    "###);
}

/// Determinism: running the same problem ten times must produce the exact
/// same solution. The deferred-requirement drain order must not depend on
/// hash iteration.
#[test]
fn test_lazy_conditional_determinism() {
    fn build_provider() -> BundleBoxProvider {
        BundleBoxProvider::from_packages(&[
            ("foo", 1, vec!["bla; if a", "ext; if b"]),
            ("foo", 2, vec!["bla; if a", "ext; if b"]),
            ("bla", 1, vec![]),
            ("ext", 1, vec![]),
            ("a", 1, vec![]),
            ("b", 1, vec![]),
        ])
    }

    let baseline = {
        let mut provider = build_provider();
        let requirements = provider.requirements(&["foo", "a", "b"]);
        let mut solver = Solver::new(provider);
        let problem = Problem::new().requirements(requirements);
        let solved = solver.solve(problem).unwrap();
        transaction_to_string(solver.provider(), &solved)
    };

    for _ in 0..9 {
        let mut provider = build_provider();
        let requirements = provider.requirements(&["foo", "a", "b"]);
        let mut solver = Solver::new(provider);
        let problem = Problem::new().requirements(requirements);
        let solved = solver.solve(problem).unwrap();
        let result = transaction_to_string(solver.provider(), &solved);
        assert_eq!(result, baseline, "non-deterministic solve");
    }
}

/// Multi-conjunct (`a AND b`) condition. `convert_conditions_to_dnf`
/// distributes AND over OR, so `cond_a AND cond_b` becomes a single disjunct
/// with two conjuncts. The lazy path's `condition_disjunct_holds` only fires
/// when *every* conjunct in the disjunct holds.
///
/// Existing tests cover only single-conjunct disjuncts (`if x` or `if x or
/// y`), leaving the `disjunct.iter().all(...)` arm of
/// `condition_disjunct_holds` unexercised. This test fixes that gap with
/// three sub-cases sharing the same fixture but with different root
/// requirements.
mod test_lazy_conditional_multi_conjunct_requires_all {
    use super::*;

    fn build_provider() -> BundleBoxProvider {
        BundleBoxProvider::from_packages(&[
            ("a", 1, vec!["b; if cond_a and cond_b"]),
            ("b", 1, vec![]),
            ("cond_a", 1, vec![]),
            ("cond_b", 1, vec![]),
        ])
    }

    /// Neither conjunct fires. `b` must not be in the resolution and must
    /// not have its candidates fetched.
    #[test]
    #[traced_test]
    fn neither_selected() {
        let mut provider = build_provider();
        let requirements = provider.requirements(&["a"]);
        let mut solver = Solver::new(provider);
        let problem = Problem::new().requirements(requirements);
        let solved = solver.solve(problem).unwrap();
        let result = transaction_to_string(solver.provider(), &solved);
        assert_snapshot!(result, @"a=1");

        let fetched = solver.provider().requested_package_names();
        assert!(
            !fetched.iter().any(|n| n == "b"),
            "expected `b` to remain unfetched when neither conjunct holds, \
             got {fetched:?}"
        );
        assert_eq!(
            solver.deferred_requirements_count(),
            1,
            "the AND-disjunct never held, its deferred entry must remain"
        );
    }

    /// Only the first conjunct fires. `b` must not be in the resolution and
    /// must not have its candidates fetched: the lazy path requires *every*
    /// conjunct of the AND-disjunct to hold.
    #[test]
    #[traced_test]
    fn only_first_selected() {
        let mut provider = build_provider();
        let requirements = provider.requirements(&["a", "cond_a"]);
        let mut solver = Solver::new(provider);
        let problem = Problem::new().requirements(requirements);
        let solved = solver.solve(problem).unwrap();
        let result = transaction_to_string(solver.provider(), &solved);
        assert_snapshot!(result, @r###"
        a=1
        cond_a=1
        "###);

        let fetched = solver.provider().requested_package_names();
        assert!(
            !fetched.iter().any(|n| n == "b"),
            "expected `b` to remain unfetched when only one conjunct holds, \
             got {fetched:?}"
        );
        assert_eq!(
            solver.deferred_requirements_count(),
            1,
            "second conjunct (cond_b) did not hold, deferred entry must remain"
        );
    }

    /// Both conjuncts fire. `b` must be in the resolution.
    #[test]
    #[traced_test]
    fn both_selected() {
        let mut provider = build_provider();
        let requirements = provider.requirements(&["a", "cond_a", "cond_b"]);
        let mut solver = Solver::new(provider);
        let problem = Problem::new().requirements(requirements);
        let solved = solver.solve(problem).unwrap();
        let result = transaction_to_string(solver.provider(), &solved);
        assert_snapshot!(result, @r###"
        a=1
        b=1
        cond_a=1
        cond_b=1
        "###);

        assert_eq!(
            solver.deferred_requirements_count(),
            0,
            "both conjuncts hold, the deferred entry must have been drained"
        );
    }
}

/// Determinism under non-deterministic async ordering. The plain
/// [`test_lazy_conditional_determinism`] uses the default
/// `sleep_before_return = false`, so every `get_candidates` future resolves
/// synchronously and ordering is trivially deterministic. This test enables
/// `sleep_before_return = true` so each provider call awaits a 10 ms timer
/// inside the tokio runtime, forcing the `FuturesUnordered` driving the
/// encoder to actually interleave futures.
///
/// We then assert that across 10 runs of the same problem:
/// - the resolution (the solvables list) is byte-identical, and
/// - the sorted set of packages whose candidates were fetched
///   (`requested_package_names`) is byte-identical.
///
/// The second assertion is the load-bearing one for this test: it guards the
/// deferred-requirement drain order. A regression that drains
/// `SolverState::deferred_requirements` by hash iteration would still yield
/// the same final resolution (CDCL is deterministic given the same input
/// clauses) but the *order* in which conditional packages get fetched would
/// drift, observable here as differing `requested_package_names`.
#[test]
fn test_lazy_conditional_determinism_async() {
    fn build_provider() -> BundleBoxProvider {
        let mut p = BundleBoxProvider::from_packages(&[
            // Multiple deferred conditional requirements sharing distinct
            // conditions, plus an inner conditional on `bla` that only
            // fires once `a` is selected. The graph is non-trivial enough
            // that a buggy drain order would surface.
            ("foo", 1, vec!["bla; if a", "ext; if b", "tail; if c"]),
            ("foo", 2, vec!["bla; if a", "ext; if b", "tail; if c"]),
            ("bla", 1, vec!["inner; if a"]),
            ("ext", 1, vec![]),
            ("tail", 1, vec![]),
            ("inner", 1, vec![]),
            ("a", 1, vec![]),
            ("b", 1, vec![]),
            ("c", 1, vec![]),
        ]);
        // Force every provider future to await a real timer so the encoder's
        // `FuturesUnordered` driver actually has to interleave them.
        p.sleep_before_return = true;
        p
    }

    fn run() -> (String, Vec<String>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let mut provider = build_provider();
        let requirements = provider.requirements(&["foo", "a", "b", "c"]);
        let mut solver = Solver::new(provider).with_runtime(runtime);
        let problem = Problem::new().requirements(requirements);
        let solved = solver.solve(problem).unwrap();
        let result = transaction_to_string(solver.provider(), &solved);
        let mut fetched = solver.provider().requested_package_names();
        // Sort so the assertion targets the *set* of fetched packages,
        // which is what determinism guarantees -- the *insertion* order
        // into the underlying HashSet is hash-randomised and not part of
        // the solver's contract.
        fetched.sort();
        (result, fetched)
    }

    let baseline = run();
    for i in 0..9 {
        let next = run();
        assert_eq!(
            next.0, baseline.0,
            "non-deterministic resolution under async ordering at iteration {i}"
        );
        assert_eq!(
            next.1, baseline.1,
            "non-deterministic fetched-packages set under async ordering at \
             iteration {i}"
        );
    }
}

/// Pin the observable semantic divergence between the lazy-conditional path
/// and the prior eager SAT encoding.
///
/// Setup: `cond` has versions `1` and `2`. The condition VS `cond 2..3` matches
/// only `cond=2`; its complement is `{cond=1}`. We externally exclude `cond=1`
/// so the *complement* is fully decided-false, while *nothing* positively
/// selects `cond=2`. The conditional requirement is `a` requires `b` if
/// `cond>=2`.
///
/// Behaviour comparison:
/// - **Eager (legacy) path**: emits the SAT clause `¬a v ¬C!1 v b1` (where
///   `C!1` is the complement candidate `cond=1`). With `cond=1` excluded
///   (decided false), `¬C!1` is satisfied, so the clause forces `b1`. `b`
///   would appear in the resolution.
/// - **Lazy path (current implementation)**: `condition_disjunct_holds`
///   checks whether any **matching** candidate of the condition VS is
///   positively decided. `cond=2` is never positively decided here, so the
///   disjunct never holds and the deferred requirement never gets encoded.
///   `b` is therefore absent from the resolution.
///
/// This divergence is deliberate and is documented in
/// `docs/lazy-conditional-candidates.md` (the `condition_holds` pseudocode at
/// ~lines 104-117). The lazy semantics align with what a human would expect
/// from "if `cond>=2` is selected": exclusion of `cond=1` is not the same as
/// selection of `cond=2`.
///
/// This test pins the lazy behaviour so any future regression that re-derives
/// the eager semantics fails loudly.
#[test]
#[traced_test]
fn test_lazy_conditional_externally_excluded_complement_does_not_fire() {
    let mut provider = BundleBoxProvider::from_packages(&[
        ("a", 1, vec!["b; if cond 2..3"]),
        ("b", 1, vec![]),
        ("cond", 1, vec![]),
        ("cond", 2, vec![]),
    ]);
    // Externally reject the *complement* candidate `cond=1`. Nothing forces
    // `cond=2` to be selected; the only requirement is on `a`.
    provider.exclude("cond", 1, "it is externally excluded");

    let requirements = provider.requirements(&["a"]);
    let mut solver = Solver::new(provider);
    let problem = Problem::new().requirements(requirements);
    let solved = solver.solve(problem).unwrap();
    let result = transaction_to_string(solver.provider(), &solved);

    // `b` must NOT appear in the resolution: the lazy path correctly sees
    // that `cond=2` (the *matching* candidate of `cond 2..3`) is never
    // positively decided, so the deferred requirement never fires. The
    // legacy eager path would have included `b` here -- see the doc-comment
    // above for the contrast.
    assert_snapshot!(result, @"a=1");

    // The deferred entry must still be sitting in the map (its disjunct
    // never held), confirming the lazy path's predicate is what kept `b`
    // out -- not some unrelated reason like `a` being uninstallable.
    assert_eq!(
        solver.deferred_requirements_count(),
        1,
        "the `if cond 2..3` disjunct never fired, so its deferred entry \
         must remain"
    );
}

#[cfg(feature = "serde")]
fn serialize_snapshot(
    snapshot: &resolvo::snapshot::DependencySnapshot,
    destination: impl AsRef<std::path::Path>,
) {
    let file = std::io::BufWriter::new(std::fs::File::create(destination.as_ref()).unwrap());
    serde_json::to_writer_pretty(file, snapshot).unwrap()
}

fn solve_for_snapshot<D: DependencyProvider>(
    provider: D,
    root_reqs: &[ConditionalRequirement],
    root_constraints: &[VersionSetId],
) -> String {
    let mut solver = Solver::new(provider);
    let problem = Problem::new()
        .requirements(root_reqs.iter().cloned().collect())
        .constraints(root_constraints.iter().copied().map(Into::into).collect());
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
