"""Inspect environment packages and version sets of a benchmark snapshot.

Usage: python inspect_snapshot.py <snapshot.json> [--singleton <out.json>]

Prints the environment packages, their machine candidates, and every version
set targeting them. With --singleton, writes an environment model JSON that
pins every environment literal to its truth value on the simulated machine
(matching-candidates based; __archspec uses DAG lineage semantics via a
hardcoded lineage table for the benchmark machines).
"""

import json
import sys

# Lineage sets for the simulated machines (DAG semantics: a literal for X is
# true when the machine's microarchitecture lineage includes X).
ARCHSPEC_LINEAGE = {
    "x86_64_v3": {"x86_64", "x86_64_v2", "x86_64_v3"},
    "m1": {"aarch64", "armv8.0a", "armv8.1a", "armv8.2a", "armv8.3a", "armv8.4a", "m1"},
}


def main():
    path = sys.argv[1]
    singleton_out = None
    if "--singleton" in sys.argv:
        singleton_out = sys.argv[sys.argv.index("--singleton") + 1]

    with open(path, "r", encoding="utf-8") as f:
        snapshot = json.load(f)

    packages = snapshot["packages"]
    version_sets = snapshot["version_sets"]
    solvables = snapshot["solvables"]

    env_packages = {}  # name_id -> package
    for name_id, package in enumerate(packages):
        if package is not None and package.get("environment") is not None:
            env_packages[name_id] = package

    machine_lineage = None
    clauses = []
    for name_id, package in env_packages.items():
        name = package["name"]
        machine = [solvables[s]["display"] for s in package["solvables"]]
        print(f"== {name} (can_be_absent={package['environment']['can_be_absent']})")
        print(f"   machine candidates: {machine}")
        machine_solvables = set(package["solvables"])
        if name == "__archspec":
            build = machine[0].split("=")[-1]
            machine_lineage = ARCHSPEC_LINEAGE[build]
        for vs_id, version_set in enumerate(version_sets):
            if version_set is None or version_set["name"] != name_id:
                continue
            matching = set(version_set.get("matching_candidates", []))
            concrete_truth = bool(matching & machine_solvables)
            if name == "__archspec":
                # DAG lineage semantics: the build component of the spec
                # display is the microarchitecture name.
                spec_build = version_set["display"].split(" ")[-1]
                truth = (
                    spec_build in machine_lineage
                    if version_set["display"] != "*"
                    else True
                )
                note = " (DAG)" if truth != concrete_truth else ""
            else:
                truth = concrete_truth
                note = ""
            print(f"   [{vs_id}] '{version_set['display']}' -> {truth}{note}")
            clauses.append(
                [{"package": name, "matches": version_set["display"], "positive": truth}]
            )

    if singleton_out:
        with open(singleton_out, "w", encoding="utf-8") as f:
            json.dump({"clauses": clauses}, f, indent=2)
        print(f"wrote singleton model with {len(clauses)} clauses to {singleton_out}")


if __name__ == "__main__":
    main()
