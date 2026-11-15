"""Synthetic benchmark scenarios, timed on the brute-force baseline and the Rust engine.

Run with: PYTHONPATH=src python3 -m depsolver.benchmarks
"""

from __future__ import annotations

import random
from dataclasses import dataclass
from time import perf_counter
from typing import Dict, List, Optional

from depsolver import native
from depsolver.baseline import BaselineSolver, SearchBudgetExceeded
from depsolver.model import Problem, check_solution


@dataclass(frozen=True)
class Scenario:
    name: str
    problem: Problem


def late_pin(packages: int, versions: int) -> Scenario:
    """A chain pk0 -> pk1 -> ... -> late, where every version of `late` pins pk0 to its
    oldest version. A solver that prefers newest versions commits to the wrong pk0 first
    and only learns so at the bottom of the chain."""
    pool = [f"1.{minor}.0" for minor in range(1, versions + 1)]
    chain = [f"pk{index}" for index in range(packages)] + ["late"]
    cookbooks: Dict[str, Dict[str, Dict[str, str]]] = {}
    for name, successor in zip(chain, chain[1:]):
        cookbooks[name] = {version: {successor: "~> 1.0"} for version in pool}
    cookbooks["late"] = {version: {"pk0": f"= {pool[0]}"} for version in pool}
    return Scenario(f"late_pin(packages={packages}, versions={versions})", Problem(cookbooks, {"pk0": "~> 1.0"}))


def random_universe(cookbooks: int, versions: int, seed: int, run_list_size: int = 5) -> Scenario:
    """A seeded random universe where dependencies differ from version to version,
    newer releases tend to require newer dependencies, and some carry exact pins."""
    rng = random.Random(seed)
    names = [f"cb{index}" for index in range(cookbooks)]
    pools = {
        name: [f"{major}.{minor}.0" for major in range(1, 4) for minor in range(versions // 3 + 1)][:versions]
        for name in names
    }
    universe: Dict[str, Dict[str, Dict[str, str]]] = {}
    for index, name in enumerate(names):
        universe[name] = {}
        for version in pools[name]:
            major = int(version.split(".")[0])
            dependencies: Dict[str, str] = {}
            for target in rng.sample(names, min(len(names), rng.randint(0, 4))):
                if target == name:
                    continue
                roll = rng.random()
                if roll < 0.15:
                    dependencies[target] = f"= {rng.choice(pools[target])}"
                elif roll < 0.6:
                    dependencies[target] = f"~> {rng.randint(1, major)}.0"
                elif roll < 0.8:
                    dependencies[target] = f">= {rng.randint(1, major)}.0.0"
                else:
                    dependencies[target] = f"< {rng.randint(2, 4)}.0.0"
            universe[name][version] = dependencies
    run_list = {name: ">= 0.0.0" for name in rng.sample(names, min(run_list_size, len(names)))}
    return Scenario(f"random_universe(cookbooks={cookbooks}, versions={versions}, seed={seed})", Problem(universe, run_list))


def layered_universe(
    libraries: int = 60,
    applications: int = 60,
    roles: int = 20,
    seed: int = 1,
    run_list_size: int = 6,
) -> Scenario:
    """A universe shaped like a Chef organisation rather than a random graph.

    Three layers: library cookbooks (few dependencies, many releases), application
    cookbooks that depend on libraries, and role/wrapper cookbooks that depend on
    applications and often pin them exactly. Dependencies only point down a layer or
    sideways to an earlier cookbook, each release mostly keeps its predecessor's
    dependencies, and constraints drift upward as dependencies publish new majors.
    The run list is a handful of role cookbooks.
    """
    rng = random.Random(seed)

    def releases(count: int) -> List[str]:
        versions, major, minor, patch = [], rng.choice([0, 1, 1, 2]), 0, 0
        if major == 0:
            minor = 1
        for _ in range(count):
            versions.append(f"{major}.{minor}.{patch}")
            roll = rng.random()
            if roll < 0.1:
                major, minor, patch = major + 1, 0, 0
            elif roll < 0.5:
                minor, patch = minor + 1, 0
            else:
                patch += 1
        return versions

    layers = [
        ("lib", libraries, (6, 30), (0, 2)),
        ("app", applications, (4, 20), (2, 6)),
        ("role", roles, (3, 12), (2, 5)),
    ]
    universe: Dict[str, Dict[str, Dict[str, str]]] = {}
    pools: Dict[str, List[str]] = {}
    by_layer: Dict[str, List[str]] = {}

    def constraint_on(target: str, progress: float, pin_chance: float) -> str:
        # Aim at the release the dependency had reached at the same point in its history.
        pool = pools[target]
        current = pool[min(len(pool) - 1, int(progress * len(pool)))]
        major, minor, _ = current.split(".")
        roll = rng.random()
        if roll < pin_chance:
            return f"= {current}"
        if roll < pin_chance + 0.45:
            return f"~> {major}.{minor}"
        if roll < pin_chance + 0.65:
            return f"~> {current}"
        if roll < pin_chance + 0.9:
            return f">= {current}"
        return f"< {int(major) + 1}.0.0"

    for layer_index, (prefix, count, release_range, dependency_range) in enumerate(layers):
        names = [f"{prefix}_{index:03d}" for index in range(count)]
        by_layer[prefix] = names
        pin_chance = [0.02, 0.08, 0.3][layer_index]
        for position, name in enumerate(names):
            pools[name] = releases(rng.randint(*release_range))
            below = by_layer[layers[layer_index - 1][0]] if layer_index else []
            candidates = below + names[:position]
            wanted = min(len(candidates), rng.randint(*dependency_range))
            targets = rng.sample(candidates, wanted)
            universe[name] = {}
            dependencies: Dict[str, str] = {}
            for release_index, version in enumerate(pools[name]):
                progress = release_index / len(pools[name])
                for target in targets:
                    # Most releases keep the previous constraint; some re-resolve it.
                    if target not in dependencies or rng.random() < 0.25:
                        dependencies[target] = constraint_on(target, progress, pin_chance)
                if candidates and rng.random() < 0.05:  # occasionally pick up a new dependency
                    extra = rng.choice(candidates)
                    if extra not in dependencies:
                        targets.append(extra)
                        dependencies[extra] = constraint_on(extra, progress, pin_chance)
                universe[name][version] = dict(dependencies)

    run_list = {name: "" for name in rng.sample(by_layer["role"], min(run_list_size, roles))}
    return Scenario(
        f"layered_universe(libraries={libraries}, applications={applications}, roles={roles}, seed={seed})",
        Problem(universe, run_list),
    )


def random_small_problem(rng: random.Random) -> Problem:
    """A tiny random problem, small enough for the brute-force baseline, mixing every
    operator with environment constraints and the occasional missing cookbook."""
    names = [f"p{index}" for index in range(rng.randint(2, 6))]
    pool = ["1.0.0", "1.1.0", "1.2.0", "1.2.5", "1.3.0", "2.0.0", "2.1.0"]
    constraints = ["", "= 1.1.0", "= 2.0.0", ">= 1.2.0", "> 1.2.0", "< 1.2.0", "<= 1.2.5", "~> 1.1", "~> 1.2.0", "~> 2.0"]
    cookbooks = {}
    for name in names:
        others = [other for other in names if other != name]
        cookbooks[name] = {
            version: {
                dep: rng.choice(constraints) for dep in rng.sample(others, rng.randint(0, min(3, len(others))))
            }
            for version in rng.sample(pool, rng.randint(1, 5))
        }
    if rng.random() < 0.1:  # occasionally depend on a cookbook that does not exist
        cookbooks[names[0]][next(iter(cookbooks[names[0]]))]["ghost"] = ""
    run_list = {name: rng.choice(constraints) for name in rng.sample(names, rng.randint(1, 2))}
    environment = {name: rng.choice(constraints) for name in rng.sample(names, rng.randint(0, 2))}
    return Problem(cookbooks, run_list, environment)


def default_scenarios() -> List[Scenario]:
    return [
        late_pin(3, 8),
        late_pin(4, 8),
        late_pin(5, 10),
        late_pin(8, 20),
        late_pin(50, 50),
        random_universe(30, 9, seed=1),
        random_universe(200, 12, seed=2),
        random_universe(1000, 12, seed=3, run_list_size=20),
    ]


def run_scenario(scenario: Scenario, baseline_max_steps: Optional[int] = 200_000) -> dict:
    """Times both solvers on `scenario` and cross-checks their answers.

    `baseline_seconds` is None when the baseline exceeds its step budget.
    """
    start = perf_counter()
    outcome = native.solve(scenario.problem)
    native_seconds = perf_counter() - start
    if outcome["status"] == "error":
        raise ValueError(f"{scenario.name}: {outcome['message']}")
    native_solution = outcome.get("solution")
    if native_solution is not None:
        violations = check_solution(scenario.problem, native_solution)
        if violations:
            raise AssertionError(f"{scenario.name}: invalid native solution: {violations}")

    baseline = BaselineSolver(scenario.problem, max_steps=baseline_max_steps)
    start = perf_counter()
    try:
        baseline_solution = baseline.solve()
        baseline_seconds: Optional[float] = perf_counter() - start
        if (baseline_solution is None) != (native_solution is None):
            raise AssertionError(f"{scenario.name}: solvers disagree on solvability")
    except SearchBudgetExceeded:
        baseline_seconds = None

    return {
        "scenario": scenario.name,
        "solvable": native_solution is not None,
        "native_seconds": native_seconds,
        "baseline_seconds": baseline_seconds,
        "baseline_steps": baseline.steps,
    }


if __name__ == "__main__":
    for scenario in default_scenarios():
        row = run_scenario(scenario)
        baseline = "gave up" if row["baseline_seconds"] is None else f"{row['baseline_seconds']:.4f}s"
        print(
            f"{row['scenario']:<58} solvable={row['solvable']!s:<5} "
            f"native={row['native_seconds']:.4f}s  baseline={baseline} ({row['baseline_steps']} steps)"
        )
