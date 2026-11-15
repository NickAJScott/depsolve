//! Differential test: on thousands of small random problems the engine must agree
//! with an exhaustive brute-force search about whether a solution exists, and every
//! solution it returns must satisfy the run list, the environment and all dependencies.
//!
//! The brute-force search shares the engine's constraint parser (which has its own
//! unit tests) but none of its solving logic.

use std::collections::BTreeMap;

use depsolve_rust::{parse_constraint, solve, Outcome, Problem, Version};
use pubgrub::Ranges;

type Dependencies = Vec<(String, Ranges<Version>)>;
/// name -> eligible (version, dependencies), after the environment filter
type Releases = BTreeMap<String, Vec<(Version, Dependencies)>>;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `low..=high`.
    fn between(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low + 1) as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.between(0, items.len() - 1)]
    }

    /// `count` distinct items, in random order.
    fn sample<T: Clone>(&mut self, items: &[T], count: usize) -> Vec<T> {
        let mut pool = items.to_vec();
        let mut chosen = Vec::new();
        for _ in 0..count.min(pool.len()) {
            let index = self.between(0, pool.len() - 1);
            chosen.push(pool.swap_remove(index));
        }
        chosen
    }
}

fn random_problem(rng: &mut Rng) -> Problem {
    const POOL: [&str; 7] = ["1.0.0", "1.1.0", "1.2.0", "1.2.5", "1.3.0", "2.0.0", "2.1.0"];
    const CONSTRAINTS: [&str; 10] =
        ["", "= 1.1.0", "= 2.0.0", ">= 1.2.0", "> 1.2.0", "< 1.2.0", "<= 1.2.5", "~> 1.1", "~> 1.2.0", "~> 2.0"];

    let names: Vec<String> = (0..rng.between(2, 6)).map(|index| format!("p{index}")).collect();
    let mut cookbooks = BTreeMap::new();
    for name in &names {
        let others: Vec<String> = names.iter().filter(|other| *other != name).cloned().collect();
        let mut versions = BTreeMap::new();
        let version_count = rng.between(1, 5);
        for version in rng.sample(&POOL, version_count) {
            let dependency_count = rng.between(0, others.len().min(3));
            let dependencies: BTreeMap<String, String> = rng
                .sample(&others, dependency_count)
                .into_iter()
                .map(|dependency| (dependency, rng.pick(&CONSTRAINTS).to_string()))
                .collect();
            versions.insert(version.to_string(), dependencies);
        }
        cookbooks.insert(name.clone(), versions);
    }
    if rng.between(0, 9) == 0 {
        // Occasionally depend on a cookbook that does not exist.
        let versions: &mut BTreeMap<String, BTreeMap<String, String>> = cookbooks.get_mut(&names[0]).unwrap();
        versions.values_mut().next().unwrap().insert("ghost".into(), String::new());
    }

    let run_list_size = rng.between(1, 2);
    let run_list = rng
        .sample(&names, run_list_size)
        .into_iter()
        .map(|name| (name, rng.pick(&CONSTRAINTS).to_string()))
        .collect();
    let environment_size = rng.between(0, 2);
    let environment = rng
        .sample(&names, environment_size)
        .into_iter()
        .map(|name| (name, rng.pick(&CONSTRAINTS).to_string()))
        .collect();
    Problem { cookbooks, run_list, environment, timeout_ms: None }
}

fn eligible_releases(problem: &Problem) -> Releases {
    let mut releases = Releases::new();
    for (name, versions) in &problem.cookbooks {
        let allowed = problem.environment.get(name).map(|raw| parse_constraint(raw).unwrap());
        let entries = releases.entry(name.clone()).or_default();
        for (raw_version, dependencies) in versions {
            let version = Version::parse(raw_version).unwrap();
            if allowed.as_ref().is_some_and(|ranges| !ranges.contains(&version)) {
                continue;
            }
            let parsed = dependencies
                .iter()
                .map(|(dependency, raw)| (dependency.clone(), parse_constraint(raw).unwrap()))
                .collect();
            entries.push((version, parsed));
        }
    }
    releases
}

/// Exhaustive chronological backtracking with no learning.
fn brute_force(
    releases: &Releases,
    requirements: &BTreeMap<String, Vec<Ranges<Version>>>,
    selected: &mut BTreeMap<String, Version>,
) -> bool {
    let Some(name) = requirements.keys().find(|name| !selected.contains_key(*name)) else {
        return true;
    };
    for (version, dependencies) in releases.get(name).map(Vec::as_slice).unwrap_or_default() {
        if !requirements[name].iter().all(|ranges| ranges.contains(version)) {
            continue;
        }
        let consistent = dependencies
            .iter()
            .all(|(dependency, ranges)| selected.get(dependency).is_none_or(|chosen| ranges.contains(chosen)));
        if !consistent {
            continue;
        }
        let mut next = requirements.clone();
        for (dependency, ranges) in dependencies {
            next.entry(dependency.clone()).or_default().push(ranges.clone());
        }
        selected.insert(name.clone(), *version);
        if brute_force(releases, &next, selected) {
            return true;
        }
        selected.remove(name);
    }
    false
}

fn violations(problem: &Problem, releases: &Releases, solution: &BTreeMap<String, String>) -> Vec<String> {
    let chosen: BTreeMap<&str, Version> =
        solution.iter().map(|(name, raw)| (name.as_str(), Version::parse(raw).unwrap())).collect();
    let mut found = Vec::new();
    for (name, raw) in &problem.run_list {
        match chosen.get(name.as_str()) {
            None => found.push(format!("run list item {name} is missing")),
            Some(version) if !parse_constraint(raw).unwrap().contains(version) => {
                found.push(format!("run list wants {name} {raw}, got {version}"));
            }
            Some(_) => {}
        }
    }
    for (name, version) in &chosen {
        let release = releases.get(*name).and_then(|entries| entries.iter().find(|(candidate, _)| candidate == version));
        let Some((_, dependencies)) = release else {
            found.push(format!("{name} {version} does not exist or is excluded by the environment"));
            continue;
        };
        for (dependency, ranges) in dependencies {
            match chosen.get(dependency.as_str()) {
                None => found.push(format!("{name} {version} needs {dependency}, which is missing")),
                Some(got) if !ranges.contains(got) => {
                    found.push(format!("{name} {version} wants {dependency} {ranges}, got {got}"));
                }
                Some(_) => {}
            }
        }
    }
    found
}

#[test]
fn engine_agrees_with_brute_force_on_random_problems() {
    let mut rng = Rng(0x2026_1115_dead_beef);
    let (mut solvable, mut unsolvable) = (0, 0);

    for case in 0..3000 {
        let problem = random_problem(&mut rng);
        let releases = eligible_releases(&problem);
        let requirements = problem
            .run_list
            .iter()
            .map(|(name, raw)| (name.clone(), vec![parse_constraint(raw).unwrap()]))
            .collect();
        let expected = brute_force(&releases, &requirements, &mut BTreeMap::new());
        let context = || serde_json::to_string(&problem).unwrap();

        match solve(&problem) {
            Outcome::Ok { solution } => {
                assert!(expected, "case {case}: engine found {solution:?}, brute force found nothing\n{}", context());
                let problems = violations(&problem, &releases, &solution);
                assert!(problems.is_empty(), "case {case}: invalid solution {solution:?}: {problems:?}\n{}", context());
                solvable += 1;
            }
            Outcome::Unsolvable { .. } | Outcome::InvalidRunList { .. } => {
                assert!(!expected, "case {case}: engine found nothing, brute force found a solution\n{}", context());
                unsolvable += 1;
            }
            other => panic!("case {case}: unexpected outcome {other:?}\n{}", context()),
        }
    }

    // Guard against a generator that only produces one kind of problem.
    assert!(solvable > 300 && unsolvable > 300, "solvable={solvable} unsolvable={unsolvable}");
}
