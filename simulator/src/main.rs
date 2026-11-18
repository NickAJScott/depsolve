//! Discrete-event simulator of erchef's depsolver worker pool under chef-client load.
//!
//! It answers one question: why do a few slow solves occasionally turn into a spike of
//! errors on healthy nodes, and which settings make that better or worse?
//!
//! The model follows erchef's code (see README.md for the sources of each rule):
//!
//! * Every converge makes one depsolve request. It needs a free worker from a small pool
//!   (default 5). With `pooler_wait` 0 (erchef's default) there is no queue: a request that
//!   finds every worker busy is answered 503 at once.
//! * A solve that reaches the timeout is answered 412, and its worker is thrown away and
//!   replaced, so it is unavailable for `restart` seconds on top of the time it spent solving.
//! * chef-client retries 503 (any 5xx) with exponential backoff, which adds load exactly when
//!   the pool is full. It does not retry 412.
//! * Nodes converge on an interval plus random splay, either as a daemon (the next run is
//!   `interval + rand(splay)` after the previous one finished) or on a timer (fixed slots with a
//!   random delay up to `splay`).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Mutex;

// ---------------------------------------------------------------------------------------
// Random numbers
// ---------------------------------------------------------------------------------------

/// xoshiro256** seeded through splitmix64.
struct Rng {
    s: [u64; 4],
}

impl Rng {
    fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Rng { s: [next(), next(), next(), next()] }
    }

    fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn normal(&mut self) -> f64 {
        let u1 = 1.0 - self.unit(); // (0, 1]
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    fn lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        if sigma == 0.0 {
            median
        } else {
            median * (sigma * self.normal()).exp()
        }
    }
}

// ---------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// Next run starts `interval + rand(splay)` after the previous one finished.
    Daemon,
    /// Runs sit in fixed `interval` slots, each delayed by `rand(splay)` (cron, systemd timer).
    Timer,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Start {
    /// Nodes are already spread across the interval (steady state).
    Spread,
    /// Every node starts together, as after a fleet-wide restart, deploy or cron-aligned schedule.
    Synced,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Backoff {
    /// chef-client's own: `1 + 2^n + rand(2^n)` seconds after the n-th failed attempt.
    Chef,
    /// A fixed `retry_delay`.
    Fixed,
}

#[derive(Clone, Debug)]
struct Config {
    // Fleet
    nodes: usize,
    interval: f64,
    splay: f64,
    mode: Mode,
    start: Start,
    bad_start: Option<Start>,
    converge: f64,
    // Run lists
    bad_fraction: f64,
    fast: f64,
    fast_sigma: f64,
    slow: f64,
    slow_sigma: f64,
    // Server
    workers: usize,
    frontends: usize,
    pooler_wait: f64,
    timeout: f64,
    timeout_factor: f64,
    restart: f64,
    // Client
    retries: u32,
    backoff: Backoff,
    retry_delay: f64,
    // Run
    duration: f64,
    warmup: Option<f64>,
    spike_threshold: f64,
    seed: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            nodes: 10_000,
            interval: 1800.0,
            splay: 300.0,
            mode: Mode::Daemon,
            start: Start::Spread,
            bad_start: None,
            converge: 60.0,
            bad_fraction: 0.01,
            fast: 0.5,
            fast_sigma: 0.5,
            slow: 8.0,
            slow_sigma: 0.3,
            workers: 5,
            frontends: 1,
            pooler_wait: 0.0,
            timeout: 5.0,
            timeout_factor: 1.0,
            restart: 2.0,
            retries: 5,
            backoff: Backoff::Chef,
            retry_delay: 5.0,
            duration: 86_400.0,
            warmup: None,
            spike_threshold: 0.10,
            seed: 1,
        }
    }
}

/// (key, default, description). The order is the order shown by `params`.
const PARAMS: &[(&str, &str, &str)] = &[
    ("nodes", "10000", "number of nodes converging"),
    ("interval", "30m", "chef-client interval"),
    ("splay", "5m", "chef-client splay: a random delay of 0..splay added to each run"),
    ("mode", "daemon", "daemon (sleep interval+rand(splay) after each run) or timer (fixed slots plus rand(splay))"),
    ("start", "spread", "spread (steady state) or synced (all nodes start together, e.g. after a fleet restart or cron at :00)"),
    ("bad_start", "(= start)", "start for the slow-run-list nodes only, to align just those"),
    ("converge", "60s", "time a successful converge takes after its solve (delays the next daemon run)"),
    ("bad_fraction", "0.01", "share of nodes whose run list is slow to solve"),
    ("fast", "0.5s", "median solve time of an ordinary run list"),
    ("fast_sigma", "0.5", "log-normal spread of ordinary solve times (0 = all identical)"),
    ("slow", "8s", "median solve time of a slow run list"),
    ("slow_sigma", "0.3", "log-normal spread of slow solve times"),
    ("workers", "5", "depsolver_worker_count: Ruby workers per frontend"),
    ("frontends", "1", "erchef frontends behind the load balancer; each has its own pool"),
    ("pooler_wait", "0", "depsolver_pooler_timeout: how long a request waits for a free worker before 503 (0 = no queue)"),
    ("timeout", "5s", "depsolver_timeout: solves reaching this are answered 412"),
    ("timeout_factor", "1", "how long a timed-out solve holds its worker, as a multiple of timeout (erchef allows up to 2)"),
    ("restart", "2s", "time to replace a worker after a timeout (start Ruby, load dep_selector)"),
    ("retries", "5", "chef-client http_retry_count: retries after a 503"),
    ("retry_backoff", "chef", "chef (1+2^n+rand(2^n) s) or fixed"),
    ("retry_delay", "5s", "delay for retry_backoff=fixed"),
    ("duration", "24h", "simulated time measured"),
    ("warmup", "auto", "simulated time discarded first (auto: none for start=synced, else interval+splay)"),
    ("spike_threshold", "0.10", "a minute is a spike when this share of ordinary requests failed"),
    ("seed", "1", "random seed"),
];

const SECONDS_KEYS: &[&str] =
    &["interval", "splay", "converge", "fast", "slow", "pooler_wait", "timeout", "restart", "retry_delay", "duration", "warmup"];

fn parse_secs(value: &str) -> Result<f64, String> {
    let value = value.trim();
    let split = value.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: f64 = number.parse().map_err(|_| format!("'{value}' is not a duration"))?;
    let factor = match unit {
        "" | "s" => 1.0,
        "ms" => 0.001,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        other => return Err(format!("unknown time unit '{other}' in '{value}' (use ms, s, m, h or d)")),
    };
    Ok(number * factor)
}

impl Config {
    fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let secs = || parse_secs(value);
        let number = || value.parse::<f64>().map_err(|_| format!("{key}: '{value}' is not a number"));
        let count = || value.parse::<usize>().map_err(|_| format!("{key}: '{value}' is not a whole number"));
        let start = |v: &str| match v {
            "spread" => Ok(Start::Spread),
            "synced" => Ok(Start::Synced),
            other => Err(format!("start must be spread or synced, not '{other}'")),
        };
        match key {
            "nodes" => self.nodes = count()?,
            "interval" => self.interval = secs()?,
            "splay" => self.splay = secs()?,
            "mode" => {
                self.mode = match value {
                    "daemon" => Mode::Daemon,
                    "timer" => Mode::Timer,
                    other => return Err(format!("mode must be daemon or timer, not '{other}'")),
                }
            }
            "start" => self.start = start(value)?,
            "bad_start" => self.bad_start = Some(start(value)?),
            "converge" => self.converge = secs()?,
            "bad_fraction" => self.bad_fraction = number()?,
            "fast" => self.fast = secs()?,
            "fast_sigma" => self.fast_sigma = number()?,
            "slow" => self.slow = secs()?,
            "slow_sigma" => self.slow_sigma = number()?,
            "workers" => self.workers = count()?,
            "frontends" => self.frontends = count()?,
            "pooler_wait" => self.pooler_wait = secs()?,
            "timeout" => self.timeout = secs()?,
            "timeout_factor" => self.timeout_factor = number()?,
            "restart" => self.restart = secs()?,
            "retries" => self.retries = count()? as u32,
            "retry_backoff" => {
                self.backoff = match value {
                    "chef" => Backoff::Chef,
                    "fixed" => Backoff::Fixed,
                    other => return Err(format!("retry_backoff must be chef or fixed, not '{other}'")),
                }
            }
            "retry_delay" => self.retry_delay = secs()?,
            "duration" => self.duration = secs()?,
            "warmup" => self.warmup = if value == "auto" { None } else { Some(secs()?) },
            "spike_threshold" => self.spike_threshold = number()?,
            "seed" => self.seed = value.parse().map_err(|_| format!("seed: '{value}' is not a whole number"))?,
            other => return Err(format!("unknown parameter '{other}' (run `simulate params` for the list)")),
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.nodes == 0 || self.workers == 0 || self.frontends == 0 {
            return Err("nodes, workers and frontends must be at least 1".into());
        }
        if !(0.0..=1.0).contains(&self.bad_fraction) {
            return Err("bad_fraction must be between 0 and 1".into());
        }
        if self.interval <= 0.0 || self.duration <= 0.0 || self.timeout <= 0.0 {
            return Err("interval, duration and timeout must be positive".into());
        }
        Ok(())
    }

    fn effective_warmup(&self) -> f64 {
        self.warmup.unwrap_or(if self.start == Start::Synced { 0.0 } else { self.interval + self.splay + self.converge })
    }

    fn bad_nodes(&self) -> usize {
        ((self.nodes as f64) * self.bad_fraction).round() as usize
    }
}

// ---------------------------------------------------------------------------------------
// Simulation
// ---------------------------------------------------------------------------------------

/// A depsolve request in flight: one node's attempt number `attempt` of the run that began at `first_t`.
#[derive(Clone, Copy)]
struct Req {
    node: usize,
    attempt: u32,
    first_t: f64,
}

enum Ev {
    /// A node starts a converge.
    Run(usize),
    /// A request (first try or retry) reaches erchef.
    Attempt(Req),
    /// A worker finishes the solve it was running.
    Done { frontend: usize, req: Req, timed_out: bool },
    /// A replacement worker is ready.
    Online(usize),
    /// A queued request gives up waiting.
    Expire { frontend: usize, ticket: u64 },
}

struct Item {
    t: f64,
    seq: u64,
    ev: Ev,
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.t == other.t && self.seq == other.seq
    }
}
impl Eq for Item {}
impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Item {
    /// Reversed, so the binary heap pops the earliest event.
    fn cmp(&self, other: &Self) -> Ordering {
        other.t.total_cmp(&self.t).then(other.seq.cmp(&self.seq))
    }
}

struct Node {
    bad: bool,
    slot: u64,
    phase: f64,
}

struct Waiter {
    ticket: u64,
    req: Req,
}

struct Frontend {
    idle: usize,
    busy: usize,
    offline: usize,
    queue: VecDeque<Waiter>,
    last_t: f64,
}

/// Index 0 is ordinary run lists, 1 is slow ones.
#[derive(Default)]
struct Stats {
    responses: [u64; 2],
    errors_503: [u64; 2],
    errors_412: [u64; 2],
    runs: [u64; 2],
    runs_failed: [u64; 2],
    /// Runs that failed because every retry was answered 503 (the pool's fault, not the node's own solve).
    runs_failed_503: [u64; 2],
    minute_responses: Vec<u32>,
    minute_errors: Vec<u32>,
    latencies: Vec<f32>,
    busy_seconds: f64,
    offline_seconds: f64,
    saturated_seconds: f64,
    max_occupied: usize,
}

struct Sim<'a> {
    cfg: &'a Config,
    rng: Rng,
    heap: BinaryHeap<Item>,
    seq: u64,
    nodes: Vec<Node>,
    frontends: Vec<Frontend>,
    stats: Stats,
    warmup: f64,
    end: f64,
    ticket: u64,
}

impl<'a> Sim<'a> {
    fn new(cfg: &'a Config) -> Self {
        let mut rng = Rng::new(cfg.seed);
        let warmup = cfg.effective_warmup();
        let end = warmup + cfg.duration;
        let bad_nodes = cfg.bad_nodes();
        let minutes = (cfg.duration / 60.0).ceil() as usize + 1;
        let frontends = (0..cfg.frontends)
            .map(|_| Frontend { idle: cfg.workers, busy: 0, offline: 0, queue: VecDeque::new(), last_t: 0.0 })
            .collect();
        let mut sim = Sim {
            cfg,
            rng: Rng::new(0),
            heap: BinaryHeap::new(),
            seq: 0,
            nodes: Vec::with_capacity(cfg.nodes),
            frontends,
            stats: Stats { minute_responses: vec![0; minutes], minute_errors: vec![0; minutes], ..Stats::default() },
            warmup,
            end,
            ticket: 0,
        };

        for index in 0..cfg.nodes {
            let bad = index < bad_nodes;
            let start = if bad { cfg.bad_start.unwrap_or(cfg.start) } else { cfg.start };
            let (phase, first) = match (cfg.mode, start) {
                (Mode::Daemon, Start::Spread) => {
                    // Start each node where a long-running fleet would be: part-way through a cycle
                    // (converge + interval + rand(splay)) picked in proportion to its length. Spreading
                    // first runs evenly over interval+splay instead makes the first and second waves
                    // overlap and overload the pool for a few minutes.
                    let longest = cfg.converge + cfg.interval + cfg.splay;
                    let cycle = loop {
                        let cycle = cfg.converge + cfg.interval + rng.unit() * cfg.splay;
                        if rng.unit() * longest <= cycle {
                            break cycle;
                        }
                    };
                    (0.0, rng.unit() * cycle)
                }
                (Mode::Daemon, Start::Synced) => (0.0, rng.unit() * cfg.splay),
                (Mode::Timer, Start::Spread) => {
                    let phase = rng.unit() * cfg.interval;
                    (phase, phase + rng.unit() * cfg.splay)
                }
                (Mode::Timer, Start::Synced) => (0.0, rng.unit() * cfg.splay),
            };
            sim.nodes.push(Node { bad, slot: 0, phase });
            sim.push(first, Ev::Run(index));
        }
        sim.rng = rng;
        sim
    }

    fn push(&mut self, t: f64, ev: Ev) {
        self.seq += 1;
        self.heap.push(Item { t, seq: self.seq, ev });
    }

    fn in_window(&self, t: f64) -> bool {
        t >= self.warmup && t < self.end
    }

    /// Adds the time since this frontend's last state change to the occupancy totals.
    fn account(&mut self, frontend: usize, t: f64) {
        let f = &mut self.frontends[frontend];
        let from = f.last_t.max(self.warmup);
        let to = t.min(self.end);
        if to > from {
            let dt = to - from;
            self.stats.busy_seconds += f.busy as f64 * dt;
            self.stats.offline_seconds += f.offline as f64 * dt;
            if f.idle == 0 {
                self.stats.saturated_seconds += dt;
            }
        }
        f.last_t = t;
    }

    fn class(&self, node: usize) -> usize {
        self.nodes[node].bad as usize
    }

    fn record_response(&mut self, node: usize, t: f64, outcome: Option<u16>) {
        if !self.in_window(t) {
            return;
        }
        let class = self.class(node);
        self.stats.responses[class] += 1;
        match outcome {
            Some(503) => self.stats.errors_503[class] += 1,
            Some(_) => self.stats.errors_412[class] += 1,
            None => {}
        }
        if class == 0 {
            let minute = ((t - self.warmup) / 60.0) as usize;
            self.stats.minute_responses[minute] += 1;
            if outcome == Some(503) {
                self.stats.minute_errors[minute] += 1;
            }
        }
    }

    /// A run ended, successfully or not: record it and schedule the node's next run.
    fn finish_run(&mut self, req: Req, t: f64, ok: bool, pool_refused: bool) {
        let class = self.class(req.node);
        if self.in_window(req.first_t) {
            self.stats.runs[class] += 1;
            if !ok {
                self.stats.runs_failed[class] += 1;
                self.stats.runs_failed_503[class] += pool_refused as u64;
            } else if class == 0 {
                self.stats.latencies.push((t - req.first_t) as f32);
            }
        }
        let cfg = self.cfg;
        let jitter = self.rng.unit() * cfg.splay;
        let next = match cfg.mode {
            Mode::Daemon => t + if ok { cfg.converge } else { 0.0 } + cfg.interval + jitter,
            Mode::Timer => {
                let node = &mut self.nodes[req.node];
                node.slot += 1;
                (node.slot as f64 * cfg.interval + node.phase + jitter).max(t)
            }
        };
        self.push(next, Ev::Run(req.node));
    }

    fn on_attempt(&mut self, t: f64, req: Req) {
        let frontend = if self.cfg.frontends > 1 { self.rng.below(self.cfg.frontends as u64) as usize } else { 0 };
        self.account(frontend, t);
        if self.frontends[frontend].idle > 0 {
            self.start_service(frontend, req, t);
        } else if self.cfg.pooler_wait > 0.0 {
            self.ticket += 1;
            let ticket = self.ticket;
            self.frontends[frontend].queue.push_back(Waiter { ticket, req });
            self.push(t + self.cfg.pooler_wait, Ev::Expire { frontend, ticket });
        } else {
            self.reject(req, t);
        }
    }

    fn start_service(&mut self, frontend: usize, req: Req, t: f64) {
        let cfg = self.cfg;
        let (median, sigma) = if self.nodes[req.node].bad { (cfg.slow, cfg.slow_sigma) } else { (cfg.fast, cfg.fast_sigma) };
        let solve = self.rng.lognormal(median, sigma);
        let timed_out = solve >= cfg.timeout;
        let held = if timed_out { cfg.timeout * cfg.timeout_factor } else { solve };

        let counted = self.in_window(t);
        let f = &mut self.frontends[frontend];
        f.idle -= 1;
        f.busy += 1;
        debug_assert_eq!(f.idle + f.busy + f.offline, cfg.workers);
        if counted {
            self.stats.max_occupied = self.stats.max_occupied.max(f.busy + f.offline);
        }
        self.push(t + held, Ev::Done { frontend, req, timed_out });
    }

    /// Hands free workers to queued requests, oldest first.
    fn drain(&mut self, frontend: usize, t: f64) {
        while self.frontends[frontend].idle > 0 {
            let Some(waiter) = self.frontends[frontend].queue.pop_front() else { break };
            self.start_service(frontend, waiter.req, t);
        }
    }

    /// No worker available: 503, then chef-client retries or gives up.
    fn reject(&mut self, req: Req, t: f64) {
        self.record_response(req.node, t, Some(503));
        if req.attempt < self.cfg.retries {
            let tried = req.attempt + 1; // chef's http_attempts, counting from 1
            let delay = match self.cfg.backoff {
                Backoff::Chef => chef_backoff(tried, &mut self.rng),
                Backoff::Fixed => self.cfg.retry_delay,
            };
            self.push(t + delay, Ev::Attempt(Req { attempt: tried, ..req }));
        } else {
            self.finish_run(req, t, false, true);
        }
    }

    fn run(&mut self) {
        let grace = 600.0;
        while let Some(Item { t, ev, .. }) = self.heap.pop() {
            if t > self.end + grace {
                break;
            }
            match ev {
                Ev::Run(node) => self.on_attempt(t, Req { node, attempt: 0, first_t: t }),
                Ev::Attempt(req) => self.on_attempt(t, req),
                Ev::Done { frontend, req, timed_out } => {
                    self.account(frontend, t);
                    self.frontends[frontend].busy -= 1;
                    if timed_out {
                        // erchef returns the worker as failed: it is replaced, not reused.
                        self.frontends[frontend].offline += 1;
                        self.push(t + self.cfg.restart, Ev::Online(frontend));
                        self.record_response(req.node, t, Some(412));
                        self.finish_run(req, t, false, false);
                    } else {
                        self.frontends[frontend].idle += 1;
                        self.record_response(req.node, t, None);
                        self.finish_run(req, t, true, false);
                        self.drain(frontend, t);
                    }
                }
                Ev::Online(frontend) => {
                    self.account(frontend, t);
                    self.frontends[frontend].offline -= 1;
                    self.frontends[frontend].idle += 1;
                    self.drain(frontend, t);
                }
                Ev::Expire { frontend, ticket } => {
                    let queue = &mut self.frontends[frontend].queue;
                    if let Some(position) = queue.iter().position(|w| w.ticket == ticket) {
                        let waiter = queue.remove(position).expect("position is valid");
                        self.reject(waiter.req, t);
                    }
                }
            }
        }
        for frontend in 0..self.frontends.len() {
            self.account(frontend, self.end);
        }
    }
}

/// chef-client's retry sleep after its `attempts`-th failed try: `1 + 2^n + rand(2^n)`.
fn chef_backoff(attempts: u32, rng: &mut Rng) -> f64 {
    let range = 2u64.pow(attempts.min(20));
    1.0 + range as f64 + rng.below(range) as f64
}

// ---------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Metrics {
    /// Share of ordinary nodes' converges that failed because the pool refused every try (503 after all retries).
    collateral: f64,
    /// Share of ordinary nodes' converges that failed for any reason, including their own solve timing out.
    typical_failed: f64,
    /// Share of ordinary requests whose own solve reached the timeout (412).
    typical_timeouts: f64,
    /// Share of slow-run-list nodes' converges that failed.
    bad_failed: f64,
    /// Share of ordinary requests (including retries) answered 503.
    typical_503: f64,
    /// Separate bursts of spike minutes per day.
    spikes: f64,
    spike_minutes: f64,
    longest_spike: f64,
    /// Worst single minute: share of ordinary requests that failed.
    worst_minute: f64,
    /// Share of the time every worker was busy or being replaced.
    saturated: f64,
    /// Average number of workers busy or being replaced, per frontend.
    occupied: f64,
    /// Share of worker time spent replacing timed-out workers.
    offline: f64,
    p50: f64,
    p99: f64,
    typical_runs: f64,
    bad_runs: f64,
    peak_occupied: f64,
}

const METRICS: &[(&str, &str)] = &[
    ("collateral", "% of ordinary nodes' converges that failed because the pool refused every try (503 after all retries)"),
    ("typical_failed", "% of ordinary nodes' converges that failed for any reason, including their own solve timing out"),
    ("typical_timeouts", "% of ordinary requests whose own solve reached the timeout (412)"),
    ("bad_failed", "% of slow-run-list nodes' converges that failed"),
    ("typical_503", "% of ordinary requests answered 503"),
    ("spikes", "error bursts per day (minutes where >= spike_threshold of ordinary requests failed)"),
    ("spike_minutes", "minutes in a spike, in total"),
    ("longest_spike", "longest spike, minutes"),
    ("worst_minute", "% of ordinary requests that failed in the worst minute"),
    ("saturated", "% of time every worker was busy or being replaced"),
    ("occupied", "average workers busy or being replaced, per frontend"),
    ("offline", "% of worker time spent replacing timed-out workers"),
    ("p50", "median seconds from first request to answer, ordinary nodes that succeeded"),
    ("p99", "99th percentile of the same"),
];

impl Metrics {
    fn get(&self, name: &str) -> Option<f64> {
        Some(match name {
            "collateral" => self.collateral,
            "typical_failed" => self.typical_failed,
            "typical_timeouts" => self.typical_timeouts,
            "bad_failed" => self.bad_failed,
            "typical_503" => self.typical_503,
            "spikes" => self.spikes,
            "spike_minutes" => self.spike_minutes,
            "longest_spike" => self.longest_spike,
            "worst_minute" => self.worst_minute,
            "saturated" => self.saturated,
            "occupied" => self.occupied,
            "offline" => self.offline,
            "p50" => self.p50,
            "p99" => self.p99,
            _ => return None,
        })
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

struct Outcome {
    metrics: Metrics,
    /// Share of ordinary requests that failed, per 5-minute bucket (None where there were none).
    timeline: Vec<Option<f64>>,
}

fn simulate(cfg: &Config) -> Outcome {
    let mut sim = Sim::new(cfg);
    sim.run();
    let stats = &sim.stats;

    // Spikes: minutes where at least spike_threshold of ordinary requests failed.
    const MIN_REQUESTS: u32 = 5;
    let (mut spike_minutes, mut spikes, mut longest, mut current, mut worst) = (0u64, 0u64, 0u64, 0u64, 0.0f64);
    for (&responses, &errors) in stats.minute_responses.iter().zip(&stats.minute_errors) {
        let rate = if responses > 0 { errors as f64 / responses as f64 } else { 0.0 };
        if responses >= MIN_REQUESTS {
            worst = worst.max(rate);
        }
        if responses >= MIN_REQUESTS && rate >= cfg.spike_threshold {
            spike_minutes += 1;
            current += 1;
            if current == 1 {
                spikes += 1;
            }
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }

    let mut latencies = stats.latencies.clone();
    latencies.sort_by(|a, b| a.total_cmp(b));
    let quantile = |q: f64| latencies.get(((latencies.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0.0) as f64;

    let days = cfg.duration / 86_400.0;
    let worker_seconds = cfg.duration * (cfg.workers * cfg.frontends) as f64;
    let typical_requests = stats.responses[0];

    let timeline = stats
        .minute_responses
        .chunks(5)
        .zip(stats.minute_errors.chunks(5))
        .map(|(responses, errors)| {
            let (r, e): (u32, u32) = (responses.iter().sum(), errors.iter().sum());
            (r > 0).then(|| e as f64 / r as f64)
        })
        .collect();

    let metrics = Metrics {
        collateral: percent(stats.runs_failed_503[0], stats.runs[0]),
        typical_failed: percent(stats.runs_failed[0], stats.runs[0]),
        typical_timeouts: percent(stats.errors_412[0], typical_requests),
        bad_failed: percent(stats.runs_failed[1], stats.runs[1]),
        typical_503: percent(stats.errors_503[0], typical_requests),
        spikes: spikes as f64 / days,
        spike_minutes: spike_minutes as f64,
        longest_spike: longest as f64,
        worst_minute: 100.0 * worst,
        saturated: 100.0 * stats.saturated_seconds / (cfg.duration * cfg.frontends as f64),
        occupied: (stats.busy_seconds + stats.offline_seconds) / cfg.duration / cfg.frontends as f64,
        offline: 100.0 * stats.offline_seconds / worker_seconds,
        p50: quantile(0.5),
        p99: quantile(0.99),
        typical_runs: stats.runs[0] as f64,
        bad_runs: stats.runs[1] as f64,
        peak_occupied: stats.max_occupied as f64,
    };
    Outcome { metrics, timeline }
}

/// Averages (or takes the worst of) several seeds.
fn simulate_seeds(cfg: &Config, seeds: u32, worst: bool) -> Metrics {
    let runs: Vec<Metrics> = (0..seeds)
        .map(|i| {
            let mut c = cfg.clone();
            c.seed = cfg.seed + i as u64;
            simulate(&c).metrics
        })
        .collect();
    let combine = |f: fn(&Metrics) -> f64| {
        let values = runs.iter().map(f);
        if worst {
            values.fold(f64::MIN, f64::max)
        } else {
            values.sum::<f64>() / runs.len() as f64
        }
    };
    Metrics {
        collateral: combine(|m| m.collateral),
        typical_failed: combine(|m| m.typical_failed),
        typical_timeouts: combine(|m| m.typical_timeouts),
        bad_failed: combine(|m| m.bad_failed),
        typical_503: combine(|m| m.typical_503),
        spikes: combine(|m| m.spikes),
        spike_minutes: combine(|m| m.spike_minutes),
        longest_spike: combine(|m| m.longest_spike),
        worst_minute: combine(|m| m.worst_minute),
        saturated: combine(|m| m.saturated),
        occupied: combine(|m| m.occupied),
        offline: combine(|m| m.offline),
        p50: combine(|m| m.p50),
        p99: combine(|m| m.p99),
        typical_runs: combine(|m| m.typical_runs),
        bad_runs: combine(|m| m.bad_runs),
        peak_occupied: combine(|m| m.peak_occupied),
    }
}

/// Runs `f` over `items` on all cores, keeping the order of the results.
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, AtomicOrdering::SeqCst);
                if index >= items.len() {
                    break;
                }
                let result = f(&items[index]);
                results.lock().expect("no worker panics while holding the lock")[index] = Some(result);
            });
        }
    });
    results.into_inner().expect("lock is not poisoned").into_iter().map(|r| r.expect("every item was processed")).collect()
}

// ---------------------------------------------------------------------------------------
// Analytic cross-check
// ---------------------------------------------------------------------------------------

/// Erlang-B blocking probability for `servers` servers offered `load` erlangs with no queue.
fn erlang_b(servers: usize, load: f64) -> f64 {
    (1..=servers).fold(1.0, |b, k| load * b / (k as f64 + load * b))
}

struct Load {
    /// Requests per second reaching erchef, ignoring retries.
    arrival_rate: f64,
    /// Average worker-seconds each request occupies (solve time, plus replacement after a timeout).
    service: f64,
    /// Average workers occupied per frontend if arrivals were smooth.
    erlangs: f64,
    /// Erlang-B blocking if arrivals were random (Poisson) with no retries.
    random_blocking: f64,
    /// Most worker-seconds an average request could occupy before the pool runs out of workers.
    capacity: f64,
}

fn offered_load(cfg: &Config) -> Load {
    let cycle = match cfg.mode {
        Mode::Daemon => cfg.interval + cfg.splay / 2.0 + cfg.converge,
        Mode::Timer => cfg.interval,
    };
    let arrival_rate = cfg.nodes as f64 / cycle;
    let mut rng = Rng::new(7);
    let mean_hold = |median: f64, sigma: f64, rng: &mut Rng| {
        const SAMPLES: usize = 20_000;
        (0..SAMPLES)
            .map(|_| {
                let solve = rng.lognormal(median, sigma);
                if solve >= cfg.timeout {
                    cfg.timeout * cfg.timeout_factor + cfg.restart
                } else {
                    solve
                }
            })
            .sum::<f64>()
            / SAMPLES as f64
    };
    let bad = cfg.bad_fraction;
    let service = (1.0 - bad) * mean_hold(cfg.fast, cfg.fast_sigma, &mut rng) + bad * mean_hold(cfg.slow, cfg.slow_sigma, &mut rng);
    let erlangs = arrival_rate * service / cfg.frontends as f64;
    Load {
        arrival_rate,
        service,
        erlangs,
        random_blocking: erlang_b(cfg.workers, erlangs),
        capacity: (cfg.workers * cfg.frontends) as f64 / arrival_rate,
    }
}

// ---------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------

fn fmt_secs(s: f64) -> String {
    if s >= 3600.0 && (s / 3600.0).fract() == 0.0 {
        format!("{}h", s / 3600.0)
    } else if s >= 60.0 && (s / 60.0).fract() == 0.0 {
        format!("{}m", s / 60.0)
    } else if s >= 1.0 {
        format!("{s}s")
    } else {
        format!("{}ms", s * 1000.0)
    }
}

fn describe(cfg: &Config) -> String {
    let mode = match cfg.mode {
        Mode::Daemon => "daemon",
        Mode::Timer => "timer",
    };
    let start = |s: Start| if s == Start::Synced { "synced" } else { "spread" };
    let bad_start = match cfg.bad_start {
        Some(s) if s != cfg.start => format!(" (slow nodes {})", start(s)),
        _ => String::new(),
    };
    format!(
        "{} nodes, {} {} every {} + splay {}, start {}{}; {:.1}% slow run lists (solve ~{}), ordinary ~{}; \
         {} worker(s) x {} frontend(s), pooler_wait {}, timeout {}, restart {}; {} retries",
        cfg.nodes,
        mode,
        if cfg.mode == Mode::Daemon { "run" } else { "slot" },
        fmt_secs(cfg.interval),
        fmt_secs(cfg.splay),
        start(cfg.start),
        bad_start,
        cfg.bad_fraction * 100.0,
        fmt_secs(cfg.slow),
        fmt_secs(cfg.fast),
        cfg.workers,
        cfg.frontends,
        fmt_secs(cfg.pooler_wait),
        fmt_secs(cfg.timeout),
        fmt_secs(cfg.restart),
        cfg.retries,
    )
}

fn bar(rate: Option<f64>) -> char {
    match rate {
        None => ' ',
        Some(r) if r == 0.0 => '.',
        Some(r) if r < 0.01 => '\u{2581}',
        Some(r) if r < 0.05 => '\u{2582}',
        Some(r) if r < 0.10 => '\u{2583}',
        Some(r) if r < 0.25 => '\u{2585}',
        Some(r) if r < 0.50 => '\u{2586}',
        Some(r) if r < 1.0 => '\u{2587}',
        Some(_) => '\u{2588}',
    }
}

fn print_run(cfg: &Config, seeds: u32, timeline: bool) {
    let outcome = simulate(cfg);
    let m = if seeds > 1 { simulate_seeds(cfg, seeds, false) } else { outcome.metrics.clone() };
    let load = offered_load(cfg);

    println!("{}\n", describe(cfg));
    println!(
        "Capacity: {} workers / {:.2} requests/s = {:.2} worker-seconds available per request. Average use: {:.2} ({:.0}% of capacity).",
        cfg.workers * cfg.frontends,
        load.arrival_rate,
        load.capacity,
        load.service,
        100.0 * load.service / load.capacity
    );
    println!(
        "Erlang-B blocking if arrivals were random and un-retried: {:.4}% -- the gap to the simulated 503 rate is the effect of alignment and retries\n",
        load.random_blocking * 100.0
    );
    if seeds > 1 {
        println!("(mean of {seeds} seeds)\n");
    }
    println!("Ordinary nodes ({:.0} converges)", m.typical_runs);
    println!("  converges that failed       {:>8.3}%", m.typical_failed);
    println!("  requests answered 503       {:>8.3}%", m.typical_503);
    println!("  worst minute                {:>8.1}% of requests failed", m.worst_minute);
    println!(
        "  spikes (>= {:.0}% failing)     {:>8.2} per day, {:.0} minutes in total, longest {:.0} min",
        cfg.spike_threshold * 100.0,
        m.spikes,
        m.spike_minutes,
        m.longest_spike
    );
    println!("  answer time p50 / p99       {:>8.2}s / {:.2}s", m.p50, m.p99);
    println!("Slow-run-list nodes ({:.0} converges)", m.bad_runs);
    println!("  converges that failed       {:>8.3}%", m.bad_failed);
    println!("Pool");
    println!("  workers busy or replacing   {:>8.2} of {} on average, peak {:.0}", m.occupied, cfg.workers, m.peak_occupied);
    println!("  every worker unavailable    {:>8.3}% of the time", m.saturated);
    println!("  replacing timed-out workers {:>8.3}% of worker time", m.offline);

    if timeline {
        println!("\nShare of ordinary requests failing, per 5 minutes ('.' none, then \u{2581} <1% \u{2582} <5% \u{2583} <10% \u{2585} <25% \u{2586} <50% \u{2587} <100% \u{2588} all):");
        let per_row = 72;
        for (row, chunk) in outcome.timeline.chunks(per_row).enumerate() {
            let hours = (row * per_row * 5) as f64 / 60.0;
            println!("  +{hours:>5.1}h  {}", chunk.iter().map(|&r| bar(r)).collect::<String>());
        }
    }
}

fn print_params() {
    println!("Parameters (pass as --name value; times accept ms, s, m, h, d):\n");
    for (name, default, doc) in PARAMS {
        println!("  {name:<16} {default:<10} {doc}");
    }
    println!("\nMetrics for `sweep --metric`:\n");
    for (name, doc) in METRICS {
        println!("  {name:<16} {doc}");
    }
}

/// Splits `key=a,b,c` into the key and its values.
fn parse_vary(spec: &str) -> Result<(String, Vec<String>), String> {
    let (key, values) = spec.split_once('=').ok_or_else(|| format!("--vary wants key=v1,v2,..., got '{spec}'"))?;
    Ok((key.to_string(), values.split(',').map(str::to_string).collect()))
}

fn label(key: &str, value: &str) -> String {
    if SECONDS_KEYS.contains(&key) {
        parse_secs(value).map(fmt_secs).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    }
}

fn format_metric(name: &str, value: f64) -> String {
    match name {
        "spikes" | "occupied" | "p50" | "p99" => format!("{value:.2}"),
        "spike_minutes" | "longest_spike" => format!("{value:.0}"),
        "collateral" | "typical_failed" | "typical_timeouts" | "bad_failed" | "typical_503" | "saturated" | "offline" | "worst_minute" => {
            if value == 0.0 {
                "0".to_string()
            } else if value < 0.01 {
                "<0.01".to_string()
            } else if value < 10.0 {
                format!("{value:.2}")
            } else {
                format!("{value:.1}")
            }
        }
        _ => format!("{value:.2}"),
    }
}

struct SweepOptions {
    seeds: u32,
    worst: bool,
    metric: String,
}

/// Builds the configuration for one cell of a sweep.
fn cell_config(base: &Config, settings: &[(&str, &str)]) -> Result<Config, String> {
    let mut cfg = base.clone();
    for (key, value) in settings {
        cfg.set(key, value)?;
    }
    cfg.validate()?;
    Ok(cfg)
}

/// One vary: a row per value with the headline metrics. Two varies: a matrix of one metric.
fn print_sweep(base: &Config, varies: &[(String, Vec<String>)], opts: &SweepOptions) -> Result<(), String> {
    let run_all = |configs: &[Config]| parallel_map(configs, |cfg| simulate_seeds(cfg, opts.seeds, opts.worst));

    match varies {
        [(key, values)] => {
            let configs = values.iter().map(|v| cell_config(base, &[(key.as_str(), v.as_str())])).collect::<Result<Vec<_>, _>>()?;
            let results = run_all(&configs);
            let columns = ["collateral", "typical_503", "spikes", "worst_minute", "saturated", "occupied", "p99"];
            let headers = ["pool-caused failures %", "requests 503 %", "spikes/day", "worst minute %", "all busy %", "workers busy", "p99 s"];
            println!("| {key} | {} |", headers.join(" | "));
            println!("|{}|", vec!["---"; headers.len() + 1].join("|"));
            for (value, m) in values.iter().zip(&results) {
                let cells: Vec<String> = columns.iter().map(|c| format_metric(c, m.get(c).unwrap())).collect();
                println!("| {} | {} |", label(key, value), cells.join(" | "));
            }
        }
        [(row_key, row_values), (col_key, col_values)] => {
            let metric = opts.metric.as_str();
            if Metrics::default().get(metric).is_none() {
                return Err(format!("unknown metric '{metric}' (run `simulate params`)"));
            }
            let mut configs = Vec::new();
            for row in row_values {
                for col in col_values {
                    configs.push(cell_config(base, &[(row_key.as_str(), row.as_str()), (col_key.as_str(), col.as_str())])?);
                }
            }
            let results = run_all(&configs);
            let header: Vec<String> = col_values.iter().map(|v| label(col_key, v)).collect();
            println!("{metric} -- rows: {row_key}, columns: {col_key}\n");
            println!("| {row_key} \\ {col_key} | {} |", header.join(" | "));
            println!("|{}|", vec!["---"; col_values.len() + 1].join("|"));
            for (row, chunk) in row_values.iter().zip(results.chunks(col_values.len())) {
                let cells: Vec<String> = chunk.iter().map(|m| format_metric(metric, m.get(metric).unwrap())).collect();
                println!("| {} | {} |", label(row_key, row), cells.join(" | "));
            }
        }
        _ => return Err("sweep takes one or two --vary options".into()),
    }
    Ok(())
}

struct LimitOptions {
    seeds: u32,
    worst: bool,
    metric: String,
    max: f64,
    find: String,
    low: f64,
    high: f64,
}

/// The largest value of `find` for which `metric` stays at or below `max` (results are assumed
/// to get worse as `find` grows). Returns Err(side) when even one end of the range is on the
/// wrong side: `Ok(None)` is "below the range", otherwise the value found.
enum Found {
    BelowRange,
    AboveRange,
    At(f64),
}

fn find_limit(cfg: &Config, opts: &LimitOptions) -> Found {
    let acceptable = |value: f64| {
        let mut c = cfg.clone();
        // Counts must be whole numbers; fractions and time-valued parameters (in seconds) can be anything.
        let text = if ["nodes", "workers", "frontends", "retries"].contains(&opts.find.as_str()) {
            format!("{}", value.round() as u64)
        } else {
            value.to_string()
        };
        c.set(&opts.find, &text).expect("the parameter was validated");
        simulate_seeds(&c, opts.seeds, opts.worst).get(&opts.metric).unwrap() <= opts.max
    };
    if !acceptable(opts.low) {
        return Found::BelowRange;
    }
    if acceptable(opts.high) {
        return Found::AboveRange;
    }
    let (mut good, mut bad) = (opts.low, opts.high);
    for _ in 0..11 {
        let middle = (good + bad) / 2.0;
        if acceptable(middle) {
            good = middle;
        } else {
            bad = middle;
        }
    }
    Found::At(good)
}

fn format_found(find: &str, found: &Found, opts: &LimitOptions) -> String {
    let show = |value: f64| match find {
        "bad_fraction" => format!("{:.1}%", value * 100.0),
        "nodes" => format!("{:.0}", (value / 100.0).round() * 100.0),
        "workers" | "frontends" | "retries" => format!("{:.0}", value.round()),
        k if SECONDS_KEYS.contains(&k) => fmt_secs(value),
        _ => format!("{value:.2}"),
    };
    match found {
        Found::BelowRange => format!("under {}", show(opts.low)),
        Found::AboveRange => format!("over {}", show(opts.high)),
        Found::At(value) => show(*value),
    }
}

/// For each row (and column) value, the largest `find` that keeps `metric` within `max`.
fn print_limit(base: &Config, varies: &[(String, Vec<String>)], opts: &LimitOptions) -> Result<(), String> {
    if Metrics::default().get(&opts.metric).is_none() {
        return Err(format!("unknown metric '{}' (run `simulate params`)", opts.metric));
    }
    cell_config(base, &[(opts.find.as_str(), &opts.low.to_string())])?; // rejects an unknown parameter early
    let (row_key, row_values) = varies.first().ok_or("limit needs a --vary to give each row its own limit")?;
    let (col_key, col_values) = match varies.get(1) {
        Some((k, v)) => (Some(k.as_str()), v.clone()),
        None => (None, vec![String::new()]),
    };

    let mut configs = Vec::new();
    for row in row_values {
        for col in &col_values {
            let mut settings = vec![(row_key.as_str(), row.as_str())];
            if let Some(key) = col_key {
                settings.push((key, col.as_str()));
            }
            configs.push(cell_config(base, &settings)?);
        }
    }
    let found = parallel_map(&configs, |cfg| find_limit(cfg, opts));

    println!(
        "Largest {} with {} <= {} ({} seeds, {}) -- rows: {}{}\n",
        opts.find,
        opts.metric,
        opts.max,
        opts.seeds,
        if opts.worst { "worst seed" } else { "mean" },
        row_key,
        col_key.map(|k| format!(", columns: {k}")).unwrap_or_default()
    );
    let header: Vec<String> = match col_key {
        Some(key) => col_values.iter().map(|v| label(key, v)).collect(),
        None => vec![format!("largest {}", opts.find)],
    };
    println!("| {row_key} | {} |", header.join(" | "));
    println!("|{}|", vec!["---"; col_values.len() + 1].join("|"));
    for (row, chunk) in row_values.iter().zip(found.chunks(col_values.len())) {
        let cells: Vec<String> = chunk.iter().map(|f| format_found(&opts.find, f, opts)).collect();
        println!("| {} | {} |", label(row_key, row), cells.join(" | "));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------------------

const USAGE: &str = "\
simulate: why occasional slow depsolves become error spikes

  simulate [run] [--param value ...] [--seeds N] [--timeline]
      One scenario: a summary, optionally with a per-5-minute failure timeline.
  simulate sweep --vary param=v1,v2,... [--vary param=...] [--metric NAME] [--agg mean|max] [--seeds N] [--param value ...]
      One --vary: a row per value with the headline metrics.
      Two --vary: a matrix of one metric (default collateral).
  simulate limit --find param [--range lo,hi] --vary param=v1,v2,... [--vary param=...] [--metric NAME] [--max X] [--agg mean|max] [--seeds N] [--param value ...]
      For each --vary value, the largest `param` (e.g. bad_fraction, nodes) for which METRIC stays <= X
      (default: worst_minute <= 10, worst of the seeds, i.e. no spike minute in any seed).
  simulate report [--seeds N] [--param value ...]
      The standard set of sweeps that answer \"what makes this bad?\".
  simulate params
      Every parameter and metric.

Example: simulate sweep --vary splay=0,1m,5m,15m,30m --vary slow=2s,4s,8s --bad_fraction 0.02
Example: simulate limit --find bad_fraction --vary slow=5s,10s,20s,60s --timeout 10s
";

struct Parsed {
    config: Config,
    varies: Vec<(String, Vec<String>)>,
    seeds: Option<u32>,
    timeline: bool,
    metric: Option<String>,
    worst: Option<bool>,
    find: Option<String>,
    range: Option<(f64, f64)>,
    max: Option<f64>,
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut parsed = Parsed {
        config: Config::default(),
        varies: Vec::new(),
        seeds: None,
        timeline: false,
        metric: None,
        worst: None,
        find: None,
        range: None,
        max: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let flag = arg.strip_prefix("--").ok_or_else(|| format!("unexpected argument '{arg}'"))?;
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (flag, None),
        };
        if name == "timeline" {
            parsed.timeline = true;
            continue;
        }
        let value = match inline {
            Some(value) => value,
            None => iter.next().ok_or_else(|| format!("--{name} needs a value"))?.clone(),
        };
        match name {
            "vary" => parsed.varies.push(parse_vary(&value)?),
            "seeds" => parsed.seeds = Some(value.parse().map_err(|_| "--seeds wants a whole number".to_string())?),
            "metric" => parsed.metric = Some(value),
            "agg" => {
                parsed.worst = Some(match value.as_str() {
                    "mean" => false,
                    "max" => true,
                    other => return Err(format!("--agg must be mean or max, not '{other}'")),
                })
            }
            "find" => parsed.find = Some(value),
            "max" => parsed.max = Some(value.parse().map_err(|_| "--max wants a number".to_string())?),
            "range" => {
                let (low, high) = value.split_once(',').ok_or("--range wants lo,hi")?;
                let bound = |v: &str| parse_secs(v).map_err(|_| format!("--range: '{v}' is not a number"));
                parsed.range = Some((bound(low)?, bound(high)?));
            }
            key => parsed.config.set(key, &value)?,
        }
    }
    parsed.config.validate()?;
    Ok(parsed)
}

fn heading(title: &str, note: &str) {
    println!("\n### {title}\n");
    if !note.is_empty() {
        println!("{note}\n");
    }
}

/// The standard set of sweeps, built around the base configuration.
fn report(base: &Config, seeds: u32) -> Result<(), String> {
    let sweep = |config: &Config, varies: Vec<(&str, &str)>, metric: &str| -> Result<(), String> {
        let varies: Vec<(String, Vec<String>)> =
            varies.into_iter().map(|(k, v)| (k.to_string(), v.split(',').map(str::to_string).collect())).collect();
        print_sweep(config, &varies, &SweepOptions { seeds, worst: false, metric: metric.to_string() })
    };
    let limit = |config: &Config, find: &str, low: f64, high: f64, varies: Vec<(&str, &str)>| -> Result<(), String> {
        let varies: Vec<(String, Vec<String>)> =
            varies.into_iter().map(|(k, v)| (k.to_string(), v.split(',').map(str::to_string).collect())).collect();
        // Searches run many simulations each; six simulated hours is plenty once the fleet is in steady state.
        let mut config = config.clone();
        config.duration = config.duration.min(6.0 * 3600.0);
        let config = &config;
        let options = LimitOptions {
            seeds: seeds.min(3),
            worst: true,
            metric: "worst_minute".into(),
            max: 10.0,
            find: find.to_string(),
            low,
            high,
        };
        print_limit(config, &varies, &options)
    };

    let load = offered_load(base);
    println!("## Depsolver pool simulation\n");
    println!("Base scenario: {}.\n", describe(base));
    println!(
        "Each cell is the mean of {seeds} simulated {} (limits: the worst of {} seeds of 6h each). \"Slow run lists\" are the ones that take much longer than the rest; \
         everything about them is a parameter, not a measurement.",
        fmt_secs(base.duration),
        seeds.min(3)
    );
    println!(
        "\n**Pool capacity.** {} workers serving {:.1} requests/s can absorb an average of {:.1} worker-seconds per request before they run out. \
         Ordinary solves use {:.2} of that on their own.",
        base.workers * base.frontends,
        load.arrival_rate,
        load.capacity,
        load.service
    );
    println!(
        "\n\"Spikes\" are minutes in which at least {:.0}% of ordinary requests were answered 503. \"Pool-caused failures\" are ordinary converges that were refused on every one of their retries.",
        base.spike_threshold * 100.0
    );

    heading("Baseline", "");
    print_run(base, seeds, false);

    // A fleet loaded to about 90% of the pool's capacity by slow run lists whose solves hit a 10s timeout.
    let mut near = base.clone();
    near.set("slow", "15s")?;
    near.set("timeout", "10s")?;
    let service_at = |share: f64| {
        let mut c = near.clone();
        c.bad_fraction = share;
        offered_load(&c)
    };
    let (none, all) = (service_at(0.0), service_at(1.0));
    let share = ((0.9 * none.capacity - none.service) / (all.service - none.service)).clamp(0.0, 0.9);
    near.bad_fraction = share;
    let near_load = offered_load(&near);

    heading(
        "1. How slow, and how many?",
        "Worst-minute 503 rate for ordinary requests, by how long the slow solves take and how many nodes have such a run list. \
         Expect a cliff, not a slope: nothing happens until the pool's capacity is used up.",
    );
    for timeout in ["5s", "10s"] {
        println!("depsolver_timeout {timeout}\n");
        let mut config = base.clone();
        config.set("timeout", timeout)?;
        sweep(&config, vec![("slow", "2s,4s,6s,8s,15s,60s"), ("bad_fraction", "0.05,0.1,0.2,0.3,0.4,0.5,0.7")], "worst_minute")?;
        println!();
    }

    heading(
        "2. The limit",
        "The largest share of the fleet that can have a slow run list before a spike appears in any of the seeds.",
    );
    limit(base, "bad_fraction", 0.0, 0.9, vec![("slow", "3s,5s,8s,15s,60s"), ("timeout", "5s,10s")])?;

    heading("3. Fleet growth", "The largest fleet the pool can serve with a given share of slow run lists (slow solves ~15s, timeout 10s).");
    let mut growth = base.clone();
    growth.set("slow", "15s")?;
    growth.set("timeout", "10s")?;
    limit(&growth, "nodes", 1_000.0, 100_000.0, vec![("bad_fraction", "0.01,0.05,0.1,0.2,0.4")])?;

    heading(
        "4. Alignment: do the slow nodes all converge together?",
        &format!(
            "The slow nodes start in step (a shared deploy, a role that was converged at once, a restart), the rest of the fleet is spread out. \
             Splay against slow share, slow solves ~15s, timeout 10s. Metric: spikes per day over {}.",
            fmt_secs(base.duration)
        ),
    );
    let mut aligned = growth.clone();
    aligned.set("bad_start", "synced")?;
    aligned.set("warmup", "0")?;
    sweep(&aligned, vec![("splay", "0,1m,5m,15m,30m"), ("bad_fraction", "0.02,0.05,0.1,0.2")], "spikes")?;
    println!("\nThe same, as pool-caused failures:\n");
    sweep(&aligned, vec![("splay", "0,1m,5m,15m,30m"), ("bad_fraction", "0.02,0.05,0.1,0.2")], "collateral")?;

    heading(
        "5. Alignment: the whole fleet in step",
        "Every node starts together (after an outage, or a fleet-wide client restart) and then keeps its interval. `daemon` re-draws the splay each run, \
         so the herd smears out over hours; `timer` keeps every node in the same slot every interval (cron). Ordinary solves take ~1.5s here so the herd is large.",
    );
    let mut herd = base.clone();
    herd.set("fast", "1500ms")?;
    herd.set("start", "synced")?;
    for mode in ["daemon", "timer"] {
        println!("{mode}\n");
        herd.set("mode", mode)?;
        sweep(&herd, vec![("splay", "0,30s,1m,2m,5m,15m,30m")], "collateral")?;
        println!();
    }

    heading(
        "6. What helps when the pool is nearly full?",
        &format!(
            "A fleet loaded to {:.0}% of capacity ({:.1}% slow run lists, solves ~15s, timeout 10s), steady state. One setting changed at a time.",
            100.0 * near_load.service / near_load.capacity,
            100.0 * share
        ),
    );
    let around = |base: usize, steps: &[f64]| {
        let mut values: Vec<usize> = steps.iter().map(|f| ((base as f64 * f).round() as usize).max(1)).collect();
        values.dedup();
        values.iter().map(usize::to_string).collect::<Vec<_>>().join(",")
    };
    let workers = around(base.workers, &[0.67, 1.0, 1.33, 2.0]);
    let frontends = around(base.frontends, &[0.75, 1.0, 1.25, 1.5, 2.0]);
    for (name, values) in [
        ("workers", workers.as_str()),
        ("frontends", frontends.as_str()),
        ("pooler_wait", "0,5s,15s,30s,60s,120s"),
        ("timeout", "3s,5s,10s,20s"),
        ("restart", "0,1s,2s,5s,10s"),
        ("retries", "0,2,5,8"),
    ] {
        println!("{name}\n");
        sweep(&near, vec![(name, values)], "collateral")?;
        println!();
    }

    heading("7. If every solve were fast", "Every run list solving in 50ms, the fleet from section 6.");
    let mut fast = near.clone();
    fast.set("fast", "50ms")?;
    fast.set("slow", "50ms")?;
    sweep(&fast, vec![("splay", &fmt_secs(fast.splay))], "collateral")?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.first().map(String::as_str) {
        Some(c @ ("run" | "sweep" | "limit" | "report" | "params")) => (c, &args[1..]),
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return;
        }
        _ => ("run", &args[..]),
    };

    let result = (|| -> Result<(), String> {
        if command == "params" {
            print_params();
            return Ok(());
        }
        let parsed = parse_args(rest)?;
        match command {
            "run" => print_run(&parsed.config, parsed.seeds.unwrap_or(1), parsed.timeline),
            "sweep" => {
                let options = SweepOptions {
                    seeds: parsed.seeds.unwrap_or(5),
                    worst: parsed.worst.unwrap_or(false),
                    metric: parsed.metric.unwrap_or_else(|| "collateral".into()),
                };
                print_sweep(&parsed.config, &parsed.varies, &options)?;
            }
            "limit" => {
                let find = parsed.find.ok_or("limit needs --find <parameter>")?;
                let (low, high) = match (parsed.range, find.as_str()) {
                    (Some(range), _) => range,
                    (None, "bad_fraction") => (0.0, 0.9),
                    (None, "nodes") => (1_000.0, 400_000.0),
                    (None, "fast") | (None, "slow") => (0.05, 60.0),
                    (None, _) => return Err(format!("give --range lo,hi for {find}")),
                };
                let options = LimitOptions {
                    seeds: parsed.seeds.unwrap_or(3),
                    worst: parsed.worst.unwrap_or(true),
                    metric: parsed.metric.unwrap_or_else(|| "worst_minute".into()),
                    max: parsed.max.unwrap_or(10.0),
                    find,
                    low,
                    high,
                };
                print_limit(&parsed.config, &parsed.varies, &options)?;
            }
            "report" => report(&parsed.config, parsed.seeds.unwrap_or(5))?,
            _ => unreachable!(),
        }
        Ok(())
    })();

    if let Err(message) = result {
        eprintln!("simulate: {message}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single erchef frontend with erchef's own defaults (5 workers, no queue), unless overridden.
    fn config(settings: &[(&str, &str)]) -> Config {
        let mut cfg = Config::default();
        cfg.workers = 5;
        cfg.frontends = 1;
        cfg.pooler_wait = 0.0;
        for (key, value) in settings {
            cfg.set(key, value).unwrap();
        }
        cfg.validate().unwrap();
        cfg
    }

    #[test]
    fn erlang_b_matches_known_values() {
        assert!((erlang_b(1, 1.0) - 0.5).abs() < 1e-12);
        assert!((erlang_b(2, 1.0) - 0.2).abs() < 1e-12);
        assert!((erlang_b(5, 4.0) - 0.19907).abs() < 1e-4);
    }

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_secs("30m").unwrap(), 1800.0);
        assert_eq!(parse_secs("500ms").unwrap(), 0.5);
        assert_eq!(parse_secs("2h").unwrap(), 7200.0);
        assert_eq!(parse_secs("7").unwrap(), 7.0);
        assert!(parse_secs("5x").is_err());
    }

    #[test]
    fn chef_backoff_stays_in_chefs_range() {
        let mut rng = Rng::new(3);
        for attempts in 1..=5u32 {
            for _ in 0..200 {
                let delay = chef_backoff(attempts, &mut rng);
                let low = 1.0 + 2f64.powi(attempts as i32);
                assert!(delay >= low && delay < low + 2f64.powi(attempts as i32), "attempt {attempts}: {delay}");
            }
        }
    }

    #[test]
    fn same_seed_gives_same_result() {
        let cfg = config(&[("nodes", "500"), ("duration", "2h")]);
        let (a, b) = (simulate(&cfg).metrics, simulate(&cfg).metrics);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn simultaneous_start_serves_only_as_many_as_there_are_workers() {
        // 100 nodes all ask at the same instant for a 10s solve; 5 workers, no queue, no retries.
        let cfg = config(&[
            ("nodes", "100"),
            ("start", "synced"),
            ("splay", "0"),
            ("interval", "1h"),
            ("duration", "30m"),
            ("bad_fraction", "0"),
            ("fast", "10s"),
            ("fast_sigma", "0"),
            ("timeout", "60s"),
            ("retries", "0"),
        ]);
        let m = simulate(&cfg).metrics;
        assert_eq!(m.typical_runs, 100.0);
        assert!((m.typical_failed - 95.0).abs() < 1e-9, "failed {}%", m.typical_failed);
    }

    #[test]
    fn queueing_lets_the_herd_through_when_the_wait_is_long_enough() {
        // Same herd, but requests may wait: 5 workers x 10s each clear 100 requests in 200s.
        let cfg = config(&[
            ("nodes", "100"),
            ("start", "synced"),
            ("splay", "0"),
            ("interval", "1h"),
            ("duration", "30m"),
            ("bad_fraction", "0"),
            ("fast", "10s"),
            ("fast_sigma", "0"),
            ("timeout", "60s"),
            ("retries", "0"),
            ("pooler_wait", "300s"),
        ]);
        assert_eq!(simulate(&cfg).metrics.typical_failed, 0.0);
        let impatient = config(&[
            ("nodes", "100"), ("start", "synced"), ("splay", "0"), ("interval", "1h"), ("duration", "30m"),
            ("bad_fraction", "0"), ("fast", "10s"), ("fast_sigma", "0"), ("timeout", "60s"), ("retries", "0"),
            ("pooler_wait", "30s"),
        ]);
        // 30s of waiting is three rounds of five workers: 15 served, the other 85 give up.
        assert!((simulate(&impatient).metrics.typical_failed - 85.0).abs() < 1e-9);
    }

    #[test]
    fn random_arrivals_match_erlang_b() {
        // 1,200 nodes every 10 minutes with splay across the whole interval arrive close to randomly.
        // 2 requests/s x 2s = 4 erlangs on 5 workers, no queue and no retries.
        let cfg = config(&[
            ("nodes", "1200"),
            ("mode", "timer"),
            ("interval", "10m"),
            ("splay", "10m"),
            ("bad_fraction", "0"),
            ("fast", "2s"),
            ("fast_sigma", "0"),
            ("timeout", "60s"),
            ("retries", "0"),
            ("duration", "12h"),
        ]);
        let simulated = simulate(&cfg).metrics.typical_503 / 100.0;
        let expected = erlang_b(5, 4.0);
        assert!((simulated - expected).abs() < 0.02, "simulated {simulated:.4}, Erlang-B {expected:.4}");
    }

    #[test]
    fn a_timeout_costs_the_worker_a_restart_and_is_not_retried() {
        let cfg = config(&[
            ("nodes", "1"),
            ("bad_fraction", "1"),
            ("slow", "100s"),
            ("slow_sigma", "0"),
            ("timeout", "5s"),
            ("restart", "2s"),
            ("interval", "10m"),
            ("splay", "0"),
            ("duration", "2h"),
        ]);
        let mut sim = Sim::new(&cfg);
        sim.run();
        let runs = sim.stats.runs[1];
        assert!(runs > 5);
        assert_eq!(sim.stats.runs_failed[1], runs, "every run times out");
        assert_eq!(sim.stats.errors_412[1], runs, "one 412 per run");
        assert_eq!(sim.stats.errors_503[1], 0, "412 is never retried");
        // 2s of every 10-minute-plus cycle is spent replacing the worker: 2 / (5 workers x cycle).
        assert!(sim.stats.offline_seconds > 0.0);
    }

    #[test]
    fn workers_are_never_over_committed() {
        let cfg = config(&[("nodes", "3000"), ("bad_fraction", "0.2"), ("duration", "3h"), ("start", "synced"), ("splay", "10s")]);
        let mut sim = Sim::new(&cfg);
        sim.run();
        assert!(sim.stats.max_occupied <= cfg.workers);
    }

    #[test]
    fn slow_run_lists_hurt_ordinary_nodes_only_when_workers_run_out() {
        let calm = config(&[("nodes", "3000"), ("bad_fraction", "0.005"), ("duration", "6h")]);
        let mut swamped = calm.clone();
        swamped.set("bad_fraction", "0.5").unwrap();
        assert!(simulate(&calm).metrics.typical_failed < 1.0);
        assert!(simulate(&swamped).metrics.typical_failed > 10.0);
    }
}
