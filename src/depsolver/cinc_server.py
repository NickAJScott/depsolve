"""End-to-end test against a real CINC Server running in Docker.

Uploads generated cookbook universes, then asks the server's depsolve endpoint
(POST /organizations/ORG/environments/_default/cookbook_versions) for the same run
lists twice: once with the stock worker (dep_selector on Gecode) and once with the
worker from cinc-patch/ (the depsolve Rust engine).

Prerequisites, from the repository root:

    docker build -f docker/cinc-baseline/Dockerfile -t depsolve-cinc-baseline .
    docker build -t depsolve-cinc-server docker/cinc-server
    docker run -d --name depsolve-cinc-server --privileged --hostname cinc.test \\
        -p 127.0.0.1:8643:443 depsolve-cinc-server      # wait for "cinc-server ready"

Then: PYTHONPATH=src python3 -m depsolver.cinc_server
"""

from __future__ import annotations

import argparse
import json
import random
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import List

from depsolver.benchmarks import layered_universe, random_universe
from depsolver.model import Problem, check_solution

CONTAINER = "depsolve-cinc-server"
BASELINE_IMAGE = "depsolve-cinc-baseline"
REPO_ROOT = Path(__file__).resolve().parents[2]
RUBY = "/opt/cinc-project/embedded/bin/ruby"


def _docker(*args: str, stdin: str | None = None, check: bool = True) -> subprocess.CompletedProcess:
    result = subprocess.run(["docker", *args], input=stdin, capture_output=True, text=True, check=False)
    if check and result.returncode != 0:
        raise RuntimeError(f"docker {' '.join(args[:4])}... failed:\n{result.stdout[-2000:]}\n{result.stderr[-2000:]}")
    return result


def install_files() -> None:
    """Copies the API helper, the patched worker and the solver library into the server."""
    _docker("exec", CONTAINER, "mkdir", "-p", "/opt/depsolve")
    with tempfile.TemporaryDirectory() as scratch:
        library = Path(scratch) / "libdepsolve_rust.so"
        holder = _docker("create", "--platform", "linux/amd64", BASELINE_IMAGE).stdout.strip()
        try:
            _docker("cp", f"{holder}:/opt/depsolve/libdepsolve_rust.so", str(library))
        finally:
            _docker("rm", holder)
        sources = [
            library,
            REPO_ROOT / "docker" / "cinc-server" / "cinc_api.rb",
            REPO_ROOT / "cinc-patch" / "depselector.rb",
            REPO_ROOT / "cinc-patch" / "depsolve-patch",
        ]
        for source in sources:
            _docker("cp", str(source), f"{CONTAINER}:/opt/depsolve/{source.name}")
    _docker("exec", CONTAINER, "chmod", "+x", "/opt/depsolve/depsolve-patch")


def set_worker(mode: str) -> None:
    """Switches erchef's depsolver worker: 'enable' for depsolve, 'disable' for stock."""
    _docker("exec", CONTAINER, "/opt/depsolve/depsolve-patch", mode)


def create_org(org: str) -> None:
    existing = _docker("exec", CONTAINER, "cinc-server-ctl", "org-list").stdout.split()
    if org not in existing:
        _docker("exec", CONTAINER, "cinc-server-ctl", "org-create", org, f"depsolve {org}")


def load_universe(org: str, problem: Problem) -> dict:
    result = _docker("exec", "-i", CONTAINER, RUBY, "/opt/depsolve/cinc_api.rb", "load", org,
                     stdin=json.dumps(problem.to_dict()), check=False)
    try:
        return json.loads(result.stdout.strip().splitlines()[-1])
    except (IndexError, json.JSONDecodeError):
        raise RuntimeError(f"upload to {org} failed:\n{result.stdout[-2000:]}\n{result.stderr[-2000:]}") from None


def solve(org: str, problem: Problem) -> dict:
    """Asks the server to depsolve `problem.run_list` against the universe loaded in `org`."""
    result = _docker("exec", "-i", CONTAINER, RUBY, "/opt/depsolve/cinc_api.rb", "solve", org,
                     stdin=json.dumps({"run_list": problem.run_list}))
    return json.loads(result.stdout.strip().splitlines()[-1])


@dataclass(frozen=True)
class Case:
    org: str
    description: str
    universe: Problem
    run_lists: List[dict]


def default_cases(run_lists_per_case: int = 4) -> List[Case]:
    cases = []
    for seed in (1, 2):
        scenario = layered_universe(seed=seed)
        roles = sorted(name for name in scenario.problem.cookbooks if name.startswith("role_"))
        rng = random.Random(seed)
        run_lists = [{name: "" for name in rng.sample(roles, size)} for size in (1, 3, 6, 12)][:run_lists_per_case]
        cases.append(Case(f"layered{seed}", scenario.name, scenario.problem, run_lists))

    scenario = random_universe(cookbooks=200, versions=12, seed=2)
    names = sorted(scenario.problem.cookbooks)
    rng = random.Random(2)
    run_lists = [{name: "" for name in rng.sample(names, size)} for size in (1, 3, 5, 10)][:run_lists_per_case]
    cases.append(Case("random200", scenario.name, scenario.problem, run_lists))
    return cases


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--skip-upload", action="store_true", help="the universes are already on the server")
    args = parser.parse_args()

    cases = default_cases()
    install_files()

    if not args.skip_upload:
        for case in cases:
            create_org(case.org)
            versions = sum(len(releases) for releases in case.universe.cookbooks.values())
            report = load_universe(case.org, case.universe)
            print(f"{case.org}: uploaded {report['uploaded']}/{versions} cookbook versions ({case.description})")
            if report["failed"]:
                raise RuntimeError(f"upload failures: {report['first_failures']}")

    results: dict = {}
    for mode, label in (("disable", "stock"), ("enable", "depsolve")):
        set_worker(mode)
        for case in cases:
            solve(case.org, Problem(case.universe.cookbooks, case.run_lists[0]))  # warm-up, discarded
            for index, run_list in enumerate(case.run_lists):
                results[(case.org, index, label)] = solve(case.org, Problem(case.universe.cookbooks, run_list))
    set_worker("disable")  # leave the server as shipped

    print()
    header = f"{'org':<10} {'run list':>8}   {'stock worker':<24} {'depsolve worker':<24} {'same answer':<12} valid"
    print(header)
    for case in cases:
        for index, run_list in enumerate(case.run_lists):
            problem = Problem(case.universe.cookbooks, run_list)
            cells, solutions, valid = [], [], []
            for label in ("stock", "depsolve"):
                outcome = results[(case.org, index, label)]
                cells.append(f"HTTP {outcome['http']} in {outcome['seconds']:.3f}s")
                solutions.append(outcome.get("solution"))
                if outcome["status"] == "ok":
                    valid.append("yes" if not check_solution(problem, outcome["solution"]) else "NO")
                else:
                    valid.append("-")
            same = "-" if None in solutions else str(solutions[0] == solutions[1])
            print(f"{case.org:<10} {len(run_list):>8}   {cells[0]:<24} {cells[1]:<24} {same:<12} {'/'.join(valid)}")

    failures = {key: value for key, value in results.items() if value["status"] != "ok"}
    for (org, index, label), outcome in list(failures.items())[:4]:
        print(f"\n{org} run list {index} ({label}): HTTP {outcome['http']}: {outcome['body'][:600]}")


if __name__ == "__main__":
    main()
