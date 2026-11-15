# depsolve

A replacement dependency solver for Chef/CINC cookbooks, built on the [`pubgrub`](https://crates.io/crates/pubgrub) crate (the same PubGrub implementation uv uses for Python).

The goal is a server-side solver that stays fast on large cookbook universes, where CINC Server's current solver (`dep_selector` on Gecode) can hit its 5 second timeout, and that explains itself readably when no solution exists. PubGrub learns the cause of each conflict and jumps straight back to the decision that mattered, instead of searching for a globally best assignment.

## Layout

| Path | What it is |
|---|---|
| [rust/src/lib.rs](rust/src/lib.rs) | The engine: Chef versions and constraints, the solver, and a C ABI (`depsolve_solve` / `depsolve_free`) |
| [rust/src/bin/depsolve.rs](rust/src/bin/depsolve.rs) | CLI: JSON problem in (file or stdin), JSON outcome out |
| [src/depsolver/model.py](src/depsolver/model.py) | Python mirror of the problem model, plus `check_solution` |
| [src/depsolver/baseline.py](src/depsolver/baseline.py) | Brute-force reference solver, used only as a correctness oracle |
| [src/depsolver/native.py](src/depsolver/native.py) | ctypes binding to the Rust engine |
| [src/depsolver/benchmarks.py](src/depsolver/benchmarks.py) | Synthetic scenarios and timing harness |
| [src/depsolver/cinc_compare.py](src/depsolver/cinc_compare.py) | Runs the engine and CINC Server's own solver on the same problems |
| [docker/cinc-baseline/](docker/cinc-baseline/) | Image holding CINC Server's solver and the engine side by side |
| [docker/cinc-server/](docker/cinc-server/) | A real single-node CINC Server in a container |
| [cinc-patch/](cinc-patch/) | Replacement erchef depsolver worker and the script that switches it in and out |
| [src/depsolver/cinc_server.py](src/depsolver/cinc_server.py) | Uploads generated universes to the server and compares stock and patched workers |
| [ruby/solver_bridge.rb](ruby/solver_bridge.rb) | Ruby binding to the Rust engine (Fiddle) |

## Problem format

Every component speaks the same JSON:

```json
{
  "cookbooks":   { "app": { "1.1.0": { "lib": "~> 1.0" }, "1.0.0": {} },
                   "lib": { "1.0.0": {}, "1.5.0": {}, "2.0.0": {} } },
  "run_list":    { "app": "= 1.1.0" },
  "environment": { "lib": "< 2.0.0" }
}
```

- `cookbooks` is the universe. Each cookbook *version* has its own dependencies.
- `run_list` is what must be installed, with one constraint per cookbook.
- `environment` is optional. It restricts which versions are eligible without forcing the cookbook into the solution.

Versions are `x.y` or `x.y.z`, numeric only. Constraints are a single Chef operator and a version: `=`, `>`, `>=`, `<`, `<=`, `~>`. A bare version means `=`, and an empty string means any version. `~> 1.2` allows `>= 1.2.0, < 2.0.0`; `~> 1.2.3` allows `>= 1.2.3, < 1.3.0`.

The outcome is one of:

```json
{ "status": "ok", "solution": { "app": "1.1.0", "lib": "1.5.0" } }
{ "status": "unsolvable", "explanation": "Because ... , version solving failed." }
{ "status": "error", "message": "app 1.0.0 depends on lib: invalid constraint ..." }
```

`unsolvable` means the input was well formed but has no solution, including a dependency on a cookbook that is not in the universe. `error` means the input itself was malformed.

## Build and test

```bash
cargo build --release --manifest-path rust/Cargo.toml
cargo test --release --manifest-path rust/Cargo.toml
python3 -m pytest -q
```

The Python tests that need the Rust library are skipped if it has not been built, and the CINC comparison test is skipped unless the Docker image exists. The tests include a differential fuzz: 3,000 random problems where the engine must agree with the brute-force baseline on solvability, and every solution it returns is independently validated.

## Comparison with CINC Server's solver

The target is the server-side depsolver. In CINC Server 15.10.127, erchef hands each solve to a Ruby worker (`depselector.rb`) that uses the `dep_selector` gem on Gecode, with a 5 second timeout by default.

[docker/cinc-baseline/Dockerfile](docker/cinc-baseline/Dockerfile) unpacks the official CINC Server package (nothing is installed or started) and drives that same gem bundle and Gecode build through [cinc_solve.rb](docker/cinc-baseline/cinc_solve.rb), which builds the graph the way `depselector.rb` does. The Rust engine is compiled into the same image, so both run in one environment.

```bash
docker build -f docker/cinc-baseline/Dockerfile -t depsolve-cinc-baseline .
PYTHONPATH=src python3 -m depsolver.cinc_compare
```

CINC Server is published for amd64 only. On an arm64 host the image runs under emulation, which slows both solvers alike.

### Agreement

On 2,000 small random problems (arm64 Mac, emulated):

| | |
|---|---|
| Both found a solution | 575 |
| ...with identical versions | 557 |
| Both found no solution | 1,425 |
| Disagreements on solvability | 0 |
| Invalid solutions from either | 0 |
| Median time per problem | CINC 0.30ms, Rust 0.014ms |

The 18 differing solutions are all valid. They differ because the two solvers prefer different things: `dep_selector` searches for a globally best assignment and will take an older run-list cookbook to pull in fewer or newer dependencies, while the engine takes the newest version of each cookbook in the order it decides them.

### Timing

| Scenario | CINC Server | Rust engine |
|---|---|---|
| late pin, 5 cookbooks × 10 versions | 0.0011s | 0.0005s |
| late pin, 8 × 20 | 0.0050s | 0.0031s |
| late pin, 50 × 50 | 0.088s | 0.096s |
| random universe, 30 cookbooks × 9 versions | 0.053s | 0.0007s |
| random universe, 200 × 12 | timeout (5s) | 0.0045s |
| random universe, 1000 × 12 | timeout (5s) | 0.093s |

Two things follow. The late-pin chain is not a problem for CINC's solver: Gecode's constraint propagation handles it as well as PubGrub does. Where CINC's solver falls over is large, tangled universes, where its search for the best assignment runs out of time while the engine returns a valid answer in milliseconds. These universes are synthetic, so this shows where the difference lies, not how large it is on real data.

The brute-force baseline can be timed against the engine on the host with `PYTHONPATH=src python3 -m depsolver.benchmarks`. It is a correctness oracle, not a performance reference.

## Patching CINC Server

erchef does not solve dependencies itself. It keeps a pool of Ruby worker processes (`priv/depselector_rb/depselector.rb` in the `chef_objects` application) and sends each solve to one over an Erlang port. That worker is the patch point: [cinc-patch/depselector.rb](cinc-patch/depselector.rb) speaks the same port protocol and returns the same reply shapes, but calls the Rust engine instead of `dep_selector`. No Erlang code changes.

```bash
depsolve-patch enable    # install the worker, restart erchef
depsolve-patch disable   # restore the stock worker, restart erchef
depsolve-patch status
```

The script expects `depselector.rb` and `libdepsolve_rust.so` in `/opt/depsolve/` on the server. The stock worker is kept beside the replacement as `depselector.rb.stock`.

### End-to-end test

```bash
docker build -t depsolve-cinc-server docker/cinc-server
docker run -d --name depsolve-cinc-server --privileged --hostname cinc.test \
    -p 127.0.0.1:8643:443 depsolve-cinc-server      # wait for "cinc-server ready" in the logs
PYTHONPATH=src python3 -m depsolver.cinc_server
```

This uploads three generated universes into three organisations, then sends the same run lists to the server's depsolve endpoint (`POST /organizations/ORG/environments/_default/cookbook_versions`) with the stock worker and with the patched one. Cookbook versions are uploaded as metadata only (name, version, dependencies), which is all the depsolver reads. It finishes with the stock worker restored.

Results on CINC Server 15.10.127 (arm64 Mac, emulated), four run lists per organisation:

| Organisation | Cookbook versions | Stock worker | Patched worker |
|---|---|---|---|
| layered1 | 1,881 | 412 timeout after 5.3s, all four | 200 in 0.26–0.28s, all four |
| layered2 | 1,866 | 412 timeout after 5.3–5.9s, all four | 200 in 0.26–0.29s, all four |
| random200 | 2,400 | 412 timeout after 5.4s, all four | 200 in 0.26–0.28s, all four |

Every answer from the patched worker passes `check_solution`. Most of its 0.27s is the HTTP round trip and the server fetching the universe; the solve itself takes about 10ms. Unsolvable run lists come back as the usual 412, with PubGrub's explanation in `message`.

The stock failures are not only the 5 second limit. Run directly with a 60 second budget, `dep_selector` still does not finish on the layered1 universe. On smaller universes from the same generator it does finish, with time growing steeply:

| Layered universe | CINC solver | Rust engine |
|---|---|---|
| 182 versions | 0.02s | 0.0006s |
| 326 versions | 0.03s | 0.0009s |
| 630 versions | 0.09s | 0.0013s |
| 1,146 versions | 0.52s | 0.0023s |
| 1,881 versions | no answer in 60s | 0.012s |

Production servers with thousands of cookbook versions do solve with the stock worker, so the generated universes are evidently harder for it than typical real ones. Treat these numbers as showing the failure mode, not its frequency.

### Known gaps in the patch

- **No timeout.** The engine has no time limit of its own. erchef still kills a worker that takes more than twice `depsolver_timeout`, but the engine should enforce the limit itself (the `pubgrub` crate has a cancellation hook).
- **Different answers.** Where both solvers succeed on the layered universes, they usually choose different valid versions (see Agreement above).
- **Error detail.** On failure erchef's response has `unsatisfiable_run_list_item`, `non_existent_cookbooks` and `most_constrained_cookbooks`. The patched worker fills in `message` only and leaves the others empty. A run list constrained to no versions is reported as "no solution" rather than "invalid constraints".
- **Still a Ruby process.** The worker keeps the Ruby runtime and the JSON hop. Calling the engine from Erlang directly (a NIF) would remove both, at the cost of changing and rebuilding erchef.
- **Not run against the server's own API test suite** (oc-chef-pedant has depsolver specs).

## Not done yet

- **Real data.** All universes are generated. A real organisation's cookbooks, run lists and environment constraints would show how often the stock solver actually struggles and how often answers differ.
- **Version preference.** About 3% of solvable fuzz cases get a different (valid) answer from CINC's solver. A drop-in replacement has to decide whether to reproduce CINC's preference or accept the difference. The run list is also an unordered map here, while erchef passes an ordered list.
- **Environments.** The end-to-end test only uses the `_default` environment. Environment constraints are covered by the unit and fuzz tests, not through the server.
