"""ctypes binding to the Rust engine (rust/src/lib.rs).

Build the library first: cargo build --release --manifest-path rust/Cargo.toml
Set DEPSOLVE_LIB to point at a library in a non-default location.
"""

from __future__ import annotations

import ctypes
import json
import os
from functools import lru_cache
from pathlib import Path
from typing import Any, Dict, Optional

from depsolver.model import Problem

_RUST_TARGET = Path(__file__).resolve().parents[2] / "rust" / "target"
_LIBRARY_NAMES = ("libdepsolve_rust.dylib", "libdepsolve_rust.so", "depsolve_rust.dll")


def library_path() -> Optional[Path]:
    """Path of the compiled Rust library, or None if it has not been built."""
    override = os.environ.get("DEPSOLVE_LIB")
    if override:
        return Path(override)
    for profile in ("release", "debug"):
        for name in _LIBRARY_NAMES:
            candidate = _RUST_TARGET / profile / name
            if candidate.exists():
                return candidate
    return None


@lru_cache(maxsize=1)
def _library() -> ctypes.CDLL:
    path = library_path()
    if path is None:
        raise FileNotFoundError(
            "Rust solver library not found; build it with "
            "`cargo build --release --manifest-path rust/Cargo.toml`"
        )
    lib = ctypes.CDLL(str(path))
    lib.depsolve_solve.argtypes = [ctypes.c_char_p]
    lib.depsolve_solve.restype = ctypes.c_void_p
    lib.depsolve_free.argtypes = [ctypes.c_void_p]
    lib.depsolve_free.restype = None
    return lib


# Statuses that mean "well-formed problem, no solution exists".
NO_SOLUTION_STATUSES = frozenset({"unsolvable", "invalid_run_list"})


def solve(problem: Problem) -> Dict[str, Any]:
    """Solves `problem` with the Rust engine.

    Returns one of:
        {"status": "ok", "solution": {name: version}}
        {"status": "unsolvable", "explanation": str, "non_existent_cookbooks": [name]}
        {"status": "invalid_run_list", "non_existent_cookbooks": [name], "no_versions": [str]}
        {"status": "timeout", "message": str}
        {"status": "error", "message": str}
    """
    lib = _library()
    pointer = lib.depsolve_solve(json.dumps(problem.to_dict()).encode())
    try:
        return json.loads(ctypes.string_at(pointer))
    finally:
        lib.depsolve_free(pointer)
