"""Brute-force reference solver.

Exhaustive chronological backtracking with no learning. It exists to be obviously
correct, so the real engine can be checked against it on small problems; it is
exponential on anything adversarial and is not a performance baseline.
"""

from __future__ import annotations

from typing import Dict, List, Optional

from depsolver.model import Constraint, Problem, Version, parse_version


class SearchBudgetExceeded(Exception):
    """Raised when the search visits more nodes than `max_steps` allows."""


class BaselineSolver:
    def __init__(self, problem: Problem, max_steps: Optional[int] = None):
        environment = {name: Constraint.parse(raw) for name, raw in problem.environment.items()}
        # name -> [(version, raw version, parsed dependencies)], newest first
        self._candidates: Dict[str, List[tuple[Version, str, Dict[str, Constraint]]]] = {}
        for name, versions in problem.cookbooks.items():
            entries = []
            for raw_version, dependencies in versions.items():
                version = parse_version(raw_version)
                if name in environment and not environment[name].matches(version):
                    continue
                parsed = {dep: Constraint.parse(raw) for dep, raw in dependencies.items()}
                entries.append((version, raw_version, parsed))
            entries.sort(key=lambda entry: entry[0], reverse=True)
            self._candidates[name] = entries
        self._run_list = {name: [Constraint.parse(raw)] for name, raw in problem.run_list.items()}
        self._max_steps = max_steps
        self.steps = 0

    def solve(self) -> Optional[Dict[str, str]]:
        """Returns a valid assignment, or None if none exists."""
        self.steps = 0
        return self._search({}, {}, self._run_list)

    def _search(
        self,
        selected: Dict[str, Version],
        names: Dict[str, str],
        requirements: Dict[str, List[Constraint]],
    ) -> Optional[Dict[str, str]]:
        self.steps += 1
        if self._max_steps is not None and self.steps > self._max_steps:
            raise SearchBudgetExceeded(f"gave up after {self._max_steps} steps")

        package = next((name for name in sorted(requirements) if name not in selected), None)
        if package is None:
            return names

        for version, raw_version, dependencies in self._candidates.get(package, []):
            if not all(constraint.matches(version) for constraint in requirements[package]):
                continue
            if any(dep in selected and not constraint.matches(selected[dep]) for dep, constraint in dependencies.items()):
                continue

            next_requirements = dict(requirements)
            for dep, constraint in dependencies.items():
                next_requirements[dep] = next_requirements.get(dep, []) + [constraint]

            result = self._search({**selected, package: version}, {**names, package: raw_version}, next_requirements)
            if result is not None:
                return result

        return None
