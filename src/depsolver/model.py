"""Chef/CINC cookbook dependency problem model shared by every solver.

Mirrors the JSON accepted by the Rust engine (see rust/src/lib.rs):

    {
      "cookbooks":   {"app": {"1.1.0": {"lib": "~> 1.0"}, "1.0.0": {}}},
      "run_list":    {"app": "= 1.1.0"},
      "environment": {"lib": "< 2.0.0"},
      "timeout_ms": 5000
    }
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, Optional, Tuple

Version = Tuple[int, int, int]

# name -> version -> {dependency name: constraint}
Universe = Dict[str, Dict[str, Dict[str, str]]]

# Two-character operators first so ">=" is not read as ">".
_OPERATORS = ("~>", ">=", "<=", "=", ">", "<")


def _parse_version_with_precision(raw: str) -> tuple[Version, int]:
    pieces = raw.strip().split(".")
    if not 2 <= len(pieces) <= 3 or not all(piece.isascii() and piece.isdigit() for piece in pieces):
        raise ValueError(f"invalid cookbook version {raw!r}: expected x.y or x.y.z")
    numbers = [int(piece) for piece in pieces] + [0] * (3 - len(pieces))
    return (numbers[0], numbers[1], numbers[2]), len(pieces)


def parse_version(raw: str) -> Version:
    """Parses a Chef cookbook version (`x.y` or `x.y.z`, numeric only)."""
    return _parse_version_with_precision(raw)[0]


def format_version(version: Version) -> str:
    return ".".join(str(part) for part in version)


@dataclass(frozen=True)
class Constraint:
    """A single Chef version constraint such as `~> 1.2` or `>= 1.0.0`."""

    operator: str
    version: Version
    precision: int  # how many components were written; `~>` depends on it

    @classmethod
    def parse(cls, raw: str) -> "Constraint":
        text = raw.strip()
        if not text:
            return cls(">=", (0, 0, 0), 3)
        operator, rest = "=", text
        for candidate in _OPERATORS:
            if text.startswith(candidate):
                operator, rest = candidate, text[len(candidate):]
                break
        try:
            version, precision = _parse_version_with_precision(rest)
        except ValueError as err:
            raise ValueError(f"invalid constraint {raw!r}: {err}") from None
        return cls(operator, version, precision)

    def matches(self, version: Version) -> bool:
        if self.operator == "=":
            return version == self.version
        if self.operator == ">":
            return version > self.version
        if self.operator == ">=":
            return version >= self.version
        if self.operator == "<":
            return version < self.version
        if self.operator == "<=":
            return version <= self.version
        # "~> 1.2" allows < 2.0.0; "~> 1.2.3" allows < 1.3.0.
        major, minor, _ = self.version
        upper = (major + 1, 0, 0) if self.precision == 2 else (major, minor + 1, 0)
        return self.version <= version < upper


@dataclass(frozen=True)
class Problem:
    cookbooks: Universe
    run_list: Dict[str, str]
    environment: Dict[str, str] = field(default_factory=dict)
    timeout_ms: Optional[int] = None  # only the Rust engine honours this

    def to_dict(self) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "cookbooks": self.cookbooks,
            "run_list": self.run_list,
            "environment": self.environment,
        }
        if self.timeout_ms is not None:
            payload["timeout_ms"] = self.timeout_ms
        return payload

    @classmethod
    def from_dict(cls, payload: Dict[str, Any]) -> "Problem":
        return cls(
            cookbooks=payload["cookbooks"],
            run_list=payload["run_list"],
            environment=payload.get("environment", {}),
            timeout_ms=payload.get("timeout_ms"),
        )


def check_solution(problem: Problem, solution: Dict[str, str]) -> list[str]:
    """Returns every way `solution` violates `problem`; an empty list means it is valid."""
    problems: list[str] = []
    chosen = {name: parse_version(version) for name, version in solution.items()}

    for name, raw in problem.run_list.items():
        if name not in chosen:
            problems.append(f"run list item {name} is missing")
        elif not Constraint.parse(raw).matches(chosen[name]):
            problems.append(f"run list wants {name} {raw}, got {solution[name]}")

    for name, version in solution.items():
        # Compare parsed versions so "1.2" in the universe matches "1.2.0" in a solution.
        releases = {parse_version(raw): deps for raw, deps in problem.cookbooks.get(name, {}).items()}
        if chosen[name] not in releases:
            problems.append(f"{name} {version} does not exist")
            continue
        if name in problem.environment and not Constraint.parse(problem.environment[name]).matches(chosen[name]):
            problems.append(f"environment wants {name} {problem.environment[name]}, got {version}")
        for dependency, raw in releases[chosen[name]].items():
            if dependency not in chosen:
                problems.append(f"{name} {version} needs {dependency}, which is missing")
            elif not Constraint.parse(raw).matches(chosen[dependency]):
                problems.append(f"{name} {version} wants {dependency} {raw}, got {solution[dependency]}")

    return problems
