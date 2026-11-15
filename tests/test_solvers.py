"""Behavioural tests run against both solvers, plus a differential fuzz of the
Rust engine against the brute-force baseline."""

import json
import random
import shutil
import subprocess
from pathlib import Path

import pytest

from depsolver import native
from depsolver.baseline import BaselineSolver, SearchBudgetExceeded
from depsolver.benchmarks import late_pin, random_small_problem, random_universe, run_scenario
from depsolver.model import Problem, check_solution, format_version, parse_version

REPO_ROOT = Path(__file__).resolve().parents[1]

requires_native = pytest.mark.skipif(
    native.library_path() is None,
    reason="Rust library not built (cargo build --release --manifest-path rust/Cargo.toml)",
)


def solve_baseline(problem):
    solution = BaselineSolver(problem).solve()
    if solution is None:
        return None
    return {name: format_version(parse_version(version)) for name, version in solution.items()}


def solve_native(problem):
    outcome = native.solve(problem)
    assert outcome["status"] != "error", outcome
    return outcome.get("solution")


SOLVERS = [
    pytest.param(solve_baseline, id="baseline"),
    pytest.param(solve_native, id="native", marks=requires_native),
]


@pytest.mark.parametrize("solve", SOLVERS)
def test_picks_newest_versions_that_satisfy_constraints(solve):
    problem = Problem(
        cookbooks={
            "app": {"1.0.0": {}, "1.1.0": {"lib": "~> 1.0"}},
            "lib": {"1.0.0": {}, "1.5.0": {}, "2.0.0": {}},
        },
        run_list={"app": "= 1.1.0"},
    )
    assert solve(problem) == {"app": "1.1.0", "lib": "1.5.0"}


@pytest.mark.parametrize("solve", SOLVERS)
def test_dependencies_are_per_version(solve):
    problem = Problem(
        cookbooks={
            "app": {"1.0.0": {"lib": "~> 1.0"}, "2.0.0": {"lib": "~> 3.0"}},
            "lib": {"1.4.0": {}, "2.0.0": {}},
        },
        run_list={"app": ">= 0.0.0"},
    )
    assert solve(problem) == {"app": "1.0.0", "lib": "1.4.0"}


@pytest.mark.parametrize("solve", SOLVERS)
def test_late_pin_forces_an_early_choice_back(solve):
    assert solve(late_pin(packages=2, versions=4).problem) == {"pk0": "1.1.0", "pk1": "1.4.0", "late": "1.4.0"}


@pytest.mark.parametrize("solve", SOLVERS)
def test_environment_restricts_without_requiring(solve):
    problem = Problem(
        cookbooks={
            "app": {"1.0.0": {"lib": ">= 1.0.0"}},
            "lib": {"1.0.0": {}, "2.0.0": {}},
            "unused": {"1.0.0": {}},
        },
        run_list={"app": "= 1.0.0"},
        environment={"lib": "< 2.0.0", "unused": "= 1.0.0"},
    )
    assert solve(problem) == {"app": "1.0.0", "lib": "1.0.0"}


@pytest.mark.parametrize("solve", SOLVERS)
def test_conflicting_requirements_have_no_solution(solve):
    problem = Problem(
        cookbooks={"app": {"1.0.0": {"lib": "~> 1.0"}}, "lib": {"1.0.0": {}, "2.0.0": {}}},
        run_list={"app": "= 1.0.0", "lib": "= 2.0.0"},
    )
    assert solve(problem) is None


@pytest.mark.parametrize("solve", SOLVERS)
def test_dependency_on_unknown_cookbook_has_no_solution(solve):
    problem = Problem(cookbooks={"app": {"1.0.0": {"ghost": ">= 0.0.0"}}}, run_list={"app": "= 1.0.0"})
    assert solve(problem) is None


def test_baseline_step_budget():
    solver = BaselineSolver(late_pin(packages=4, versions=8).problem, max_steps=50)
    with pytest.raises(SearchBudgetExceeded):
        solver.solve()


@requires_native
def test_native_explains_conflicts():
    problem = Problem(
        cookbooks={"app": {"1.0.0": {"lib": "~> 1.0"}}, "lib": {"1.0.0": {}, "2.0.0": {}}},
        run_list={"app": "= 1.0.0", "lib": "= 2.0.0"},
    )
    outcome = native.solve(problem)
    assert outcome["status"] == "unsolvable"
    assert "app" in outcome["explanation"] and "lib" in outcome["explanation"]


@requires_native
def test_native_validates_the_run_list_first():
    problem = Problem(
        cookbooks={"foo": {"1.2.3": {}}, "bar": {"2.0.0": {}}},
        run_list={"foo": "= 3.0.0", "bar": "", "nope": ""},
    )
    assert native.solve(problem) == {
        "status": "invalid_run_list",
        "non_existent_cookbooks": ["nope"],
        "no_versions": ["(foo = 3.0.0)"],
    }


@requires_native
def test_native_names_missing_dependencies():
    problem = Problem(cookbooks={"app": {"1.0.0": {"ghost": ""}}}, run_list={"app": ""})
    outcome = native.solve(problem)
    assert outcome["status"] == "unsolvable"
    assert outcome["non_existent_cookbooks"] == ["ghost"]


@requires_native
def test_native_honours_timeout():
    cookbooks = {"app": {"1.0.0": {}}}
    assert native.solve(Problem(cookbooks, {"app": ""}, timeout_ms=0))["status"] == "timeout"
    assert native.solve(Problem(cookbooks, {"app": ""}, timeout_ms=60_000))["status"] == "ok"


@requires_native
def test_native_reports_malformed_input_as_error():
    problem = Problem(cookbooks={"app": {"1.0.0": {"lib": "!= 1.0.0"}}}, run_list={"app": "= 1.0.0"})
    outcome = native.solve(problem)
    assert outcome["status"] == "error"
    assert "app 1.0.0 depends on lib" in outcome["message"]


@requires_native
def test_native_handles_a_late_pin_the_baseline_cannot():
    row = run_scenario(late_pin(packages=50, versions=50), baseline_max_steps=20_000)
    assert row["solvable"] is True
    assert row["baseline_seconds"] is None
    assert row["native_seconds"] < 5


@requires_native
def test_native_agrees_with_baseline_on_random_problems():
    rng = random.Random(20261115)
    solvable = 0
    for _ in range(3000):
        problem = random_small_problem(rng)
        expected = BaselineSolver(problem).solve()
        outcome = native.solve(problem)
        context = json.dumps(problem.to_dict())
        assert outcome["status"] in {"ok", *native.NO_SOLUTION_STATUSES}, f"{outcome}\n{context}"
        if expected is None:
            assert outcome["status"] in native.NO_SOLUTION_STATUSES, f"native found {outcome}, baseline found nothing\n{context}"
        else:
            assert outcome["status"] == "ok", f"native found nothing, baseline found {expected}\n{context}"
            assert check_solution(problem, outcome["solution"]) == [], context
            solvable += 1
    # Guard against a degenerate generator that only produces one kind of problem.
    assert 300 < solvable < 2700


@requires_native
def test_random_universe_solutions_are_valid():
    for seed in range(5):
        scenario = random_universe(cookbooks=60, versions=9, seed=seed)
        outcome = native.solve(scenario.problem)
        assert outcome["status"] in {"ok", *native.NO_SOLUTION_STATUSES}
        if outcome["status"] == "ok":
            assert check_solution(scenario.problem, outcome["solution"]) == []


@requires_native
@pytest.mark.skipif(shutil.which("ruby") is None, reason="ruby not installed")
def test_ruby_bridge_returns_the_same_outcome():
    problem = Problem(
        cookbooks={"app": {"1.1.0": {"lib": "~> 1.0"}}, "lib": {"1.0.0": {}, "1.5.0": {}, "2.0.0": {}}},
        run_list={"app": "= 1.1.0"},
    )
    script = "require 'json'; puts JSON.generate(Depsolve::RustBridge.solve(JSON.parse(STDIN.read)))"
    result = subprocess.run(
        ["ruby", "-r", "./ruby/solver_bridge.rb", "-e", script],
        input=json.dumps(problem.to_dict()),
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {"status": "ok", "solution": {"app": "1.1.0", "lib": "1.5.0"}}
