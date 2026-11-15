"""Compares the Rust engine with CINC Server's own solver (dep_selector on Gecode).

Both run inside the same container, on the same problems, so answers and timings are
directly comparable. Build the image first, from the repository root:

    docker build -f docker/cinc-baseline/Dockerfile -t depsolve-cinc-baseline .

Then: PYTHONPATH=src python3 -m depsolver.cinc_compare
"""

from __future__ import annotations

import argparse
import json
import random
import subprocess
from statistics import median
from typing import Dict, List

from depsolver.benchmarks import default_scenarios, random_small_problem
from depsolver.model import Problem, check_solution
from depsolver.native import NO_SOLUTION_STATUSES

IMAGE = "depsolve-cinc-baseline"

# Solved first and discarded, so one-off start-up cost is not charged to a real problem.
_WARM_UP = Problem({"warm": {"1.0.0": {"up": "~> 1.0"}}, "up": {"1.0.0": {}}}, {"warm": ""})


def run_in_container(command: str, problems: List[Problem], timeout_ms: int = 5000) -> List[dict]:
    """Solves `problems` with `command` (solve-cinc or solve-rust) in the comparison image."""
    payload = "".join(json.dumps(problem.to_dict()) + "\n" for problem in [_WARM_UP, *problems])
    result = subprocess.run(
        ["docker", "run", "--rm", "-i", "--platform", "linux/amd64", "-e", f"CINC_TIMEOUT_MS={timeout_ms}", IMAGE, command],
        input=payload,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"{command} failed: {result.stderr.strip()}")
    outcomes = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
    if len(outcomes) != len(problems) + 1:
        raise RuntimeError(f"{command} returned {len(outcomes) - 1} outcomes for {len(problems)} problems")
    return outcomes[1:]


def compare(problems: List[Problem], timeout_ms: int = 5000) -> List[Dict[str, dict]]:
    cinc = run_in_container("solve-cinc", problems, timeout_ms)
    rust = run_in_container("solve-rust", problems, timeout_ms)
    return [{"cinc": c, "rust": r} for c, r in zip(cinc, rust)]


def summarize_agreement(problems: List[Problem], rows: List[Dict[str, dict]]) -> dict:
    """Counts how often the two solvers agree, and collects every disagreement."""
    summary = {
        "problems": len(problems),
        "both_solved": 0,
        "both_unsolvable": 0,
        "same_solution": 0,
        "cinc_timeouts": 0,
        "disagreements": [],
        "invalid_solutions": [],
    }
    for index, (problem, row) in enumerate(zip(problems, rows)):
        cinc, rust = row["cinc"], row["rust"]
        for solver, outcome in row.items():
            if outcome["status"] == "ok":
                violations = check_solution(problem, outcome["solution"])
                if violations:
                    summary["invalid_solutions"].append({"index": index, "solver": solver, "violations": violations})
        if cinc["status"] == "timeout":
            summary["cinc_timeouts"] += 1
        elif cinc["status"] == "ok" and rust["status"] == "ok":
            summary["both_solved"] += 1
            summary["same_solution"] += cinc["solution"] == rust["solution"]
        elif cinc["status"] == "unsolvable" and rust["status"] in NO_SOLUTION_STATUSES:
            summary["both_unsolvable"] += 1
        else:
            summary["disagreements"].append({"index": index, "cinc": cinc, "rust": rust, "problem": problem.to_dict()})
    return summary


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fuzz", type=int, default=2000, help="number of small random problems (default 2000)")
    parser.add_argument("--seed", type=int, default=20261115)
    parser.add_argument("--timeout-ms", type=int, default=5000, help="CINC solver timeout; erchef's default is 5000")
    args = parser.parse_args()

    rng = random.Random(args.seed)
    fuzz = [random_small_problem(rng) for _ in range(args.fuzz)]
    rows = compare(fuzz, args.timeout_ms)
    summary = summarize_agreement(fuzz, rows)

    print(f"== {summary['problems']} small random problems")
    print(f"both solved:      {summary['both_solved']} ({summary['same_solution']} with identical versions)")
    print(f"both unsolvable:  {summary['both_unsolvable']}")
    print(f"CINC timeouts:    {summary['cinc_timeouts']}")
    print(f"disagreements:    {len(summary['disagreements'])}")
    print(f"invalid answers:  {len(summary['invalid_solutions'])}")
    print(
        f"median time:      CINC {median(row['cinc']['seconds'] for row in rows) * 1000:.3f}ms, "
        f"Rust {median(row['rust']['seconds'] for row in rows) * 1000:.3f}ms"
    )
    for item in summary["disagreements"][:3]:
        print("disagreement:", json.dumps(item))
    for item in summary["invalid_solutions"][:3]:
        print("invalid:", json.dumps(item))

    scenarios = default_scenarios()
    rows = compare([scenario.problem for scenario in scenarios], args.timeout_ms)
    print("\n== benchmark scenarios")
    for scenario, row in zip(scenarios, rows):
        cinc, rust = row["cinc"], row["rust"]
        same = cinc.get("solution") == rust.get("solution") if cinc["status"] == "ok" == rust["status"] else "-"
        print(
            f"{scenario.name:<58} CINC {cinc['status']:<10} {cinc['seconds']:>8.4f}s   "
            f"Rust {rust['status']:<10} {rust['seconds']:>8.4f}s   same solution: {same}"
        )


if __name__ == "__main__":
    main()
