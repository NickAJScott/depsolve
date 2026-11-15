"""Checks the Rust engine against CINC Server's own solver. Needs Docker and the
comparison image (see docker/cinc-baseline/Dockerfile); skipped otherwise."""

import random
import shutil
import subprocess

import pytest

from depsolver.benchmarks import random_small_problem
from depsolver.cinc_compare import IMAGE, compare, summarize_agreement


def _image_available() -> bool:
    if shutil.which("docker") is None:
        return False
    return subprocess.run(["docker", "image", "inspect", IMAGE], capture_output=True, check=False).returncode == 0


@pytest.mark.skipif(not _image_available(), reason=f"Docker image {IMAGE} not built")
def test_rust_and_cinc_agree_on_solvability():
    rng = random.Random(7)
    problems = [random_small_problem(rng) for _ in range(300)]
    summary = summarize_agreement(problems, compare(problems))

    assert summary["disagreements"] == []
    assert summary["invalid_solutions"] == []
    assert summary["cinc_timeouts"] == 0
    assert summary["both_solved"] > 30 and summary["both_unsolvable"] > 30
