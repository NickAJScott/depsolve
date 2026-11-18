# depsolve

A faster dependency solver for Chef Infra Server and CINC Server, with a cookbook that installs it on a live server.

When a node asks the server which cookbook versions to use, the stock solver (`dep_selector` on Gecode) searches for a globally best assignment. On large or tangled cookbook universes that search can run out of time, and the node gets `412 unable to solve dependencies in alotted time`. depsolve replaces the solver with [PubGrub](https://crates.io/crates/pubgrub), the algorithm uv uses for Python packages. It finds a valid, newest-leaning answer in milliseconds and explains conflicts in plain sentences.

## How it works

The server (erchef) does not solve dependencies itself. It keeps a pool of Ruby worker processes and sends each solve to one of them. The worker is a single file:

```
<install root>/embedded/service/opscode-erchef/lib/chef_objects-<version>/priv/depselector_rb/depselector.rb
```

depsolve swaps that file for [its own worker](cookbook/depsolve/files/default/depselector.rb), which speaks the same protocol to erchef but calls the depsolve engine, a small shared library written in Rust. Nothing else on the server changes: no Erlang code, no configuration, no database.

The servers need nothing extra installed. The engine is one self-contained file (`libdepsolve_rust.so`, about 1 MB) that the cookbook delivers, and the worker runs on the Ruby the server already embeds. Rust is only needed to build the library, and `build.sh` does that inside Docker.

## Repository layout

| Path | What it is |
|---|---|
| [rust/](rust/) | The engine: Chef versions and constraints, the solver, a C interface, and tests |
| [cookbook/depsolve/](cookbook/depsolve/) | Cookbook that installs (`depsolve`) or removes (`depsolve::remove`) the worker |
| [build.sh](build.sh) | Builds the engine for Linux in Docker and places it in the cookbook |
| [test/](test/) | End-to-end test against a real CINC Server in Docker |
| [simulator/](simulator/) | Simulates the server's depsolver worker pool under fleet load, to see when slow solves cause 503 spikes ([README](simulator/README.md)) |

## Deploying

### 1. Build the engine

```bash
./build.sh                                  # x86_64 servers
DEPSOLVE_PLATFORM=linux/arm64 ./build.sh    # aarch64 servers
```

This needs Docker and nothing else. It runs the engine's tests, then writes `cookbook/depsolve/files/default/libdepsolve_rust.so`. The library is built against glibc 2.28, so one build should load on RHEL, Rocky and Alma 8 or later, Amazon Linux 2023, Ubuntu 20.04 or later, Debian 11 or later, and SLES 15. It has been loaded on Ubuntu 24.04; the cookbook's self-test will catch a server where it does not load.

The library is not committed to git. Build it before uploading the cookbook, or host it somewhere and point the cookbook at it (see `library_url` below).

### 2. Run the cookbook on each server

The cookbook has to run on the Chef Infra Server machines themselves, on every node that runs erchef (each frontend in a tiered or HA setup).

If the servers are already managed by Chef, upload the cookbook and add `recipe[depsolve]` to their run lists:

```bash
knife cookbook upload depsolve --cookbook-path cookbook
```

Otherwise, copy `cookbook/` to the server and run it once with the client the server embeds:

```bash
# Chef Infra Server
sudo /opt/opscode/embedded/bin/chef-client --local-mode --chef-license accept \
    --config-option cookbook_path=/path/to/cookbook --override-runlist 'recipe[depsolve]'

# CINC Server
sudo /opt/cinc-project/embedded/bin/cinc-client --local-mode \
    --config-option cookbook_path=/path/to/cookbook --override-runlist 'recipe[depsolve]'
```

The recipe does the following, in this order:

1. Finds the server installation (`/opt/opscode` or `/opt/cinc-project`) and its worker file.
2. Refuses to continue if the stock worker is not one it recognises (see below).
3. Stages the engine and the new worker in `/opt/depsolve`.
4. Runs a self-test: the staged worker loads the engine under the server's own Ruby and checks its replies for a solvable request, an unsolvable one, an invalid run list and a timeout. If this fails, the run stops and the server is untouched.
5. Copies the stock worker to `depselector.rb.stock` beside the original.
6. Installs the new worker and restarts erchef (`chef-server-ctl restart opscode-erchef`), then waits for it to answer.

The restart interrupts the API on that server for a few seconds. In a multi-frontend setup, converge one frontend at a time.

Later runs change nothing unless the engine or worker has changed.

### The stock worker check

The recipe only replaces a stock worker whose SHA-256 it knows, because a different stock worker may mean that server version talks to its workers differently. The list ships with one entry, the worker from CINC Server 15.10.127, which is the only version this has been tested on.

On any other version the first run stops with a message that includes the checksum it found. Before overriding it, run the test rig against that version if you can (set `CINC_SERVER_VERSION` and `CINC_SERVER_SHA256` in [test/cinc-server/Dockerfile](test/cinc-server/Dockerfile)), or compare your server's `depselector.rb` with the one in that image. Then either add the checksum to `node['depsolve']['known_stock_checksums']` or set `node['depsolve']['allow_unknown_stock_worker']` to `true`.

### Attributes

| Attribute | Default | Purpose |
|---|---|---|
| `['depsolve']['install_root']` | detected | Server install directory, if it is not `/opt/opscode` or `/opt/cinc-project` |
| `['depsolve']['library_url']` | `nil` | Download the engine from here instead of shipping it in the cookbook |
| `['depsolve']['library_checksum']` | `nil` | SHA-256 of the engine; required with `library_url` |
| `['depsolve']['known_stock_checksums']` | CINC 15.10.127 | Stock workers the recipe will replace |
| `['depsolve']['allow_unknown_stock_worker']` | `false` | Replace an unrecognised stock worker anyway |
| `['depsolve']['restart_command']` | `chef-server-ctl restart opscode-erchef` | How to restart erchef |
| `['depsolve']['erchef_port']` | `8000` | Port polled after the restart |

### Checking it worked

```bash
grep -c libdepsolve_rust /opt/opscode/embedded/service/opscode-erchef/lib/chef_objects-*/priv/depselector_rb/depselector.rb   # 1 when installed
sudo chef-server-ctl test --focus depsolver    # the server's own depsolver API tests
```

With depsolve installed, three of the twenty depsolver tests fail. That is expected; see Differences.

### Rolling back

Run `recipe[depsolve::remove]` the same way. It restores the stock worker, restarts erchef and deletes `/opt/depsolve`.

By hand, if needed:

```bash
cd /opt/opscode/embedded/service/opscode-erchef/lib/chef_objects-*/priv/depselector_rb
sudo cp -p depselector.rb.stock depselector.rb
sudo chef-server-ctl restart opscode-erchef
```

### Server upgrades

A server package upgrade installs a new `chef_objects-<version>` directory with a stock worker in it, so the server goes back to the stock solver until the cookbook runs again. If the new version's stock worker has a different checksum, the recipe stops and asks you to confirm it, as above.

## Differences from the stock solver

- **Which versions are chosen.** Both solvers return valid solutions, but not always the same one.
  - The stock solver counts how many cookbooks are at their very latest version, run-list cookbooks first, and searches for the assignment with the highest count. A cookbook one release behind scores the same as one that is years old, and when several assignments score the same it returns whichever it found.
  - depsolve decides run-list cookbooks first, in run-list order, giving each the newest version that works with what has already been decided, then does the same for their dependencies. An earlier run-list item therefore gets first pick.
  - In testing the two chose identical versions in 560 of 575 solvable small random problems (97%) and 15 of 31 larger layered ones. Scored by the stock solver's own count, the differing answers were a tie in 19 cases, better from the stock solver in 11 and better from depsolve in 1. In the ties depsolve's run-list cookbooks were often newer.
  - Expect some nodes to resolve to different cookbook versions after the switch.
- **The `message` text when there is no solution.** It is PubGrub's explanation, for example `Because foo depends on bar >2.0.0 and the run list depends on foo, the run list is forbidden.` Three of the server's twenty depsolver tests fail for this reason alone: they assert the stock message word for word.
- **Which cookbooks `most_constrained_cookbooks` names.** depsolve lists the cookbooks for which some requirement matched no available version. On involved conflicts that may not be the same set the stock solver would name.
- **How long a timeout takes.** The stock solver can run for up to twice `depsolver_timeout` before giving up. depsolve stops at the configured time, counted from when the worker receives the request.

## Evidence

Measured on CINC Server 15.10.127 in Docker, on an arm64 Mac under amd64 emulation.

Against a real server loaded with generated cookbook universes, four run lists per organisation:

| Cookbook versions in the organisation | Stock worker | depsolve worker |
|---|---|---|
| 1,881 (layered: libraries, applications, roles) | 412 timeout after 5.3s, all four | 200 in 0.26–0.28s, all four |
| 1,866 (layered) | 412 timeout after 5.3–5.9s, all four | 200 in 0.26–0.29s, all four |
| 2,400 (random) | 412 timeout after 5.4s, all four | 200 in 0.26–0.29s, all four |

One solve in a fresh worker process (the server's Ruby with the solver loaded), same generator at increasing sizes, run list of six role cookbooks:

| Cookbook versions | Stock: time | Stock: CPU | Stock: peak memory | depsolve: time | depsolve: CPU | depsolve: peak memory |
|---|---|---|---|---|---|---|
| 182 | 0.010s | 0.010s | 48 MB | 0.006s | 0.006s | 32 MB |
| 630 | 0.091s | 0.091s | 56 MB | 0.008s | 0.008s | 33 MB |
| 1,146 | 3.4s | 3.4s | 68 MB | 0.009s | 0.009s | 34 MB |
| 1,881 | no answer in 60s | 60s | 88 MB | 0.014s | 0.014s | 36 MB |

- **CPU** is processor time used by the solve. Both solvers are single-threaded, so a solve occupies one core for its whole duration; a stock solve that times out holds a core until it gives up.
- **Peak memory** is the worker process's highest resident size. Before solving anything a stock worker sits at 46 MB and a depsolve worker at 31 MB, and erchef keeps a pool of workers (five by default).
- depsolve's time includes passing the problem from Ruby to the engine and back; the engine alone takes 0.0006s to 0.012s on these four.
- The stock solver's time depends heavily on the run list: the 1,146-version universe took 3.4s with six run-list items and 60s with a different four.

On 2,000 small random problems the two solvers agreed on whether a solution exists every time, and every solution from either was valid.

The generated universes are evidently harder for the stock solver than many real ones, so these numbers show where it fails, not how often a given server will hit it. The scripts that produced them are in the repository's first commit.

## Limits

- **Tested on CINC Server 15.10.127 only.** Chef Infra Server is built from the same source and the cookbook detects its paths, but it has not been run there.
- **Standalone and tiered installs only.** Servers deployed through Chef Automate (Habitat packages) lay files out differently and are not handled.
- **Generated data only.** No real organisation's cookbooks have been through it.
- **Timeouts are hard to provoke.** With `depsolver_timeout` lowered to 1 ms a real server returned the standard timeout response and recovered. The engine is fast enough that no test universe here reaches a realistic limit, so behaviour under a genuinely long solve is covered only by the engine's own tests.

## Development

```bash
cd rust && cargo test --release    # unit tests plus a differential test against brute force
./test/run.sh                      # end-to-end against CINC Server in Docker
```

`test/run.sh` builds the engine, starts a CINC Server container (about ten minutes the first time), converges the cookbook twice, runs the server's depsolver API tests, removes the cookbook and runs them again. With depsolve installed it requires that the only failures are the three wording tests and that each response differs from the expected one in the `message` alone; after removal it requires none.

`rust/src/bin/depsolve.rs` is a command-line front end for trying the engine directly. It reads a problem as JSON:

```json
{
  "cookbooks":   { "app": { "1.1.0": { "lib": "~> 1.0" }, "1.0.0": {} },
                   "lib": { "1.0.0": {}, "1.5.0": {}, "2.0.0": {} } },
  "run_list":    [["app", "= 1.1.0"]],
  "environment": { "lib": "< 2.0.0" },
  "timeout_ms":  5000
}
```

and prints an outcome whose `status` is `ok`, `unsolvable`, `invalid_run_list`, `timeout` or `error`. `run_list` may also be written as an object, `{"app": "= 1.1.0"}`, when its order does not matter.
