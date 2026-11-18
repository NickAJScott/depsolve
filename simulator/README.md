# Depsolver pool simulator

A discrete-event simulation of erchef's depsolver worker pool under a chef-client fleet. It runs a day of converges in about a second, so you can see when slow solves turn into 503 spikes and which settings move the limit, without running real solves or converges.

```bash
cd simulator
cargo build --release
./target/release/simulate --timeline                 # one scenario, with a failure timeline
./target/release/simulate params                     # every parameter and metric
```

The defaults are chef-client's and erchef's own: a daemon every 30 minutes with 5 minutes of splay, 5 retries, and one frontend with 5 workers, no pooler wait and a 5 s solve timeout, serving 10,000 nodes. Override any of them with `--name value`. Times accept `ms`, `s`, `m`, `h` and `d`.

## What it models

Each rule comes from the erchef 15.10.127 or chef-client 18.10 source:

| Rule | Source |
|---|---|
| Each frontend has its own pool of `depsolver_worker_count` Ruby workers | `oc_erchef.config.erb`: pooler `max_count`/`init_count` |
| A request waits up to `depsolver_pooler_timeout` for a free worker, then gets 503 "Dependency solver overloaded" (the default is 0: no waiting) | `chef_depsolver_worker:solve_dependencies`, `chef_wm_depsolver` |
| A solve that reaches `depsolver_timeout` gets 412, and its worker is discarded and replaced, not reused | `chef_depsolver_worker`: `return_member(..., fail)` |
| chef-client retries any 5xx `http_retry_count` times (default 5), sleeping `1 + 2^n + rand(2^n)` seconds after the n-th failure. It does not retry 412 | `Chef::HTTP#retrying_http_errors` |
| A daemon sleeps `interval + rand(splay)` after each run | chef-client daemon mode |

Two kinds of node are simulated: ordinary run lists, and a share (`bad_fraction`) of slow ones. Each kind gets its own log-normal solve time (`fast`, `slow` and their `_sigma`).

Two things are assumptions rather than measurements:

- **Worker restart time** (`restart`, 2 s): how long a replacement Ruby worker takes to start and load dep_selector.
- **Converge length** (`converge`, 60 s): only affects when a daemon's next run starts.

The model also leaves out everything that isn't the depsolver pool: the database, nginx, the load balancer (requests are spread randomly over frontends), and any other erchef endpoint.

## Commands

| Command | What it does |
|---|---|
| `simulate [run] [--seeds N] [--timeline]` | One scenario: failure rates, spikes, pool use and a capacity estimate |
| `simulate sweep --vary p=a,b,c [--vary q=...] [--metric m]` | One `--vary` gives a row per value with the headline metrics. Two give a matrix of one metric |
| `simulate limit --find p --vary q=... [--metric m --max x]` | For each row, bisects for the largest `p` (for example `nodes` or `bad_fraction`) that keeps the metric within `x` |
| `simulate report` | The standard set of sweeps, as markdown |

The main metrics:

- **`collateral`:** ordinary converges that failed because the pool refused every retry. This is the pool's fault, not the node's own solve.
- **`typical_503`:** ordinary requests answered 503. It includes requests that later succeeded on a retry, so it is what an error-rate dashboard shows.
- **`worst_minute`:** the 503 share in the worst minute.
- **`spikes`:** separate bursts per day of minutes in which at least `spike_threshold` (10%) of ordinary requests got 503.
- **`typical_timeouts`:** ordinary requests that failed on their own timeout (412). These are not pool failures.

`limit` defaults to "no spike minute in any of 3 seeds" (`--metric worst_minute --max 10 --agg max`).

`cargo test --release` checks the model against cases worked out by hand and against Erlang-B, the queueing formula for random arrivals at a pool with no queue.

## What it shows

General results, true for any fleet and pool size. Run `simulate report`, or `sweep` and `limit` with your own settings, to get the numbers for a particular deployment.

- **Capacity is a fixed budget.** The pool can absorb `workers × frontends ÷ requests per second` worker-seconds per request on average. A request uses its solve time; a timed-out one also uses its worker's replacement time. `simulate run` prints this budget and how much of it a scenario uses.
- **The failure is a cliff, not a slope.** Below the budget nothing fails. Within a few percent of it, bursts of arrivals push the pool over briefly and retries add load at the worst moment, so spikes come and go. In a steady simulated fleet that band is narrow; a real fleet's day-to-day variation widens it into an occasional spike. Past the budget the pool stays saturated and converges fail continuously.
- **Splay matters only when nodes are aligned.** In a fleet already spread across its interval, splay changes almost nothing. When a group of nodes converges in step (after a shared deploy, a restart, or cron), more splay spreads the burst out, and splay as long as the interval removes it.
- **The limit scales with 1 ÷ average solve time.** Halving the average solve doubles the fleet a pool can carry. Solves that always time out cost a worker for the whole timeout plus a replacement, and that cost weighs more as the ordinary solves get faster.
- **Workers help only with the CPU to run them.** A worker runs one single-threaded solve at a time. More workers than free cores makes every solve slower and adds little capacity.
- **The pooler wait helps near the limit, not past it.** Going from no wait to a few tens of seconds absorbs most bursts. Longer waits add little real capacity: daemons that wait longer start their next interval later, so the fleet converges less often. They also run into chef-client's `rest_timeout` (300 s) and any proxy timeouts.
- **The client interval moves the limit directly.** Requests per second scale with `1 ÷ interval`, so a longer interval raises the limit in proportion, at the cost of slower rollouts.
- **The timeout must sit well above ordinary solves.** A timeout close to the average solve kills a large share of ordinary solves outright.
- **Nodes on Policyfiles never use this pool.** They take their cookbook versions from a lock file instead of asking the server's depsolver.
