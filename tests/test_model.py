import pytest

from depsolver.model import Constraint, Problem, check_solution, parse_version


def test_versions_parse_and_order_numerically():
    assert parse_version("1.2") == parse_version("1.2.0")
    assert parse_version("1.10.0") > parse_version("1.9.0")


@pytest.mark.parametrize("bad", ["1", "1.2.3.4", "1.x", "", "1..2", "v1.2.3"])
def test_malformed_versions_are_rejected(bad):
    with pytest.raises(ValueError):
        parse_version(bad)


@pytest.mark.parametrize(
    "constraint, version, expected",
    [
        ("= 1.2.3", "1.2.3", True),
        ("1.2.3", "1.2.4", False),
        ("> 1.2.3", "1.2.3", False),
        ("> 1.2.3", "1.2.4", True),
        (">= 1.2.3", "1.2.3", True),
        ("< 2.0", "1.9.9", True),
        ("< 2.0", "2.0.0", False),
        ("<= 2.0", "2.0.0", True),
        (">=1.0.0", "0.9.0", False),
        ("", "0.0.1", True),
        # Chef's pessimistic operator: the last written component may grow.
        ("~> 1.2", "1.1.9", False),
        ("~> 1.2", "1.2.0", True),
        ("~> 1.2", "1.9.9", True),
        ("~> 1.2", "2.0.0", False),
        ("~> 1.2.3", "1.2.2", False),
        ("~> 1.2.3", "1.2.99", True),
        ("~> 1.2.3", "1.3.0", False),
    ],
)
def test_constraint_matching(constraint, version, expected):
    assert Constraint.parse(constraint).matches(parse_version(version)) is expected


@pytest.mark.parametrize("bad", ["!= 1.0.0", ">= 1.0.0, < 2.0.0", "~> 1", "latest"])
def test_malformed_constraints_are_rejected(bad):
    with pytest.raises(ValueError):
        Constraint.parse(bad)


def test_problem_round_trips_through_dict():
    problem = Problem({"app": {"1.0.0": {"lib": "~> 1.0"}}, "lib": {"1.0.0": {}}}, {"app": ""}, {"lib": "< 2.0"})
    assert Problem.from_dict(problem.to_dict()) == problem


def test_check_solution_reports_each_kind_of_violation():
    problem = Problem(
        cookbooks={"app": {"1.0": {"lib": "~> 1.0"}}, "lib": {"1.0.0": {}, "2.0.0": {}}, "other": {"1.0.0": {}}},
        run_list={"app": "= 1.0.0"},
        environment={"other": "< 1.0.0"},
    )
    assert check_solution(problem, {"app": "1.0.0", "lib": "1.0.0"}) == []
    assert any("missing" in issue for issue in check_solution(problem, {"lib": "1.0.0"}))
    assert any("needs lib" in issue for issue in check_solution(problem, {"app": "1.0.0"}))
    assert any("wants lib" in issue for issue in check_solution(problem, {"app": "1.0.0", "lib": "2.0.0"}))
    assert any("does not exist" in issue for issue in check_solution(problem, {"app": "1.0.0", "lib": "1.5.0"}))
    assert any(
        "environment" in issue for issue in check_solution(problem, {"app": "1.0.0", "lib": "1.0.0", "other": "1.0.0"})
    )
